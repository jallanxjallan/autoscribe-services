use anyhow::{bail, Context, Result};
use clap::Parser;
use rusqlite::{params, Connection};
use serde_json::Value;
use srv_common::{
    canonical_json_bytes, git_output_owned, load_policy, reject_reserved_keys, sha256_hex,
    validate_absolute_target,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(about = "Validate a Control Git snapshot and atomically rebuild the trusted control DB")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    commit: String,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    check_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordKind {
    Instruction(&'static str),
    Plan,
}

#[derive(Debug)]
struct ControlRecord {
    record_key: String,
    record_type: String,
    identity: Option<String>,
    slug: Option<String>,
    scope: Option<String>,
    title: String,
    body: String,
    frontmatter: Value,
    frontmatter_json: String,
    source_path: String,
    source_commit: String,
    content_sha256: String,
}

fn classify_record_path(path: &str) -> Option<RecordKind> {
    if path.starts_with("roles/") && path.ends_with(".md") {
        Some(RecordKind::Instruction("role"))
    } else if path.starts_with("contexts/") && path.ends_with(".md") {
        Some(RecordKind::Instruction("context"))
    } else if path.starts_with("tasks/") && path.ends_with(".md") {
        Some(RecordKind::Instruction("task"))
    } else if path.starts_with("plans/") && path.ends_with(".json") {
        Some(RecordKind::Plan)
    } else {
        None
    }
}

fn parse_frontmatter(text: &str) -> Result<(String, String)> {
    let normalized = text.replace("\r\n", "\n");
    if !normalized.starts_with("---\n") {
        bail!("control record must begin with YAML frontmatter");
    }
    let rest = &normalized[4..];
    let marker = "\n---\n";
    let end = rest
        .find(marker)
        .ok_or_else(|| anyhow::anyhow!("control record frontmatter is not terminated by ---"))?;
    Ok((
        rest[..end].to_string(),
        rest[end + marker.len()..].to_string(),
    ))
}

fn required_string(map: &serde_json::Map<String, Value>, key: &str) -> Result<String> {
    let value = map
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("frontmatter.{key} must be a string"))?;
    if value.trim().is_empty() {
        bail!("frontmatter.{key} may not be empty");
    }
    Ok(value.to_string())
}

fn is_crockford_char(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
}

fn validate_instruction_identity(identity: &str, scope: &str) -> Result<()> {
    let prefix = match scope {
        "role" => "rol_",
        "context" => "ctx_",
        "task" => "tsk_",
        other => bail!("invalid instruction scope: {other}"),
    };
    let suffix = identity
        .strip_prefix(prefix)
        .ok_or_else(|| anyhow::anyhow!("{scope} identity must begin with {prefix}"))?;
    if suffix.len() != 16 || !suffix.bytes().all(is_crockford_char) {
        bail!(
            "instruction identity must be {prefix} followed by 16 uppercase Crockford Base32 characters: {identity}"
        );
    }
    Ok(())
}

fn validate_plan_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || slug.len() > 200 {
        bail!("invalid plan slug length");
    }
    let mut chars = slug.bytes();
    let first = chars
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty plan slug"))?;
    if !first.is_ascii_alphanumeric() {
        bail!("plan slug must begin with an ASCII letter or digit: {slug}");
    }
    if !chars.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) {
        bail!("plan slug contains unsafe characters: {slug}");
    }
    Ok(())
}

fn resolve_commit(repo: &Path, commit: &str) -> Result<String> {
    if commit.is_empty()
        || commit.len() > 128
        || !commit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
    {
        bail!("unsafe commit/ref name");
    }
    let args = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
        "rev-parse".to_string(),
        format!("{}^{{commit}}", commit),
    ];
    Ok(
        String::from_utf8(git_output_owned(&args, "git rev-parse control commit")?)?
            .trim()
            .to_string(),
    )
}

