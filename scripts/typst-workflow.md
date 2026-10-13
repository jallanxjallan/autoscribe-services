# Typst on AutoScribe

Typst 0.15.1 is installed as `/usr/local/bin/typst`. The polling adapter is `/usr/local/bin/srv-typst`, compiled from a single Rust source file using only the standard library. No desktop application, TeX installation, or new package stack is used.

## Dropbox project contract

Create a folder directly under `Dropbox/typst/`:

```text
Dropbox/typst/my-project/
  main.typ
  typst.conf          # optional
  images/            # optional; assets may use any relative layout
  main.pdf           # generated output by default
```

Without configuration, `main.typ` is compiled to `main.pdf` in the same folder. Names are case-sensitive. Project folder names must not start with a dot or contain control characters, slashes, or backslashes.

Optional `typst.conf` uses plain `key=value` lines, without quotes. Blank lines and lines beginning with `#` are ignored. Duplicate or unknown settings fail the project:

```ini
entrypoint=main.typ
output=report.pdf
input.title=Quarterly report
input.edition=2026-10
```

The entrypoint must be a project-relative `.typ` path, optionally in a subfolder; absolute paths and `..` traversal are rejected. The output must be a `.pdf` filename in the project root. That filename is reserved for generated output, and its previous contents do not trigger compilation. Other PDFs may be included as assets.

Read inputs in Typst through `sys.inputs`, for example:

```typst
#set page(paper: "a4", margin: 24mm)
#set text(size: 12pt)
= #sys.inputs.at("title", default: "My report")
#image("images/chart.svg", width: 80%)
```

Page size, margins, styles, and other typesetting settings live in the Typst source or an imported local settings file. Typst uses its embedded fonts plus font files inside the project; system fonts are ignored. No packages are required by the sample. Projects that import online Typst packages may cause Typst to download those packages into its normal cache.

## Operation

The adapter polls `dropbox:typst` every 30 seconds through the existing rclone configuration. It mirrors each project into `/var/lib/autoscribe/typst/<project>/source`, detects changes to source/config/assets, and waits for matching inputs across two polls. A small project normally compiles within roughly 30–70 seconds after a completed upload; larger projects or other queued projects can take longer.

Compilation runs with the project folder as its root. After compilation, the adapter rechecks the source before uploading the PDF. An edit after this last check can still make the PDF briefly stale; subsequent polls detect it and rebuild. Failed compilation or configuration leaves the previous Dropbox PDF intact. Errors go to the systemd journal and are retried on later polls. Files other than the configured PDF are never uploaded by the adapter.

The local mirror includes the existing PDF, but the PDF is excluded from input change detection. Successful fingerprints persist across service restarts. Deleting the output PDF requests a rebuild. The adapter keeps local caches for deleted or renamed projects; it does not delete Dropbox projects or older output filenames.

Subprocesses time out after 180 seconds (with a further 10 seconds before forced termination). The service has a 1 GiB memory limit. This workflow is intended for the user's own Dropbox projects.

## Installed paths

| Purpose | Server path |
| --- | --- |
| Typst binary | `/usr/local/bin/typst` |
| Rust adapter | `/usr/local/bin/srv-typst` |
| Adapter source | `/home/jeremy/services/scripts/srv-typst.rs` |
| Service settings | `/etc/autoscribe/typst.conf` |
| User service | `/home/jeremy/.config/systemd/user/autoscribe-typst.service` |
| Service/config copies | `/home/jeremy/services/systemd/autoscribe-typst.service`, `/home/jeremy/services/config/typst.conf.example` |
| State and downloaded projects | `/var/lib/autoscribe/typst` |

The `/usr/local/bin` installation remains independent of AutoScribe's immutable release symlink. Existing NDJSON services and contracts are unchanged. User lingering was already enabled, allowing this user service to run without an SSH login and start at boot.

## Administration

Run as `jeremy` on AutoScribe:

```sh
systemctl --user status autoscribe-typst.service
journalctl --user -u autoscribe-typst.service -n 50 --no-pager
systemctl --user restart autoscribe-typst.service
```

To compile all projects immediately, stop the watcher and run one pass with the same lock:

```sh
systemctl --user stop autoscribe-typst.service
/usr/bin/flock --no-fork --nonblock /var/lib/autoscribe/typst/service.lock /usr/local/bin/srv-typst /etc/autoscribe/typst.conf --once
systemctl --user start autoscribe-typst.service
```

`--once` skips the two-poll wait, still rechecks inputs before upload, and exits nonzero if a project fails. Always run the adapter under the shared lock.

To disable the workflow:

```sh
systemctl --user disable --now autoscribe-typst.service
```

Rebuild and test the adapter with the server's existing Rust toolchain:

```sh
/home/jeremy/.cargo/bin/rustc --edition 2021 --test /home/jeremy/services/scripts/srv-typst.rs -o /tmp/srv-typst-tests
/tmp/srv-typst-tests
/home/jeremy/.cargo/bin/rustc --edition 2021 -D warnings -O /home/jeremy/services/scripts/srv-typst.rs -o /tmp/srv-typst
```

Typst release source: https://github.com/typst/typst/releases/tag/v0.15.1

Verified archive SHA-256: `a6d077d0a95eed5a2eba715b2dae06be954f624ccbf85758a03f389ded33118c`.

## Verification completed

On 5 October 2026:

- Both Rust tests passed: path/config validation and input fingerprint behavior.
- The Rust adapter compiled with warnings treated as errors.
- systemd validated the user unit; the service is active and enabled, with user lingering enabled.
- The live watcher compiled `Dropbox/typst/autoscribe-smoke-20261005/main.typ`, including `badge.svg` and the configured title, into `smoke-test.pdf` in the same Dropbox folder.
- An unchanged project retained its PDF contents and modification time.
- An intentionally invalid Typst source produced a journal error without replacing the previous PDF.
- Corrected source automatically produced and uploaded a new PDF; source, config, and image contents were preserved.
- Text extracted from the returned PDF contained the configured title and `Revision: recovery smoke test passed.`

The verified sample remains in `Dropbox/typst/autoscribe-smoke-20261005/`. Its final PDF SHA-256 is `a57fb43558320ea4905227170e439f4b452d4d1d5cfe6ca73c4a1efd6b952c49`.
