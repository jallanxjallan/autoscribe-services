use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use srv_common::{
    canonical_json, effect_signature, load_policy, read_effect_key, read_ndjson,
    reject_reserved_keys, require_object, safe_identifier, sha256_hex,
    validate_absolute_target, validate_branch_name, validate_relative_path, write_ndjson,
    EFFECT_SCHEMA, RESPONSE_SCHEMA,
};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "Validate response baggage and emit authenticated trusted effects")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ResponseRecord {
    schema: String,
    call_id: String,
    content: String,
    baggage: Value,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OutputDeclaration {
    Repo {
        repo: PathBuf,
        path: PathBuf,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        create_repo: bool,
    },
    File {
        path: PathBuf,
        #[serde(default = "default_file_mode")]
        mode: FileMode,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
enum FileMode {
    Replace,
    CreateNew,
}

fn default_file_mode() -> FileMode {
    FileMode::Replace
}

#[derive(Debug, Serialize)]
struct EffectRecord {
    schema: String,
    effect_key: String,
    call_id: String,
    effect_index: usize,
    effect: Value,
    content: String,
    content_sha256: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let secret = read_effect_key(&policy.paths.effect_key_file)?;

    read_ndjson::<ResponseRecord, _>(policy.limits.max_record_bytes, |response| {
        if response.schema != RESPONSE_SCHEMA {
            bail!("unsupported response schema: {}", response.schema);
        }
        safe_identifier(&response.call_id, "call_id", 160)?;
        if response.content.as_bytes().len() > policy.limits.max_body_bytes {
            bail!("response content exceeds {} bytes", policy.limits.max_body_bytes);
        }
        require_object(&response.baggage, "baggage")?;
        reject_reserved_keys(&response.baggage)?;

        let outputs = response
            .baggage
            .get("outputs")
            .ok_or_else(|| anyhow::anyhow!("response baggage must contain outputs"))?
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("baggage.outputs must be an array"))?;
        if outputs.is_empty() {
            bail!("baggage.outputs may not be empty");
        }
        if outputs.len() > policy.limits.max_outputs {
            bail!("too many outputs: {} > {}", outputs.len(), policy.limits.max_outputs);
        }

        let content_sha256 = sha256_hex(response.content.as_bytes());

        for (index, raw) in outputs.iter().enumerate() {
            reject_reserved_keys(raw)?;
            let declaration: OutputDeclaration = serde_json::from_value(raw.clone())
                .with_context(|| format!("invalid output declaration at index {index}"))?;

            let normalized = match declaration {
                OutputDeclaration::Repo {
                    repo,
                    path,
                    branch,
                    create_repo,
                } => {
                    let repo = validate_absolute_target(&repo, &policy.paths.repo_roots)?;
                    validate_relative_path(&path)?;
                    let branch = branch.unwrap_or_else(|| policy.git.default_branch.clone());
                    validate_branch_name(&branch)?;
                    json!({
                        "kind":"repo",
                        "repo":repo,
                        "path":path,
                        "branch":branch,
                        "create_repo":create_repo,
                    })
                }
                OutputDeclaration::File { path, mode } => {
                    let path = validate_absolute_target(&path, &policy.paths.file_roots)?;
                    json!({
                        "kind":"file",
                        "path":path,
                        "mode":mode,
                    })
                }
            };

            let signed_payload = canonical_json(&json!({
                "schema": EFFECT_SCHEMA,
                "call_id": response.call_id.clone(),
                "effect_index": index,
                "effect": normalized,
                "content_sha256": content_sha256.clone(),
            }));
            let effect_key = effect_signature(&secret, &signed_payload)?;

            write_ndjson(&EffectRecord {
                schema: EFFECT_SCHEMA.to_string(),
                effect_key,
                call_id: response.call_id.clone(),
                effect_index: index,
                effect: signed_payload["effect"].clone(),
                content: response.content.clone(),
                content_sha256: content_sha256.clone(),
            })?;
        }
        Ok(())
    })
}
