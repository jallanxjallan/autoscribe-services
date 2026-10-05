use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

const ROOT_CONFIG_FILES: &[&str] = &[
    "app.json",
    "appearance.json",
    "core-plugins.json",
    "community-plugins.json",
    "hotkeys.json",
    "templates.json",
];

const SOURCE_TREES: &[(&str, &str, bool)] = &[
    ("config", ".obsidian", true),
    ("plugins", ".obsidian/plugins", false),
    ("snippets", ".obsidian/snippets", true),
    ("themes", ".obsidian/themes", false),
    ("tools", "_tools", true),
    ("templates", "_templates", true),
    ("scripts", "_scripts", true),
    ("ui", "_ui", true),
    ("views", "_views", true),
];

const PRUNABLE_TARGET_DIRS: &[&str] = &[
    "_tools",
    "_templates",
    "_scripts",
    "_ui",
    "_views",
    ".obsidian/snippets",
];

const GITIGNORE_START: &str = "# >>> vault-tools managed ignores >>>";
const GITIGNORE_END: &str = "# <<< vault-tools managed ignores <<<";
const GITIGNORE_BLOCK: &str = r#"# >>> vault-tools managed ignores >>>
.obsidian/workspace*.json
.obsidian/cache/
.trash/
.DS_Store
**/__pycache__/
# <<< vault-tools managed ignores <<<"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Updated,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedFile {
    source_rel: PathBuf,
    target_rel: PathBuf,
    reverse: bool,
}

#[derive(Debug)]
pub struct InitReport {
    pub remote: PathBuf,
    pub branch: String,
    pub initialized: bool,
}

pub fn ensure_vault_root(path: &Path) -> Result<()> {
    if !path.join(".obsidian").is_dir() {
        bail!(
            "not an Obsidian vault root: {} (expected .obsidian/ in the current directory)",
            path.display()
        );
    }
    Ok(())
}

pub fn ensure_source_root(path: &Path) -> Result<()> {
    if !path.is_dir() {
        bail!("vault source directory does not exist: {}", path.display());
    }

    let files = collect_source_files(path)?;
    if files.is_empty() {
        bail!(
            "vault source contains no managed material: {} (expected config/, plugins/, tools/, templates/, scripts/, snippets/, themes/, ui/, views/, or supported root JSON files)",
            path.display()
        );
    }

    Ok(())
}

pub fn resolve_master(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        ensure_source_root(&path)
            .with_context(|| format!("invalid --master path {}", path.display()))?;
        return Ok(path);
    }

    for variable in ["OBSIDIAN_VAULT_SOURCE", "OBSIDIAN_MASTER_VAULT"] {
        if let Some(path) = env::var_os(variable).map(PathBuf::from) {
            ensure_source_root(&path)
                .with_context(|| format!("invalid {variable} {}", path.display()))?;
            return Ok(path);
        }
    }

    let home = home_dir()?;
    let candidates = [
        home.join("Tools/vault"),
        home.join("Work/client/obsidian"),
    ];

    for candidate in candidates {
        if ensure_source_root(&candidate).is_ok() {
            return Ok(candidate);
        }
    }

    bail!(
        "cannot find the canonical Obsidian source library; expected ~/Tools/vault, or set OBSIDIAN_VAULT_SOURCE / pass --master"
    )
}

pub fn resolve_repos_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if !path.is_dir() {
            bail!("Dropbox repos root does not exist: {}", path.display());
        }
        return Ok(path);
    }

    if let Some(path) = env::var_os("VAULT_REPOS_ROOT").map(PathBuf::from) {
        if !path.is_dir() {
            bail!("VAULT_REPOS_ROOT does not exist: {}", path.display());
        }
        return Ok(path);
    }

    let path = home_dir()?.join("Dropbox/Repos");
    if !path.is_dir() {
        bail!(
            "Dropbox repos root does not exist: {}; set VAULT_REPOS_ROOT or pass --repos-root",
            path.display()
        );
    }
    Ok(path)
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn snake_case_folder_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("vault directory has no valid UTF-8 folder name")?;

    let mut out = String::new();
    let mut pending_underscore = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_underscore && !out.is_empty() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            pending_underscore = false;
        } else {
            pending_underscore = true;
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        bail!("vault folder name cannot be converted to a repository name");
    }

    Ok(out)
}

