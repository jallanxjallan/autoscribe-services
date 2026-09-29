use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::{json, Value};
use srv_common::{
    canonical_json_bytes, effect_signature, git_output_owned, load_policy, read_effect_key,
    safe_identifier, sha256_hex, validate_absolute_target, validate_branch_name,
    validate_relative_path, write_ndjson, INPUT_SCHEMA,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const RETURN_SCHEMA: &str = "autoscribe.return.v1";
const DROPBOX_INCOMING: &str = "dropbox:biznet/incoming";

#[derive(Parser, Debug)]
#[command(about = "Normalize trusted repo or direct Dropbox input into canonical AutoScribe NDJSON")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,

    #[command(subcommand)]
    command: InputMode,
}

#[derive(Subcommand, Debug)]
enum InputMode {
    /// Read eligible Markdown changed by one Git commit. The return target is the same repo/path.
    Repo {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        branch: Option<String>,
    },

    /// Read one named batch from dropbox:biznet/incoming/<batch>.
    /// The return target is dropbox:biznet/outgoing/<batch>.
    Direct {
        #[arg(long)]
        batch: String,
        #[arg(long)]
        plan: String,
    },
}

#[derive(Debug, Serialize)]
struct CanonicalInput {
    schema: String,
    record_id: String,
    source: Value,
    content: String,
    content_sha256: String,
    routing: Value,
    baggage: Value,
}

fn rclone_binary() -> OsString {
    std::env::var_os("AUTOSCRIBE_RCLONE").unwrap_or_else(|| OsString::from("rclone"))
}

fn safe_batch(value: &str) -> Result<()> {
    safe_identifier(value, "batch", 96)?;
    if value == "." || value == ".." || value.contains('/') || value.contains(':') {
        bail!("batch must be a single folder name");
    }
    Ok(())
}

fn safe_plan(value: &str) -> Result<()> {
    safe_identifier(value, "plan", 160)?;
    if !value.starts_with("pln_") {
        bail!("plan identity must start with pln_");
    }
    Ok(())
}

fn safe_remote_path(path: &str) -> Result<PathBuf> {
    if path.chars().any(char::is_control) || path.contains('\\') {
        bail!("Dropbox path contains unsafe characters");
    }
    let path = PathBuf::from(path);
    validate_relative_path(&path)?;
    Ok(path)
}

fn git_bytes(repo: &Path, args: &[String], label: &str) -> Result<Vec<u8>> {
    let mut full = vec!["--git-dir".to_string(), repo.display().to_string()];
    full.extend(args.iter().cloned());
    git_output_owned(&full, label)
}

fn git_text(repo: &Path, args: &[String], label: &str) -> Result<String> {
    Ok(String::from_utf8(git_bytes(repo, args, label)?)?
        .trim()
        .to_string())
}

fn resolve_commit(repo: &Path, commit: &str) -> Result<String> {
    safe_identifier(commit, "commit", 128)?;
    let resolved = git_text(
        repo,
        &["rev-parse".to_string(), format!("{commit}^{{commit}}")],
        "git rev-parse commit",
    )?;
    safe_identifier(&resolved, "resolved commit", 64)?;
    Ok(resolved)
}

fn plan_from_commit(repo: &Path, commit: &str) -> Result<Option<String>> {
    let message = git_text(
        repo,
        &[
            "show".to_string(),
            "-s".to_string(),
            "--format=%B".to_string(),
            commit.to_string(),
        ],
        "git show commit message",
    )?;

    let matches: Vec<&str> = message
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("Plan:").map(str::trim))
        .collect();

    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() != 1 {
        bail!("dispatch commit requires exactly one Plan: line");
    }
    let plan = matches[0]
        .split_whitespace()
        .last()
        .ok_or_else(|| anyhow::anyhow!("blank Plan: line"))?;
    safe_plan(plan)?;
    Ok(Some(plan.to_string()))
}

fn changed_markdown_paths(repo: &Path, commit: &str) -> Result<Vec<PathBuf>> {
    let parents = git_text(
        repo,
        &[
            "rev-list".to_string(),
            "--parents".to_string(),
            "-n".to_string(),
            "1".to_string(),
            commit.to_string(),
        ],
        "git rev-list parents",
    )?;
    let fields: Vec<&str> = parents.split_whitespace().collect();
    if fields.is_empty() {
        bail!("commit not found: {commit}");
    }
    if fields.len() > 2 {
        bail!("merge dispatch commits are not supported");
    }

    let args = if fields.len() == 1 {
        vec![
            "diff-tree".to_string(),
            "--root".to_string(),
            "--no-commit-id".to_string(),
            "--name-only".to_string(),
            "-r".to_string(),
            "--diff-filter=AM".to_string(),
            "-z".to_string(),
            commit.to_string(),
        ]
    } else {
        vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "--no-renames".to_string(),
            "--diff-filter=AM".to_string(),
            "-z".to_string(),
            fields[1].to_string(),
            commit.to_string(),
        ]
    };

    let raw = git_bytes(repo, &args, "git changed paths")?;
    let mut paths = Vec::new();
    for raw_path in raw.split(|b| *b == 0).filter(|part| !part.is_empty()) {
        let path = std::str::from_utf8(raw_path).context("Git path is not UTF-8")?;
        if !path.to_ascii_lowercase().ends_with(".md") {
            continue;
        }
        let path = PathBuf::from(path);
        validate_relative_path(&path)?;
        paths.push(path);
    }
    Ok(paths)
}

