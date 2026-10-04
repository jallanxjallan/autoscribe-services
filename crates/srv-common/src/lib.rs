use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

pub const INPUT_SCHEMA: &str = "autoscribe.input.v1";
pub const RESPONSE_SCHEMA: &str = "autoscribe.response.v1";
pub const EFFECT_SCHEMA: &str = "autoscribe.effect.v1";
pub const RECEIPT_SCHEMA: &str = "autoscribe.receipt.v1";

#[derive(Debug, Clone, Deserialize)]
pub struct Policy {
    pub paths: PathsPolicy,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub git: GitPolicy,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PathsPolicy {
    #[serde(default)]
    pub repo_roots: Vec<PathBuf>,
    #[serde(default)]
    pub file_roots: Vec<PathBuf>,
    pub effect_key_file: PathBuf,
    #[serde(default = "default_effects_db")]
    pub effects_db: PathBuf,
    #[serde(default = "default_control_db")]
    pub control_db: PathBuf,
    #[serde(default = "default_context_db")]
    pub context_db: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    #[serde(default = "default_max_record_bytes")]
    pub max_record_bytes: usize,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_max_outputs")]
    pub max_outputs: usize,
    #[serde(default = "default_max_control_file_bytes")]
    pub max_control_file_bytes: usize,
    #[serde(default = "default_max_control_records")]
    pub max_control_records: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_record_bytes: default_max_record_bytes(),
            max_body_bytes: default_max_body_bytes(),
            max_outputs: default_max_outputs(),
            max_control_file_bytes: default_max_control_file_bytes(),
            max_control_records: default_max_control_records(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitPolicy {
    #[serde(default = "default_branch")]
    pub default_branch: String,
    #[serde(default = "default_git_user_name")]
    pub user_name: String,
    #[serde(default = "default_git_user_email")]
    pub user_email: String,
}

impl Default for GitPolicy {
    fn default() -> Self {
        Self {
            default_branch: default_branch(),
            user_name: default_git_user_name(),
            user_email: default_git_user_email(),
        }
    }
}

fn default_effects_db() -> PathBuf {
    PathBuf::from("/var/lib/autoscribe/effects.sqlite")
}

fn default_control_db() -> PathBuf {
    PathBuf::from("/var/lib/autoscribe/control.sqlite")
}

fn default_context_db() -> PathBuf {
    PathBuf::from("/var/lib/autoscribe/context.sqlite")
}

fn default_max_record_bytes() -> usize {
    2 * 1024 * 1024
}

fn default_max_body_bytes() -> usize {
    1024 * 1024
}

fn default_max_outputs() -> usize {
    16
}

fn default_max_control_file_bytes() -> usize {
    256 * 1024
}

fn default_max_control_records() -> usize {
    10_000
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_git_user_name() -> String {
    "AutoScribe".to_string()
}

fn default_git_user_email() -> String {
    "autoscribe@localhost".to_string()
}

pub fn load_policy(path: &Path) -> Result<Policy> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read policy {}", path.display()))?;
    let policy: Policy = toml::from_str(&text)
        .with_context(|| format!("failed to parse policy {}", path.display()))?;

    if policy.paths.repo_roots.is_empty() && policy.paths.file_roots.is_empty() {
        bail!("policy must declare at least one repo_root or file_root");
    }
    if policy.limits.max_record_bytes == 0
        || policy.limits.max_body_bytes == 0
        || policy.limits.max_outputs == 0
        || policy.limits.max_control_file_bytes == 0
        || policy.limits.max_control_records == 0
    {
        bail!("all configured limits must be greater than zero");
    }
    validate_branch_name(&policy.git.default_branch)?;
    Ok(policy)
}

pub fn read_effect_key(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to stat effect key {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "effect key {} is too permissive ({mode:o}); require mode 0600 or stricter",
            path.display()
        );
    }

    let raw =
        fs::read(path).with_context(|| format!("failed to read effect key {}", path.display()))?;
    let trimmed = trim_ascii_whitespace(&raw);
    if trimmed.len() == 64 && trimmed.iter().all(|b| b.is_ascii_hexdigit()) {
        let decoded = hex::decode(trimmed).context("effect key contains invalid hex")?;
        if decoded.len() < 32 {
            bail!("effect key must contain at least 32 bytes");
        }
        return Ok(decoded);
    }
    if trimmed.len() < 32 {
        bail!("effect key must contain at least 32 bytes");
    }
    Ok(trimmed.to_vec())
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_json(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

pub fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(&canonical_json(value)).context("failed to serialize canonical JSON")
}

pub fn effect_signature(secret: &[u8], payload: &Value) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret).context("invalid HMAC key")?;
    mac.update(&canonical_json_bytes(payload)?);
    Ok(format!("eff_{}", hex::encode(mac.finalize().into_bytes())))
}

pub fn verify_effect_signature(secret: &[u8], payload: &Value, supplied: &str) -> Result<()> {
    let expected = effect_signature(secret, payload)?;
    if expected != supplied {
        bail!("effect signature mismatch");
    }
    Ok(())
}

pub fn read_ndjson<T, F>(limit: usize, mut f: F) -> Result<()>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(T) -> Result<()>,
{
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = Vec::new();
    let mut line_no = 0usize;

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        line_no += 1;
        if line.len() > limit {
            bail!("NDJSON line {line_no} exceeds {limit} bytes");
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let record: T = serde_json::from_slice(&line)
            .with_context(|| format!("invalid JSON on NDJSON line {line_no}"))?;
        f(record).with_context(|| format!("failed processing NDJSON line {line_no}"))?;
    }
    Ok(())
}

pub fn write_ndjson<T: Serialize>(value: &T) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, value)?;
    lock.write_all(b"\n")?;
    lock.flush()?;
    Ok(())
}

