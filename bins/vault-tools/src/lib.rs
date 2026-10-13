use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
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
    let candidates = [home.join(".config/obsidian-vault")];

    for candidate in candidates {
        if ensure_source_root(&candidate).is_ok() {
            return Ok(candidate);
        }
    }

    bail!(
        "cannot find the canonical Obsidian source library; expected ~/.config/obsidian-vault, or set OBSIDIAN_VAULT_SOURCE / pass --master"
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
    validate_installation_source(source, &mappings)?;
    let mut changes = Vec::new();

    for mapping in &mappings {
        let from = source.join(&mapping.source_rel);
        let to = vault.join(&mapping.target_rel);

        let desired = effective_bytes(&from, &to, &mapping.target_rel, source, vault)?;
        if !to.exists() || fs::read(&to)? != desired {
            let kind = if to.exists() {
                ChangeKind::Updated
            } else {
                ChangeKind::Added
            };
            if !AUTHORING_AUDIT.with(|v| v.get()) {
                write_bytes_atomic(&desired, &to, vault)?;
            }
            changes.push(Change {
                kind,
                path: mapping.target_rel.clone(),
            });
        }
    }

    let retired = retired_paths(source)?;
    for rel in retired {
        if vault.join(&rel).is_file() {
            changes.push(Change {
                kind: ChangeKind::Removed,
                path: rel.clone(),
            });
            if prune && !AUTHORING_AUDIT.with(|v| v.get()) {
                ensure_safe_destination(vault, &vault.join(&rel))?;
                fs::remove_file(vault.join(&rel))?;
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

    // Inspect known plugin configuration for credentials even if not capture-whitelisted.
    if !allow_sensitive {
        for mapping in collect_source_files(source)? {
            if mapping.target_rel.ends_with("manifest.json") {
                let rel = mapping.target_rel.parent().unwrap().join("data.json");
                if vault.join(&rel).is_file() {
                    ensure_no_sensitive_material(&vault.join(&rel), &rel)?;
                }
            }
        }
    }
    let mappings = collect_source_files(source)?;
    let mut reverse = BTreeMap::<PathBuf, PathBuf>::new();

    for mapping in &mappings {
        if mapping.reverse {
            reverse.insert(mapping.target_rel.clone(), mapping.source_rel.clone());
        }
    }

    // Reverse capture is an explicit existing-resource whitelist. Project additions require source review.
    reverse.retain(|target, _| {
        !target.starts_with("_tools/templates")
            && target != Path::new(".obsidian/authoring.json")
            && target != Path::new(".obsidian/managed-resources.json")
    });

    let mut changes = Vec::new();

    for (target_rel, source_rel) in reverse {
        let from = vault.join(&target_rel);
        if !from.is_file() {
            continue;
        }

        let to = source.join(&source_rel);
        let desired = reverse_bytes(&from, &to, &target_rel)?;
        if to.exists() && fs::read(&to)? == desired {
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
            write_bytes_atomic(&desired, &to, source)?;
        }
    }

    // Reverse propagation is deliberately additive/update-only. A project vault
    // can never delete canonical source material.
    Ok(changes)
}

pub fn initialize_backup_repo(vault: &Path, repos_root: &Path) -> Result<InitReport> {
    ensure_vault_root(vault)?;

    if !repos_root.is_dir() {
        bail!(
            "Dropbox repos root does not exist: {}",
            repos_root.display()
        );
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
        let status = git_command()
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
            bail!(
                "managed source directory may not be a symlink: {}",
                root.display()
            );
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
                || (*source_dir == "plugins" && rel.file_name() == Some(OsStr::new("data.json")));

            let mapping = ManagedFile {
                source_rel: PathBuf::from(source_dir).join(&rel),
                target_rel,
                reverse,
            };
            insert_mapping(&mut by_target, mapping)?;
        }
    }

    if source.join("config/authoring.json").exists() {
        let root = templates_root()?;
        let profile = selected_profile();
        let profiles: Value =
            serde_json::from_slice(&fs::read(root.join("obsidian/profiles.json"))?)?;
        let definition = profiles
            .get(&profile)
            .context("unknown authoring profile")?;
        let excluded = definition["exclude"]
            .as_array()
            .context("missing profile exclusions")?;
        for rel in collect_relative_files(&root.join("obsidian"))? {
            if rel.extension() != Some(OsStr::new("md"))
                || is_auxiliary_source_file(&rel)
                || excluded.iter().any(|v| v.as_str() == rel.to_str())
            {
                continue;
            }
            insert_mapping(
                &mut by_target,
                ManagedFile {
                    source_rel: root.join("obsidian").join(&rel),
                    target_rel: PathBuf::from("_tools/templates").join(rel),
                    reverse: false,
                },
            )?;
        }
    }
    Ok(by_target.into_values().collect())
}

// A partially packaged source library must fail before changing the vault.
fn validate_installation_source(source: &Path, mappings: &[ManagedFile]) -> Result<()> {
    let read_json = |target: &str| -> Result<Value> {
        let mapping = mappings.iter().find(|m| m.target_rel == Path::new(target))
            .with_context(|| format!("incomplete vault template: missing {target}; restore canonical config/ and plugins/"))?;
        let path = source.join(&mapping.source_rel);
        serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid template JSON: {}", path.display()))
    };
    if !read_json(".obsidian/hotkeys.json")?.is_object() {
        bail!("template hotkeys.json must be an object");
    }
    let enabled = read_json(".obsidian/community-plugins.json")?;
    let enabled = enabled
        .as_array()
        .context("template community-plugins.json must be an array")?;
    for id in enabled {
        let id = id.as_str().context("template plugin ID must be a string")?;
        if id.is_empty()
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            bail!("unsafe template plugin ID: {id}");
        }
        let manifest = read_json(&format!(".obsidian/plugins/{id}/manifest.json"))?;
        if manifest["id"].as_str() != Some(id) {
            bail!("template plugin manifest ID mismatch: {id}");
        }
        let target = PathBuf::from(format!(".obsidian/plugins/{id}/main.js"));
        if !mappings.iter().any(|m| m.target_rel == target) {
            bail!("incomplete vault template: missing plugin executable for {id}");
        }
    }
    Ok(())
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
        bail!(
            "managed source file may not be a symlink: {}",
            path.display()
        );
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
            bail!(
                "managed source path may not be a symlink: {}",
                path.display()
            );
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
    let a = fs::canonicalize(a).with_context(|| format!("failed to resolve {}", a.display()))?;
    let b = fs::canonicalize(b).with_context(|| format!("failed to resolve {}", b.display()))?;
    Ok(a == b)
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
                bail!(
                    "destination parent is not a directory: {}",
                    current.display()
                );
            }
        }
    }

    Ok(())
}

