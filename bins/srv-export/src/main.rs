use anyhow::{bail, Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use srv_common::{load_policy, safe_identifier, sha256_hex};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;
use tempfile::NamedTempFile;

const DROPBOX_OUTGOING: &str = "dropbox:biznet/outgoing";

#[derive(Parser, Debug)]
#[command(
    about = "Poll AutoScribe pending exports and write response NDJSON to Dropbox"
)]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,

    /// Process the current export queue once and exit.
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

fn asc_output(args: &[&str], label: &str) -> Result<Vec<u8>> {
    command_output(asc_binary(), args, label)
}

fn rclone_output(args: &[&str], label: &str) -> Result<Vec<u8>> {
    command_output(rclone_binary(), args, label)
}

fn pending_records(max_record_bytes: usize) -> Result<Vec<Value>> {
    let output = asc_output(&["export", "pending"], "asc export pending")?;
    let text = String::from_utf8(output).context("asc export pending is not UTF-8")?;
    let mut records = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        if raw.as_bytes().len() > max_record_bytes {
            bail!("pending export line {line_number} exceeds {max_record_bytes} bytes");
        }
        let value: Value = serde_json::from_str(raw)
            .with_context(|| format!("pending export line {line_number} is invalid JSON"))?;
        records.push(value);
    }
    Ok(records)
}

fn process_record(record: &Value, max_body_bytes: usize) -> Result<()> {
    let object = record
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("pending export must be a JSON object"))?;

    let schema = object.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != "autoscribe.export-pending.v1" {
        let message = object
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unsupported pending export schema");
        bail!("{message}");
    }

    let call_id = object
        .get("call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("pending export missing call_id"))?;
    safe_identifier(call_id, "call_id", 160)?;

    let baggage = object
        .get("baggage")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("pending export missing baggage"))?;
    if !baggage.is_object() {
        bail!("pending export baggage must be a JSON object");
    }

    let content = asc_output(
        &["export", "content", call_id],
        "asc export content",
    )?;
    if content.len() > max_body_bytes {
        bail!("response content exceeds {max_body_bytes} bytes");
    }
    let result = String::from_utf8(content).context("response content is not UTF-8")?;
    let result_sha256 = sha256_hex(result.as_bytes());

    let response = json!({
        "schema": "autoscribe.client-response.v1",
        "call_id": call_id,
        "result": result,
        "baggage": baggage,
    });

    let final_remote = format!("{DROPBOX_OUTGOING}/{call_id}.ndjson");
    let partial_remote = format!("{DROPBOX_OUTGOING}/.{call_id}.ndjson.partial");

    let mut tmp = NamedTempFile::new().context("failed creating temporary response file")?;
    serde_json::to_writer(&mut tmp, &response)?;
    tmp.write_all(b"\n")?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;

    let local = tmp
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("temporary response path is not UTF-8"))?;
    rclone_output(
        &["copyto", local, &partial_remote],
        "rclone copyto partial outgoing response",
    )?;
    rclone_output(
        &["moveto", &partial_remote, &final_remote],
        "rclone moveto final outgoing response",
    )?;

    asc_output(
        &[
            "export",
            "complete",
            call_id,
            &final_remote,
            &result_sha256,
        ],
        "asc export complete",
    )?;

    println!(
        "{}",
        json!({
            "event": "export_written",
            "call_id": call_id,
            "target": final_remote,
            "result_sha256": result_sha256,
        })
    );
    Ok(())
}

fn poll_once(max_record_bytes: usize, max_body_bytes: usize) -> Result<()> {
    let records = pending_records(max_record_bytes)?;
    let mut failures = 0usize;

    for record in records {
        if let Err(error) = process_record(&record, max_body_bytes) {
            failures += 1;
            eprintln!(
                "{}",
                json!({
                    "event": "export_failed",
                    "call_id": record.get("call_id"),
                    "error": error.to_string(),
                })
            );
        }
    }

    if failures == 0 {
        Ok(())
    } else {
        bail!("{failures} pending export(s) failed")
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
                json!({"event":"export_poll_failed","error":error.to_string()})
            );
        }
        thread::sleep(Duration::from_secs_f64(args.poll_seconds));
    }
}
