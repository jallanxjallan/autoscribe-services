use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Serialize;
use serde_json::{json, Map, Value};
use srv_common::{canonical_json_bytes, load_policy, safe_identifier, sha256_hex, INPUT_SCHEMA};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DROPBOX_INCOMING: &str = "dropbox:biznet/incoming";
const MAX_TRANSPORT_FILE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    about = "Poll Dropbox for client NDJSON, validate text records, and enqueue them into AutoScribe"
)]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,

    /// Process the current incoming queue once and exit.
    #[arg(long)]
    once: bool,

    /// Poll interval in seconds when running as a daemon.
    #[arg(long, default_value_t = 1.0)]
    poll_seconds: f64,
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

#[derive(Debug)]
enum RoutedRecord {
    Call(CanonicalInput),
    Context(Value),
}

#[derive(Debug, Default)]
struct TransportBatch {
    calls: Vec<CanonicalInput>,
    contexts: Vec<Value>,
}

fn rclone_binary() -> OsString {
    std::env::var_os("AUTOSCRIBE_RCLONE").unwrap_or_else(|| OsString::from("rclone"))
}

fn asc_binary() -> OsString {
    std::env::var_os("AUTOSCRIBE_ASC").unwrap_or_else(|| OsString::from("asc"))
}

fn context_binary() -> OsString {
    if let Some(value) = std::env::var_os("AUTOSCRIBE_CONTEXT") {
        return value;
    }
    std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name("srv-context").into_os_string())
        .unwrap_or_else(|| OsString::from("srv-context"))
}

fn command_output(program: OsString, args: &[&str], label: &str) -> Result<Vec<u8>> {
    let output = Command::new(program)
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

fn rclone_output(args: &[&str], label: &str) -> Result<Vec<u8>> {
    command_output(rclone_binary(), args, label)
}

fn safe_transport_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(char::is_control)
    {
        bail!("unsafe Dropbox transport filename: {name:?}");
    }
    if !name.ends_with(".ndjson") {
        bail!("transport file must end in .ndjson: {name}");
    }
    Ok(())
}

fn validate_plan(plan: &str) -> Result<()> {
    safe_identifier(plan, "plan", 160)?;
    if !plan.starts_with("pln_") {
        bail!("plan must be a pln_ identity");
    }
    Ok(())
}

fn validate_text(content: &str, max_body_bytes: usize) -> Result<()> {
    if content.is_empty() {
        bail!("content must be non-empty text");
    }
    if content.len() > max_body_bytes {
        bail!("content exceeds {max_body_bytes} bytes");
    }
    for ch in content.chars() {
        if ch == '\0' || (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t' | '\u{000C}')) {
            bail!(
                "content contains a non-text control character U+{:04X}",
                ch as u32
            );
        }
    }
    Ok(())
}

fn canonical_input(
    transport_file: &str,
    line_number: usize,
    value: Value,
    max_body_bytes: usize,
) -> Result<CanonicalInput> {
    let mut object: Map<String, Value> = value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("line {line_number} must be a JSON object"))?;

    let content = object
        .remove("content")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("line {line_number} content must be non-empty text"))?;
    validate_text(&content, max_body_bytes)
        .with_context(|| format!("line {line_number} content rejected"))?;

    let plan = object
        .remove("plan")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("line {line_number} plan must be non-empty text"))?;
    validate_plan(&plan).with_context(|| format!("line {line_number} plan rejected"))?;

    let baggage = Value::Object(object);
    let content_sha256 = sha256_hex(content.as_bytes());
    let identity = json!({
        "plan": &plan,
        "content_sha256": &content_sha256,
        "baggage": &baggage,
    });
    let record_id = format!(
        "inp_{}",
        &sha256_hex(&canonical_json_bytes(&identity)?)[..32]
    );

    Ok(CanonicalInput {
        schema: INPUT_SCHEMA.to_string(),
        record_id,
        source: json!({
            "kind": "dropbox_ndjson",
            "transport_file": transport_file,
            "line": line_number,
        }),
        content,
        content_sha256,
        routing: json!({"plan_id": plan}),
        baggage,
    })
}

