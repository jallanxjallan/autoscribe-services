use anyhow::{bail, Context, Result};
use clap::Parser;
use fs2::FileExt;
use serde::Deserialize;
use serde_json::{json, Value};
use srv_common::{
    canonical_json, existing_receipt, load_policy, open_effects_db, read_effect_key,
    read_ndjson, record_receipt, safe_identifier, sha256_hex, validate_absolute_target,
    verify_effect_signature, write_ndjson, EFFECT_SCHEMA,
};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Parser, Debug)]
#[command(about = "Apply authenticated file-export effects atomically")]
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
    effect: Value,
    content: String,
    content_sha256: String,
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
        .read(true)
        .write(true)
        .open(dir.join(effect_key))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("export target lacks parent"))?;
    fs::create_dir_all(parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(content)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
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
        if sha256_hex(record.content.as_bytes()) != record.content_sha256 {
            bail!("effect content hash mismatch");
        }

        let signed_payload = canonical_json(&json!({
            "schema": EFFECT_SCHEMA,
            "call_id": record.call_id.clone(),
            "effect_index": record.effect_index,
            "effect": record.effect.clone(),
            "content_sha256": record.content_sha256.clone(),
        }));
        verify_effect_signature(&secret, &signed_payload, &record.effect_key)?;

        let effect = signed_payload["effect"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("effect must be an object"))?;
        if effect.get("kind").and_then(Value::as_str) != Some("file") {
            bail!("srv-export accepts only file effects");
        }
        let path = PathBuf::from(
            effect
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("file effect missing path"))?,
        );
        let mode = effect
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("file effect missing mode"))?;
        if !matches!(mode, "replace" | "create_new") {
            bail!("unsupported file effect mode: {mode}");
        }

        let path = validate_absolute_target(&path, &policy.paths.file_roots)?;
        let _effect_lock = effect_lock(&policy.paths.effects_db, &record.effect_key)?;
        let conn = open_effects_db(&policy.paths.effects_db)?;
        if let Some(receipt) = existing_receipt(&conn, &record.effect_key)? {
            return write_ndjson(&receipt);
        }

        if mode == "create_new" && path.exists() {
            let existing = fs::read(&path)
                .with_context(|| format!("failed to read existing {}", path.display()))?;
            if sha256_hex(&existing) != record.content_sha256 {
                bail!("create_new export target already exists with different content");
            }
        } else {
            write_atomic(&path, record.content.as_bytes())
                .with_context(|| format!("failed exporting {}", path.display()))?;
        }

        let receipt = record_receipt(
            &conn,
            &record.effect_key,
            "file",
            &path.display().to_string(),
            &record.content_sha256,
        )?;
        write_ndjson(&receipt)
    })
}
