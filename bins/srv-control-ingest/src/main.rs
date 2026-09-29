use anyhow::{bail, Context, Result};
use clap::Parser;
use rusqlite::{params, Connection, Transaction};
use serde_json::Value;
use srv_common::{
    canonical_json_bytes, git_output_owned, load_policy, reject_reserved_keys, sha256_hex,
    validate_absolute_target,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    about = "Validate a Control Git snapshot and atomically rebuild the trusted relational control DB"
)]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    commit: String,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    check_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordKind {
    Instruction(&'static str),
    Plan,
}

#[derive(Debug)]
struct InstructionRecord {
    id: String,
    kind: String,
    label: String,
    body: String,
    source_path: String,
    source_commit: String,
    content_sha256: String,
}

#[derive(Debug)]
struct InstructionRef {
    component: String,
    position: i64,
    instruction_id: String,
}

fn classify_record_path(path: &str) -> Option<RecordKind> {
    if path.starts_with("roles/") && path.ends_with(".md") {
        Some(RecordKind::Instruction("role"))
    } else if path.starts_with("contexts/") && path.ends_with(".md") {
        Some(RecordKind::Instruction("context"))
    } else if path.starts_with("tasks/") && path.ends_with(".md") {
        Some(RecordKind::Instruction("task"))
    } else if path.starts_with("plans/") && path.ends_with(".json") {
        Some(RecordKind::Plan)
    } else {
        None
    }
}

fn parse_frontmatter(text: &str) -> Result<(String, String)> {
    let normalized = text.replace("\r\n", "\n");
    if !normalized.starts_with("---\n") {
        bail!("control record must begin with YAML frontmatter");
    }
    let rest = &normalized[4..];
    let marker = "\n---\n";
    let end = rest
        .find(marker)
        .ok_or_else(|| anyhow::anyhow!("control record frontmatter is not terminated by ---"))?;
    Ok((
        rest[..end].to_string(),
        rest[end + marker.len()..].to_string(),
    ))
}

fn required_string(map: &serde_json::Map<String, Value>, key: &str) -> Result<String> {
    let value = map
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{key} must be a string"))?;
    if value.trim().is_empty() {
        bail!("{key} may not be empty");
    }
    Ok(value.to_string())
}

fn optional_string(map: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.to_string())),
        Some(_) => bail!("{key} must be a string or null"),
    }
}

fn is_crockford_char(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
}

fn validate_prefixed_identity(identity: &str, prefix: &str, label: &str) -> Result<()> {
    let suffix = identity
        .strip_prefix(prefix)
        .ok_or_else(|| anyhow::anyhow!("{label} identity must begin with {prefix}"))?;
    if suffix.len() != 16 || !suffix.bytes().all(is_crockford_char) {
        bail!(
            "{label} identity must be {prefix} followed by 16 uppercase Crockford Base32 characters: {identity}"
        );
    }
    Ok(())
}

fn validate_instruction_identity(identity: &str, scope: &str) -> Result<()> {
    let prefix = match scope {
        "role" => "rol_",
        "context" => "ctx_",
        "task" => "tsk_",
        other => bail!("invalid instruction scope: {other}"),
    };
    validate_prefixed_identity(identity, prefix, scope)
}

fn validate_plan_identity(identity: &str) -> Result<()> {
    validate_prefixed_identity(identity, "pln_", "plan")
}

fn resolve_commit(repo: &Path, commit: &str) -> Result<String> {
    if commit.is_empty()
        || commit.len() > 128
        || !commit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
    {
        bail!("unsafe commit/ref name");
    }
    let args = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
        "rev-parse".to_string(),
        format!("{}^{{commit}}", commit),
    ];
    Ok(
        String::from_utf8(git_output_owned(&args, "git rev-parse control commit")?)?
            .trim()
            .to_string(),
    )
}

fn list_paths(repo: &Path, commit: &str) -> Result<Vec<String>> {
    let args = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
        "ls-tree".to_string(),
        "-r".to_string(),
        "-z".to_string(),
        "--name-only".to_string(),
        commit.to_string(),
    ];
    let output = git_output_owned(&args, "git ls-tree control snapshot")?;
    output
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8(part.to_vec()).context("control path must be UTF-8"))
        .collect()
}

fn read_blob(repo: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    if path.contains('\0') || path.chars().any(char::is_control) {
        bail!("unsafe control path");
    }
    let args = vec![
        "--git-dir".to_string(),
        repo.display().to_string(),
        "cat-file".to_string(),
        "blob".to_string(),
        format!("{commit}:{path}"),
    ];
    git_output_owned(&args, "git cat-file control record")
}

