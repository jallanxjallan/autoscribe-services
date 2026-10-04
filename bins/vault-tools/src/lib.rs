use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

const MANAGED_FILES: &[&str] = &[
    ".obsidian/app.json",
    ".obsidian/appearance.json",
    ".obsidian/core-plugins.json",
    ".obsidian/community-plugins.json",
    ".obsidian/hotkeys.json",
    ".obsidian/templates.json",
];

const MANAGED_DIRS: &[&str] = &[
    "_templates",
    "_scripts",
    "_ui",
    "_views",
    "_tools",
    ".obsidian/plugins",
    ".obsidian/snippets",
    ".obsidian/themes",
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

pub fn resolve_master(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        ensure_vault_root(&path)
            .with_context(|| format!("invalid --master path {}", path.display()))?;
        return Ok(path);
    }

    if let Some(path) = env::var_os("OBSIDIAN_MASTER_VAULT").map(PathBuf::from) {
        ensure_vault_root(&path)
            .with_context(|| format!("invalid OBSIDIAN_MASTER_VAULT {}", path.display()))?;
        return Ok(path);
    }

    let home = home_dir()?;
    let candidates = [
        home.join("Studio/Obsidian"),
        home.join("Work/Obsidian"),
    ];

    for candidate in candidates {
        if candidate.join(".obsidian").is_dir() {
            return Ok(candidate);
        }
    }

    bail!(
        "cannot find the master Obsidian vault; set OBSIDIAN_MASTER_VAULT or pass --master"
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

pub fn sync_master_to_vault(master: &Path, vault: &Path, prune: bool) -> Result<Vec<Change>> {
    ensure_vault_root(master)?;
    ensure_vault_root(vault)?;
    ensure_distinct(master, vault)?;

    let source_files = collect_managed_files(master)?;
    if source_files.is_empty() {
        bail!(
            "master vault contains no managed Obsidian configuration or editing assets: {}",
            master.display()
        );
    }

    let mut changes = Vec::new();

    for rel in &source_files {
        let source = master.join(rel);
        let target = vault.join(rel);

        if file_differs(&source, &target)? {
            let kind = if target.exists() {
                ChangeKind::Updated
            } else {
                ChangeKind::Added
            };
            copy_file_atomic(&source, &target)?;
            changes.push(Change {
                kind,
                path: rel.clone(),
            });
        }
    }

    if prune {
        let target_files = collect_managed_files(vault)?;
        for rel in target_files.difference(&source_files) {
            let target = vault.join(rel);
            ensure_safe_destination(vault, &target)?;
            fs::remove_file(&target)
                .with_context(|| format!("failed to remove {}", target.display()))?;
            changes.push(Change {
                kind: ChangeKind::Removed,
                path: rel.clone(),
            });
        }
    }

    Ok(changes)
}

pub fn propagate_vault_to_master(
    source: &Path,
    master: &Path,
    apply: bool,
    allow_sensitive: bool,
) -> Result<Vec<Change>> {
    ensure_vault_root(source)?;
    ensure_vault_root(master)?;
    ensure_distinct(source, master)?;

    if apply {
        ensure_git_root(master)?;
        ensure_git_clean(master)?;
    }

    let source_files = collect_managed_files(source)?;
    let mut changes = Vec::new();

    for rel in source_files {
        let from = source.join(&rel);
        let to = master.join(&rel);

        if !file_differs(&from, &to)? {
            continue;
        }

        if !allow_sensitive {
            ensure_no_sensitive_material(&from, &rel)?;
        }

        changes.push(Change {
            kind: if to.exists() {
                ChangeKind::Updated
            } else {
                ChangeKind::Added
            },
            path: rel.clone(),
        });

        if apply {
            copy_file_atomic(&from, &to)?;
        }
    }

    // Reverse propagation is deliberately additive/update-only. A project vault
    // can never delete canonical master material. Deletions remain a conscious
    // edit made in the master itself.
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

fn collect_managed_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut files = BTreeSet::new();

    for rel in MANAGED_FILES {
        let path = root.join(rel);
        if !path.exists() {
            continue;
        }
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if meta.file_type().is_symlink() {
            bail!("managed path may not be a symlink: {}", path.display());
        }
        if !meta.is_file() {
            bail!("managed path is not a file: {}", path.display());
        }
        files.insert(PathBuf::from(rel));
    }

    for rel in MANAGED_DIRS {
        let path = root.join(rel);
        if !path.exists() {
            continue;
        }
        collect_directory(root, &path, &mut files)?;
    }

    Ok(files)
}

fn collect_directory(root: &Path, dir: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    let meta = fs::symlink_metadata(dir)
        .with_context(|| format!("failed to inspect {}", dir.display()))?;
    if meta.file_type().is_symlink() {
        bail!("managed directory may not be a symlink: {}", dir.display());
    }

    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("{} escaped {}", path.display(), root.display()))?
            .to_path_buf();

        if is_forbidden_relative(&rel) {
            continue;
        }

        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;

        if meta.file_type().is_symlink() {
            bail!("managed path may not be a symlink: {}", path.display());
        }

        if meta.is_dir() {
            collect_directory(root, &path, files)?;
        } else if meta.is_file() {
            files.insert(rel);
        }
    }

    Ok(())
}

