# Vault tools

Three Rust binaries manage one generic Obsidian vault model. There are no vault
profiles or vault types.

## Commands

### `init-project-vault`

Run from the root of an existing Obsidian-created vault.

It:

- copies the managed editing configuration from the master vault;
- initializes Git on `main` if the vault is not already a repository;
- maintains a small volatile-state block in `.gitignore`;
- creates a bare backup repository at
  `~/Dropbox/Repos/<snake_case_of_vault_folder>.git`;
- configures that bare repository as `origin`;
- for a new repository, makes the initial commit and pushes it.

It refuses to initialize a vault nested inside another Git repository and refuses
to connect a new local repository to a pre-existing bare backup.

### `update-project-vault`

Copies managed editing configuration from the master into the current vault.
It is additive/update-only by default. `--prune` explicitly removes managed
target files that no longer exist in the master.

### `update-master-vault`

Compares the current vault's managed editing surface with the master. It is a
dry run by default. Re-run with `--apply` to copy reviewed additions and
updates into the master.

Reverse propagation never deletes master files. When applying changes, the
master must be the root of a clean Git repository. Likely credentials in JSON
configuration are rejected unless `--allow-sensitive` is explicitly supplied.

## Master vault discovery

Normal use needs no path argument. The master is resolved in this order:

1. `--master PATH`
2. `OBSIDIAN_MASTER_VAULT`
3. `~/Studio/Obsidian`
4. `~/Work/Obsidian` (legacy fallback)

The Dropbox repository root is resolved in this order:

1. `--repos-root PATH`
2. `VAULT_REPOS_ROOT`
3. `~/Dropbox/Repos`

## Managed surface

The tools deliberately distinguish editing machinery from vault content.

Managed top-level Obsidian configuration:

- `.obsidian/app.json`
- `.obsidian/appearance.json`
- `.obsidian/core-plugins.json`
- `.obsidian/community-plugins.json`
- `.obsidian/hotkeys.json`
- `.obsidian/templates.json`

Managed trees when present:

- `.obsidian/plugins/`
- `.obsidian/snippets/`
- `.obsidian/themes/`
- `_templates/`
- `_scripts/`
- `_tools/`
- `_ui/`
- `_views/`

The tools never synchronize ordinary notes. They also exclude workspace/session
state, caches, nested Git repositories, `node_modules`, and Python bytecode
caches. Managed symlinks are rejected.

## Build

From the services workspace:

```bash
cargo test -p vault-tools
cargo build --release -p vault-tools
```

The client binaries are then:

```text
target/release/init-project-vault
target/release/update-project-vault
target/release/update-master-vault
```
