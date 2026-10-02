use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use srv_common::{
    canonical_json, effect_signature, load_policy, read_effect_key, read_ndjson, safe_identifier,
    sha256_hex, validate_absolute_target, validate_branch_name, validate_relative_path,
    verify_effect_signature, write_ndjson, EFFECT_SCHEMA, RESPONSE_SCHEMA,
};
use std::path::{Path, PathBuf};

const RETURN_SCHEMA: &str = "autoscribe.return.v1";
const REPO_OUTPUT_BRANCH: &str = "autoscribe-output";

#[derive(Parser, Debug)]
#[command(about = "Verify the trusted input return route and emit one authenticated output effect")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,

    #[arg(long)]
    replay_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseRecord {
    schema: String,
    call_id: String,
    content: String,
    baggage: Value,
}

#[derive(Debug, Deserialize)]
struct ReturnEnvelope {
    schema: String,
    record_id: String,
    route: Value,
    signature: String,
}

#[derive(Debug, Serialize)]
struct EffectRecord {
    schema: String,
    effect_key: String,
    call_id: String,
    effect_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_id: Option<String>,
    effect: Value,
    content: String,
    content_sha256: String,
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

fn normalized_effect(route: &Value, policy: &srv_common::Policy) -> Result<Value> {
    let route = route
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("return route must be an object"))?;
    match route.get("kind").and_then(Value::as_str) {
        Some("repo") => {
            let repo = PathBuf::from(
                route
                    .get("repo")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("repo route missing repo"))?,
            );
            let path = PathBuf::from(
                route
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("repo route missing path"))?,
            );
            if let Some(branch) = route.get("branch").and_then(Value::as_str) {
                validate_branch_name(branch)?;
            }
            let repo = validate_absolute_target(&repo, &policy.paths.repo_roots)?;
            validate_relative_path(&path)?;
            validate_branch_name(REPO_OUTPUT_BRANCH)?;
            Ok(json!({
                "kind": "repo",
                "repo": repo,
                "path": path,
                "branch": REPO_OUTPUT_BRANCH,
                "create_repo": false,
            }))
        }
        Some("dropbox") => {
            let batch = route
                .get("batch")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Dropbox route missing batch"))?;
            safe_batch(batch)?;
            let path = PathBuf::from(
                route
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Dropbox route missing path"))?,
            );
            safe_dropbox_path(&path)?;
            Ok(json!({
                "kind": "dropbox",
                "batch": batch,
                "path": path,
            }))
        }
        Some(other) => bail!("unsupported trusted return route kind: {other}"),
        None => bail!("return route missing kind"),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let secret = read_effect_key(&policy.paths.effect_key_file)?;
    if let Some(replay_id) = args.replay_id.as_deref() {
        safe_identifier(replay_id, "replay_id", 160)?;
    }

    read_ndjson::<ResponseRecord, _>(policy.limits.max_record_bytes, |response| {
        if response.schema != RESPONSE_SCHEMA {
            bail!("unsupported response schema: {}", response.schema);
        }
        safe_identifier(&response.call_id, "call_id", 160)?;
        if response.content.len() > policy.limits.max_body_bytes {
            bail!(
                "response content exceeds {} bytes",
                policy.limits.max_body_bytes
            );
        }
        let baggage = response
            .baggage
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("baggage must be an object"))?;
        if baggage.contains_key("outputs") {
            bail!("baggage.outputs is no longer accepted; output is bound at ingress");
        }
        let raw_return = baggage
            .get("autoscribe_return")
            .ok_or_else(|| anyhow::anyhow!("response baggage missing autoscribe_return"))?;
        let envelope: ReturnEnvelope = serde_json::from_value(raw_return.clone())
            .context("invalid autoscribe_return envelope")?;
        if envelope.schema != RETURN_SCHEMA {
            bail!("unsupported return-route schema: {}", envelope.schema);
        }
        safe_identifier(&envelope.record_id, "record_id", 160)?;
        safe_identifier(&envelope.signature, "return signature", 96)?;

        let signed_route = canonical_json(&json!({
            "schema": RETURN_SCHEMA,
            "record_id": envelope.record_id,
            "route": envelope.route,
        }));
        verify_effect_signature(&secret, &signed_route, &envelope.signature)
            .context("trusted return-route signature mismatch")?;
        let effect = normalized_effect(&signed_route["route"], &policy)?;
        let content_sha256 = sha256_hex(response.content.as_bytes());
        let mut effect_payload = json!({
            "schema": EFFECT_SCHEMA,
            "call_id": response.call_id,
            "effect_index": 0,
            "effect": effect,
            "content_sha256": content_sha256,
        });
        if let Some(replay_id) = args.replay_id.as_deref() {
            effect_payload["replay_id"] = Value::String(replay_id.to_string());
        }
        let signed_effect = canonical_json(&effect_payload);
        let effect_key = effect_signature(&secret, &signed_effect)?;

        write_ndjson(&EffectRecord {
            schema: EFFECT_SCHEMA.to_string(),
            effect_key,
            call_id: signed_effect["call_id"]
                .as_str()
                .expect("call_id is text")
                .to_string(),
            effect_index: 0,
            replay_id: args.replay_id.clone(),
            effect: signed_effect["effect"].clone(),
            content: response.content,
            content_sha256: signed_effect["content_sha256"]
                .as_str()
                .expect("content hash is text")
                .to_string(),
        })
    })
}
