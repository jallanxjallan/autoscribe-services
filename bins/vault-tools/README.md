# Vault tools

Three Rust binaries manage one generic Obsidian vault model. There are no vault
profiles or vault types.

The canonical source is a **source library**, not an Obsidian vault. It does
not contain or require `.obsidian/`.

## Source layout

The normal source is `~/.config/obsidian-vault`. Recognized source paths map into a live
vault like this:

```text
~/.config/obsidian-vault/                    target vault/
├── config/                 ->   .obsidian/
├── plugins/                ->   .obsidian/plugins/
├── snippets/               ->   .obsidian/snippets/
├── themes/                 ->   .obsidian/themes/
├── tools/                  ->   _tools/
├── templates/              ->   _templates/
├── scripts/                ->   _scripts/
├── ui/                     ->   _ui/
└── views/                  ->   _views/
```

For convenience, these JSON files may also live directly at the source root
and are written into `.obsidian/`:

- `app.json`
- `appearance.json`
- `core-plugins.json`
- `community-plugins.json`
- `hotkeys.json`
- `templates.json`

`README.md` and `AGENTS.md` files inside managed source trees are treated as
source documentation and are not copied into project vaults.

Forward installation validates `hotkeys.json`, `community-plugins.json`, and every enabled plugin's matching manifest and executable before copying any files. Missing packaged assets are hard failures.

Workspace/session state, caches, nested Git repositories, `node_modules`, and
Python bytecode caches are excluded. Managed symlinks are rejected.

## Commands

### `init-project-vault`

Run from the root of an existing Obsidian-created vault.

It:

- applies managed editing configuration from the source library;
- initializes Git on `main` if the vault is not already a repository;
- maintains a small volatile-state block in `.gitignore`;
- creates a bare backup repository at
  `~/Dropbox/Repos/<snake_case_of_vault_folder>.git`;
- configures that bare repository as `origin`;
- for a new repository, makes the initial commit and pushes it.

It refuses to initialize a vault nested inside another Git repository and
refuses to connect a newly-created local repository to a pre-existing bare
backup.

### `update-project-vault`

Reapplies the managed source library to the current vault. It is additive/update
only by default. `--prune` applies only to explicitly managed editing trees;
it never treats ordinary vault content as managed state.

### `update-master-vault`

Compares reusable configuration in the current vault with the source library.
It is a dry run by default. Re-run with `--apply` after reviewing the proposed
changes.

Reverse propagation is intentionally narrower than forward installation:

- reusable config and editing tools/templates may flow back;
- plugin `data.json` may flow back for plugins known to the source library;
- plugin executables such as `main.js` never flow back;
- ordinary notes never flow back;
- source files are never deleted automatically;
- likely credentials/tokens are rejected unless `--allow-sensitive` is
  explicitly supplied;
- on `--apply`, the source subtree must be clean in its containing Git
  repository before the write begins.

## Source discovery

Normal use needs no path argument. The source is resolved in this order:

1. `--master PATH` (retained for command compatibility)
2. `OBSIDIAN_VAULT_SOURCE`
3. `OBSIDIAN_MASTER_VAULT` (legacy environment-variable compatibility)
4. `~/.config/obsidian-vault`
5. `~/Work/client/obsidian` (legacy source-library fallback)

The Dropbox repository root is resolved in this order:

1. `--repos-root PATH`
2. `VAULT_REPOS_ROOT`
3. `~/Dropbox/Repos`

## Build

Client releases should be built statically for Linux:

```bash
rustup target add x86_64-unknown-linux-musl
cargo test -p vault-tools
cargo clippy -p vault-tools --all-targets -- -D warnings
cargo build --release -p vault-tools --target x86_64-unknown-linux-musl
```

The binaries are:

```text
target/x86_64-unknown-linux-musl/release/init-project-vault
target/x86_64-unknown-linux-musl/release/update-project-vault
target/x86_64-unknown-linux-musl/release/update-master-vault
```

## Current defaults and optional overrides

Run from an existing vault root containing `.obsidian/`:

```sh
init-project-vault
update-project-vault
update-master-vault
```

The master defaults to `~/.config/obsidian-vault`; backup repositories default
to `~/Dropbox/Repos`. Existing explicit master/repository environment variables
remain supported. Git operations use `~/.local/bin/git.py` through
`/usr/bin/python3`, or the explicit `VAULT_GIT_ADAPTER` path. No Git synchronization
runs automatically; initialization deliberately creates/pushes its bare backup.

Optional JSON configuration and Python-style keyword assignments:

```sh
update-project-vault config="/absolute/options.json"
update-project-vault master="/absolute/master" prune=False
init-project-vault master="/absolute/master" repos_root="/absolute/backups"
update-master-vault apply=True allow_sensitive=False
```

The JSON object accepts `master` and `repos_root` path strings or null. Relative
paths in the file resolve against that file's directory. Inline values override
the file; existing `--config FILE`, `--master PATH`, `--repos-root PATH` and
operation flags remain supported. `config=None` means no config file. Quotes
around a keyword value and Python `True`/`False` are supported, but no Python
code is evaluated and tilde/environment interpolation is not performed by the
commands. Quote paths with spaces in the invoking shell.

An explicitly requested missing/malformed config is an error. Operational
switches are never loaded from configuration: pruning and reverse writes must
be requested on the command line. Reverse propagation remains a dry run by
default, requires a clean Git source for `apply=True`, and rejects sensitive
configuration unless explicitly permitted. Raw note bodies/frontmatter are not
rewritten by resource synchronization.

Server verification: `cargo test -p vault-tools`, strict Clippy, static musl
release build, then `python3 scripts/vault-tools-smoke.py /absolute/release /absolute/git.py`.
The smoke script creates only disposable vaults and backup repositories.