fn ensure_no_sensitive_material(path: &Path, rel: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("failed to inspect {}", path.display()))?;

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
    let output = git_command()
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to invoke git")?;

    if !output.status.success() {
        return Ok(None);
    }

    let root =
        String::from_utf8(output.stdout).context("git returned a non-UTF-8 repository path")?;
    Ok(Some(PathBuf::from(root.trim())))
}

pub fn ensure_git_clean_for_path(path: &Path) -> Result<()> {
    let root = git_root(path)?.with_context(|| {
        format!(
            "vault source is not inside a Git repository: {}",
            path.display()
        )
    })?;

    let canonical_root = fs::canonicalize(&root)?;
    let canonical_path = fs::canonicalize(path)?;
    let relative = canonical_path
        .strip_prefix(&canonical_root)
        .with_context(|| {
            format!(
                "{} is outside repository {}",
                path.display(),
                root.display()
            )
        })?;

    let output = git_command()
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain", "--untracked-files=all", "--"])
        .arg(if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        })
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
    let output = git_command()
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-bare-repository"])
        .output()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "true" {
        bail!(
            "existing backup path is not a bare Git repository: {}",
            path.display()
        );
    }
    Ok(())
}

fn ensure_origin(vault: &Path, remote: &Path) -> Result<()> {
    let output = git_command()
        .arg("-C")
        .arg(vault)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("failed to inspect Git origin")?;

    if output.status.success() {
        let existing = String::from_utf8(output.stdout).context("Git origin is not valid UTF-8")?;
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

    let status = git_command()
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
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?
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

    let mut file =
        fs::File::create(&path).with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

fn current_branch(vault: &Path) -> Result<String> {
    let output = git_command()
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
    Ok(git_command()
        .arg("-C")
        .arg(vault)
        .args(["rev-parse", "--verify", "HEAD"])
        .status()
        .context("failed to inspect Git HEAD")?
        .success())
}

fn has_staged_changes(vault: &Path) -> Result<bool> {
    let status = git_command()
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
    let output = git_command()
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
        fs::write(
            root.join("config/community-plugins.json"),
            br#"["example"]"#,
        )
        .unwrap();
        fs::write(
            root.join("plugins/example/manifest.json"),
            br#"{"id":"example"}"#,
        )
        .unwrap();
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
            b"{\n  \"x\": 1\n}"
        );
        assert_eq!(
            fs::read(target.join(".obsidian/plugins/example/main.js")).unwrap(),
            b"plugin"
        );
        assert_eq!(
            fs::read(target.join("_tools/templates/article.md")).unwrap(),
            b"template"
        );
        assert_eq!(changes.len(), 5);
    }

    #[test]
    fn incomplete_template_fails_before_any_target_changes() {
        for missing in [
            "config/hotkeys.json",
            "config/community-plugins.json",
            "plugins/example/manifest.json",
            "plugins/example/main.js",
        ] {
            let temp = tempdir().unwrap();
            let source = temp.path().join("source");
            let target = temp.path().join("target");
            make_source(&source);
            make_vault(&target);
            fs::remove_file(source.join(missing)).unwrap();
            assert!(sync_master_to_vault(&source, &target, false).is_err());
            assert_eq!(fs::read_dir(target.join(".obsidian")).unwrap().count(), 0);
            assert!(!target.join("_tools").exists());
        }
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
        fs::write(
            target.join(".obsidian/plugins/example/main.js"),
            b"changed plugin",
        )
        .unwrap();
        fs::write(
            target.join("_tools/templates/article.md"),
            b"changed template",
        )
        .unwrap();
        fs::write(target.join("chapter.md"), b"content").unwrap();

        let changes = propagate_vault_to_master(&target, &source, false, false).unwrap();

        let paths = changes
            .iter()
            .map(|change| slash_path(&change.path))
            .collect::<BTreeSet<_>>();

        assert!(paths.contains("config/hotkeys.json"));
        assert!(!paths.contains("tools/templates/article.md"));
        assert!(!paths.contains("plugins/example/main.js"));
        assert!(!paths.contains("chapter.md"));
    }

    #[test]
    fn reverse_sync_does_not_capture_unlisted_plugin_data() {
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

        assert!(changes.is_empty());
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

        let error = propagate_vault_to_master(&target, &source, false, false).unwrap_err();
        assert!(error.to_string().contains("apiKey"));
    }

    #[test]
    fn audit_and_retirement_preserve_project_customizations() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        make_source(&source);
        make_vault(&target);
        fs::write(
            source.join("config/managed-resources.json"),
            br#"{"retired":["_tools/old.js"],"retired_commands":["retired"]}"#,
        )
        .unwrap();
        fs::create_dir_all(target.join("_tools")).unwrap();
        fs::write(target.join("_tools/old.js"), "retired").unwrap();
        fs::write(target.join("_tools/custom.js"), "project").unwrap();
        fs::write(
            target.join(".obsidian/hotkeys.json"),
            br#"{"x":9,"project":[],"retired":[]}"#,
        )
        .unwrap();
        AUTHORING_AUDIT.with(|v| v.set(true));
        let drift = sync_master_to_vault(&source, &target, true).unwrap();
        AUTHORING_AUDIT.with(|v| v.set(false));
        assert!(!drift.is_empty());
        assert!(target.join("_tools/old.js").exists());
        assert!(!target.join("_tools/templates/article.md").exists());
        sync_master_to_vault(&source, &target, true).unwrap();
        assert!(!target.join("_tools/old.js").exists());
        assert_eq!(
            fs::read_to_string(target.join("_tools/custom.js")).unwrap(),
            "project"
        );
        let hotkeys: Value =
            serde_json::from_slice(&fs::read(target.join(".obsidian/hotkeys.json")).unwrap())
                .unwrap();
        assert_eq!(hotkeys["x"], 1);
        assert!(hotkeys.get("project").is_some());
        assert!(hotkeys.get("retired").is_none());
        assert!(sync_master_to_vault(&source, &target, true)
            .unwrap()
            .is_empty());
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

/// Optional resource configuration. Operational switches remain explicit on the
/// command line so a file can never silently enable pruning or reverse writes.
#[derive(clap::Args, Debug, Default)]
pub struct ResourceOptions {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub master: Option<PathBuf>,
}

pub struct ResourceConfig {
    pub master: Option<PathBuf>,
    pub repos_root: Option<PathBuf>,
}

pub fn load_resource_config(
    options: ResourceOptions,
    repos_root: Option<PathBuf>,
) -> Result<ResourceConfig> {
    let mut config = ResourceConfig {
        master: None,
        repos_root: None,
    };
    if let Some(path) = options.config {
        let path = if path.is_absolute() {
            path
        } else {
            env::current_dir()?.join(path)
        };
        let bytes =
            fs::read(&path).with_context(|| format!("cannot read config {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid JSON config {}", path.display()))?;
        let object = value
            .as_object()
            .context("configuration must be a JSON object")?;
        for (key, value) in object {
            if key != "master" && key != "repos_root" {
                bail!("unknown config key {key}; expected master or repos_root (operation switches must be supplied inline)");
            }
            if value.is_null() {
                continue;
            }
            let raw = value
                .as_str()
                .with_context(|| format!("config {key} must be a path string or null"))?;
            if raw.is_empty() {
                bail!("config {key} must not be empty");
            }
            let p = PathBuf::from(raw);
            let p = if p.is_absolute() {
                p
            } else {
                path.parent().context("config has no parent")?.join(p)
            };
            if key == "master" {
                config.master = Some(p);
            } else {
                config.repos_root = Some(p);
            }
        }
    }
    if options.master.is_some() {
        config.master = options.master;
    }
    if repos_root.is_some() {
        config.repos_root = repos_root;
    }
    Ok(config)
}

/// Accept shell-safe Python-style keyword assignments without evaluating code.
pub fn keyword_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut result = Vec::new();
    let mut expecting_flag_value = false;
    for (index, arg) in args.into_iter().enumerate() {
        if index == 0 {
            result.push(arg);
            continue;
        }
        if expecting_flag_value {
            result.push(arg);
            expecting_flag_value = false;
            continue;
        }
        if let Some(text) = arg.to_str() {
            if ["--config", "--master", "--repos-root"].contains(&text) {
                expecting_flag_value = true;
            }
            if !text.starts_with('-') {
                if let Some((key, raw)) = text.split_once('=') {
                    let known = [
                        "config",
                        "master",
                        "repos_root",
                        "prune",
                        "apply",
                        "allow_sensitive",
                    ];
                    if known.contains(&key) {
                        if raw == "None" && ["config", "master", "repos_root"].contains(&key) {
                            continue;
                        }
                        let value = if raw.len() >= 2
                            && ((raw.starts_with('"') && raw.ends_with('"'))
                                || (raw.starts_with('\'') && raw.ends_with('\'')))
                        {
                            &raw[1..raw.len() - 1]
                        } else {
                            raw
                        };
                        let value = match value {
                            "True" => "true",
                            "False" => "false",
                            _ => value,
                        };
                        result.push(format!("--{}={value}", key.replace('_', "-")).into());
                        continue;
                    }
                }
            }
        }
        result.push(arg);
    }
    result
}

