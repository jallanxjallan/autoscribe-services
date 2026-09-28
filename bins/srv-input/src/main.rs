use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use srv_common::{
    canonical_json_bytes, git_output_owned, load_policy, read_ndjson, reject_reserved_keys,
    require_object, safe_identifier, sha256_hex, validate_absolute_target,
    validate_relative_path, write_ndjson, INPUT_SCHEMA,
};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(about = "Normalize untrusted file/Git inputs into canonical AutoScribe NDJSON")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,
}

#[derive(Debug, Deserialize)]
struct InputRequest {
    #[serde(default)]
    schema: Option<String>,
    source: InputSource,
    #[serde(default = "empty_object")]
    routing: Value,
    #[serde(default = "empty_object")]
    baggage: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum InputSource {
    File { path: PathBuf },
    Git {
        repo: PathBuf,
        commit: String,
        path: PathBuf,
    },
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

fn empty_object() -> Value {
    Value::Object(Default::default())
}

fn read_git_blob(repo: &Path, commit: &str, path: &Path, repo_roots: &[PathBuf]) -> Result<(Vec<u8>, String)> {
    let repo = validate_absolute_target(repo, repo_roots)?;
    if !repo.is_dir() {
        bail!("Git source is not a directory: {}", repo.display());
    }
    validate_relative_path(path)?;
    safe_identifier(commit, "commit", 128)?;

    let repo_s = repo.display().to_string();
    let resolved_args = vec![
        "--git-dir".to_string(),
        repo_s.clone(),
        "rev-parse".to_string(),
        format!("{}^{{commit}}", commit),
    ];
    let resolved = String::from_utf8(git_output_owned(&resolved_args, "git rev-parse")?)?
        .trim()
        .to_string();
    safe_identifier(&resolved, "resolved commit", 64)?;

    let blob_spec = format!("{}:{}", resolved, path.display());
    let blob_args = vec![
        "--git-dir".to_string(),
        repo_s,
        "cat-file".to_string(),
        "blob".to_string(),
        blob_spec,
    ];
    let bytes = git_output_owned(&blob_args, "git cat-file blob")?;
    Ok((bytes, resolved))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;

    read_ndjson::<InputRequest, _>(policy.limits.max_record_bytes, |request| {
        if let Some(schema) = &request.schema {
            if schema != "autoscribe.input.request.v1" {
                bail!("unsupported input request schema: {schema}");
            }
        }
        require_object(&request.routing, "routing")?;
        require_object(&request.baggage, "baggage")?;
        reject_reserved_keys(&request.routing)?;
        reject_reserved_keys(&request.baggage)?;

        let (bytes, source) = match &request.source {
            InputSource::File { path } => {
                let path = validate_absolute_target(path, &policy.paths.file_roots)?;
                let bytes = fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                (bytes, json!({"kind":"file","path":path}))
            }
            InputSource::Git { repo, commit, path } => {
                let (bytes, resolved) = read_git_blob(repo, commit, path, &policy.paths.repo_roots)?;
                let repo = validate_absolute_target(repo, &policy.paths.repo_roots)?;
                (bytes, json!({
                    "kind":"git",
                    "repo":repo,
                    "commit":resolved,
                    "path":path,
                }))
            }
        };

        if bytes.len() > policy.limits.max_body_bytes {
            bail!("input body exceeds {} bytes", policy.limits.max_body_bytes);
        }
        let content = String::from_utf8(bytes).context("input content must be UTF-8 text")?;
        let content_sha256 = sha256_hex(content.as_bytes());
        let identity_payload = json!({
            "source": source,
            "content_sha256": content_sha256,
            "routing": request.routing,
            "baggage": request.baggage,
        });
        let record_id = format!(
            "inp_{}",
            &sha256_hex(&canonical_json_bytes(&identity_payload)?)[..32]
        );

        let record = CanonicalInput {
            schema: INPUT_SCHEMA.to_string(),
            record_id,
            source: identity_payload["source"].clone(),
            content,
            content_sha256: identity_payload["content_sha256"]
                .as_str()
                .unwrap()
                .to_string(),
            routing: identity_payload["routing"].clone(),
            baggage: identity_payload["baggage"].clone(),
        };
        write_ndjson(&record)
    })
}