pub fn sync_master_to_vault(source: &Path, vault: &Path, prune: bool) -> Result<Vec<Change>> {
    ensure_source_root(source)?;
    ensure_vault_root(vault)?;
    ensure_distinct(source, vault)?;

    let mappings = collect_source_files(source)?;
    let mut changes = Vec::new();

    for mapping in &mappings {
        let from = source.join(&mapping.source_rel);
        let to = vault.join(&mapping.target_rel);

        if file_differs(&from, &to)? {
            let kind = if to.exists() {
                ChangeKind::Updated
            } else {
                ChangeKind::Added
            };
            copy_file_atomic(&from, &to, vault)?;
            changes.push(Change {
                kind,
                path: mapping.target_rel.clone(),
            });
        }
    }

    if prune {
        let managed_targets = mappings
            .iter()
            .map(|mapping| mapping.target_rel.clone())
            .collect::<BTreeSet<_>>();

        for rel in collect_prunable_target_files(vault)? {
            if !managed_targets.contains(&rel) {
                let target = vault.join(&rel);
                ensure_safe_destination(vault, &target)?;
                fs::remove_file(&target)
                    .with_context(|| format!("failed to remove {}", target.display()))?;
                changes.push(Change {
                    kind: ChangeKind::Removed,
                    path: rel,
                });
            }
        }
    }

    Ok(changes)
}

pub fn propagate_vault_to_master(
    vault: &Path,
    source: &Path,
    apply: bool,
    allow_sensitive: bool,
) -> Result<Vec<Change>> {
    ensure_vault_root(vault)?;
    ensure_source_root(source)?;
    ensure_distinct(vault, source)?;

    if apply {
        ensure_git_clean_for_path(source)?;
    }

    let mappings = collect_source_files(source)?;
    let mut reverse = BTreeMap::<PathBuf, PathBuf>::new();

    for mapping in &mappings {
        if mapping.reverse {
            reverse.insert(mapping.target_rel.clone(), mapping.source_rel.clone());
        }
    }

    add_new_reverse_candidates(vault, source, &mut reverse)?;

    let mut changes = Vec::new();

    for (target_rel, source_rel) in reverse {
        let from = vault.join(&target_rel);
        if !from.is_file() {
            continue;
        }

        let to = source.join(&source_rel);
        if !file_differs(&from, &to)? {
            continue;
        }

        if !allow_sensitive {
            ensure_no_sensitive_material(&from, &target_rel)?;
        }

        changes.push(Change {
            kind: if to.exists() {
                ChangeKind::Updated
            } else {
                ChangeKind::Added
            },
            path: source_rel.clone(),
        });

        if apply {
            copy_file_atomic(&from, &to, source)?;
        }
    }

    // Reverse propagation is deliberately additive/update-only. A project vault
    // can never delete canonical source material.
    Ok(changes)
}

pub fn initialize_backup_repo(vault: &Path, repos_root: &Path) -> Result<InitReport> {
    ensure_vault_root(vault)?;

    if !repos_root.is_dir() {
        bail!("Dropbox repos root does not exist: {}", repos_root.display());
    }

    let repo_name = format!("{}.git", snake_case_folder_name(vault)?);
    let remote = repos_root.join(repo_name);

    let existing_root = git_root(vault)?;
    let initialized = match existing_root {
        Some(root) => {
            if !same_existing_path(&root, vault)? {
                bail!(
                    "vault is nested inside another Git repository: {}",
                    root.display()
                );
            }
            false
        }
        None => {
            if remote.exists() {
                bail!(
                    "backup remote already exists but this vault is not a Git repository: {}",
                    remote.display()
                );
            }
            run_git(vault, &["init", "-b", "main"])?;
            true
        }
    };

    write_managed_gitignore(vault)?;

    // New vaults are initialized on main, but an existing vault may legitimately
    // still use another branch name. Make a newly-created bare backup agree with
    // the local repository so a later clone has a valid HEAD.
    let branch = current_branch(vault)?;

    let remote_created = if remote.exists() {
        ensure_bare_repo(&remote)?;
        false
    } else {
        let status = Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg(format!("--initial-branch={branch}"))
            .arg(&remote)
            .status()
            .with_context(|| format!("failed to create bare repository {}", remote.display()))?;
        if !status.success() {
            bail!("git init --bare failed for {}", remote.display());
        }
        true
    };

    ensure_origin(vault, &remote)?;

    if initialized {
        run_git(vault, &["add", "-A"])?;
        if has_staged_changes(vault)? {
            run_git(vault, &["commit", "-m", "Initialize vault"])?;
        } else {
            bail!("new vault repository has nothing to commit");
        }
        push_current_branch(vault)?;
    } else if remote_created && has_head(vault)? {
        push_current_branch(vault)?;
    }

    Ok(InitReport {
        remote,
        branch,
        initialized,
    })
}