fn read_git_blob(repo: &Path, commit: &str, path: &Path) -> Result<Vec<u8>> {
    let spec = format!("{commit}:{}", path.display());
    git_bytes(
        repo,
        &["cat-file".to_string(), "blob".to_string(), spec],
        "git cat-file blob",
    )
}

fn eligible_markdown(bytes: &[u8]) -> bool {
    bytes.starts_with(b"---\n") || bytes.starts_with(b"---\r\n")
}

fn signed_baggage(secret: &[u8], record_id: &str, route: Value) -> Result<Value> {
    let payload = json!({
        "schema": RETURN_SCHEMA,
        "record_id": record_id,
        "route": route,
    });
    let signature = effect_signature(secret, &payload)?;
    Ok(json!({
        "autoscribe_return": {
            "schema": RETURN_SCHEMA,
            "record_id": record_id,
            "route": payload["route"].clone(),
            "signature": signature,
        }
    }))
}

fn emit_input(
    secret: &[u8],
    source: Value,
    content: String,
    routing: Value,
    route: Value,
    max_body_bytes: usize,
) -> Result<()> {
    if content.len() > max_body_bytes {
        bail!("input body exceeds {max_body_bytes} bytes");
    }
    let content_sha256 = sha256_hex(content.as_bytes());
    let identity_payload = json!({
        "source": source,
        "content_sha256": content_sha256,
        "routing": routing,
    });
    let record_id = format!(
        "inp_{}",
        &sha256_hex(&canonical_json_bytes(&identity_payload)?)[..32]
    );
    let baggage = signed_baggage(secret, &record_id, route)?;

    write_ndjson(&CanonicalInput {
        schema: INPUT_SCHEMA.to_string(),
        record_id,
        source: identity_payload["source"].clone(),
        content,
        content_sha256: identity_payload["content_sha256"]
            .as_str()
            .expect("content hash is text")
            .to_string(),
        routing: identity_payload["routing"].clone(),
        baggage,
    })
}

fn run_repo(
    policy: &srv_common::Policy,
    secret: &[u8],
    repo: &Path,
    commit: &str,
    branch: Option<&str>,
) -> Result<()> {
    let repo = validate_absolute_target(repo, &policy.paths.repo_roots)?;
    if !repo.is_dir() {
        bail!("Git source is not a directory: {}", repo.display());
    }
    let branch = branch.unwrap_or(&policy.git.default_branch);
    validate_branch_name(branch)?;
    let commit = resolve_commit(&repo, commit)?;
    let Some(plan) = plan_from_commit(&repo, &commit)? else {
        return Ok(());
    };

    let mut emitted = 0usize;
    for path in changed_markdown_paths(&repo, &commit)? {
        let bytes = read_git_blob(&repo, &commit, &path)?;
        if !eligible_markdown(&bytes) {
            continue;
        }
        let content = String::from_utf8(bytes)
            .with_context(|| format!("{} is not UTF-8 text", path.display()))?;
        let source = json!({
            "kind": "git",
            "repo": &repo,
            "commit": &commit,
            "path": &path,
        });
        let routing = json!({"plan_id": plan.as_str()});
        let route = json!({
            "kind": "repo",
            "repo": source["repo"].clone(),
            "path": source["path"].clone(),
            "branch": branch,
        });
        emit_input(
            secret,
            source,
            content,
            routing,
            route,
            policy.limits.max_body_bytes,
        )?;
        emitted += 1;
    }
    if emitted == 0 {
        bail!("dispatch commit contains no eligible AutoScribe Markdown files");
    }
    Ok(())
}

fn rclone_output(args: &[&str], label: &str) -> Result<Vec<u8>> {
    let output = Command::new(rclone_binary())
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {label}"))?;
    if !output.status.success() {
        bail!(
            "{label} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn run_direct(policy: &srv_common::Policy, secret: &[u8], batch: &str, plan: &str) -> Result<()> {
    safe_batch(batch)?;
    safe_plan(plan)?;
    let batch_root = format!("{DROPBOX_INCOMING}/{batch}");
    let listing = rclone_output(
        &["lsf", "--files-only", "--recursive", &batch_root],
        "rclone lsf incoming batch",
    )?;
    let listing = String::from_utf8(listing).context("rclone listing is not UTF-8")?;

    let mut emitted = 0usize;
    for line in listing.lines().filter(|line| !line.is_empty()) {
        let path = safe_remote_path(line)?;
        let remote = format!("{batch_root}/{}", path.display());
        let bytes = rclone_output(&["cat", &remote], "rclone cat incoming file")?;
        let content = String::from_utf8(bytes)
            .with_context(|| format!("{remote} is not UTF-8 text"))?;
        let source = json!({
            "kind": "dropbox",
            "batch": batch,
            "path": path,
            "remote": remote,
        });
        let routing = json!({"plan_id": plan});
        let route = json!({
            "kind": "dropbox",
            "batch": batch,
            "path": source["path"].clone(),
        });
        emit_input(
            secret,
            source,
            content,
            routing,
            route,
            policy.limits.max_body_bytes,
        )?;
        emitted += 1;
    }
    if emitted == 0 {
        bail!("incoming batch is empty: {batch_root}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let secret = read_effect_key(&policy.paths.effect_key_file)?;

    match args.command {
        InputMode::Repo {
            repo,
            commit,
            branch,
        } => run_repo(&policy, &secret, &repo, &commit, branch.as_deref()),
        InputMode::Direct { batch, plan } => run_direct(&policy, &secret, &batch, &plan),
    }
}
