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
    Ok((rest[..end].to_string(), rest[end + marker.len()..].to_string()))
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
        "task" => "spc_",
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
    let first = chars.next().ok_or_else(|| anyhow::anyhow!("empty plan slug"))?;
    if !first.is_ascii_alphanumeric() {
        bail!("plan slug must begin with an ASCII letter or digit: {slug}");
    }
    if !chars.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) {
        bail!("plan slug contains unsafe characters: {slug}");
    }
    Ok(())
}

fn validate_tags(map: &serde_json::Map<String, Value>) -> Result<()> {
    let tags = map
        .get("tags")
        .ok_or_else(|| anyhow::anyhow!("instruction frontmatter.tags is required"))?
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("instruction frontmatter.tags must be an array"))?;
    for tag in tags {
        let tag = tag
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("instruction tags must be strings"))?;
        if tag.len() > 100 || tag.chars().any(char::is_control) {
            bail!("invalid instruction tag");
        }
    }
    Ok(())
}

fn reject_legacy_instruction_fields(map: &serde_json::Map<String, Value>) -> Result<()> {
    const LEGACY: &[&str] = &[
        "slug",
        "component",
        "kind",
        "record",
        "label",
        "version",
        "operation",
        "role",
        "context",
    ];
    for key in LEGACY {
        if map.contains_key(*key) {
            bail!("legacy instruction frontmatter field is not canonical: {key}");
        }
    }
    Ok(())
}

fn validate_instruction(map: &serde_json::Map<String, Value>, body: &str) -> Result<(String, String, String)> {
    let identity = required_string(map, "identity")?;
    let title = required_string(map, "title")?;
    let scope = required_string(map, "scope")?;
    validate_instruction_identity(&identity, &scope)?;
    validate_tags(map)?;
    reject_legacy_instruction_fields(map)?;
    if body.trim().is_empty() {
        bail!("instruction body may not be empty");
    }
    Ok((identity, title, scope))
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
    Ok(String::from_utf8(git_output_owned(&args, "git rev-parse control commit")?)?
        .trim()
        .to_string())
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

fn parse_record(path: &str, bytes: Vec<u8>, commit: &str, max_bytes: usize) -> Result<ControlRecord> {
    if bytes.len() > max_bytes {
        bail!("control record {path} exceeds {max_bytes} bytes");
    }
    let text = String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8"))?;
    if text.contains('\0') {
        bail!("control record {path} contains NUL bytes");
    }
    let (yaml_text, body) = parse_frontmatter(&text).with_context(|| path.to_string())?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&yaml_text)
        .with_context(|| format!("invalid YAML frontmatter in {path}"))?;
    let frontmatter = serde_json::to_value(yaml)
        .with_context(|| format!("frontmatter in {path} is not JSON-compatible"))?;
    reject_reserved_keys(&frontmatter).with_context(|| path.to_string())?;
    let map = frontmatter
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("frontmatter in {path} must be a mapping"))?;

    let record_type = required_string(map, "type")?;
    let (record_key, identity, slug, scope, title) = match record_type.as_str() {
        "instruction" => {
            let (identity, title, scope) = validate_instruction(map, &body)
                .with_context(|| path.to_string())?;
            (
                format!("instruction:{identity}"),
                Some(identity),
                None,
                Some(scope),
                title,
            )
        }
        "plan" => {
            let slug = required_string(map, "slug")?;
            validate_plan_slug(&slug).with_context(|| path.to_string())?;
            let title = required_string(map, "title")?;
            (
                format!("plan:{slug}"),
                None,
                Some(slug),
                None,
                title,
            )
        }
        other => bail!("unsupported Control record type in {path}: {other}"),
    };

    let frontmatter_json = String::from_utf8(canonical_json_bytes(&frontmatter)?)?;
    Ok(ControlRecord {
        record_key,
        record_type,
        identity,
        slug,
        scope,
        title,
        body,
        frontmatter,
        frontmatter_json,
        source_path: path.to_string(),
        source_commit: commit.to_string(),
        content_sha256: sha256_hex(text.as_bytes()),
    })
}

fn collect_instruction_refs(value: &Value, refs: &mut Vec<(String, String)>) -> Result<()> {
    match value {
        Value::Object(map) => {
            if let Some(instructions) = map.get("instructions") {
                let obj = instructions
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("plan instructions must be an object"))?;
                let expected: HashSet<&str> = ["role", "context", "task"].into_iter().collect();
                let actual: HashSet<&str> = obj.keys().map(String::as_str).collect();
                if actual != expected {
                    bail!("plan instructions require exactly role, context and task arrays");
                }
                for scope in ["role", "context", "task"] {
                    let values = obj[scope]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("plan instruction references must be arrays"))?;
                    for value in values {
                        let identity = value.as_str().ok_or_else(|| {
                            anyhow::anyhow!("plan instruction identities must be strings")
                        })?;
                        validate_instruction_identity(identity, scope)?;
                        refs.push((scope.to_string(), identity.to_string()));
                    }
                }
            }
            for (key, child) in map {
                if key != "instructions" {
                    collect_instruction_refs(child, refs)?;
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_instruction_refs(child, refs)?;
            }
        }
        _ => {}
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
        if path == "README.md" || path.starts_with('.') || !path.ends_with(".md") {
            continue;
        }
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
