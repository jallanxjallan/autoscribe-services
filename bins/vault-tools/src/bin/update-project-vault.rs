use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use vault_tools::{ensure_vault_root, print_changes, resolve_master, sync_master_to_vault};

#[derive(Parser, Debug)]
#[command(about = "Update a vault from the canonical Obsidian source library")]
struct Args {
    #[arg(long)]
    master: Option<PathBuf>,

    /// Remove stale files only from explicitly managed editing trees.
    #[arg(long)]
    prune: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let vault = std::env::current_dir()?;
    ensure_vault_root(&vault)?;

    let master = resolve_master(args.master)?;
    let changes = sync_master_to_vault(&master, &vault, args.prune)?;
    print_changes("vault configuration", &changes);

    Ok(())
}
