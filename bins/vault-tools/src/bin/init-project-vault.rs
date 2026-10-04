use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use vault_tools::{
    ensure_vault_root, initialize_backup_repo, print_changes, resolve_master, resolve_repos_root,
    sync_master_to_vault,
};

#[derive(Parser, Debug)]
#[command(about = "Configure an existing Obsidian vault and attach its Dropbox bare backup repo")]
struct Args {
    #[arg(long)]
    master: Option<PathBuf>,

    #[arg(long)]
    repos_root: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let vault = std::env::current_dir()?;
    ensure_vault_root(&vault)?;

    let master = resolve_master(args.master)?;
    let changes = sync_master_to_vault(&master, &vault, false)?;
    print_changes("vault configuration", &changes);

    let repos_root = resolve_repos_root(args.repos_root)?;
    let report = initialize_backup_repo(&vault, &repos_root)?;

    println!("vault:  {}", vault.display());
    println!("remote: {}", report.remote.display());
    println!("branch: {}", report.branch);
    if report.initialized {
        println!("git:    initialized and pushed");
    } else {
        println!("git:    existing repository retained");
    }

    Ok(())
}
