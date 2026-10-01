use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use srv_common::{
    git_output_owned, load_policy, run_checked, validate_absolute_target, validate_branch_name,
};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const CANDIDATE_REF: &str = "refs/autoscribe/control-sync-candidate";
const DEFAULT_INGEST: &str = "/opt/autoscribe/services/current/bin/srv-control-ingest";

#[derive(Parser, Debug)]
#[command(
    about = "Fetch, validate, verify and atomically promote the authoritative Control snapshot"
)]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,

    /// Bare Control repository. Defaults to $HOME/Repos/control.git.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Git remote name or URL used as the authoritative source.
    #[arg(long, default_value = "origin")]
    remote: String,

    /// Authoritative branch. Defaults to policy.git.default_branch.
    #[arg(long)]
    branch: Option<String>,

    /// Live Control SQLite path. Defaults to policy.paths.control_db.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Snapshot validator/builder binary.
    #[arg(long, default_value = DEFAULT_INGEST)]
    ingest: PathBuf,

    /// Rebuild from the current local branch without fetching first.
    #[arg(long)]
    no_fetch: bool,
}

#[derive(Debug, Clone, Copy)]
struct CatalogueCounts {
    instructions: usize,
    steps: usize,
    plans: usize,
    plan_steps: usize,
    step_instructions: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct CatalogueDelta {
    added: usize,
    changed: usize,
    deleted: usize,
}

fn default_repo() -> Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join("Repos/control.git"))
}

fn validate_remote(remote: &str) -> Result<()> {
    if remote.is_empty() || remote.starts_with('-') {
        bail!("invalid Control remote");
    }
    if remote.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        bail!("Control remote may not contain control characters or whitespace");
    }
    Ok(())
}

fn git(repo: &Path, args: &[String], label: &str) -> Result<String> {
    let mut owned = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
    ];
    owned.extend(args.iter().cloned());
    let output = git_output_owned(&owned, label)?;
    Ok(String::from_utf8(output)
        .with_context(|| format!("{label} returned non-UTF-8 output"))?
        .trim()
        .to_string())
}

fn resolve_commit(repo: &Path, reference: &str) -> Result<String> {
    git(
        repo,
        &[
            "rev-parse".to_string(),
            "--verify".to_string(),
            format!("{reference}^{{commit}}"),
        ],
        "git rev-parse Control commit",
    )
}

fn assert_bare_repo(repo: &Path) -> Result<()> {
    let value = git(
        repo,
        &["rev-parse".to_string(), "--is-bare-repository".to_string()],
        "git verify bare Control repo",
    )?;
    if value != "true" {
        bail!("Control repo must be bare: {}", repo.display());
    }
    Ok(())
}

fn fetch_candidate(repo: &Path, remote: &str, branch: &str) -> Result<String> {
    let refspec = format!("+refs/heads/{branch}:{CANDIDATE_REF}");
    git(
        repo,
        &[
            "fetch".to_string(),
            "--no-tags".to_string(),
            remote.to_string(),
            refspec,
        ],
        "git fetch authoritative Control snapshot",
    )?;
    resolve_commit(repo, CANDIDATE_REF)
}

fn delete_candidate_ref(repo: &Path) {
    let _ = git(
        repo,
        &[
            "update-ref".to_string(),
            "-d".to_string(),
            CANDIDATE_REF.to_string(),
        ],
        "git delete Control candidate ref",
    );
}

fn update_main_ref(repo: &Path, branch: &str, new_commit: &str, old_commit: &str) -> Result<()> {
    git(
        repo,
        &[
            "update-ref".to_string(),
            format!("refs/heads/{branch}"),
            new_commit.to_string(),
            old_commit.to_string(),
        ],
        "git promote Control snapshot",
    )?;
    Ok(())
}