pub fn print_changes(label: &str, changes: &[Change]) {
    if changes.is_empty() {
        println!("{label}: already current");
        return;
    }

    println!("{label}:");
    for change in changes {
        let marker = match change.kind {
            ChangeKind::Added => "+",
            ChangeKind::Updated => "~",
            ChangeKind::Removed => "-",
        };
        println!("  {marker} {}", change.path.display());
    }
}

fn collect_source_files(source: &Path) -> Result<Vec<ManagedFile>> {
    if !source.is_dir() {
        return Ok(Vec::new());
    }

    let mut by_target = BTreeMap::<PathBuf, ManagedFile>::new();

    for name in ROOT_CONFIG_FILES {
        let source_rel = PathBuf::from(name);
        let path = source.join(&source_rel);
        if path.exists() {
            validate_source_file(&path)?;
            let mapping = ManagedFile {
                source_rel,
                target_rel: PathBuf::from(".obsidian").join(name),
                reverse: true,
            };
            insert_mapping(&mut by_target, mapping)?;
        }
    }

    for (source_dir, target_dir, reverse_tree) in SOURCE_TREES {
        let root = source.join(source_dir);
        if !root.exists() {
            continue;
        }

        let meta = fs::symlink_metadata(&root)
            .with_context(|| format!("failed to inspect {}", root.display()))?;
        if meta.file_type().is_symlink() {
            bail!("managed source directory may not be a symlink: {}", root.display());
        }
        if !meta.is_dir() {
            bail!("managed source path is not a directory: {}", root.display());
        }

        let files = collect_relative_files(&root)?;
        for rel in files {
            if is_auxiliary_source_file(&rel) {
                continue;
            }

            let target_rel = PathBuf::from(target_dir).join(&rel);
            if is_forbidden_target_relative(&target_rel) {
                continue;
            }

            let reverse = *reverse_tree
                || (*source_dir == "plugins"
                    && rel.file_name() == Some(OsStr::new("data.json")));

            let mapping = ManagedFile {
                source_rel: PathBuf::from(source_dir).join(&rel),
                target_rel,
                reverse,
            };
            insert_mapping(&mut by_target, mapping)?;
        }
    }

    Ok(by_target.into_values().collect())
}

fn insert_mapping(
    mappings: &mut BTreeMap<PathBuf, ManagedFile>,
    mapping: ManagedFile,
) -> Result<()> {
    if let Some(previous) = mappings.get(&mapping.target_rel) {
        bail!(
            "two source files map to the same vault path {}: {} and {}",
            mapping.target_rel.display(),
            previous.source_rel.display(),
            mapping.source_rel.display()
        );
    }
    mappings.insert(mapping.target_rel.clone(), mapping);
    Ok(())
}

fn validate_source_file(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!("managed source file may not be a symlink: {}", path.display());
    }
    if !meta.is_file() {
        bail!("managed source path is not a file: {}", path.display());
    }
    Ok(())
}

fn collect_relative_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_relative_files_inner(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_relative_files_inner(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;

        if meta.file_type().is_symlink() {
            bail!("managed source path may not be a symlink: {}", path.display());
        }

        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("{} escaped {}", path.display(), root.display()))?
            .to_path_buf();

        if has_forbidden_component(&rel) {
            continue;
        }

        if meta.is_dir() {
            collect_relative_files_inner(root, &path, files)?;
        } else if meta.is_file() {
            files.push(rel);
        }
    }

    Ok(())
}

