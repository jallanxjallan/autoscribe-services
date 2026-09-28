# AutoScribe Services

Rust trust-boundary and operational code.

Rules:

- `asc` and `srv` are separate domains.
- One operation = one Rust binary.
- Shared implementation belongs in libraries/crates, not a catch-all executable.
- Rust validates/normalizes untrusted inputs before the Python pipeline sees them.
- Rust validates/authorizes effects before external writeback/export.
- `Control.git` is data and is untrusted; control ingestion is a Rust boundary operation.

This repository intentionally begins small. Code should be moved here from the
v0.9 migration baseline only when its boundary responsibility is explicit.