fn run_ingest(
    ingest: &Path,
    policy: &Path,
    repo: &Path,
    commit: &str,
    db: Option<&Path>,
    check_only: bool,
) -> Result<String> {
    if !ingest.is_file() {
        bail!("Control ingester is not a file: {}", ingest.display());
    }

    let mut cmd = Command::new(ingest);
    cmd.arg("--policy")
        .arg(policy)
        .arg("--repo")
        .arg(repo)
        .arg("--commit")
        .arg(commit);

    if let Some(db) = db {
        cmd.arg("--db").arg(db);
    }
    if check_only {
        cmd.arg("--check-only");
    }

    let output = run_checked(cmd, "srv-control-ingest")?;
    Ok(String::from_utf8(output.stdout)
        .context("srv-control-ingest returned non-UTF-8 output")?
        .trim()
        .to_string())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn cleanup_candidate_db(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        let _ = fs::remove_file(candidate);
    }
}

fn candidate_db_path(db: &Path) -> Result<PathBuf> {
    let parent = db
        .parent()
        .ok_or_else(|| anyhow!("Control DB has no parent: {}", db.display()))?;
    let name = db
        .file_name()
        .ok_or_else(|| anyhow!("Control DB has no filename: {}", db.display()))?
        .to_string_lossy();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    Ok(parent.join(format!(
        ".{name}.candidate.{}.{}",
        std::process::id(),
        stamp
    )))
}

fn lock_path(db: &Path) -> Result<PathBuf> {
    let parent = db
        .parent()
        .ok_or_else(|| anyhow!("Control DB has no parent: {}", db.display()))?;
    let name = db
        .file_name()
        .ok_or_else(|| anyhow!("Control DB has no filename: {}", db.display()))?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.sync.lock")))
}

fn acquire_lock(db: &Path) -> Result<File> {
    let path = lock_path(db)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open sync lock {}", path.display()))?;
    file.try_lock_exclusive()
        .with_context(|| format!("another Control sync is already running ({})", path.display()))?;
    Ok(file)
}

fn finalize_candidate_db(path: &Path) -> Result<()> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open candidate DB {}", path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA wal_checkpoint(TRUNCATE);")?;
    let journal_mode: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        bail!(
            "candidate DB refused DELETE journal mode: {}",
            journal_mode
        );
    }
    drop(conn);

    for path in [sidecar(path, "-wal"), sidecar(path, "-shm")] {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove candidate sidecar {}", path.display()))?;
        }
    }

    File::open(path)
        .with_context(|| format!("failed to reopen candidate DB {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to fsync candidate DB {}", path.display()))?;
    Ok(())
}

fn meta_value(conn: &Connection, key: &str) -> Result<String> {
    conn.query_row(
        "SELECT value FROM control_meta WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| anyhow!("candidate DB missing control_meta key {key}"))
}

fn table_count(conn: &Connection, table: &str) -> Result<usize> {
    let sql = match table {
        "instructions" => "SELECT COUNT(*) FROM instructions",
        "steps" => "SELECT COUNT(*) FROM steps",
        "plans" => "SELECT COUNT(*) FROM plans",
        "plan_steps" => "SELECT COUNT(*) FROM plan_steps",
        "step_instructions" => "SELECT COUNT(*) FROM step_instructions",
        _ => bail!("unsupported Control table {table}"),
    };
    let count: i64 = conn.query_row(sql, [], |row| row.get(0))?;
    usize::try_from(count).context("negative Control table count")
}

fn meta_count(conn: &Connection, key: &str) -> Result<usize> {
    meta_value(conn, key)?
        .parse::<usize>()
        .with_context(|| format!("invalid numeric control_meta value for {key}"))
}

fn verify_database(path: &Path, expected_commit: &str) -> Result<CatalogueCounts> {
    if !path.is_file() {
        bail!("Control DB is missing: {}", path.display());
    }

    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open Control DB {}", path.display()))?;

    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("SQLite integrity_check failed: {integrity}");
    }

    {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let table: String = row.get(0)?;
            let rowid: Option<i64> = row.get(1)?;
            let parent: String = row.get(2)?;
            let fkid: i64 = row.get(3)?;
            bail!(
                "SQLite foreign_key_check failed: table={table} rowid={rowid:?} parent={parent} fkid={fkid}"
            );
        }
    }

    let schema_version = meta_value(&conn, "schema_version")?;
    if schema_version != "3" {
        bail!("unexpected Control schema version {schema_version}");
    }

    let source_commit = meta_value(&conn, "source_commit")?;
    if source_commit != expected_commit {
        bail!(
            "Control DB source_commit mismatch: expected {expected_commit}, found {source_commit}"
        );
    }

    let counts = CatalogueCounts {
        instructions: table_count(&conn, "instructions")?,
        steps: table_count(&conn, "steps")?,
        plans: table_count(&conn, "plans")?,
        plan_steps: table_count(&conn, "plan_steps")?,
        step_instructions: table_count(&conn, "step_instructions")?,
    };

    for (key, actual) in [
        ("instruction_count", counts.instructions),
        ("step_count", counts.steps),
        ("plan_count", counts.plans),
        ("plan_step_count", counts.plan_steps),
        ("step_instruction_count", counts.step_instructions),
    ] {
        let recorded = meta_count(&conn, key)?;
        if recorded != actual {
            bail!(
                "Control DB count mismatch for {key}: metadata={recorded}, actual={actual}"
            );
        }
    }

    Ok(counts)
}