fn collect_prunable_target_files(vault: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut result = BTreeSet::new();
    for target_dir in PRUNABLE_TARGET_DIRS {
        let root = vault.join(target_dir);
        if !root.is_dir() {
            continue;
        }
        for rel in collect_relative_files(&root)? {
            let target_rel = PathBuf::from(target_dir).join(rel);
            if !is_forbidden_target_relative(&target_rel) {
                result.insert(target_rel);
            }
        }
    }
    Ok(result)
}

fn add_new_reverse_candidates(
    vault: &Path,
    source: &Path,
    reverse: &mut BTreeMap<PathBuf, PathBuf>,
) -> Result<()> {
    // _tools and the other explicitly-managed editing trees are safe places for
    // new reusable files created inside a project vault.
    for (source_dir, target_dir, reverse_tree) in SOURCE_TREES {
        if !*reverse_tree || *source_dir == "config" {
            continue;
        }

        let target_root = vault.join(target_dir);
        if !target_root.is_dir() {
            continue;
        }

        for rel in collect_relative_files(&target_root)? {
            let target_rel = PathBuf::from(target_dir).join(&rel);
            if is_forbidden_target_relative(&target_rel) {
                continue;
            }
            reverse
                .entry(target_rel)
                .or_insert_with(|| PathBuf::from(source_dir).join(rel));
        }
    }

    // Supported top-level Obsidian JSON configuration is captured into config/
    // unless an existing source file already owns the target path.
    for name in ROOT_CONFIG_FILES {
        let target_rel = PathBuf::from(".obsidian").join(name);
        if reverse.contains_key(&target_rel) {
            continue;
        }
        if vault.join(&target_rel).is_file() {
            reverse.insert(target_rel, PathBuf::from("config").join(name));
        }
    }

    // Plugin executables are installation artifacts and never flow back from a
    // vault. Plugin data.json is reusable configuration and may be captured for
    // plugins known to the source library.
    let plugin_root = vault.join(".obsidian/plugins");
    if plugin_root.is_dir() {
        let mut entries = fs::read_dir(&plugin_root)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let plugin_id = entry.file_name();
            let Some(plugin_id_str) = plugin_id.to_str() else {
                continue;
            };
            let target_rel = PathBuf::from(".obsidian/plugins")
                .join(plugin_id_str)
                .join("data.json");

            if reverse.contains_key(&target_rel) || !vault.join(&target_rel).is_file() {
                continue;
            }

            let known_plugin = source.join("plugins").join(plugin_id_str).is_dir()
                || source
                    .join("config/plugins")
                    .join(plugin_id_str)
                    .exists();

            if known_plugin {
                reverse.insert(
                    target_rel,
                    PathBuf::from("config/plugins")
                        .join(plugin_id_str)
                        .join("data.json"),
                );
            }
        }
    }

    Ok(())
}

fn is_auxiliary_source_file(rel: &Path) -> bool {
    matches!(
        rel.file_name().and_then(OsStr::to_str),
        Some("README.md" | "AGENTS.md")
    )
}

fn is_forbidden_target_relative(rel: &Path) -> bool {
    let normalized = slash_path(rel);

    normalized == ".DS_Store"
        || normalized.ends_with("/.DS_Store")
        || normalized.starts_with(".obsidian/workspace")
        || normalized == ".obsidian/cache"
        || normalized.starts_with(".obsidian/cache/")
        || has_forbidden_component(rel)
}

fn has_forbidden_component(rel: &Path) -> bool {
    rel.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if name == OsStr::new(".git")
                    || name == OsStr::new("node_modules")
                    || name == OsStr::new("__pycache__")
                    || name == OsStr::new(".cache")
        )
    })
}

fn ensure_distinct(a: &Path, b: &Path) -> Result<()> {
    if same_existing_path(a, b)? {
        bail!("source library and destination vault are the same directory");
    }
    Ok(())
}

fn same_existing_path(a: &Path, b: &Path) -> Result<bool> {
    let a = fs::canonicalize(a)
        .with_context(|| format!("failed to resolve {}", a.display()))?;
    let b = fs::canonicalize(b)
        .with_context(|| format!("failed to resolve {}", b.display()))?;
    Ok(a == b)
}