fn git_command() -> Command {
    let adapter = env::var_os("VAULT_GIT_ADAPTER")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/git.py")))
        .unwrap_or_else(|| PathBuf::from("/nonexistent/git.py"));
    use std::os::unix::fs::PermissionsExt;
    if fs::metadata(&adapter)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
    {
        Command::new(adapter)
    } else {
        let mut command = Command::new("/usr/bin/python3");
        command.arg(adapter);
        command
    }
}

#[cfg(test)]
mod resource_option_tests {
    use super::*;
    #[test]
    fn optional_config_and_cli_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("options.json");
        fs::write(
            &path,
            r#"{"master":"relative master","repos_root":"backups"}"#,
        )
        .unwrap();
        let config = load_resource_config(
            ResourceOptions {
                config: Some(path),
                master: Some(PathBuf::from("/explicit")),
            },
            None,
        )
        .unwrap();
        assert_eq!(config.master.unwrap(), Path::new("/explicit"));
        assert_eq!(config.repos_root.unwrap(), dir.path().join("backups"));
        let config = load_resource_config(ResourceOptions::default(), None).unwrap();
        assert!(config.master.is_none() && config.repos_root.is_none());
    }
    #[test]
    fn explicit_missing_invalid_and_unsafe_configs_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("options.json");
        assert!(load_resource_config(
            ResourceOptions {
                config: Some(path.clone()),
                master: None
            },
            None
        )
        .is_err());
        for text in [
            "[]",
            "invalid",
            r#"{"apply":true}"#,
            r#"{"master":7}"#,
            r#"{"master":""}"#,
        ] {
            fs::write(&path, text).unwrap();
            assert!(load_resource_config(
                ResourceOptions {
                    config: Some(path.clone()),
                    master: None
                },
                None
            )
            .is_err());
        }
    }
    #[test]
    fn python_keywords_preserve_values_without_evaluation() {
        let args = [
            "update",
            "master=\"/path with spaces\"",
            "prune=False",
            "config=None",
            "allow_sensitive=True",
        ]
        .map(std::ffi::OsString::from);
        let got = keyword_args(args);
        assert_eq!(
            got,
            [
                "update",
                "--master=/path with spaces",
                "--prune=false",
                "--allow-sensitive=true"
            ]
            .map(std::ffi::OsString::from)
        );
        let got =
            keyword_args(["update", "--master", "master=literal"].map(std::ffi::OsString::from));
        assert_eq!(got[2], "master=literal");
    }
}

