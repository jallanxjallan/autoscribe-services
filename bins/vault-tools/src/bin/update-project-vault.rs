use anyhow::Result;
use clap::Parser;

use vault_tools::{ensure_vault_root, print_changes, resolve_master, sync_master_to_vault};

#[derive(Parser, Debug)]
#[command(about = "Update a vault from the canonical Obsidian source library")]
struct Args {
    #[command(flatten)]
    resources: vault_tools::ResourceOptions,

    /// Remove stale files only from explicitly managed editing trees.
    #[arg(long, num_args=0..=1, default_missing_value="true", action=clap::ArgAction::Set)]
    prune: Option<bool>,
}

fn main() -> Result<()> {
    let args = Args::parse_from(vault_tools::keyword_args(std::env::args_os()));
    let vault = std::env::current_dir()?;
    ensure_vault_root(&vault)?;

    let config = vault_tools::load_resource_config(args.resources, None)?;
    let master = resolve_master(config.master)?;
    let changes = sync_master_to_vault(&master, &vault, args.prune.unwrap_or(false))?;
    print_changes("vault configuration", &changes);

    Ok(())
}
