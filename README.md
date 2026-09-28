# AutoScribe Services

Rust trust-boundary operations for AutoScribe.

This repository intentionally has **no `srv` catch-all executable**. Each operation is its own binary and shared implementation lives only in library crates.

## Binaries

| Binary | Boundary responsibility |
| --- | --- |
| `srv-input` | Read an explicitly declared file or Git blob from allow-listed roots and emit canonical `autoscribe.input.v1` NDJSON. |
| `srv-output` | Parse response baggage, validate output declarations, normalize targets, and emit HMAC-authenticated `autoscribe.effect.v1` effects. |
| `srv-control-ingest` | Read one exact commit from `control.git`, validate role/context/task/plan records, and atomically rebuild the trusted control SQLite DB. |
| `srv-writeback` | Verify a trusted repo effect, write one path into a bare Git repo, make exactly one commit, push it, and record an idempotent receipt. |
| `srv-export` | Verify a trusted file effect, atomically write the target, and record an idempotent receipt. |

`asc` remains a separate Python/application domain. These binaries do not contain Python pipeline logic.

## Trust model

```text
agents / humans / files / Git
              |
              v
           srv-input
              |
       canonical NDJSON
              |
              v
             asc
              |
        response+baggage
              |
              v
          srv-output
              |
   authenticated effect records
        /               \
       v                 v
srv-writeback        srv-export
       |                 |
       v                 v
      Git              files

agents -> control.git -> srv-control-ingest -> trusted control.sqlite -> asc/control
```

Git history is transport/history, not trust. `control.git` is explicitly untrusted because agents may commit to it. The runtime control catalogue is the last successfully validated snapshot in SQLite.

## Trusted effect keys

The Python pipeline is **not allowed to mint an effect key**. `srv-output`:

1. rejects reserved effect/receipt keys in incoming baggage;
2. parses the declarative `baggage.outputs` list;
3. normalizes and allow-lists each target;
4. hashes the response content;
5. HMAC-signs the normalized effect using `/etc/autoscribe/effect.key`.

`srv-writeback` and `srv-export` reconstruct the signed payload and verify the HMAC before touching an external target. Changing the target, content hash, call id, or effect index invalidates the effect.

The key file must be at least 32 bytes and mode `0600` (or stricter). A 64-character hex key is accepted.

## Server paths

The example production shape is:

```text
/home/jeremy/services/                         # development checkout
/home/jeremy/Repos/*.git                      # data/output bare repos
/opt/autoscribe/services/releases/<sha>/bin/  # immutable installed binaries
/opt/autoscribe/services/current -> releases/<sha>
/etc/autoscribe/services.toml                  # policy
/etc/autoscribe/effect.key                     # HMAC key; never Git
/var/lib/autoscribe/control.sqlite             # trusted control catalogue
/var/lib/autoscribe/effects.sqlite             # applied-effect receipts
```

## Policy

Copy `config/services.toml.example` to `/etc/autoscribe/services.toml` and keep the allowed roots narrow. The example permits Git operations only under `/home/jeremy/Repos` and ordinary files only under `/var/lib/autoscribe/files`.

Targets are required to be absolute, lexically safe, and contained by an allow-listed root. Existing symlink escapes are rejected. Paths *inside* a Git repo are required to be relative and may not contain `..`.

## Input contract

One request per NDJSON line.

File input:

```json
{"schema":"autoscribe.input.request.v1","source":{"kind":"file","path":"/var/lib/autoscribe/files/draft.md"},"routing":{"plan_id":"pln_example"},"baggage":{}}
```

Git input:

```json
{"schema":"autoscribe.input.request.v1","source":{"kind":"git","repo":"/home/jeremy/Repos/book.git","commit":"main","path":"Contents/Opening.md"},"routing":{"plan_id":"pln_example"},"baggage":{}}
```

`srv-input` resolves Git refs to an immutable commit SHA and emits the actual UTF-8 content plus its SHA-256 digest.

## Response/output contract

The pipeline returns `autoscribe.response.v1` records. Output instructions live in `baggage.outputs` and remain declarative until `srv-output` validates them.

Repo output:

```json
{"schema":"autoscribe.response.v1","call_id":"01EXAMPLE","content":"Rewritten text\n","baggage":{"outputs":[{"kind":"repo","repo":"/home/jeremy/Repos/book.git","path":"Contents/Opening.md","branch":"main","create_repo":false}]}}
```

File output:

```json
{"schema":"autoscribe.response.v1","call_id":"01EXAMPLE","content":"Rendered output\n","baggage":{"outputs":[{"kind":"file","path":"/var/lib/autoscribe/files/output.md","mode":"replace"}]}}
```

Pipe the result only to the adapter matching its effect kind:

```bash
srv-output < response.ndjson | srv-writeback
srv-output < response.ndjson | srv-export
```

For mixed effects, a dispatcher should route each authenticated effect by `effect.kind`; do not pipe mixed effects to one adapter.

## Git writeback semantics

`srv-writeback` targets **bare repositories**. It serializes local writes per repo, clones a temporary worktree, rejects symlink traversal in the target path, writes the complete file, and creates one commit even when the content is unchanged (`--allow-empty`). The commit message contains the trusted effect key.

Effect receipts make retries idempotent. If the process crashes after the Git push but before recording the receipt, the retry searches Git history for the effect key and reconstructs the receipt rather than creating another commit.

## Control records

Control is a data-only Git repository and is treated as untrusted. Canonical instruction records use the authored Control contract directly: `identity`, `type: instruction`, `scope`, `title`, `tags`, and the Markdown body. Instruction slugs and `component` aliases are rejected.

Example task instruction:

```yaml
---
title: Example Task
identity: spc_0123456789ABCDEF
type: instruction
scope: task
tags: []
---
Rewrite the supplied text without changing meaning.
```

The identity/scope mapping is:

```text
role     rol_<16 Crockford Base32 characters>
context  ctx_<16 Crockford Base32 characters>
task     spc_<16 Crockford Base32 characters>
```

The ingester rejects legacy instruction fields such as `slug`, `component`, `kind`, `version`, and embedded `role`/`context` composition. Plans remain slug-addressed records (`type: plan`) and may carry their canonical plan structure in frontmatter. Any `instructions` object found in a plan must contain exactly `role`, `context`, and `task` arrays, and every referenced identity must exist in the same Git snapshot with the matching scope.

`control-pre-receive` validates a proposed `main` commit before accepting it. `control-post-receive` then atomically loads that exact commit into SQLite. Copy the hook files into the bare Control repo after the installed service path exists.

## Build and test

On Biznet:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/smoke.sh
```

The smoke test creates only temporary repositories/files under `/tmp` and does not touch production data.

## Install

After accepting a commit:

```bash
./scripts/install-server.sh
```

The installer builds the release binaries, installs them beneath `/opt/autoscribe/services/releases/<git-sha>/bin`, and atomically repoints `/opt/autoscribe/services/current`. It does not create or overwrite secrets or production policy.