pub fn reject_reserved_keys(value: &Value) -> Result<()> {
    const RESERVED: &[&str] = &[
        "effect_key",
        "trusted_effect_key",
        "receipt",
        "receipt_key",
        "effect_signature",
    ];
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if RESERVED.iter().any(|r| key.eq_ignore_ascii_case(r)) {
                    bail!("reserved key is not accepted from untrusted input: {key}");
                }
                reject_reserved_keys(value)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_reserved_keys(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn require_object(value: &Value, label: &str) -> Result<()> {
    if !value.is_object() {
        bail!("{label} must be a JSON object");
    }
    Ok(())
}

pub fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("relative path may not be empty");
    }
    if path.is_absolute() {
        bail!("path must be relative: {}", path.display());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => bail!("unsafe relative path: {}", path.display()),
        }
    }
    Ok(())
}

pub fn validate_branch_name(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.len() > 200 {
        bail!("invalid branch name length");
    }
    if branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains('\\')
        || branch.chars().any(|c| c.is_control() || c.is_whitespace())
        || branch.contains('~')
        || branch.contains('^')
        || branch.contains(':')
        || branch.contains('?')
        || branch.contains('*')
        || branch.contains('[')
    {
        bail!("unsafe branch name: {branch}");
    }
    Ok(())
}

fn normalized_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("path must be absolute: {}", path.display());
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => result.push("/"),
            Component::Normal(part) => result.push(part),
            _ => bail!("unsafe path component in {}", path.display()),
        }
    }
    Ok(result)
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut cursor = path.to_path_buf();
    loop {
        if cursor.exists() {
            return Ok(cursor);
        }
        if !cursor.pop() {
            bail!("no existing ancestor for {}", path.display());
        }
    }
}