fn route_record(
    transport_file: &str,
    line_number: usize,
    value: Value,
    max_body_bytes: usize,
) -> Result<RoutedRecord> {
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("line {line_number} must be a JSON object"))?;

    let record_type = match object.remove("type") {
        None => "call".to_string(),
        Some(Value::String(value)) if !value.is_empty() => value,
        Some(_) => bail!("line {line_number} type must be non-empty text"),
    };

    let value = Value::Object(object);
    match record_type.as_str() {
        "call" => Ok(RoutedRecord::Call(canonical_input(
            transport_file,
            line_number,
            value,
            max_body_bytes,
        )?)),
        "context" => {
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("line {line_number} context content must be text")
                })?;
            validate_text(content, max_body_bytes)
                .with_context(|| format!("line {line_number} context content rejected"))?;
            Ok(RoutedRecord::Context(value))
        }
        other => bail!("line {line_number} has unsupported type {other:?}"),
    }
}

fn parse_transport(
    transport_file: &str,
    bytes: &[u8],
    max_record_bytes: usize,
    max_body_bytes: usize,
) -> Result<TransportBatch> {
    if bytes.len() > MAX_TRANSPORT_FILE_BYTES {
        bail!("transport file exceeds {} bytes", MAX_TRANSPORT_FILE_BYTES);
    }
    let text = std::str::from_utf8(bytes).context("transport file is not UTF-8 text")?;
    let mut batch = TransportBatch::default();

    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if raw.len() > max_record_bytes {
            bail!("line {line_number} exceeds {max_record_bytes} bytes");
        }
        let value: Value = serde_json::from_str(raw)
            .with_context(|| format!("line {line_number} is not valid JSON"))?;
        match route_record(transport_file, line_number, value, max_body_bytes)? {
            RoutedRecord::Call(record) => batch.calls.push(record),
            RoutedRecord::Context(record) => batch.contexts.push(record),
        }
    }

    if batch.calls.is_empty() && batch.contexts.is_empty() {
        bail!("transport file contains no NDJSON records");
    }
    Ok(batch)
}