fn list_paths(repo: &Path, commit: &str) -> Result<Vec<String>> {
    let args = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
        "ls-tree".to_string(),
        "-r".to_string(),
        "-z".to_string(),
        "--name-only".to_string(),
        commit.to_string(),
    ];
    let output = git_output_owned(&args, "git ls-tree control snapshot")?;
    output
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8(part.to_vec()).context("control path must be UTF-8"))
        .collect()
}

fn read_blob(repo: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    if path.contains('\0') || path.chars().any(char::is_control) {
        bail!("unsafe control path");
    }
    let args = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
        "cat-file".to_string(),
        "blob".to_string(),
        format!("{commit}:{path}"),
    ];
    git_output_owned(&args, "git cat-file control record")
}

fn checked_text(path: &str, bytes: Vec<u8>, max_bytes: usize) -> Result<String> {
    if bytes.len() > max_bytes {
        bail!("control record {path} exceeds {max_bytes} bytes");
    }
    let text = String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8"))?;
    if text.contains('\0') {
        bail!("control record {path} contains NUL bytes");
    }
    Ok(text)
}

fn parse_instruction_record(
    path: &str,
    text: String,
    commit: &str,
    expected_scope: &str,
) -> Result<ControlRecord> {
    let (yaml_text, body) = parse_frontmatter(&text).with_context(|| path.to_string())?;

    let yaml: serde_yaml::Value = serde_yaml::from_str(&yaml_text)
        .with_context(|| format!("invalid YAML frontmatter in {path}"))?;

    let authored = serde_json::to_value(yaml)
        .with_context(|| format!("frontmatter in {path} is not JSON-compatible"))?;

    let authored_map = authored
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("frontmatter in {path} must be a mapping"))?;

    // The only authored frontmatter field with runtime meaning.
    let identity = required_string(authored_map, "identity").with_context(|| path.to_string())?;

    // Identity prefix must agree with the directory-derived scope.
    validate_instruction_identity(&identity, expected_scope).with_context(|| path.to_string())?;

    if body.trim().is_empty() {
        bail!("instruction body may not be empty: {path}");
    }

    // Filesystem structure is authoritative for runtime metadata.
    let title = Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot derive title from {path}"))?
        .to_string();

    // Build a closed runtime representation. All other authored
    // frontmatter is deliberately ignored.
    let mut runtime_map = serde_json::Map::new();
    runtime_map.insert("identity".to_string(), Value::String(identity.clone()));
    runtime_map.insert("type".to_string(), Value::String("instruction".to_string()));
    runtime_map.insert(
        "scope".to_string(),
        Value::String(expected_scope.to_string()),
    );
    runtime_map.insert("title".to_string(), Value::String(title.clone()));

    let frontmatter = Value::Object(runtime_map);
    let frontmatter_json = String::from_utf8(canonical_json_bytes(&frontmatter)?)?;

    Ok(ControlRecord {
        record_key: format!("instruction:{identity}"),
        record_type: "instruction".to_string(),
        identity: Some(identity),
        slug: None,
        scope: Some(expected_scope.to_string()),
        title,
        body,
        frontmatter,
        frontmatter_json,
        source_path: path.to_string(),
        source_commit: commit.to_string(),
        content_sha256: sha256_hex(text.as_bytes()),
    })
}

fn parse_plan_record(path: &str, text: String, commit: &str) -> Result<ControlRecord> {
    let plan: Value =
        serde_json::from_str(&text).with_context(|| format!("invalid JSON plan in {path}"))?;

    reject_reserved_keys(&plan).with_context(|| path.to_string())?;

    let map = plan
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("plan {path} must be a JSON object"))?;

    let slug = required_string(map, "identity")?;
    validate_plan_slug(&slug).with_context(|| path.to_string())?;

    let title = required_string(map, "title")?;

    map.get("steps")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("plan {path} steps must be an object"))?;

    let frontmatter_json = String::from_utf8(canonical_json_bytes(&plan)?)?;

    Ok(ControlRecord {
        record_key: format!("plan:{slug}"),
        record_type: "plan".to_string(),
        identity: None,
        slug: Some(slug),
        scope: None,
        title,
        body: String::new(),
        frontmatter: plan,
        frontmatter_json,
        source_path: path.to_string(),
        source_commit: commit.to_string(),
        content_sha256: sha256_hex(text.as_bytes()),
    })
}

