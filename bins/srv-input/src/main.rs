use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Serialize;
use serde_json::{json, Map, Value};
use srv_common::{
    canonical_json_bytes, load_policy, safe_identifier, sha256_hex, INPUT_SCHEMA,
};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DROPBOX_INCOMING: &str = "dropbox:biznet/incoming";
const MAX_TRANSPORT_FILE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    about = "Validate parsed Dropbox NDJSON envelopes and enqueue them; retain legacy text compatibility"
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

fn rclone_binary() -> OsString {
    std::env::var_os("AUTOSCRIBE_RCLONE").unwrap_or_else(|| OsString::from("rclone"))
}

fn asc_binary() -> OsString {
    std::env::var_os("AUTOSCRIBE_ASC").unwrap_or_else(|| OsString::from("asc"))
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
            bail!("content contains a non-text control character U+{:04X}", ch as u32);
        }
    }
    Ok(())
}

fn relative_path(value: &str) -> Result<()> {
    if value.is_empty() || value.contains(['\\', ':'])
        || value.chars().any(char::is_control)
        || value.split('/').any(|p| p.is_empty() || p == "." || p == ".." || p.eq_ignore_ascii_case(".git")) {
        bail!("unsafe package path: {value:?}");
    }
    Ok(())
}

fn text_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing text field: {key}"))
}

fn digest(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("invalid SHA-256");
    }
    Ok(())
}

fn validate_parsed_input(value: &Value, limit: usize) -> Result<()> {
    if value["schema"] != "autoscribe.input.v2" || value["content_format"] != "markdown"
        || value["directive_policy"] != "inline.v1" {
        bail!("unsupported parsed envelope, content format, or directive policy");
    }
    safe_identifier(text_field(value, "record_id")?, "record_id", 160)?;
    let content = text_field(value, "content")?;
    validate_text(content, limit)?;
    if text_field(value, "content_sha256")? != sha256_hex(content.as_bytes()) {
        bail!("content checksum mismatch");
    }
    validate_plan(text_field(&value["routing"], "plan_id")?)?;
    let source = &value["source"];
    if source["kind"] != "obsidian" { bail!("source kind must be obsidian"); }
    relative_path(text_field(source, "path")?)?;
    safe_identifier(text_field(source, "slug")?, "slug", 160)?;
    digest(text_field(source, "vault_id")?)?;
    let client = &value["baggage"]["client"];
    if client["source_path"] != source["path"] || client["source_slug"] != source["slug"] || client["vault_id"] != source["vault_id"] {
        bail!("source/baggage identity mismatch");
    }
    digest(text_field(client, "source_body_sha256")?)?;
    text_field(client, "frontmatter")?; // opaque return bytes; never parse YAML here
    let refs = value["references"].as_array().context("references must be an array")?;
    let mut paths = std::collections::BTreeSet::new();
    for reference in refs {
        let path = text_field(reference, "path")?;
        relative_path(path)?;
        if !paths.insert(path) { bail!("duplicate source reference"); }
        safe_identifier(text_field(reference, "slug")?, "reference slug", 160)?;
        digest(text_field(reference, "content_sha256")?)?;
    }
    if !refs.iter().any(|r| r["path"] == source["path"] && r["slug"] == source["slug"]
        && r["content_sha256"] == client["source_body_sha256"]) {
        bail!("root source reference identity/checksum mismatch");
    }
    let assets = value["assets"].as_array().context("assets must be an array")?;
    let mut paths = std::collections::BTreeSet::new();
    let mut total = 0usize;
    for asset in assets {
        let path = text_field(asset, "path")?;
        relative_path(path)?;
        let lower = path.to_lowercase();
        if !path.starts_with("assets/")
            || paths.iter().any(|p: &String| p.starts_with(&(lower.clone() + "/")) || lower.starts_with(&(p.clone() + "/")))
            || !paths.insert(lower) {
            bail!("asset must have unique package-relative assets/ path");
        }
        let data = text_field(asset, "content_hex")?;
        if data.len() > limit.saturating_mul(2) { bail!("asset too large"); }
        let bytes = hex::decode(data).context("invalid asset hex bytes")?;
        total = total.checked_add(bytes.len()).context("asset size overflow")?;
        if total > limit { bail!("package assets exceed body limit"); }
        if text_field(asset, "sha256")? != sha256_hex(&bytes) { bail!("asset checksum mismatch"); }
    }
    Ok(())
}

fn canonical_input(
    transport_file: &str,
    line_number: usize,
    value: Value,
    max_body_bytes: usize,
) -> Result<Value> {
    if value.get("schema").and_then(Value::as_str).is_some_and(|s| s.starts_with("autoscribe.input."))
        || (value.get("schema").is_some() && value.get("plan").is_none()) {
        validate_parsed_input(&value, max_body_bytes)?;
        return Ok(value);
    }
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

    Ok(serde_json::to_value(CanonicalInput {
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
    })?)
}