fn checked_text(path: &str, bytes: Vec<u8>, max_bytes: usize) -> Result<String> {
    if bytes.len() > max_bytes {
        bail!("control record {path} exceeds {max_bytes} bytes");
    }
    let text = String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8"))?;
    if text.contains('\0') {
        bail!("control record {path} contains NUL bytes");
    }
    Ok(text)
}

fn parse_instruction_record(
    path: &str,
    text: String,
    commit: &str,
    expected_scope: &str,
) -> Result<InstructionRecord> {
    let (yaml_text, body) = parse_frontmatter(&text).with_context(|| path.to_string())?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&yaml_text)
        .with_context(|| format!("invalid YAML frontmatter in {path}"))?;
    let authored = serde_json::to_value(yaml)
        .with_context(|| format!("frontmatter in {path} is not JSON-compatible"))?;
    let authored_map = authored
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("frontmatter in {path} must be a mapping"))?;

    let identity = required_string(authored_map, "identity").with_context(|| path.to_string())?;
    validate_instruction_identity(&identity, expected_scope).with_context(|| path.to_string())?;

    if body.trim().is_empty() {
        bail!("instruction body may not be empty: {path}");
    }

    let label = Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot derive label from {path}"))?
        .to_string();

    Ok(InstructionRecord {
        id: identity,
        kind: expected_scope.to_string(),
        label,
        body,
        source_path: path.to_string(),
        source_commit: commit.to_string(),
        content_sha256: sha256_hex(text.as_bytes()),
    })
}

struct ReusableStepRecord {
    id: String,
    label: String,
    engine_kind: String,
    engine: String,
    model: Option<String>,
    script: Option<String>,
    rag_profile: Option<String>,
    args_json: String,
    instructions: Vec<InstructionRef>,
    source_path: String,
    source_commit: String,
    content_sha256: String,
}

struct PlanStepLink {
    position: i64,
    step_id: String,
}

struct ReusablePlanRecord {
    id: String,
    label: String,
    description: String,
    scope: Option<String>,
    source_path: String,
    source_commit: String,
    content_sha256: String,
    steps: Vec<PlanStepLink>,
}

fn is_reusable_step_path(path: &str) -> bool {
    let mut parts = path.split('/');
    matches!(parts.next(), Some("steps"))
        && matches!(parts.next(), Some(name) if name.ends_with(".json"))
        && parts.next().is_none()
}

fn validate_reusable_step_identity(identity: &str) -> Result<()> {
    const STEP_ID_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    let suffix = identity
        .strip_prefix("stp_")
        .ok_or_else(|| anyhow::anyhow!("step identity must begin with stp_"))?;

    if suffix.len() != 16
        || !suffix
            .chars()
            .all(|ch| ch.is_ascii() && STEP_ID_ALPHABET.contains(ch))
    {
        bail!("step identity must be stp_ followed by 16 uppercase Crockford Base32 characters");
    }
    Ok(())
}

fn parse_instruction_refs(step_id: &str, value: &Value) -> Result<Vec<InstructionRef>> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("step {step_id} instructions must be an object"))?;

    let expected = ["role", "context", "task"];
    if obj.len() != expected.len() || expected.iter().any(|key| !obj.contains_key(*key)) {
        bail!("step {step_id} instructions must contain exactly role, context and task arrays");
    }

    let mut refs = Vec::new();
    let mut seen = HashSet::new();

    for component in expected {
        let values = obj
            .get(component)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!("step {step_id} instruction component {component} must be an array")
            })?;

        for (index, value) in values.iter().enumerate() {
            let identity = value.as_str().ok_or_else(|| {
                anyhow::anyhow!("step {step_id} instruction identities must be strings")
            })?;
            validate_instruction_identity(identity, component)
                .with_context(|| format!("step {step_id}"))?;
            if !seen.insert(identity.to_string()) {
                bail!("step {step_id} repeats instruction {identity}");
            }
            refs.push(InstructionRef {
                component: component.to_string(),
                position: (index + 1) as i64,
                instruction_id: identity.to_string(),
            });
        }
    }

    Ok(refs)
}

