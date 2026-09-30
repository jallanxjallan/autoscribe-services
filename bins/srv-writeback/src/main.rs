use anyhow::{bail, Context, Result};
use clap::Parser;
use fs2::FileExt;
use serde::Deserialize;
use serde_json::{json, Value};
use srv_common::{
    canonical_json, existing_receipt, load_policy, open_effects_db, read_effect_key, read_ndjson,
    record_receipt, run_checked, safe_identifier, sha256_hex, validate_absolute_target,
    validate_branch_name, validate_relative_path, verify_effect_signature, write_ndjson,
    EFFECT_SCHEMA,
};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{NamedTempFile, TempDir};

#[derive(Parser, Debug)]
#[command(about = "Apply authenticated repository write effects as one Git commit per effect")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,
}

#[derive(Debug, Deserialize)]
struct EffectRecord {
    schema: String,
    effect_key: String,
    call_id: String,
    effect_index: usize,
    effect: Value,
    content: String,
    content_sha256: String,
}

fn effect_lock(db_path: &Path, effect_key: &str) -> Result<File> {
    safe_identifier(effect_key, "effect_key", 96)?;
    let parent = db_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("effects DB must have a parent directory"))?;
    let dir = parent.join("effect-locks");
    fs::create_dir_all(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(effect_key))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn ensure_no_symlink_path(root: &Path, rel: &Path) -> Result<PathBuf> {
    validate_relative_path(rel)?;
    let mut cursor = root.to_path_buf();
    let components: Vec<_> = rel.components().collect();
    for (index, component) in components.iter().enumerate() {
        cursor.push(component.as_os_str());
        if cursor.exists() {
            let meta = fs::symlink_metadata(&cursor)?;
            if meta.file_type().is_symlink() {
                bail!("writeback target crosses symlink: {}", cursor.display());
            }
            if index + 1 < components.len() && !meta.is_dir() {
                bail!("writeback parent is not a directory: {}", cursor.display());
            }
        }
    }
    Ok(cursor)
}

fn git_success(args: &[String]) -> Result<bool> {
    Ok(Command::new("git").args(args).status()?.success())
}

fn git_text(args: &[String], label: &str) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    Ok(String::from_utf8(run_checked(cmd, label)?.stdout)?
        .trim()
        .to_string())
}

fn ensure_bare_repo(repo: &Path, branch: &str, create_repo: bool) -> Result<()> {
    if !repo.exists() {
        if !create_repo {
            bail!("target repository does not exist: {}", repo.display());
        }
        if let Some(parent) = repo.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut cmd = Command::new("git");
        cmd.args(["init", "--bare", "-b", branch]).arg(repo);
        run_checked(cmd, "git init --bare")?;
    }
    if !repo.is_dir() {
        bail!("target repository is not a directory: {}", repo.display());
    }
    let value = git_text(
        &[
            "--git-dir".to_string(),
            repo.display().to_string(),
            "rev-parse".to_string(),
            "--is-bare-repository".to_string(),
        ],
        "git rev-parse --is-bare-repository",
    )?;
    if value != "true" {
        bail!(
            "writeback target must be a bare Git repository: {}",
            repo.display()
        );
    }
    Ok(())
}

fn prior_commit(repo: &Path, effect_key: &str) -> Result<Option<String>> {
    let args = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
        "log".to_string(),
        "--all".to_string(),
        "--fixed-strings".to_string(),
        "--grep".to_string(),
        effect_key.to_string(),
        "--format=%H".to_string(),
        "-1".to_string(),
    ];
    let mut cmd = Command::new("git");
    cmd.args(&args);
    let output = run_checked(cmd, "git log effect lookup")?;
    let value = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(if value.is_empty() { None } else { Some(value) })
}