fn parse_transport(
    transport_file: &str,
    bytes: &[u8],
    max_record_bytes: usize,
    max_body_bytes: usize,
) -> Result<Vec<Value>> {
    if bytes.len() > MAX_TRANSPORT_FILE_BYTES {
        bail!(
            "transport file exceeds {} bytes",
            MAX_TRANSPORT_FILE_BYTES
        );
    }
    let text = std::str::from_utf8(bytes).context("transport file is not UTF-8 text")?;
    let mut records = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if raw.as_bytes().len() > max_record_bytes {
            bail!("line {line_number} exceeds {max_record_bytes} bytes");
        }
        let value: Value = serde_json::from_str(raw)
            .with_context(|| format!("line {line_number} is not valid JSON"))?;
        records.push(canonical_input(
            transport_file,
            line_number,
            value,
            max_body_bytes,
        )?);
    }

    if records.is_empty() {
        bail!("transport file contains no NDJSON records");
    }
    Ok(records)
}

fn enqueue_records(records: &[Value]) -> Result<Vec<u8>> {
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
) -> Result<usize> {
    safe_transport_name(name)?;
    let remote = format!("{DROPBOX_INCOMING}/{name}");
    let bytes = rclone_output(&["cat", &remote], "rclone cat incoming transport")?;
    let records = parse_transport(name, &bytes, max_record_bytes, max_body_bytes)?;
    let enqueue_output = enqueue_records(&records)?;

    rclone_output(
        &["deletefile", &remote],
        "rclone deletefile accepted transport",
    )?;

    let enqueue_text = String::from_utf8_lossy(&enqueue_output);
    println!(
        "{}",
        json!({
            "event": "ingest_enqueued",
            "transport": name,
            "records": records.len(),
            "enqueue_results": enqueue_text.lines().filter(|line| !line.trim().is_empty()).count(),
        })
    );
    Ok(records.len())
}

fn poll_once(max_record_bytes: usize, max_body_bytes: usize) -> Result<()> {
    let names = list_incoming()?;
    let mut failures = Vec::new();

    for name in names {
        if let Err(error) = process_file(&name, max_record_bytes, max_body_bytes) {
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
    fn parsed() -> Value {
        json!({"schema":"autoscribe.input.v2","record_id":"inp_test","content_format":"markdown",
            "directive_policy":"inline.v1","content":"::: directive\nKeep inline\n:::\nBody\n",
            "content_sha256":sha256_hex(b"::: directive\nKeep inline\n:::\nBody\n"),
            "source":{"kind":"obsidian","path":"Notes/a.md","slug":"psg_a","vault_id":sha256_hex(b"vault")},
            "routing":{"plan_id":"pln_TEST"},
            "baggage":{"client":{"source_path":"Notes/a.md","source_slug":"psg_a","vault_id":sha256_hex(b"vault"),"source_body_sha256":sha256_hex(b"body"),"frontmatter":"---\nslug: psg_a\n---\n"}},
            "references":[{"path":"Notes/a.md","slug":"psg_a","content_sha256":sha256_hex(b"body")}],
            "assets":[{"path":"assets/pic.png","sha256":sha256_hex(b"abc"),"content_hex":"616263"}]})
    }
    #[test]
    fn parsed_envelope_is_forwarded_without_transformations() {
        let input = parsed();
        assert_eq!(canonical_input("a.ndjson",1,input.clone(),1048576).unwrap(),input);
    }
    #[test]
    fn rejects_version_hash_identity_and_asset_errors() {
        for (pointer, value) in [
            ("/schema",json!("autoscribe.input.v99")),
            ("/content_sha256",json!("0".repeat(64))),
            ("/baggage/client/source_slug",json!("other")),
            ("/assets/0/path",json!("assets/../escape")),
            ("/assets/0/path",json!("/absolute")),
            ("/assets/0/path",json!("assets/a:b")),
            ("/assets/0/sha256",json!("0".repeat(64))),
            ("/assets/0/content_hex",json!("xx")),
            ("/directive_policy",json!("extract")),
        ] {
            let mut input=parsed(); *input.pointer_mut(pointer).unwrap()=value;
            assert!(canonical_input("a.ndjson",1,input,1048576).is_err(),"{pointer}");
        }
        let mut input=parsed();let duplicate=input["assets"][0].clone();
        input["assets"].as_array_mut().unwrap().push(duplicate);
        assert!(validate_parsed_input(&input,1048576).is_err());
    }
    #[test]
    fn legacy_record_retains_existing_content_and_baggage() {
        let input=json!({"content":"::: directive\nKeep\n:::\nBody","plan":"pln_TEST","client":{"x":1}});
        let output=canonical_input("a.ndjson",1,input.clone(),1048576).unwrap();
        assert_eq!(output["content"],input["content"]);
        assert_eq!(output["baggage"]["client"],input["client"]);
        assert_eq!(output["schema"],INPUT_SCHEMA);
    }
    #[test]
    fn invalid_later_record_rejects_entire_batch_before_enqueue() {
        let bytes=format!("{}\n{{}}\n",parsed());
        assert!(parse_transport("a.ndjson",bytes.as_bytes(),2097152,1048576).is_err());
    }
}