fn parse_reusable_step_record(
    path: &str,
    text: String,
    commit: &str,
) -> Result<ReusableStepRecord> {
    let step: Value =
        serde_json::from_str(&text).with_context(|| format!("invalid JSON step in {path}"))?;
    reject_reserved_keys(&step).with_context(|| path.to_string())?;

    let map = step
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("step {path} must be a JSON object"))?;

    let id = required_string(map, "identity").with_context(|| path.to_string())?;
    validate_reusable_step_identity(&id).with_context(|| path.to_string())?;

    let expected_path = format!("steps/{id}.json");
    if path != expected_path {
        bail!("step identity/filename mismatch: {path} contains {id}");
    }

    let label = required_string(map, "label").with_context(|| path.to_string())?;
    let engine = required_string(map, "engine").with_context(|| format!("step {id}"))?;
    let engine_kind = required_string(map, "engine_kind").with_context(|| format!("step {id}"))?;

    let (model, script, rag_profile) = match engine_kind.as_str() {
        "llm" => (
            Some(required_string(map, "model").with_context(|| format!("step {id}"))?),
            None,
            None,
        ),
        "script" => (
            None,
            Some(required_string(map, "script").with_context(|| format!("step {id}"))?),
            None,
        ),
        "rag" => (
            None,
            None,
            Some(required_string(map, "rag_profile").with_context(|| format!("step {id}"))?),
        ),
        other => bail!("step {id} engine_kind must be llm, script or rag, not {other}"),
    };

    for (field, allowed) in [
        ("model", engine_kind == "llm"),
        ("script", engine_kind == "script"),
        ("rag_profile", engine_kind == "rag"),
    ] {
        if !allowed && map.contains_key(field) {
            bail!("step {id} has conflicting field {field}");
        }
    }

    let args = match map.get("args") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(value)) => value.clone(),
        Some(_) => bail!("step {id} args must be an object when present"),
    };
    let args_json = String::from_utf8(canonical_json_bytes(&Value::Object(args))?)?;

    let instructions = match map.get("instructions") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => parse_instruction_refs(&id, value)?,
    };

    Ok(ReusableStepRecord {
        id,
        label,
        engine_kind,
        engine,
        model,
        script,
        rag_profile,
        args_json,
        instructions,
        source_path: path.to_string(),
        source_commit: commit.to_string(),
        content_sha256: sha256_hex(text.as_bytes()),
    })
}

fn parse_plan_record(path: &str, text: String, commit: &str) -> Result<ReusablePlanRecord> {
    let plan: Value =
        serde_json::from_str(&text).with_context(|| format!("invalid JSON plan in {path}"))?;
    reject_reserved_keys(&plan).with_context(|| path.to_string())?;

    let map = plan
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("plan {path} must be a JSON object"))?;

    let id = required_string(map, "identity").with_context(|| path.to_string())?;
    validate_plan_identity(&id).with_context(|| path.to_string())?;
    let label = required_string(map, "title").with_context(|| path.to_string())?;
    let description = optional_string(map, "description")
        .with_context(|| path.to_string())?
        .unwrap_or_default();
    let scope = optional_string(map, "scope").with_context(|| path.to_string())?;

    let steps_array = map
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("plan {path} steps must be an array of step identities"))?;
    if steps_array.is_empty() {
        bail!("plan {path} must include at least one step");
    }

    let mut steps = Vec::with_capacity(steps_array.len());
    for (index, value) in steps_array.iter().enumerate() {
        let step_id = value.as_str().ok_or_else(|| {
            anyhow::anyhow!(
                "plan {path} step {} must be a step identity string",
                index + 1
            )
        })?;
        validate_reusable_step_identity(step_id)
            .with_context(|| format!("plan {path} step {}", index + 1))?;
        steps.push(PlanStepLink {
            position: (index + 1) as i64,
            step_id: step_id.to_string(),
        });
    }

    Ok(ReusablePlanRecord {
        id,
        label,
        description,
        scope,
        source_path: path.to_string(),
        source_commit: commit.to_string(),
        content_sha256: sha256_hex(text.as_bytes()),
        steps,
    })
}

fn validate_cross_references(
    instructions: &[InstructionRecord],
    steps: &[ReusableStepRecord],
    plans: &[ReusablePlanRecord],
) -> Result<()> {
    let instruction_kinds: HashMap<&str, &str> = instructions
        .iter()
        .map(|record| (record.id.as_str(), record.kind.as_str()))
        .collect();

    for step in steps {
        for reference in &step.instructions {
            let actual = instruction_kinds
                .get(reference.instruction_id.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "step {} references missing instruction {}",
                        step.id,
                        reference.instruction_id
                    )
                })?;
            if *actual != reference.component.as_str() {
                bail!(
                    "step {} references {} as {}, but instruction kind is {}",
                    step.id,
                    reference.instruction_id,
                    reference.component,
                    actual
                );
            }
        }
    }

    let step_ids: HashSet<&str> = steps.iter().map(|step| step.id.as_str()).collect();
    for plan in plans {
        for link in &plan.steps {
            if !step_ids.contains(link.step_id.as_str()) {
                bail!(
                    "plan {} step {} references missing reusable step {}",
                    plan.source_path,
                    link.position,
                    link.step_id
                );
            }
        }
    }

    Ok(())
}

fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open control DB {}", path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

fn create_schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        r#"DROP TABLE IF EXISTS control_records;
           DROP TABLE IF EXISTS plan_steps;
           DROP TABLE IF EXISTS step_instructions;
           DROP TABLE IF EXISTS steps;
           DROP TABLE IF EXISTS plans;
           DROP TABLE IF EXISTS instructions;
           DROP TABLE IF EXISTS control_meta;

           CREATE TABLE instructions (
              id TEXT PRIMARY KEY,
              kind TEXT NOT NULL CHECK (kind IN ('role', 'context', 'task')),
              label TEXT NOT NULL,
              body TEXT NOT NULL,
              source_path TEXT NOT NULL UNIQUE,
              source_commit TEXT NOT NULL,
              content_sha256 TEXT NOT NULL
           );

           CREATE TABLE steps (
              id TEXT PRIMARY KEY,
              label TEXT NOT NULL,
              engine_kind TEXT NOT NULL CHECK (engine_kind IN ('llm', 'script', 'rag')),
              engine TEXT NOT NULL,
              model TEXT,
              script TEXT,
              rag_profile TEXT,
              args_json TEXT NOT NULL DEFAULT '{}',
              source_path TEXT NOT NULL UNIQUE,
              source_commit TEXT NOT NULL,
              content_sha256 TEXT NOT NULL,
              CHECK (
                  (engine_kind = 'llm' AND model IS NOT NULL AND script IS NULL AND rag_profile IS NULL)
                  OR
                  (engine_kind = 'script' AND model IS NULL AND script IS NOT NULL AND rag_profile IS NULL)
                  OR
                  (engine_kind = 'rag' AND model IS NULL AND script IS NULL AND rag_profile IS NOT NULL)
              )
           );

           CREATE TABLE step_instructions (
              step_id TEXT NOT NULL REFERENCES steps(id) ON DELETE CASCADE,
              instruction_id TEXT NOT NULL REFERENCES instructions(id) ON DELETE RESTRICT,
              component TEXT NOT NULL CHECK (component IN ('role', 'context', 'task')),
              position INTEGER NOT NULL CHECK (position >= 1),
              PRIMARY KEY (step_id, component, position),
              UNIQUE (step_id, instruction_id)
           );

           CREATE TABLE plans (
              id TEXT PRIMARY KEY,
              label TEXT NOT NULL,
              description TEXT NOT NULL DEFAULT '',
              scope TEXT,
              source_path TEXT NOT NULL UNIQUE,
              source_commit TEXT NOT NULL,
              content_sha256 TEXT NOT NULL
           );

           CREATE TABLE plan_steps (
              plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
              step_id TEXT NOT NULL REFERENCES steps(id) ON DELETE RESTRICT,
              position INTEGER NOT NULL CHECK (position >= 1),
              PRIMARY KEY (plan_id, position)
           );

           CREATE TABLE control_meta (
              key TEXT PRIMARY KEY,
              value TEXT NOT NULL
           );

           CREATE INDEX instructions_kind_idx ON instructions(kind);
           CREATE INDEX step_instructions_instruction_idx
              ON step_instructions(instruction_id);
           CREATE INDEX plan_steps_step_idx ON plan_steps(step_id);
           CREATE INDEX plan_steps_plan_idx ON plan_steps(plan_id, position);"#,
    )?;
    Ok(())
}

