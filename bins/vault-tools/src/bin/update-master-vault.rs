use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use vault_tools::{
    ensure_vault_root, print_changes, propagate_vault_to_master, resolve_master,
};

#[derive(Parser, Debug)]
#[command(about = "Review or propagate reusable vault configuration back to the canonical source library")]
struct Args {
    #[arg(long)]
    master: Option<PathBuf>,

    /// Actually write the reviewed changes. Without this flag the command is a dry run.
    #[arg(long)]
    apply: bool,

    /// Permit copying files whose configuration appears to contain credentials or tokens.
    #[arg(long)]
    allow_sensitive: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let source = std::env::current_dir()?;
    ensure_vault_root(&source)?;

    let master = resolve_master(args.master)?;
    let changes =
        propagate_vault_to_master(&source, &master, args.apply, args.allow_sensitive)?;
    print_changes(
        if args.apply {
            "master changes applied"
        } else {
            "master changes proposed (dry run)"
        },
        &changes,
    );

    if !args.apply && !changes.is_empty() {
        println!("No files were changed. Re-run with --apply after reviewing the list.");
    }

    Ok(())
}
