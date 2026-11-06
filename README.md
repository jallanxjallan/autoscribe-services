# AutoScribe Services

Job submission, response delivery and acknowledgement use SSH/SCP exclusively.
The server `asc job` interface owns verified request packages and response
receipts; this workspace has no polling submission or response transport.

## Retained tools

| Tool | Purpose |
| --- | --- |
| `srv-output` | Verify a signed return route and emit one authenticated repository effect |
| `srv-writeback` | Verify and atomically apply the repository effect, retaining an idempotent receipt |
| `srv-control-ingest` | Ingest canonical control records |
| `srv-plan-select` | Select existing control plans |
| vault tools | Existing independent vault management; see [its documentation](bins/vault-tools/README.md) |

Signed server output effects are independent of client local update/create and
cannot select an unsigned destination. Only a completed, failure-free response
can create an effect. The permitted route is `repo`; its path/root/branch are
checked against the trusted route and configured policy. The HMAC return envelope
remains `autoscribe.return.v1`.

`autoscribe.effect.v2` carries `submission_id`, `call_id`, complete opaque
`baggage`, the authorized effect and content checksum. Its signature binds those
fields. `autoscribe.receipt.v2` returns the same execution IDs and baggage without
reinterpretation, together with effect key, kind, target and result. The existing
effects database stores those IDs and `baggage_json` with the receipt. The system
is preproduction; canonical schemas are updated directly, without migrations or
retired-contract adapters.

The separate repository effect path is:

```sh
asc response CALL_ID | srv-output --policy /absolute/services.toml | srv-writeback --policy /absolute/services.toml
```

`asc effects pending` and `asc effects notify` serve the signed repository-output
coordinator. They are not response delivery to clients. SSH response delivery
uses `asc job status` / verified SCP retrieval / checksum-bound `asc job cleanup`.

The service policy example authorizes repository roots, signing key, effects and
control databases, limits and Git identity. No transport roots are configurable.
Manuscript asset retrieval remains owned by the existing Extensions adapter;
external asset links and their resolution semantics are unchanged. Independent
publishing and vault backup tooling are unchanged.

## Verification and installation boundaries

Use `cargo test --offline --locked --workspace` for components and the coordinated
AutoScribe `app/tests/qualification_smoke.py` for isolated client/server/local-file
and signed-repository integration. `scripts/server-test.sh` never fetches, resets,
installs or activates a checkout. The separate install script's binary inventory
matches this workspace. Do not run installation or activation without a separate
approved release instruction.