fn rebuild_db(
    conn: &mut Connection,
    instructions: &[InstructionRecord],
    steps: &[ReusableStepRecord],
    plans: &[ReusablePlanRecord],
    commit: &str,
) -> Result<(usize, usize, usize)> {
    let tx = conn.transaction()?;
    create_schema(&tx)?;

    for record in instructions {
        tx.execute(
            "INSERT INTO instructions
             (id, kind, label, body, source_path, source_commit, content_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &record.id,
                &record.kind,
                &record.label,
                &record.body,
                &record.source_path,
                &record.source_commit,
                &record.content_sha256,
            ],
        )?;
    }

    let mut instruction_link_count = 0usize;
    for step in steps {
        tx.execute(
            "INSERT INTO steps
             (id, label, engine_kind, engine, model, script, rag_profile, args_json,
              source_path, source_commit, content_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                &step.id,
                &step.label,
                &step.engine_kind,
                &step.engine,
                step.model.as_deref(),
                step.script.as_deref(),
                step.rag_profile.as_deref(),
                &step.args_json,
                &step.source_path,
                &step.source_commit,
                &step.content_sha256,
            ],
        )?;

        for reference in &step.instructions {
            tx.execute(
                "INSERT INTO step_instructions
                 (step_id, instruction_id, component, position)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    &step.id,
                    &reference.instruction_id,
                    &reference.component,
                    reference.position,
                ],
            )?;
            instruction_link_count += 1;
        }
    }

    let mut plan_step_count = 0usize;
    for plan in plans {
        tx.execute(
            "INSERT INTO plans
             (id, label, description, scope, source_path, source_commit, content_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &plan.id,
                &plan.label,
                &plan.description,
                plan.scope.as_deref(),
                &plan.source_path,
                &plan.source_commit,
                &plan.content_sha256,
            ],
        )?;

        for link in &plan.steps {
            tx.execute(
                "INSERT INTO plan_steps(plan_id, step_id, position)
                 VALUES (?1, ?2, ?3)",
                params![&plan.id, &link.step_id, link.position],
            )?;
            plan_step_count += 1;
        }
    }

    for (key, value) in [
        ("schema_version", "3".to_string()),
        ("source_commit", commit.to_string()),
        ("instruction_count", instructions.len().to_string()),
        ("step_count", steps.len().to_string()),
        ("plan_count", plans.len().to_string()),
        ("plan_step_count", plan_step_count.to_string()),
        ("step_instruction_count", instruction_link_count.to_string()),
    ] {
        tx.execute(
            "INSERT INTO control_meta(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
    }

    tx.commit()?;
    Ok((steps.len(), plan_step_count, instruction_link_count))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let repo = validate_absolute_target(&args.repo, &policy.paths.repo_roots)?;
    if !repo.is_dir() {
        bail!("Control repo is not a directory: {}", repo.display());
    }

    let commit = resolve_commit(&repo, &args.commit)?;
    let paths = list_paths(&repo, &commit)?;

    let mut instructions = Vec::new();
    let mut steps = Vec::new();
    let mut plans = Vec::new();
    let mut identities = HashSet::new();
    let mut source_paths = HashSet::new();
    let mut source_record_count = 0usize;

    for path in paths {
        let is_step = is_reusable_step_path(&path);
        let kind = if is_step {
            None
        } else {
            classify_record_path(&path)
        };

        if !is_step && kind.is_none() {
            continue;
        }

        source_record_count += 1;
        if source_record_count > policy.limits.max_control_records {
            bail!(
                "control snapshot exceeds {} source records",
                policy.limits.max_control_records
            );
        }

        if !source_paths.insert(path.clone()) {
            bail!("duplicate Control source path: {path}");
        }

        let blob = read_blob(&repo, &commit, &path)?;
        let text = checked_text(&path, blob, policy.limits.max_control_file_bytes)?;

        if is_step {
            let record = parse_reusable_step_record(&path, text, &commit)?;
            if !identities.insert(record.id.clone()) {
                bail!("duplicate Control identity: {}", record.id);
            }
            steps.push(record);
            continue;
        }

        match kind.expect("record kind checked above") {
            RecordKind::Instruction(expected_scope) => {
                let record = parse_instruction_record(&path, text, &commit, expected_scope)?;
                if !identities.insert(record.id.clone()) {
                    bail!("duplicate Control identity: {}", record.id);
                }
                instructions.push(record);
            }
            RecordKind::Plan => {
                let record = parse_plan_record(&path, text, &commit)?;
                if !identities.insert(record.id.clone()) {
                    bail!("duplicate Control identity: {}", record.id);
                }
                plans.push(record);
            }
        }
    }

    validate_cross_references(&instructions, &steps, &plans)?;

    let plan_step_count: usize = plans.iter().map(|plan| plan.steps.len()).sum();
    let instruction_link_count: usize = steps.iter().map(|step| step.instructions.len()).sum();

    if args.check_only {
        println!(
            "validated {} instructions, {} reusable steps, {} plans, {} plan-step links and {} step-instruction links at {}",
            instructions.len(),
            steps.len(),
            plans.len(),
            plan_step_count,
            instruction_link_count,
            commit
        );
        return Ok(());
    }

    let db_path = args.db.unwrap_or_else(|| policy.paths.control_db.clone());
    let mut conn = open_db(&db_path)?;
    let (written_steps, written_plan_steps, written_instruction_links) =
        rebuild_db(&mut conn, &instructions, &steps, &plans, &commit)?;

    println!(
        "ingested {} instructions, {} reusable steps, {} plans, {} plan-step links and {} step-instruction links from {} into {}",
        instructions.len(),
        written_steps,
        plans.len(),
        written_plan_steps,
        written_instruction_links,
        commit,
        db_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_canonical_control_paths() {
        assert_eq!(
            classify_record_path("roles/Writer.md"),
            Some(RecordKind::Instruction("role"))
        );
        assert_eq!(
            classify_record_path("contexts/Project.md"),
            Some(RecordKind::Instruction("context"))
        );
        assert_eq!(
            classify_record_path("tasks/Proofread.md"),
            Some(RecordKind::Instruction("task"))
        );
        assert_eq!(
            classify_record_path("plans/Example.json"),
            Some(RecordKind::Plan)
        );

        assert_eq!(classify_record_path("AGENTS.md"), None);
        assert_eq!(classify_record_path("Templates/New Task.md"), None);
        assert_eq!(classify_record_path("plans/Example.md"), None);
    }

    #[test]
    fn identities_use_machine_prefixes() {
        validate_instruction_identity("tsk_T3W8N5R7C2M9X6QK", "task").unwrap();
        assert!(validate_instruction_identity("spc_T3W8N5R7C2M9X6QK", "task").is_err());

        validate_plan_identity("pln_P7M4Q9V2X6C8B3RN").unwrap();
        assert!(validate_plan_identity("plan.example").is_err());
    }

    #[test]
    fn parses_canonical_relational_plan_shape() {
        let text = r#"{
            "identity": "pln_P7M4Q9V2X6C8B3RN",
            "title": "Normalize and Proofread",
            "description": "Example",
            "scope": "project",
            "steps": {
                "1": {
                    "engine": "openai",
                    "engine_kind": "llm",
                    "label": "Normalize",
                    "model": "gpt-5.6",
                    "temperature": 0.2,
                    "max_output_tokens": 1200,
                    "args": {"reasoning_effort": "medium"},
                    "instructions": {
                        "role": ["rol_K7M4Q9V2X6C8B3RN"],
                        "context": [],
                        "task": ["tsk_T3W8N5R7C2M9X6QK"]
                    }
                }
            },
            "capabilities": {
                "engines": {},
                "models": {},
                "local_scripts": {},
                "rag_profiles": {}
            }
        }"#;

        let record = parse_plan_record("plans/Example.json", text.to_string(), "abc123").unwrap();

        assert_eq!(record.id, "pln_P7M4Q9V2X6C8B3RN");
        assert_eq!(record.label, "Normalize and Proofread");
        assert_eq!(record.steps.len(), 1);
        assert_eq!(record.steps[0].engine_kind, "llm");
        assert_eq!(record.steps[0].model.as_deref(), Some("gpt-5.6"));
        assert_eq!(record.steps[0].temperature, Some(0.2));
        assert_eq!(record.steps[0].max_output_tokens, Some(1200));
        assert_eq!(record.steps[0].instructions.len(), 2);
        assert_eq!(
            record.steps[0].args_json,
            r#"{"reasoning_effort":"medium"}"#
        );
    }

    #[test]
    fn plan_steps_must_be_contiguous() {
        let text = r#"{
            "identity": "pln_P7M4Q9V2X6C8B3RN",
            "title": "Broken",
            "steps": {
                "2": {
                    "engine": "openai",
                    "engine_kind": "llm",
                    "model": "gpt-5.6",
                    "args": {},
                    "instructions": {"role": [], "context": [], "task": []}
                }
            }
        }"#;

        assert!(parse_plan_record("plans/Broken.json", text.to_string(), "abc123").is_err());
    }

    #[test]
    fn instruction_directory_must_match_scope() {
        let text = "---\nidentity: rol_K7M4Q9V2X6C8B3RN\n---\nWrite clearly.\n";
        assert!(
            parse_instruction_record("tasks/Writer.md", text.to_string(), "abc123", "task")
                .is_err()
        );
    }
}