fn repo_lock(repo: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(repo.join("autoscribe-write.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn prepare_clone(repo: &Path, branch: &str) -> Result<TempDir> {
    let tmp = tempfile::tempdir()?;
    let target = tmp.path().join("work");

    let branch_ref = format!("refs/heads/{branch}");
    let exists = git_success(&[
        "--git-dir".to_string(),
        repo.display().to_string(),
        "show-ref".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        branch_ref,
    ])?;

    let mut clone = Command::new("git");
    clone.arg("clone");
    if exists {
        clone.args(["--branch", branch, "--single-branch"]);
    }
    clone.arg(repo).arg(&target);
    run_checked(clone, "git clone writeback repo")?;

    if !exists {
        let has_head = git_success(&[
            "-C".to_string(),
            target.display().to_string(),
            "rev-parse".to_string(),
            "--verify".to_string(),
            "HEAD".to_string(),
        ])?;
        if has_head {
            let mut switch = Command::new("git");
            switch.current_dir(&target).args(["switch", "-c", branch]);
            run_checked(switch, "git switch -c")?;
        } else {
            let refname = format!("refs/heads/{branch}");
            let mut symbolic = Command::new("git");
            symbolic
                .current_dir(&target)
                .args(["symbolic-ref", "HEAD", &refname]);
            run_checked(symbolic, "git symbolic-ref empty writeback branch")?;
        }
    }
    Ok(tmp)
}

fn apply_repo_effect(
    record: &EffectRecord,
    repo: &Path,
    rel_path: &Path,
    branch: &str,
    create_repo: bool,
    user_name: &str,
    user_email: &str,
) -> Result<String> {
    ensure_bare_repo(repo, branch, create_repo)?;
    let _repo_lock = repo_lock(repo)?;

    if let Some(commit) = prior_commit(repo, &record.effect_key)? {
        return Ok(commit);
    }

    let tmp = prepare_clone(repo, branch)?;
    let work = tmp.path().join("work");

    for (key, value) in [("user.name", user_name), ("user.email", user_email)] {
        let mut cmd = Command::new("git");
        cmd.current_dir(&work).args(["config", key, value]);
        run_checked(cmd, "git config writeback identity")?;
    }

    let target = ensure_no_symlink_path(&work, rel_path)?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("writeback target lacks parent"))?;
    fs::create_dir_all(parent)?;
    let mut tmp_file = NamedTempFile::new_in(parent)?;
    use std::io::Write;
    tmp_file.write_all(record.content.as_bytes())?;
    tmp_file.flush()?;
    tmp_file.persist(&target).map_err(|e| e.error)?;

    let mut add = Command::new("git");
    add.current_dir(&work).arg("add").arg("--").arg(rel_path);
    run_checked(add, "git add writeback target")?;

    let message = format!(
        "autoscribe effect {} call {}",
        record.effect_key, record.call_id
    );
    let mut commit = Command::new("git");
    commit
        .current_dir(&work)
        .args(["commit", "--allow-empty", "-m", &message]);
    run_checked(commit, "git commit writeback effect")?;

    let commit_sha = git_text(
        &[
            "-C".to_string(),
            work.display().to_string(),
            "rev-parse".to_string(),
            "HEAD".to_string(),
        ],
        "git rev-parse writeback commit",
    )?;

    let refspec = format!("HEAD:refs/heads/{branch}");
    let mut push = Command::new("git");
    push.current_dir(&work).args(["push", "origin", &refspec]);
    run_checked(push, "git push writeback effect")?;
    Ok(commit_sha)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let secret = read_effect_key(&policy.paths.effect_key_file)?;

    read_ndjson::<EffectRecord, _>(policy.limits.max_record_bytes, |record| {
        if record.schema != EFFECT_SCHEMA {
            bail!("unsupported effect schema: {}", record.schema);
        }
        safe_identifier(&record.call_id, "call_id", 160)?;
        if sha256_hex(record.content.as_bytes()) != record.content_sha256 {
            bail!("effect content hash mismatch");
        }

        let signed_payload = canonical_json(&json!({
            "schema": EFFECT_SCHEMA,
            "call_id": record.call_id.clone(),
            "effect_index": record.effect_index,
            "effect": record.effect.clone(),
            "content_sha256": record.content_sha256.clone(),
        }));
        verify_effect_signature(&secret, &signed_payload, &record.effect_key)?;

        let effect = signed_payload["effect"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("effect must be an object"))?;
        if effect.get("kind").and_then(Value::as_str) != Some("repo") {
            bail!("srv-writeback accepts only repo effects");
        }
        let repo = PathBuf::from(
            effect
                .get("repo")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("repo effect missing repo"))?,
        );
        let rel_path = PathBuf::from(
            effect
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("repo effect missing path"))?,
        );
        let branch = effect
            .get("branch")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("repo effect missing branch"))?;
        let create_repo = effect
            .get("create_repo")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let repo = validate_absolute_target(&repo, &policy.paths.repo_roots)?;
        validate_relative_path(&rel_path)?;
        validate_branch_name(branch)?;

        let _effect_lock = effect_lock(&policy.paths.effects_db, &record.effect_key)?;
        let conn = open_effects_db(&policy.paths.effects_db)?;
        if let Some(receipt) = existing_receipt(&conn, &record.effect_key)? {
            return write_ndjson(&receipt);
        }

        let commit = apply_repo_effect(
            &record,
            &repo,
            &rel_path,
            branch,
            create_repo,
            &policy.git.user_name,
            &policy.git.user_email,
        )
        .with_context(|| format!("failed applying effect {}", record.effect_key))?;

        let target = format!("{}:{}@{}", repo.display(), rel_path.display(), branch);
        let receipt = record_receipt(&conn, &record.effect_key, "repo", &target, &commit)?;
        write_ndjson(&receipt)
    })
}