pub fn validate_absolute_target(path: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let normalized = normalized_absolute(path)?;
    if roots.is_empty() {
        bail!("no allowed roots configured");
    }

    let mut matched = None;
    for root in roots {
        let root_norm = normalized_absolute(root)?;
        if normalized == root_norm || normalized.starts_with(&root_norm) {
            matched = Some(root_norm);
            break;
        }
    }
    let root = matched.ok_or_else(|| {
        anyhow!(
            "target {} is outside configured roots",
            normalized.display()
        )
    })?;

    let root_real = fs::canonicalize(&root)
        .with_context(|| format!("configured root does not exist: {}", root.display()))?;
    let ancestor = nearest_existing_ancestor(&normalized)?;
    let ancestor_real = fs::canonicalize(&ancestor)
        .with_context(|| format!("failed to canonicalize {}", ancestor.display()))?;

    if !ancestor_real.starts_with(&root_real) {
        bail!(
            "target escapes configured root through symlink: {}",
            normalized.display()
        );
    }

    if normalized.exists() {
        let meta = fs::symlink_metadata(&normalized)?;
        if meta.file_type().is_symlink() {
            bail!(
                "final target may not be a symlink: {}",
                normalized.display()
            );
        }
        let real = fs::canonicalize(&normalized)?;
        if !real.starts_with(&root_real) {
            bail!(
                "target resolves outside configured root: {}",
                normalized.display()
            );
        }
    }

    Ok(normalized)
}

pub fn run_checked(mut cmd: Command, label: &str) -> Result<Output> {
    let output = cmd
        .output()
        .with_context(|| format!("failed to execute {label}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "{label} failed with {}\nstdout: {}\nstderr: {}",
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(output)
}

pub fn git_output(args: &[&str], label: &str) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    Ok(run_checked(cmd, label)?.stdout)
}

pub fn git_output_owned(args: &[String], label: &str) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    Ok(run_checked(cmd, label)?.stdout)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectReceipt {
    pub schema: String,
    pub effect_key: String,
    pub kind: String,
    pub target: String,
    pub result: String,
}

pub fn open_effects_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open effects DB {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS effect_receipts (
            effect_key TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            target TEXT NOT NULL,
            result TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );",
    )?;
    Ok(conn)
}

pub fn existing_receipt(conn: &Connection, effect_key: &str) -> Result<Option<EffectReceipt>> {
    conn.query_row(
        "SELECT effect_key, kind, target, result FROM effect_receipts WHERE effect_key = ?1",
        params![effect_key],
        |row| {
            Ok(EffectReceipt {
                schema: RECEIPT_SCHEMA.to_string(),
                effect_key: row.get(0)?,
                kind: row.get(1)?,
                target: row.get(2)?,
                result: row.get(3)?,
            })
        },
    )
    .optional()
    .context("failed to query effect receipt")
}

pub fn record_receipt(
    conn: &Connection,
    effect_key: &str,
    kind: &str,
    target: &str,
    result: &str,
) -> Result<EffectReceipt> {
    conn.execute(
        "INSERT INTO effect_receipts(effect_key, kind, target, result) VALUES (?1, ?2, ?3, ?4)",
        params![effect_key, kind, target, result],
    )?;
    Ok(EffectReceipt {
        schema: RECEIPT_SCHEMA.to_string(),
        effect_key: effect_key.to_string(),
        kind: kind.to_string(),
        target: target.to_string(),
        result: result.to_string(),
    })
}

pub fn safe_identifier(value: &str, label: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max {
        bail!("{label} has invalid length");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/'))
    {
        bail!("{label} contains unsafe characters: {value}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys() {
        let value: Value = serde_json::from_str(r#"{"z":1,"a":{"y":2,"b":3}}"#).unwrap();
        let bytes = canonical_json_bytes(&value).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"a":{"b":3,"y":2},"z":1}"#
        );
    }

    #[test]
    fn effect_signatures_are_stable() {
        let secret = b"01234567890123456789012345678901";
        let a: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(
            effect_signature(secret, &a).unwrap(),
            effect_signature(secret, &b).unwrap()
        );
    }

    #[test]
    fn rejects_parent_relative_path() {
        assert!(validate_relative_path(Path::new("../escape")).is_err());
    }

    #[test]
    fn rejects_reserved_effect_keys() {
        let value: Value = serde_json::from_str(r#"{"nested":{"effect_key":"x"}}"#).unwrap();
        assert!(reject_reserved_keys(&value).is_err());
    }
}