fn parse_record(
    path: &str,
    bytes: Vec<u8>,
    commit: &str,
    max_bytes: usize,
    kind: RecordKind,
) -> Result<ControlRecord> {
    let text = checked_text(path, bytes, max_bytes)?;

    match kind {
        RecordKind::Instruction(expected_scope) => {
            parse_instruction_record(path, text, commit, expected_scope)
        }
        RecordKind::Plan => parse_plan_record(path, text, commit),
    }
}

fn collect_instruction_refs(value: &Value, refs: &mut Vec<(String, String)>) -> Result<()> {
    let plan = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("plan must be an object"))?;

    let steps = plan
        .get("steps")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("plan steps must be an object"))?;

    for (step_number, step_value) in steps {
        let step = step_value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("plan step {step_number} must be an object"))?;

        let Some(instructions) = step.get("instructions") else {
            continue;
        };

        let obj = instructions.as_object().ok_or_else(|| {
            anyhow::anyhow!("plan step {step_number} instructions must be an object")
        })?;

        for (scope, identities) in obj {
            if !matches!(scope.as_str(), "role" | "context" | "task") {
                bail!("invalid instruction scope {scope} in plan step {step_number}");
            }

            let values = identities.as_array().ok_or_else(|| {
                anyhow::anyhow!("plan step {step_number} instruction references must be arrays")
            })?;

            for value in values {
                let identity = value.as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "plan step {step_number} instruction identities must be strings"
                    )
                })?;

                validate_instruction_identity(identity, scope)?;
                refs.push((scope.to_string(), identity.to_string()));
            }
        }
    }

    Ok(())
}

fn validate_cross_references(records: &[ControlRecord]) -> Result<()> {
    let mut instructions = HashMap::new();
    for record in records {
        if record.record_type == "instruction" {
            let identity = record.identity.as_deref().unwrap_or_default();
            let scope = record.scope.as_deref().unwrap_or_default();
            instructions.insert(identity, scope);
        }
    }

    for record in records {
        if record.record_type != "plan" {
            continue;
        }
        let mut refs = Vec::new();
        collect_instruction_refs(&record.frontmatter, &mut refs)
            .with_context(|| record.source_path.clone())?;
        for (scope, identity) in refs {
            let actual = instructions.get(identity.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "plan {} references missing instruction {}",
                    record.source_path,
                    identity
                )
            })?;
            if *actual != scope {
                bail!(
                    "plan {} references {} as {}, but instruction scope is {}",
                    record.source_path,
                    identity,
                    scope,
                    actual
                );
            }
        }
    }
    Ok(())
}

fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open control DB {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS control_records (
            record_key TEXT PRIMARY KEY,
            record_type TEXT NOT NULL,
            identity TEXT UNIQUE,
            slug TEXT UNIQUE,
            scope TEXT,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            frontmatter_json TEXT NOT NULL,
            source_path TEXT NOT NULL UNIQUE,
            source_commit TEXT NOT NULL,
            content_sha256 TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS control_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS control_records_type_idx ON control_records(record_type);
         CREATE INDEX IF NOT EXISTS control_records_scope_idx ON control_records(scope);",
    )?;
    Ok(conn)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let repo = validate_absolute_target(&args.repo, &policy.paths.repo_roots)?;
    if !repo.is_dir() {
        bail!("Control repo is not a directory: {}", repo.display());
    }
    let commit = resolve_commit(&repo, &args.commit)?;
    let paths = list_paths(&repo, &commit)?;

    let mut records = Vec::new();
    let mut record_keys = HashSet::new();
    let mut source_paths = HashSet::new();

    for path in paths {
        let Some(kind) = classify_record_path(&path) else {
            continue;
        };
        if records.len() >= policy.limits.max_control_records {
            bail!(
                "control snapshot exceeds {} records",
                policy.limits.max_control_records
            );
        }
        let blob = read_blob(&repo, &commit, &path)?;
        let record = parse_record(
            &path,
            blob,
            &commit,
            policy.limits.max_control_file_bytes,
            kind,
        )?;
        if !record_keys.insert(record.record_key.clone()) {
            bail!("duplicate Control key: {}", record.record_key);
        }
        if !source_paths.insert(record.source_path.clone()) {
            bail!("duplicate Control source path: {}", record.source_path);
        }
        records.push(record);
    }

    validate_cross_references(&records)?;

    if args.check_only {
        println!("validated {} Control records at {}", records.len(), commit);
        return Ok(());
    }

    let db_path = args.db.unwrap_or_else(|| policy.paths.control_db.clone());
    let mut conn = open_db(&db_path)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM control_records", [])?;
    for record in &records {
        tx.execute(
            "INSERT INTO control_records
             (record_key, record_type, identity, slug, scope, title, body, frontmatter_json,
              source_path, source_commit, content_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                &record.record_key,
                &record.record_type,
                record.identity.as_deref(),
                record.slug.as_deref(),
                record.scope.as_deref(),
                &record.title,
                &record.body,
                &record.frontmatter_json,
                &record.source_path,
                &record.source_commit,
                &record.content_sha256,
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO control_meta(key, value) VALUES ('source_commit', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![commit],
    )?;
    tx.execute(
        "INSERT INTO control_meta(key, value) VALUES ('record_count', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![records.len().to_string()],
    )?;
    tx.commit()?;

    println!(
        "ingested {} Control records from {} into {}",
        records.len(),
        commit,
        db_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_canonical_control_paths() {
        assert_eq!(
            classify_record_path("roles/Writer.md"),
            Some(RecordKind::Instruction("role"))
        );
        assert_eq!(
            classify_record_path("contexts/Project.md"),
            Some(RecordKind::Instruction("context"))
        );
        assert_eq!(
            classify_record_path("tasks/Proofread.md"),
            Some(RecordKind::Instruction("task"))
        );
        assert_eq!(
            classify_record_path("plans/plan.example.json"),
            Some(RecordKind::Plan)
        );

        assert_eq!(classify_record_path("AGENTS.md"), None);
        assert_eq!(classify_record_path("Templates/New Task.md"), None);
        assert_eq!(classify_record_path("plans/plan.example.md"), None);
    }

    #[test]
    fn task_identity_uses_tsk_prefix() {
        validate_instruction_identity("tsk_T3W8N5R7C2M9X6QK", "task").unwrap();
        assert!(validate_instruction_identity("spc_T3W8N5R7C2M9X6QK", "task").is_err());
    }

    #[test]
    fn parses_live_json_plan_shape_with_optional_empty_channels() {
        let text = r#"{
            "identity": "plan.hhp-normalize-and-proofread.8m4q2v",
            "title": "HHP Normalize and Proofread",
            "description": "Example",
            "steps": {
                "1": {
                    "engine": "chatgpt",
                    "instructions": {
                        "role": ["rol_K7M4Q9V2X6C8B3RN"],
                        "task": ["tsk_T3W8N5R7C2M9X6QK"]
                    }
                }
            }
        }"#;

        let record = parse_plan_record("plans/example.json", text.to_string(), "abc123").unwrap();

        assert_eq!(record.record_type, "plan");
        assert_eq!(
            record.slug.as_deref(),
            Some("plan.hhp-normalize-and-proofread.8m4q2v")
        );

        let mut refs = Vec::new();
        collect_instruction_refs(&record.frontmatter, &mut refs).unwrap();
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn instruction_directory_must_match_scope() {
        let text = "---\ntitle: Writer\nidentity: rol_K7M4Q9V2X6C8B3RN\ntype: instruction\nscope: role\ntags: []\n---\nWrite clearly.\n";

        assert!(
            parse_instruction_record("tasks/Writer.md", text.to_string(), "abc123", "task")
                .is_err()
        );
    }
}
