# AutoScribe Services

Rust trust-boundary operations for AutoScribe.

The core server boundary is deliberately limited to **two data modes**:

1. **Repo mode** — invoked from a bare repository `post-receive` hook. Input is read from the pushed Git commit and every response is bound to the same source repository/path.
2. **Direct mode** — invoked manually in the server terminal. Input is read from `dropbox:biznet/incoming/<batch>` and every response is bound to `dropbox:biznet/outgoing/<batch>`.

There is no arbitrary source/output matrix in the core pipeline. Source gathering, publishing, HTML generation, Google Drive delivery, and other personal orchestration belong outside this package.

## Binaries

| Binary | Boundary responsibility |
| --- | --- |
| `srv-input` | `repo` or `direct` ingress. Reads trusted source locations, validates content, emits canonical `autoscribe.input.v1` NDJSON, and signs the fixed return route. |
| `srv-output` | Verifies the signed return route copied through response baggage and emits exactly one authenticated `autoscribe.effect.v1` effect. It does not accept `baggage.outputs`. |
| `srv-control-ingest` | Reads one exact commit from `control.git`, validates Control records, and atomically rebuilds the trusted control SQLite DB. |
| `srv-writeback` | Applies authenticated repo effects to the already-existing source bare repo. |
| `srv-export` | Applies authenticated direct-mode effects only to `dropbox:biznet/outgoing/<batch>`. |

`asc` remains a separate Python/application domain. These binaries do not contain model or plan-execution logic.

## Trust model

```text
REPO MODE

repo post-receive
       |
       v
srv-input repo
       |
canonical NDJSON + signed same-repo return route
       |
       v
   asc enqueue
       |
       v
   response
       |
       v
  srv-output
       |
 authenticated repo effect
       |
       v
 srv-writeback
       |
       v
same source repo/path


DIRECT MODE

dropbox:biznet/incoming/<batch>
       |
       v
srv-input direct --batch <batch> --plan <pln_id>
       |
canonical NDJSON + signed matching-batch return route
       |
       v
   asc enqueue
       |
       v
   response
       |
       v
  srv-output
       |
 authenticated Dropbox effect
       |
       v
  srv-export
       |
       v
dropbox:biznet/outgoing/<batch>
```

The Python pipeline receives a signed `baggage.autoscribe_return` envelope. It may carry that envelope through the call, but it cannot change the destination: `srv-output` verifies the signature before producing an effect.

Incoming response baggage containing the old `outputs` declaration is rejected.

## Repo mode

`srv-input repo` resolves the supplied commit to an immutable SHA, reads its commit message, and looks for exactly one `Plan:` line. The final token on that line must be the canonical `pln_...` identity.

A commit without a `Plan:` line is ignored successfully and emits no NDJSON. This is important because an AutoScribe writeback commit re-triggers `post-receive`; since the generated writeback commit has no `Plan:` line, it cannot recursively redispatch itself.

Only added/modified Markdown files with leading YAML frontmatter are emitted as model inputs. Merge dispatch commits are rejected.

Example:

```bash
srv-input \
  --policy /etc/autoscribe/services.toml \
  repo \
  --repo /home/jeremy/Repos/book.git \
  --commit "$newrev" \
  --branch main \
| asc enqueue
```

The corresponding return route contains only that same repo, source path, and branch. `create_repo` is always false.

## Direct mode

A batch name is supplied at invocation and is a single safe folder name. `srv-input` recursively lists that batch beneath the fixed incoming root and emits one canonical input record per UTF-8 file.

Example:

```bash
srv-input \
  --policy /etc/autoscribe/services.toml \
  direct \
  --batch jakarta-guide-01 \
  --plan pln_0123456789ABCDEF \
| asc enqueue
```

This reads only:

```text
dropbox:biznet/incoming/jakarta-guide-01/
```

and signs a return route that can resolve only to:

```text
dropbox:biznet/outgoing/jakarta-guide-01/
```

The relative path within the batch is preserved. For example:

```text
incoming/jakarta-guide-01/sources/museum.txt
```

returns to:

```text
outgoing/jakarta-guide-01/sources/museum.txt
```

`rclone` is invoked from `PATH`. For testing or controlled administration its executable may be overridden with the trusted `AUTOSCRIBE_RCLONE` environment variable.

## Output contract

The pipeline returns `autoscribe.response.v1` records carrying the original `baggage.autoscribe_return` envelope.

`srv-output`:

1. rejects the old caller-selected `baggage.outputs` field;
2. verifies the ingress HMAC on `autoscribe_return`;
3. revalidates the repo/batch/path against the locked mode;
4. hashes the response content;
5. emits one authenticated effect.

Repo response:

```bash
srv-output < response.ndjson | srv-writeback
```

Direct response:

```bash
srv-output < response.ndjson | srv-export
```

Effects remain HMAC authenticated and receipts remain idempotent. The Python pipeline never mints trusted effect keys.

## Fixed external paths

The personal direct mode intentionally fixes its Dropbox roots in the Rust boundary:

```text
dropbox:biznet/incoming
dropbox:biznet/outgoing
```

There is no CLI option or response-baggage field for replacing those roots.

Repo mode still permits only repositories below the configured `paths.repo_roots`.

## Control

Control remains independent of the two data modes:

```text
control.git -> srv-control-ingest -> trusted control SQLite -> asc
```

`control.git` is untrusted authoring/transport. The runtime catalogue is the last successfully validated SQLite snapshot.

## Server paths

```text
/home/jeremy/services/                         # development checkout
/home/jeremy/Repos/*.git                      # bare repos
/opt/autoscribe/services/releases/<sha>/bin/  # immutable installed binaries
/opt/autoscribe/services/current -> releases/<sha>
/etc/autoscribe/services.toml                  # policy
/etc/autoscribe/effect.key                     # HMAC key; never Git
/var/lib/autoscribe/effects.sqlite             # effect receipts
```

The example policy keeps the Git repo root narrow. `file_roots` remains in the current shared policy schema for compatibility but is not an input/output route in the locked two-mode pipeline.

## Build and test

On Biznet:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/smoke.sh
```

The smoke test uses temporary repos and a fake local `rclone`; it does not touch production Dropbox or production repos.

## Install

After accepting a commit:

```bash
./scripts/install-server.sh
```

The installer builds the release binaries beneath `/opt/autoscribe/services/releases/<sha>/bin` and atomically repoints `/opt/autoscribe/services/current`.