fn file_differs(source: &Path, target: &Path) -> Result<bool> {
    if !target.exists() {
        return Ok(true);
    }

    let source_meta = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    if source_meta.file_type().is_symlink() || !source_meta.is_file() {
        bail!("managed source is not a regular file: {}", source.display());
    }

    let target_meta = fs::symlink_metadata(target)
        .with_context(|| format!("failed to inspect {}", target.display()))?;
    if target_meta.file_type().is_symlink() {
        bail!("refusing to read or overwrite symlink {}", target.display());
    }
    if !target_meta.is_file() {
        bail!("refusing to read or overwrite non-file {}", target.display());
    }

    if source_meta.len() != target_meta.len() {
        return Ok(true);
    }

    Ok(fs::read(source)? != fs::read(target)?)
}

fn copy_file_atomic(source: &Path, target: &Path, destination_root: &Path) -> Result<()> {
    ensure_safe_destination(destination_root, target)?;

    let parent = target
        .parent()
        .context("managed target has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;

    ensure_safe_destination(destination_root, target)?;

    let tmp = parent.join(format!(".vault-tools.{}.tmp", std::process::id()));
    if tmp.exists() {
        fs::remove_file(&tmp)
            .with_context(|| format!("failed to remove stale {}", tmp.display()))?;
    }

    fs::copy(source, &tmp).with_context(|| {
        format!(
            "failed to copy {} to temporary file {}",
            source.display(),
            tmp.display()
        )
    })?;

    let permissions = fs::metadata(source)?.permissions();
    fs::set_permissions(&tmp, permissions)?;

    fs::rename(&tmp, target).with_context(|| {
        format!(
            "failed to atomically replace {} from {}",
            target.display(),
            source.display()
        )
    })?;

    Ok(())
}

fn ensure_safe_destination(root: &Path, target: &Path) -> Result<()> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve destination root {}", root.display()))?;

    let relative = target
        .strip_prefix(&root)
        .or_else(|_| {
            // target may not exist yet, while callers supplied a non-canonical
            // spelling of root. Retry with the original path relationship.
            target.strip_prefix(root.as_path())
        })
        .with_context(|| format!("target escaped destination root: {}", target.display()))?;

    let mut current = root;
    let components = relative.components().collect::<Vec<_>>();

    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            bail!("unsafe destination path: {}", target.display());
        };

        current.push(part);

        if index + 1 == components.len() {
            if let Ok(meta) = fs::symlink_metadata(&current) {
                if meta.file_type().is_symlink() {
                    bail!("refusing to write through symlink {}", current.display());
                }
            }
            break;
        }

        if let Ok(meta) = fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                bail!("refusing to traverse symlink {}", current.display());
            }
            if !meta.is_dir() {
                bail!("destination parent is not a directory: {}", current.display());
            }
        }
    }

    Ok(())
}

fn ensure_no_sensitive_material(path: &Path, rel: &Path) -> Result<()> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;

    if rel.extension().and_then(OsStr::to_str) == Some("json") {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(key) = find_sensitive_json_key(&value) {
                bail!(
                    "refusing to propagate {}: JSON key '{}' appears to contain credentials; move the secret out of the vault or re-run with --allow-sensitive",
                    rel.display(),
                    key
                );
            }
        }
    }

    if bytes.len() <= 1_048_576 && is_text_config(rel) {
        let text = String::from_utf8_lossy(&bytes);
        const MARKERS: &[&str] = &[
            "-----BEGIN PRIVATE KEY-----",
            "github_pat_",
            "ghp_",
            "xoxb-",
            "xoxp-",
        ];
        for marker in MARKERS {
            if text.contains(marker) {
                bail!(
                    "refusing to propagate {}: content resembles a credential ('{}'); re-run with --allow-sensitive only if this is intentional",
                    rel.display(),
                    marker
                );
            }
        }
    }

    Ok(())
}

fn is_text_config(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("json" | "yaml" | "yml" | "toml" | "txt" | "md" | "js" | "css")
    )
}