fn enqueue_records(records: &[CanonicalInput]) -> Result<Vec<u8>> {
    let mut child = Command::new(asc_binary())
        .arg("enqueue")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to execute asc enqueue")?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open asc enqueue stdin"))?;
        for record in records {
            serde_json::to_writer(&mut stdin, record)?;
            stdin.write_all(b"\n")?;
        }
    }

    let output = child
        .wait_with_output()
        .context("failed waiting for asc enqueue")?;
    if !output.status.success() {
        bail!(
            "asc enqueue failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn store_context_records(records: &[Value], policy: &Path) -> Result<Vec<u8>> {
    let mut child = Command::new(context_binary())
        .arg("--policy")
        .arg(policy)
        .arg("put")
        .arg("--origin")
        .arg("upload")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to execute srv-context put")?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open srv-context stdin"))?;
        for record in records {
            serde_json::to_writer(&mut stdin, record)?;
            stdin.write_all(b"\n")?;
        }
    }

    let output = child
        .wait_with_output()
        .context("failed waiting for srv-context put")?;
    if !output.status.success() {
        bail!(
            "srv-context put failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn list_incoming() -> Result<Vec<String>> {
    let listing = rclone_output(
        &["lsf", "--files-only", DROPBOX_INCOMING],
        "rclone lsf incoming",
    )?;
    let listing = String::from_utf8(listing).context("Dropbox listing is not UTF-8")?;
    let mut names: Vec<String> = listing
        .lines()
        .map(str::trim)
        .filter(|name| name.ends_with(".ndjson"))
        .map(str::to_owned)
        .collect();
    names.sort();
    Ok(names)
}

fn process_file(
    name: &str,
    max_record_bytes: usize,
    max_body_bytes: usize,
    policy: &Path,
) -> Result<usize> {
    safe_transport_name(name)?;
    let remote = format!("{DROPBOX_INCOMING}/{name}");
    let bytes = rclone_output(&["cat", &remote], "rclone cat incoming transport")?;
    let batch = parse_transport(name, &bytes, max_record_bytes, max_body_bytes)?;

    let context_output = if batch.contexts.is_empty() {
        Vec::new()
    } else {
        store_context_records(&batch.contexts, policy)?
    };
    let enqueue_output = if batch.calls.is_empty() {
        Vec::new()
    } else {
        enqueue_records(&batch.calls)?
    };

    rclone_output(
        &["deletefile", &remote],
        "rclone deletefile accepted transport",
    )?;

    let enqueue_text = String::from_utf8_lossy(&enqueue_output);
    let context_text = String::from_utf8_lossy(&context_output);
    let total = batch.calls.len() + batch.contexts.len();
    println!(
        "{}",
        json!({
            "event": "ingest_enqueued",
            "transport": name,
            "records": total,
            "call_records": batch.calls.len(),
            "context_records": batch.contexts.len(),
            "enqueue_results": enqueue_text.lines().filter(|line| !line.trim().is_empty()).count(),
            "context_results": context_text.lines().filter(|line| !line.trim().is_empty()).count(),
        })
    );
    Ok(total)
}

fn poll_once(max_record_bytes: usize, max_body_bytes: usize, policy: &Path) -> Result<()> {
    let names = list_incoming()?;
    let mut failures = Vec::new();

    for name in names {
        if let Err(error) = process_file(&name, max_record_bytes, max_body_bytes, policy) {
            eprintln!(
                "{}",
                json!({
                    "event": "ingest_failed",
                    "transport": name,
                    "error": error.to_string(),
                })
            );
            failures.push(name);
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{} incoming transport file(s) failed", failures.len())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !args.poll_seconds.is_finite() || args.poll_seconds <= 0.0 {
        bail!("poll-seconds must be greater than zero");
    }
    let policy = load_policy(&args.policy)?;

    loop {
        let result = poll_once(
            policy.limits.max_record_bytes,
            policy.limits.max_body_bytes,
            &args.policy,
        );
        if args.once {
            return result;
        }
        if let Err(error) = result {
            eprintln!(
                "{}",
                json!({"event":"ingest_poll_failed","error":error.to_string()})
            );
        }
        thread::sleep(Duration::from_secs_f64(args.poll_seconds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_type_remains_a_call() {
        let value = json!({
            "plan": "pln_0123456789ABCDEF",
            "content": "hello",
            "slug": "source-one"
        });
        match route_record("test.ndjson", 1, value, 1024).unwrap() {
            RoutedRecord::Call(record) => {
                assert_eq!(record.content, "hello");
                assert_eq!(record.baggage["slug"], "source-one");
            }
            RoutedRecord::Context(_) => panic!("call was routed as context"),
        }
    }

    #[test]
    fn context_type_does_not_require_a_plan() {
        let value = json!({
            "type": "context",
            "project": "book-one",
            "selector": "briefing",
            "content": "project context"
        });
        match route_record("test.ndjson", 1, value, 1024).unwrap() {
            RoutedRecord::Context(record) => {
                assert_eq!(record["project"], "book-one");
                assert_eq!(record["selector"], "briefing");
                assert!(record.get("type").is_none());
            }
            RoutedRecord::Call(_) => panic!("context was routed as call"),
        }
    }

    #[test]
    fn unknown_record_type_is_rejected() {
        let value = json!({"type": "other", "content": "hello"});
        assert!(route_record("test.ndjson", 1, value, 1024).is_err());
    }
}
