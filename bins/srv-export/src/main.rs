use anyhow::{bail, Context, Result};
use clap::Parser;
use fs2::FileExt;
use serde::Deserialize;
use serde_json::{json, Value};
use srv_common::{
    canonical_json, existing_receipt, load_policy, open_effects_db, read_effect_key, read_ndjson,
    record_receipt, run_checked, safe_identifier, sha256_hex, validate_relative_path,
    verify_effect_signature, write_ndjson, EFFECT_SCHEMA,
};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

const DROPBOX_OUTGOING: &str = "dropbox:biznet/outgoing";

#[derive(Parser, Debug)]
#[command(about = "Apply authenticated direct-mode effects to the fixed Dropbox outgoing root")]
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
    replay_id: Option<String>,
    effect: Value,
    content: String,
    content_sha256: String,
}

fn rclone_binary() -> OsString {
    std::env::var_os("AUTOSCRIBE_RCLONE").unwrap_or_else(|| OsString::from("rclone"))
}

fn safe_batch(value: &str) -> Result<()> {
    safe_identifier(value, "batch", 96)?;
    if value == "." || value == ".." || value.contains('/') || value.contains(':') {
        bail!("batch must be a single folder name");
    }
    Ok(())
}

fn safe_dropbox_path(path: &Path) -> Result<()> {
    validate_relative_path(path)?;
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Dropbox path must be UTF-8"))?;
    if text.chars().any(char::is_control) || text.contains('\\') {
        bail!("Dropbox path contains unsafe characters");
    }
    Ok(())
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

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let secret = read_effect_key(&policy.paths.effect_key_file)?;

    read_ndjson::<EffectRecord, _>(policy.limits.max_record_bytes, |record| {
        if record.schema != EFFECT_SCHEMA {
            bail!("unsupported effect schema: {}", record.schema);
        }
        safe_identifier(&record.call_id, "call_id", 160)?;
        if let Some(replay_id) = record.replay_id.as_deref() {
            safe_identifier(replay_id, "replay_id", 160)?;
        }
        if sha256_hex(record.content.as_bytes()) != record.content_sha256 {
            bail!("effect content hash mismatch");
        }

        let mut payload = json!({
            "schema": EFFECT_SCHEMA,
            "call_id": record.call_id.clone(),
            "effect_index": record.effect_index,
            "effect": record.effect.clone(),
            "content_sha256": record.content_sha256.clone(),
        });
        if let Some(replay_id) = record.replay_id.as_deref() {
            payload["replay_id"] = Value::String(replay_id.to_string());
        }
        let signed_payload = canonical_json(&payload);
        verify_effect_signature(&secret, &signed_payload, &record.effect_key)?;

        let effect = signed_payload["effect"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("effect must be an object"))?;
        if effect.get("kind").and_then(Value::as_str) != Some("dropbox") {
            bail!("srv-export accepts only direct-mode Dropbox effects");
        }
        let batch = effect
            .get("batch")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Dropbox effect missing batch"))?;
        safe_batch(batch)?;
        let path = PathBuf::from(
            effect
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Dropbox effect missing path"))?,
        );
        safe_dropbox_path(&path)?;
        let remote = format!("{DROPBOX_OUTGOING}/{batch}/{}", path.display());

        let _effect_lock = effect_lock(&policy.paths.effects_db, &record.effect_key)?;
        let conn = open_effects_db(&policy.paths.effects_db)?;
        if let Some(receipt) = existing_receipt(&conn, &record.effect_key)? {
            return write_ndjson(&receipt);
        }

        let mut tmp = NamedTempFile::new().context("failed creating temporary export file")?;
        tmp.write_all(record.content.as_bytes())?;
        tmp.flush()?;
        tmp.as_file().sync_all()?;

        let mut cmd = Command::new(rclone_binary());
        cmd.arg("copyto").arg(tmp.path()).arg(&remote);
        run_checked(cmd, "rclone copyto outgoing batch")
            .with_context(|| format!("failed exporting to {remote}"))?;

        let receipt = record_receipt(
            &conn,
            &record.effect_key,
            "dropbox",
            &remote,
            &record.content_sha256,
        )?;
        write_ndjson(&receipt)
    })
}
