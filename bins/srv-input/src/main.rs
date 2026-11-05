use anyhow::{bail, Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use srv_common::load_policy;
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
    about = "Forward native-document NDJSON envelopes to the ingest adapter"
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

fn parse_transport(
    _transport_file: &str,
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
        let mut value: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                records.push(json!({"schema":"autoscribe.invalid-envelope.v1",
                    "diagnostic":{"line":line_number,"error":error.to_string(),"raw":raw}}));
                continue;
            }
        };
        let document=value.get("document").or_else(||value.get("raw_markdown"))
            .and_then(Value::as_str);
        if document.is_some_and(|text| text.len()>max_body_bytes) {
            value["schema"]=json!("autoscribe.invalid-envelope.v1");
            value["diagnostic"]=json!({"line":line_number,"error":"Document exceeds transport body limit"});
        }
        records.push(value);
    }

    if records.is_empty() {
        bail!("transport file contains no NDJSON records");
    }
    Ok(records)
}

fn enqueue_records(records: &[Value]) -> Result<Vec<u8>> {
    let mut child = Command::new(asc_binary())
        .arg("ingest")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to execute asc ingest")?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open asc ingest stdin"))?;
        for record in records {
            serde_json::to_writer(&mut stdin, record)?;
            stdin.write_all(b"\n")?;
        }
    }

    let output = child
        .wait_with_output()
        .context("failed waiting for asc ingest")?;
    if !output.status.success() {
        let failures:Vec<Value> = String::from_utf8_lossy(&output.stdout).lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value.get("failure").is_some())
            .map(|value| json!({"record_id":value.get("record_id"),
                "submission_id":value.get("submission_id"),"line":value.get("line"),
                "failure":value.get("failure")})).collect();
        bail!("asc ingest failed with {}: {}; diagnostics: {}", output.status,
            String::from_utf8_lossy(&output.stderr).trim(),json!(failures));
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
    #[test]
    fn raw_document_is_forwarded_without_origin_or_semantic_requirements() {
        let value=json!({"schema":"autoscribe.document.v1","document":"## Café 東京",
            "submission_id":"00000000-0000-4000-8000-000000000001","baggage":{"custom":[1,true]}});
        let bytes=format!("{value}\n");
        assert_eq!(parse_transport("staging.ndjson",bytes.as_bytes(),2097152,1048576).unwrap(),vec![value]);
    }
    #[test]
    fn malformed_record_does_not_block_valid_siblings() {
        let bytes=b"{invalid json}\n{\"schema\":\"autoscribe.document.v1\",\"document\":\"Body\"}\n";
        let records=parse_transport("a.ndjson",bytes,2097152,1048576).unwrap();
        assert_eq!(records.len(),2);
        assert_eq!(records[0]["schema"],"autoscribe.invalid-envelope.v1");
        assert_eq!(records[1]["document"],"Body");
    }
    #[test]
    fn body_limit_is_a_per_record_transport_diagnostic() {
        let records=parse_transport("a.ndjson",b"{\"document\":\"Long body\"}\n{\"document\":\"ok\"}\n",2097152,2).unwrap();
        assert_eq!(records[0]["schema"],"autoscribe.invalid-envelope.v1");
        assert_eq!(records[1]["document"],"ok");
    }
}
