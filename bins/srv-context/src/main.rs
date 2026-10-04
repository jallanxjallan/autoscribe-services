use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;
use srv_common::{canonical_json_bytes, load_policy, safe_identifier, sha256_hex};
use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(about = "Populate and resolve durable AutoScribe context records")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,

    #[arg(long)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create the context database schema if it does not already exist.
    Init,

    /// Transactionally upsert context records supplied as NDJSON on stdin.
    Put {
        #[arg(long, value_enum, default_value = "step")]
        origin: Origin,
    },

    /// Fetch one context record by its internal identity.
    Get { id: String },

    /// Resolve project-wide and optional source-specific records for selectors.
    Resolve {
        #[arg(long)]
        project: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long = "selector", required = true)]
        selectors: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Origin {
    Step,
    Upload,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Step => "step",
            Self::Upload => "upload",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextInput {
    project: String,
    #[serde(default)]
    source: Option<String>,
    selector: String,
    #[serde(default = "default_record_key")]
    key: String,
    content: String,
}

#[derive(Debug)]
struct PreparedContext {
    id: String,
    project: String,
    source: String,
    selector: String,
    key: String,
    content: String,
    content_sha256: String,
}

#[derive(Debug, Serialize)]
struct PutReceipt {
    schema: &'static str,
    id: String,
    project: String,
    source: Option<String>,
    selector: String,
    key: String,
    content_sha256: String,
    created: bool,
    changed: bool,
}

#[derive(Debug, Serialize)]
struct ContextRecord {
    id: String,
    project: String,
    source: Option<String>,
    selector: String,
    key: String,
    content: String,
    content_sha256: String,
    origin: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct Resolution {
    schema: &'static str,
    project: String,
    source: Option<String>,
    selectors: Vec<String>,
    records: Vec<ContextRecord>,
}

fn default_record_key() -> String {
    "default".to_string()
}

fn validate_scope_text(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 {
        bail!("{label} must be non-empty text of at most 512 bytes");
    }
    if value.trim() != value {
        bail!("{label} may not have leading or trailing whitespace");
    }
    if value
        .chars()
        .any(|ch| ch == '\0' || (ch.is_control() && ch != '\t'))
    {
        bail!("{label} contains a control character");
    }
    Ok(())
}

fn validate_context_text(content: &str, max_bytes: usize) -> Result<()> {
    if content.is_empty() {
        bail!("context content must be non-empty text");
    }
    if content.len() > max_bytes {
        bail!("context content exceeds {max_bytes} bytes");
    }
    for ch in content.chars() {
        if ch == '\0' || (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t' | '\u{000C}')) {
            bail!(
                "context content contains a non-text control character U+{:04X}",
                ch as u32
            );
        }
    }
    Ok(())
}

fn prepare_input(input: ContextInput, max_body_bytes: usize) -> Result<PreparedContext> {
    validate_scope_text(&input.project, "project")?;
    if let Some(source) = input.source.as_deref() {
        validate_scope_text(source, "source")?;
    }
    safe_identifier(&input.selector, "selector", 160)?;
    safe_identifier(&input.key, "context key", 160)?;
    validate_context_text(&input.content, max_body_bytes)?;

    let source = input.source.unwrap_or_default();
    let identity = json!({
        "project": &input.project,
        "source": &source,
        "selector": &input.selector,
        "key": &input.key,
    });
    let digest = sha256_hex(&canonical_json_bytes(&identity)?);
    let id = format!("ctx_{}", &digest[..32]);
    let content_sha256 = sha256_hex(input.content.as_bytes());

    Ok(PreparedContext {
        id,
        project: input.project,
        source,
        selector: input.selector,
        key: input.key,
        content: input.content,
        content_sha256,
    })
}

fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open context DB {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS context_records (
               id TEXT PRIMARY KEY,
               project TEXT NOT NULL,
               source TEXT NOT NULL DEFAULT '',
               selector TEXT NOT NULL,
               record_key TEXT NOT NULL,
               content TEXT NOT NULL,
               content_sha256 TEXT NOT NULL,
               origin TEXT NOT NULL CHECK (origin IN ('step', 'upload')),
               created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
               updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
               UNIQUE (project, source, selector, record_key)
           );

           CREATE INDEX IF NOT EXISTS context_resolve_idx
               ON context_records(project, selector, source, record_key);

           CREATE TABLE IF NOT EXISTS context_meta (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL
           );

           INSERT OR IGNORE INTO context_meta(key, value)
               VALUES ('schema_version', '1');"#,
    )?;
    Ok(())
}

fn read_inputs(max_record_bytes: usize, max_body_bytes: usize) -> Result<Vec<PreparedContext>> {
    let stdin = io::stdin();
    let mut records = Vec::new();
    let mut seen = HashSet::new();

    for (index, line) in stdin.lock().lines().enumerate() {
        let line_number = index + 1;
        let line = line.with_context(|| format!("failed reading NDJSON line {line_number}"))?;
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > max_record_bytes {
            bail!("NDJSON line {line_number} exceeds {max_record_bytes} bytes");
        }
        let input: ContextInput = serde_json::from_str(&line)
            .with_context(|| format!("invalid context JSON on line {line_number}"))?;
        let prepared = prepare_input(input, max_body_bytes)
            .with_context(|| format!("invalid context record on line {line_number}"))?;
        if !seen.insert(prepared.id.clone()) {
            bail!("context batch repeats {}", prepared.id);
        }
        records.push(prepared);
    }

    if records.is_empty() {
        bail!("no context records on stdin");
    }
    Ok(records)
}

fn put_records(
    conn: &mut Connection,
    records: &[PreparedContext],
    origin: Origin,
) -> Result<Vec<PutReceipt>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut receipts = Vec::with_capacity(records.len());

    for record in records {
        let existing_hash: Option<String> = tx
            .query_row(
                "SELECT content_sha256 FROM context_records WHERE id = ?1",
                params![&record.id],
                |row| row.get(0),
            )
            .optional()?;

        let created = existing_hash.is_none();
        let changed = existing_hash
            .as_deref()
            .map(|hash| hash != record.content_sha256)
            .unwrap_or(true);

        if created {
            tx.execute(
                "INSERT INTO context_records
                 (id, project, source, selector, record_key, content, content_sha256, origin)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    &record.id,
                    &record.project,
                    &record.source,
                    &record.selector,
                    &record.key,
                    &record.content,
                    &record.content_sha256,
                    origin.as_str(),
                ],
            )?;
        } else if changed {
            tx.execute(
                "UPDATE context_records
                 SET content = ?2,
                     content_sha256 = ?3,
                     origin = ?4,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?1",
                params![
                    &record.id,
                    &record.content,
                    &record.content_sha256,
                    origin.as_str(),
                ],
            )?;
        }

        receipts.push(PutReceipt {
            schema: "autoscribe.context-put.v1",
            id: record.id.clone(),
            project: record.project.clone(),
            source: if record.source.is_empty() {
                None
            } else {
                Some(record.source.clone())
            },
            selector: record.selector.clone(),
            key: record.key.clone(),
            content_sha256: record.content_sha256.clone(),
            created,
            changed,
        });
    }

    tx.commit()?;
    Ok(receipts)
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextRecord> {
    let source: String = row.get("source")?;
    Ok(ContextRecord {
        id: row.get("id")?,
        project: row.get("project")?,
        source: if source.is_empty() {
            None
        } else {
            Some(source)
        },
        selector: row.get("selector")?,
        key: row.get("record_key")?,
        content: row.get("content")?,
        content_sha256: row.get("content_sha256")?,
        origin: row.get("origin")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn get_record(conn: &Connection, id: &str) -> Result<ContextRecord> {
    safe_identifier(id, "context id", 160)?;
    conn.query_row(
        "SELECT id, project, source, selector, record_key, content, content_sha256,
                origin, created_at, updated_at
         FROM context_records WHERE id = ?1",
        params![id],
        row_to_record,
    )
    .optional()?
    .ok_or_else(|| anyhow::anyhow!("unknown context id: {id}"))
}

fn resolve_records(
    conn: &Connection,
    project: &str,
    source: Option<&str>,
    selectors: &[String],
) -> Result<Vec<ContextRecord>> {
    validate_scope_text(project, "project")?;
    if let Some(source) = source {
        validate_scope_text(source, "source")?;
    }

    let mut records = Vec::new();
    let mut seen_selectors = HashSet::new();

    for selector in selectors {
        safe_identifier(selector, "selector", 160)?;
        if !seen_selectors.insert(selector.as_str()) {
            continue;
        }

        if let Some(source) = source {
            let mut stmt = conn.prepare(
                "SELECT id, project, source, selector, record_key, content, content_sha256,
                        origin, created_at, updated_at
                 FROM context_records
                 WHERE project = ?1 AND selector = ?2 AND (source = '' OR source = ?3)
                 ORDER BY CASE WHEN source = '' THEN 0 ELSE 1 END, record_key, id",
            )?;
            let rows = stmt.query_map(params![project, selector, source], row_to_record)?;
            for row in rows {
                records.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, project, source, selector, record_key, content, content_sha256,
                        origin, created_at, updated_at
                 FROM context_records
                 WHERE project = ?1 AND selector = ?2 AND source = ''
                 ORDER BY record_key, id",
            )?;
            let rows = stmt.query_map(params![project, selector], row_to_record)?;
            for row in rows {
                records.push(row?);
            }
        }
    }

    Ok(records)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let db_path = args.db.unwrap_or_else(|| policy.paths.context_db.clone());
    let mut conn = open_db(&db_path)?;
    ensure_schema(&conn)?;

    match args.command {
        Command::Init => {
            println!("{}", db_path.display());
        }
        Command::Put { origin } => {
            let records =
                read_inputs(policy.limits.max_record_bytes, policy.limits.max_body_bytes)?;
            for receipt in put_records(&mut conn, &records, origin)? {
                println!("{}", serde_json::to_string(&receipt)?);
            }
        }
        Command::Get { id } => {
            println!("{}", serde_json::to_string(&get_record(&conn, &id)?)?);
        }
        Command::Resolve {
            project,
            source,
            selectors,
        } => {
            let records = resolve_records(&conn, &project, source.as_deref(), &selectors)?;
            let resolution = Resolution {
                schema: "autoscribe.context-resolution.v1",
                project,
                source,
                selectors,
                records,
            };
            println!("{}", serde_json::to_string(&resolution)?);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample(source: Option<&str>, key: &str, content: &str) -> PreparedContext {
        prepare_input(
            ContextInput {
                project: "book-hhp".to_string(),
                source: source.map(str::to_string),
                selector: "briefing".to_string(),
                key: key.to_string(),
                content: content.to_string(),
            },
            1024 * 1024,
        )
        .unwrap()
    }

    #[test]
    fn identity_is_stable_across_content_updates() {
        let a = sample(Some("case-1"), "default", "first");
        let b = sample(Some("case-1"), "default", "second");
        assert_eq!(a.id, b.id);
        assert_ne!(a.content_sha256, b.content_sha256);
    }

    #[test]
    fn put_is_idempotent_and_updates_changed_content() {
        let dir = tempdir().unwrap();
        let mut conn = open_db(&dir.path().join("context.sqlite")).unwrap();
        ensure_schema(&conn).unwrap();

        let first = sample(None, "default", "project briefing");
        let receipts = put_records(&mut conn, &[first], Origin::Upload).unwrap();
        assert!(receipts[0].created);
        assert!(receipts[0].changed);

        let same = sample(None, "default", "project briefing");
        let receipts = put_records(&mut conn, &[same], Origin::Upload).unwrap();
        assert!(!receipts[0].created);
        assert!(!receipts[0].changed);

        let changed = sample(None, "default", "revised briefing");
        let id = changed.id.clone();
        let receipts = put_records(&mut conn, &[changed], Origin::Step).unwrap();
        assert!(!receipts[0].created);
        assert!(receipts[0].changed);
        assert_eq!(get_record(&conn, &id).unwrap().content, "revised briefing");
    }

    #[test]
    fn resolve_combines_project_and_source_context() {
        let dir = tempdir().unwrap();
        let mut conn = open_db(&dir.path().join("context.sqlite")).unwrap();
        ensure_schema(&conn).unwrap();

        let project = sample(None, "project", "project context");
        let source = sample(Some("case-1"), "source", "source context");
        put_records(&mut conn, &[project, source], Origin::Step).unwrap();

        let records =
            resolve_records(&conn, "book-hhp", Some("case-1"), &["briefing".to_string()]).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records[0].source.is_none());
        assert_eq!(records[1].source.as_deref(), Some("case-1"));
    }
}