fn is_forbidden_relative(rel: &Path) -> bool {
    let normalized = slash_path(rel);
    if normalized == ".DS_Store" || normalized.ends_with("/.DS_Store") {
        return true;
    }

    if normalized.starts_with(".obsidian/workspace")
        || normalized.starts_with(".obsidian/cache/")
        || normalized == ".obsidian/cache"
    {
        return true;
    }

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
        bail!("source and destination vault are the same directory");
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

    let target_meta = fs::symlink_metadata(target)
        .with_context(|| format!("failed to inspect {}", target.display()))?;
    if target_meta.file_type().is_symlink() {
        bail!("refusing to overwrite symlink {}", target.display());
    }
    if !target_meta.is_file() {
        bail!("refusing to overwrite non-file {}", target.display());
    }

    let source_meta = fs::metadata(source)?;
    if source_meta.len() != target_meta.len() {
        return Ok(true);
    }

    Ok(fs::read(source)? != fs::read(target)?)
}

fn copy_file_atomic(source: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .context("managed target has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    ensure_safe_destination(parent, target)?;

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

fn ensure_safe_destination(_root: &Path, target: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() {
            bail!("refusing to write through symlink {}", target.display());
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
                    "refusing to propagate {}: JSON key '{}' appears to contain credentials; \
                     move the secret out of the vault or re-run with --allow-sensitive",
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
                    "refusing to propagate {}: content resembles a credential ('{}'); \
                     re-run with --allow-sensitive only if this is intentional",
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
        Some("json" | "yaml" | "yml" | "toml" | "txt" | "md")
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

fn ensure_git_root(path: &Path) -> Result<()> {
    let root = git_root(path)?
        .with_context(|| format!("master vault is not a Git repository: {}", path.display()))?;
    if !same_existing_path(&root, path)? {
        bail!(
            "master vault must be the root of its Git repository; repository root is {}",
            root.display()
        );
    }
    Ok(())
}

fn ensure_git_clean(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .context("failed to inspect master Git status")?;

    if !output.status.success() {
        bail!("failed to inspect master Git status");
    }

    if !output.stdout.is_empty() {
        bail!(
            "master vault has uncommitted changes; commit or discard them before propagating into it"
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
    fn sync_updates_managed_files_but_preserves_workspace() {
        let temp = tempdir().unwrap();
        let master = temp.path().join("master");
        let target = temp.path().join("target");
        make_vault(&master);
        make_vault(&target);

        fs::write(master.join(".obsidian/hotkeys.json"), b"{\"x\":1}").unwrap();
        fs::write(master.join(".obsidian/workspace.json"), b"master").unwrap();
        fs::write(target.join(".obsidian/hotkeys.json"), b"{\"x\":0}").unwrap();
        fs::write(target.join(".obsidian/workspace.json"), b"target").unwrap();

        let changes = sync_master_to_vault(&master, &target, false).unwrap();

        assert_eq!(
            fs::read(target.join(".obsidian/hotkeys.json")).unwrap(),
            b"{\"x\":1}"
        );
        assert_eq!(
            fs::read(target.join(".obsidian/workspace.json")).unwrap(),
            b"target"
        );
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn reverse_sync_never_touches_notes() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let master = temp.path().join("master");
        make_vault(&source);
        make_vault(&master);

        fs::write(source.join("chapter.md"), b"content").unwrap();
        fs::create_dir_all(source.join("_templates")).unwrap();
        fs::write(source.join("_templates/article.md"), b"template").unwrap();

        let changes = propagate_vault_to_master(&source, &master, false, false).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, PathBuf::from("_templates/article.md"));
        assert!(!master.join("chapter.md").exists());
    }

    #[test]
    fn reverse_sync_rejects_nonempty_secret_fields() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let master = temp.path().join("master");
        make_vault(&source);
        make_vault(&master);

        fs::create_dir_all(source.join(".obsidian/plugins/example")).unwrap();
        fs::write(
            source.join(".obsidian/plugins/example/data.json"),
            br#"{"apiKey":"do-not-copy"}"#,
        )
        .unwrap();

        let error =
            propagate_vault_to_master(&source, &master, false, false).unwrap_err();
        assert!(error.to_string().contains("apiKey"));
    }
}