fn find_sensitive_json_key(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let normalized = key
                    .chars()
                    .filter(|ch| ch.is_ascii_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect::<String>();

                if is_sensitive_key(&normalized) && value_has_material(value) {
                    return Some(key.clone());
                }

                if let Some(found) = find_sensitive_json_key(value) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => values.iter().find_map(find_sensitive_json_key),
        _ => None,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    matches!(
        key,
        "apikey"
            | "accesstoken"
            | "authtoken"
            | "bearertoken"
            | "clientsecret"
            | "credential"
            | "credentials"
            | "password"
            | "privatekey"
            | "refreshtoken"
            | "secret"
            | "token"
    )
}

fn value_has_material(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(_) => true,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => values.iter().any(value_has_material),
        Value::Object(values) => values.values().any(value_has_material),
    }
}

fn git_root(path: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to invoke git")?;

    if !output.status.success() {
        return Ok(None);
    }

    let root = String::from_utf8(output.stdout)
        .context("git returned a non-UTF-8 repository path")?;
    Ok(Some(PathBuf::from(root.trim())))
}

fn ensure_git_clean_for_path(path: &Path) -> Result<()> {
    let root = git_root(path)?
        .with_context(|| format!("vault source is not inside a Git repository: {}", path.display()))?;

    let canonical_root = fs::canonicalize(&root)?;
    let canonical_path = fs::canonicalize(path)?;
    let relative = canonical_path
        .strip_prefix(&canonical_root)
        .with_context(|| format!("{} is outside repository {}", path.display(), root.display()))?;

    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain", "--untracked-files=all", "--"])
        .arg(relative)
        .output()
        .context("failed to inspect vault source Git status")?;

    if !output.status.success() {
        bail!("failed to inspect vault source Git status");
    }

    if !output.stdout.is_empty() {
        bail!(
            "vault source has uncommitted changes under {}; commit or discard them before applying reverse propagation",
            path.display()
        );
    }
    Ok(())
}

fn ensure_bare_repo(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-bare-repository"])
        .output()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        bail!("existing backup path is not a bare Git repository: {}", path.display());
    }
    Ok(())
}

fn ensure_origin(vault: &Path, remote: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(vault)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("failed to inspect Git origin")?;

    if output.status.success() {
        let existing = String::from_utf8(output.stdout)
            .context("Git origin is not valid UTF-8")?;
        let existing = PathBuf::from(existing.trim());
        if existing.exists() && remote.exists() && same_existing_path(&existing, remote)? {
            return Ok(());
        }
        if existing == remote {
            return Ok(());
        }
        bail!(
            "origin already points somewhere else: {} (expected {})",
            existing.display(),
            remote.display()
        );
    }

    let status = Command::new("git")
        .arg("-C")
        .arg(vault)
        .args(["remote", "add", "origin"])
        .arg(remote)
        .status()
        .context("failed to add Git origin")?;
    if !status.success() {
        bail!("failed to add Git origin {}", remote.display());
    }
    Ok(())
}

fn write_managed_gitignore(vault: &Path) -> Result<()> {
    let path = vault.join(".gitignore");
    let mut content = if path.exists() {
        fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };

    if let Some(start) = content.find(GITIGNORE_START) {
        let after_start = start + GITIGNORE_START.len();
        let end_rel = content[after_start..]
            .find(GITIGNORE_END)
            .context("malformed vault-tools block in .gitignore")?;
        let end = after_start + end_rel + GITIGNORE_END.len();
        content.replace_range(start..end, GITIGNORE_BLOCK);
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(GITIGNORE_BLOCK);
    }

    if !content.ends_with('\n') {
        content.push('\n');
    }

    let mut file = fs::File::create(&path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

fn current_branch(vault: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(vault)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .context("failed to determine Git branch")?;
    if !output.status.success() {
        bail!("vault repository is not on a branch");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn has_head(vault: &Path) -> Result<bool> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(vault)
        .args(["rev-parse", "--verify", "HEAD"])
        .status()
        .context("failed to inspect Git HEAD")?
        .success())
}

fn has_staged_changes(vault: &Path) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(vault)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .context("failed to inspect staged changes")?;

    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!("git diff --cached failed"),
    }
}

fn push_current_branch(vault: &Path) -> Result<()> {
    run_git(vault, &["push", "-u", "origin", "HEAD"])?;
    Ok(())
}