#[cfg(test)]
mod git_root_regression {
    use super::*;
    #[test]
    fn master_at_repository_root_is_checked_and_dirty_files_rejected() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-b", "main"]).unwrap();
        ensure_git_clean_for_path(dir.path()).unwrap();
        fs::write(dir.path().join("uncommitted.txt"), "fixture").unwrap();
        assert!(ensure_git_clean_for_path(dir.path()).is_err());
    }
}

thread_local! { static AUTHORING_AUDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[derive(clap::Args, Debug, Default)]
pub struct AuthoringOptions {
    /// Shared templates repository checkout (or TEMPLATES_ROOT).
    #[arg(long)]
    pub templates_root: Option<PathBuf>,
    /// Authoring profile; existing vault selection is preserved when omitted.
    #[arg(long)]
    pub profile: Option<String>,
}

pub fn configure_authoring(vault: &Path, options: &AuthoringOptions, audit: bool) -> Result<()> {
    if let Some(root) = &options.templates_root {
        env::set_var("TEMPLATES_ROOT", fs::canonicalize(root)?);
    }
    let existing = vault.join(".obsidian/authoring.json");
    let profile = if let Some(profile) = &options.profile {
        profile.clone()
    } else if existing.exists() {
        let v: Value = serde_json::from_slice(&fs::read(existing)?)?;
        v["profile"]
            .as_str()
            .context("authoring profile missing")?
            .to_owned()
    } else {
        "general".to_owned()
    };
    env::set_var("OBSIDIAN_AUTHORING_PROFILE", profile);
    AUTHORING_AUDIT.with(|v| v.set(audit));
    Ok(())
}
fn selected_profile() -> String {
    env::var("OBSIDIAN_AUTHORING_PROFILE").unwrap_or_else(|_| "general".into())
}
fn templates_root() -> Result<PathBuf> {
    let root = PathBuf::from(env::var_os("TEMPLATES_ROOT").context(
        "set TEMPLATES_ROOT or pass --templates-root; templates are owned by the shared repository",
    )?);
    Ok(fs::canonicalize(root)?)
}
fn retired_paths(source: &Path) -> Result<Vec<PathBuf>> {
    let path = source.join("config/managed-resources.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let v: Value = serde_json::from_slice(&fs::read(path)?)?;
    let mut paths = Vec::new();
    for value in v["retired"]
        .as_array()
        .context("invalid retired resource list")?
    {
        let rel = PathBuf::from(value.as_str().context("invalid retired path")?);
        if !rel.starts_with("_tools")
            || rel.components().any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("unsafe retired path");
        }
        paths.push(rel);
    }
    if source.join("config/authoring.json").exists() {
        let root = templates_root()?;
        let profiles: Value =
            serde_json::from_slice(&fs::read(root.join("obsidian/profiles.json"))?)?;
        for value in profiles[&selected_profile()]["exclude"]
            .as_array()
            .context("invalid profile")?
        {
            let rel = PathBuf::from(value.as_str().context("invalid template exclusion")?);
            if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
                bail!("unsafe excluded template");
            }
            paths.push(PathBuf::from("_tools/templates").join(rel));
        }
    }
    Ok(paths)
}
fn write_bytes_atomic(bytes: &[u8], target: &Path, root: &Path) -> Result<()> {
    ensure_safe_destination(root, target)?;
    let parent = target.parent().context("missing parent")?;
    fs::create_dir_all(parent)?;
    ensure_safe_destination(root, target)?;
    let tmp = parent.join(format!(".vault-tools.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, target)?;
    Ok(())
}
fn merged_json(master: &Value, current: &Value) -> Value {
    match (master, current) {
        (Value::Object(m), Value::Object(c)) => {
            let mut result = c.clone();
            for (key, value) in m {
                result.insert(
                    key.clone(),
                    merged_json(value, c.get(key).unwrap_or(&Value::Null)),
                );
            }
            Value::Object(result)
        }
        _ => master.clone(),
    }
}
fn effective_bytes(
    from: &Path,
    to: &Path,
    target: &Path,
    source: &Path,
    _vault: &Path,
) -> Result<Vec<u8>> {
    validate_source_file(from)?;
    if to.exists() {
        validate_source_file(to)?;
    }
    let bytes = fs::read(from)?;
    let name = slash_path(target);
    if name == ".obsidian/authoring.json" {
        let profiles: Value =
            serde_json::from_slice(&fs::read(templates_root()?.join("obsidian/profiles.json"))?)?;
        let profile = selected_profile();
        let definition = profiles.get(&profile).context("unknown profile")?;
        return Ok(serde_json::to_vec_pretty(
            &serde_json::json!({"profile":profile,"destinations":definition["destinations"]}),
        )?);
    }
    if target.extension() != Some(OsStr::new("json")) || !name.starts_with(".obsidian/") {
        return Ok(bytes);
    }
    let master: Value = serde_json::from_slice(&bytes)?;
    let current: Value = if to.exists() {
        serde_json::from_slice(&fs::read(to)?)?
    } else {
        master.clone()
    };
    let mut merged = merged_json(&master, &current);
    if name == ".obsidian/community-plugins.json" {
        let mut items = current.as_array().context("invalid plugin list")?.clone();
        for item in master.as_array().context("invalid master plugin list")? {
            if !items.contains(item) {
                items.push(item.clone());
            }
        }
        merged = Value::Array(items);
    }
    if name == ".obsidian/plugins/quickadd/data.json" {
        let known = master["choices"].as_array().context("missing choices")?;
        let mut choices = known.clone();
        let retired = source.join("config/managed-resources.json");
        let retired: Value = if retired.exists() {
            serde_json::from_slice(&fs::read(retired)?)?
        } else {
            serde_json::json!({"retired_commands":[]})
        };
        for choice in current["choices"]
            .as_array()
            .context("missing current choices")?
        {
            let id = choice["id"].as_str().context("missing choice id")?;
            let command = format!("quickadd:choice:{id}");
            if !known.iter().any(|v| v["id"] == choice["id"])
                && !retired["retired_commands"]
                    .as_array()
                    .context("missing retired commands")?
                    .iter()
                    .any(|v| v.as_str() == Some(&command))
            {
                choices.push(choice.clone());
            }
        }
        merged["choices"] = Value::Array(choices);
    }
    if name == ".obsidian/hotkeys.json" {
        let path = source.join("config/managed-resources.json");
        if path.exists() {
            let retired: Value = serde_json::from_slice(&fs::read(path)?)?;
            for id in retired["retired_commands"]
                .as_array()
                .context("missing retired commands")?
            {
                merged
                    .as_object_mut()
                    .context("invalid hotkeys")?
                    .remove(id.as_str().context("invalid command")?);
            }
        }
    }
    // Preserve user-added Templater commands; explicitly retired templates are removed.
    if name == ".obsidian/plugins/templater-obsidian/data.json" {
        let key = "enabled_templates_hotkeys";
        let mut items = master[key]
            .as_array()
            .context("missing template commands")?
            .clone();
        for item in current[key]
            .as_array()
            .context("missing current template commands")?
        {
            if !items.iter().any(|v| v["template"] == item["template"]) {
                items.push(item.clone());
            }
        }
        let retired = retired_paths(source)?;
        items.retain(|item| {
            !retired
                .iter()
                .any(|p| p.to_str() == item["template"].as_str())
        });
        merged[key] = Value::Array(items);
    }
    filter_profile_commands(&mut merged, &master, &current, &name, source)?;
    Ok(serde_json::to_vec_pretty(&merged)?)
}
fn filter_profile_commands(
    merged: &mut Value,
    master: &Value,
    current: &Value,
    name: &str,
    source: &Path,
) -> Result<()> {
    let mut inactive = Vec::<String>::new();
    let retired = source.join("config/managed-resources.json");
    if retired.exists() {
        let v: Value = serde_json::from_slice(&fs::read(retired)?)?;
        for id in v["retired_commands"]
            .as_array()
            .context("invalid retired commands")?
        {
            inactive.push(id.as_str().context("invalid command")?.to_owned());
        }
    }
    if source.join("config/authoring.json").exists() {
        let profiles: Value =
            serde_json::from_slice(&fs::read(templates_root()?.join("obsidian/profiles.json"))?)?;
        let profile = &profiles[&selected_profile()];
        for rel in profile["exclude"]
            .as_array()
            .context("invalid exclusions")?
        {
            inactive.push(format!(
                "templater-obsidian:create-_tools/templates/{}",
                rel.as_str().context("invalid exclusion")?
            ));
        }
        if let Some(allowed) = profile["commands"].as_array() {
            let choices: Value =
                serde_json::from_slice(&fs::read(source.join("plugins/quickadd/data.json"))?)?;
            for choice in choices["choices"].as_array().context("invalid choices")? {
                if !allowed.iter().any(|v| v == &choice["name"]) {
                    inactive.push(format!(
                        "quickadd:choice:{}",
                        choice["id"].as_str().context("invalid choice")?
                    ));
                }
            }
        }
    }
    if name == ".obsidian/plugins/cmdr/data.json" {
        for (key, value) in master.as_object().context("invalid Commander settings")? {
            if let Some(items) = value.as_array() {
                let mut all = items.clone();
                if let Some(custom) = current[key].as_array() {
                    for item in custom {
                        if !all.iter().any(|v| v["id"] == item["id"]) {
                            all.push(item.clone());
                        }
                    }
                }
                all.retain(|v| !inactive.iter().any(|id| v["id"].as_str() == Some(id)));
                merged[key] = Value::Array(all);
            }
        }
    }
    if name == ".obsidian/plugins/quickadd/data.json" {
        if let Some(choices) = merged["choices"].as_array_mut() {
            choices.retain(|v| {
                !inactive.contains(&format!(
                    "quickadd:choice:{}",
                    v["id"].as_str().unwrap_or("")
                ))
            });
        }
    }
    if name == ".obsidian/hotkeys.json" {
        for id in inactive {
            merged
                .as_object_mut()
                .context("invalid hotkeys")?
                .remove(&id);
        }
    }
    Ok(())
}
fn reverse_bytes(from: &Path, to: &Path, target: &Path) -> Result<Vec<u8>> {
    validate_source_file(from)?;
    let bytes = fs::read(from)?;
    if target.extension() != Some(OsStr::new("json")) || !to.exists() {
        return Ok(bytes);
    }
    let current: Value = serde_json::from_slice(&bytes)?;
    let master: Value = serde_json::from_slice(&fs::read(to)?)?;
    // Existing keys form the whitelist; runtime histories, project additions and paths never flow back.
    fn project(current: &Value, master: &Value) -> Value {
        match master {
            Value::Object(keys) => Value::Object(
                keys.iter()
                    .map(|(k, v)| (k.clone(), project(current.get(k).unwrap_or(v), v)))
                    .collect(),
            ),
            Value::Array(_) => master.clone(),
            _ => current.clone(),
        }
    }
    let projected = project(&current, &master);
    // Keep original source formatting when values match.
    if projected == master {
        return Ok(fs::read(to)?);
    }
    Ok(serde_json::to_vec_pretty(&projected)?)
}

pub fn propagate_shared_templates(
    vault: &Path,
    source: &Path,
    apply: bool,
    allow_sensitive: bool,
) -> Result<Vec<Change>> {
    if !source.join("config/authoring.json").exists() {
        return Ok(Vec::new());
    }
    let root = templates_root()?;
    if apply {
        ensure_git_clean_for_path(&root)?;
    }
    let mut changes = Vec::new();
    for mapping in collect_source_files(source)? {
        if !mapping.target_rel.starts_with("_tools/templates") || !mapping.source_rel.is_absolute()
        {
            continue;
        }
        let from = vault.join(&mapping.target_rel);
        let to = &mapping.source_rel;
        if !from.is_file() {
            continue;
        }
        validate_source_file(&from)?;
        if fs::read(&from)? == fs::read(to)? {
            continue;
        }
        if !allow_sensitive {
            ensure_no_sensitive_material(&from, &mapping.target_rel)?;
        }
        changes.push(Change {
            kind: ChangeKind::Updated,
            path: to.strip_prefix(&root)?.to_owned(),
        });
        if apply {
            write_bytes_atomic(&fs::read(from)?, to, &root)?;
        }
    }
    Ok(changes)
}