fn load_fingerprints(path: &Path) -> Result<HashMap<String, String>> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }

    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open live Control DB {}", path.display()))?;

    let mut result = HashMap::new();
    for (table, prefix) in [
        ("instructions", "instruction"),
        ("steps", "step"),
        ("plans", "plan"),
    ] {
        let sql = format!("SELECT id, content_sha256 FROM {table}");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, hash) = row?;
            result.insert(format!("{prefix}:{id}"), hash);
        }
    }
    Ok(result)
}

fn catalogue_delta(
    old: &HashMap<String, String>,
    new: &HashMap<String, String>,
) -> CatalogueDelta {
    let mut delta = CatalogueDelta::default();

    for (identity, hash) in new {
        match old.get(identity) {
            None => delta.added += 1,
            Some(old_hash) if old_hash != hash => delta.changed += 1,
            Some(_) => {}
        }
    }

    for identity in old.keys() {
        if !new.contains_key(identity) {
            delta.deleted += 1;
        }
    }

    delta
}

fn sync_parent_directory(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

fn promote(
    repo: &Path,
    branch: &str,
    current: &str,
    candidate: &str,
    candidate_db: &Path,
    live_db: &Path,
) -> Result<()> {
    update_main_ref(repo, branch, candidate, current)?;

    if let Err(error) = fs::rename(candidate_db, live_db) {
        let rollback = update_main_ref(repo, branch, current, candidate);
        return match rollback {
            Ok(()) => Err(anyhow!(
                "failed to install candidate DB {}; Git ref rolled back: {}",
                live_db.display(),
                error
            )),
            Err(rollback_error) => bail!(
                "CRITICAL: failed to install candidate DB {} ({error}); Git ref rollback also failed: {rollback_error:#}",
                live_db.display()
            ),
        };
    }

    sync_parent_directory(live_db);
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;

    let requested_repo = match args.repo {
        Some(path) => path,
        None => default_repo()?,
    };
    let repo = validate_absolute_target(&requested_repo, &policy.paths.repo_roots)?;
    if !repo.is_dir() {
        bail!("Control repo is not a directory: {}", repo.display());
    }
    assert_bare_repo(&repo)?;

    let branch = args
        .branch
        .unwrap_or_else(|| policy.git.default_branch.clone());
    validate_branch_name(&branch)?;
    validate_remote(&args.remote)?;

    let db = args.db.unwrap_or_else(|| policy.paths.control_db.clone());
    if !db.is_absolute() {
        bail!("Control DB path must be absolute: {}", db.display());
    }
    let db_parent = db
        .parent()
        .ok_or_else(|| anyhow!("Control DB has no parent: {}", db.display()))?;
    fs::create_dir_all(db_parent)
        .with_context(|| format!("failed to create {}", db_parent.display()))?;

    let _lock = acquire_lock(&db)?;
    let current = resolve_commit(&repo, &format!("refs/heads/{branch}"))?;

    let candidate = if args.no_fetch {
        current.clone()
    } else {
        delete_candidate_ref(&repo);
        println!("Fetching Control {}/{}...", args.remote, branch);
        match fetch_candidate(&repo, &args.remote, &branch) {
            Ok(commit) => commit,
            Err(error) => {
                delete_candidate_ref(&repo);
                return Err(error);
            }
        }
    };

    let candidate_db = candidate_db_path(&db)?;
    cleanup_candidate_db(&candidate_db);

    let result = (|| -> Result<()> {
        if candidate == current {
            if let Ok(counts) = verify_database(&db, &candidate) {
                println!("Control already current: {candidate}");
                println!(
                    "catalogue: {} instructions, {} steps, {} plans",
                    counts.instructions, counts.steps, counts.plans
                );
                return Ok(());
            }
        }

        println!("Validating candidate {candidate}...");
        let validation = run_ingest(
            &args.ingest,
            &args.policy,
            &repo,
            &candidate,
            None,
            true,
        )?;
        if !validation.is_empty() {
            println!("{validation}");
        }

        println!("Building candidate database...");
        let build = run_ingest(
            &args.ingest,
            &args.policy,
            &repo,
            &candidate,
            Some(&candidate_db),
            false,
        )?;
        if !build.is_empty() {
            println!("{build}");
        }

        finalize_candidate_db(&candidate_db)?;
        let counts = verify_database(&candidate_db, &candidate)?;

        let old_fingerprints = match load_fingerprints(&db) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "warning: could not read live catalogue for delta; replacing it after candidate verification: {error:#}"
                );
                HashMap::new()
            }
        };
        let new_fingerprints = load_fingerprints(&candidate_db)?;
        let delta = catalogue_delta(&old_fingerprints, &new_fingerprints);

        println!("database integrity ok");
        println!(
            "changes: +{} ~{} -{}",
            delta.added, delta.changed, delta.deleted
        );

        promote(
            &repo,
            &branch,
            &current,
            &candidate,
            &candidate_db,
            &db,
        )?;

        println!("updated {current} -> {candidate}");
        println!(
            "catalogue: {} instructions, {} steps, {} plans, {} plan-step links, {} step-instruction links",
            counts.instructions,
            counts.steps,
            counts.plans,
            counts.plan_steps,
            counts.step_instructions
        );
        println!("Control DB: {}", db.display());

        Ok(())
    })();

    cleanup_candidate_db(&candidate_db);
    if !args.no_fetch {
        delete_candidate_ref(&repo);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_rejects_option_and_whitespace_forms() {
        assert!(validate_remote("origin").is_ok());
        assert!(validate_remote("https://github.com/example/control.git").is_ok());
        assert!(validate_remote("-evil").is_err());
        assert!(validate_remote("bad remote").is_err());
    }

    #[test]
    fn catalogue_delta_tracks_add_change_delete() {
        let old = HashMap::from([
            ("instruction:a".to_string(), "1".to_string()),
            ("step:b".to_string(), "2".to_string()),
            ("plan:c".to_string(), "3".to_string()),
        ]);
        let new = HashMap::from([
            ("instruction:a".to_string(), "1".to_string()),
            ("step:b".to_string(), "changed".to_string()),
            ("plan:d".to_string(), "4".to_string()),
        ]);

        let delta = catalogue_delta(&old, &new);
        assert_eq!(delta.added, 1);
        assert_eq!(delta.changed, 1);
        assert_eq!(delta.deleted, 1);
    }
}