fn run_git(vault: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .arg("-C")
        .arg(vault)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(output)
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_vault(root: &Path) {
        fs::create_dir_all(root.join(".obsidian")).unwrap();
    }

    fn make_source(root: &Path) {
        fs::create_dir_all(root.join("plugins/example")).unwrap();
        fs::create_dir_all(root.join("tools/templates")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(root.join("plugins/example/main.js"), b"plugin").unwrap();
        fs::write(root.join("tools/templates/article.md"), b"template").unwrap();
        fs::write(root.join("config/hotkeys.json"), b"{\"x\":1}").unwrap();
    }

    #[test]
    fn folder_name_becomes_snake_case() {
        assert_eq!(
            snake_case_folder_name(Path::new("/tmp/HHP Law Firm")).unwrap(),
            "hhp_law_firm"
        );
        assert_eq!(
            snake_case_folder_name(Path::new("/tmp/One--Man  Airforce")).unwrap(),
            "one_man_airforce"
        );
    }

    #[test]
    fn source_library_does_not_need_obsidian_directory() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        make_source(&source);
        make_vault(&target);

        let changes = sync_master_to_vault(&source, &target, false).unwrap();

        assert_eq!(
            fs::read(target.join(".obsidian/hotkeys.json")).unwrap(),
            b"{\"x\":1}"
        );
        assert_eq!(
            fs::read(target.join(".obsidian/plugins/example/main.js")).unwrap(),
            b"plugin"
        );
        assert_eq!(
            fs::read(target.join("_tools/templates/article.md")).unwrap(),
            b"template"
        );
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn source_workspace_state_is_never_copied() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        make_source(&source);
        make_vault(&target);

        fs::write(source.join("config/workspace.json"), b"source").unwrap();
        fs::write(target.join(".obsidian/workspace.json"), b"target").unwrap();

        sync_master_to_vault(&source, &target, false).unwrap();

        assert_eq!(
            fs::read(target.join(".obsidian/workspace.json")).unwrap(),
            b"target"
        );
    }

    #[test]
    fn reverse_sync_updates_config_and_tools_but_not_plugin_binary() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        make_source(&source);
        make_vault(&target);
        sync_master_to_vault(&source, &target, false).unwrap();

        fs::write(target.join(".obsidian/hotkeys.json"), b"{\"x\":2}").unwrap();
        fs::write(target.join(".obsidian/plugins/example/main.js"), b"changed plugin").unwrap();
        fs::write(target.join("_tools/templates/article.md"), b"changed template").unwrap();
        fs::write(target.join("chapter.md"), b"content").unwrap();

        let changes = propagate_vault_to_master(&target, &source, false, false).unwrap();

        let paths = changes
            .iter()
            .map(|change| slash_path(&change.path))
            .collect::<BTreeSet<_>>();

        assert!(paths.contains("config/hotkeys.json"));
        assert!(paths.contains("tools/templates/article.md"));
        assert!(!paths.contains("plugins/example/main.js"));
        assert!(!paths.contains("chapter.md"));
    }

    #[test]
    fn reverse_sync_can_add_plugin_data_for_known_plugin() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        make_source(&source);
        make_vault(&target);
        sync_master_to_vault(&source, &target, false).unwrap();

        fs::write(
            target.join(".obsidian/plugins/example/data.json"),
            br#"{"setting":true}"#,
        )
        .unwrap();

        let changes = propagate_vault_to_master(&target, &source, false, false).unwrap();

        assert!(changes.iter().any(|change| {
            change.path == PathBuf::from("config/plugins/example/data.json")
                && change.kind == ChangeKind::Added
        }));
    }

    #[test]
    fn reverse_sync_rejects_nonempty_secret_fields() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        make_source(&source);
        make_vault(&target);
        sync_master_to_vault(&source, &target, false).unwrap();

        fs::write(
            target.join(".obsidian/plugins/example/data.json"),
            br#"{"apiKey":"do-not-copy"}"#,
        )
        .unwrap();

        let error =
            propagate_vault_to_master(&target, &source, false, false).unwrap_err();
        assert!(error.to_string().contains("apiKey"));
    }

    #[test]
    fn auxiliary_readmes_are_not_materialized() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        make_source(&source);
        make_vault(&target);

        fs::write(source.join("tools/templates/README.md"), b"docs").unwrap();
        sync_master_to_vault(&source, &target, false).unwrap();

        assert!(!target.join("_tools/templates/README.md").exists());
    }
}
