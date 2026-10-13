use anyhow::Result;
use clap::Parser;

use vault_tools::{ensure_vault_root, print_changes, propagate_vault_to_master, resolve_master};

#[derive(Parser, Debug)]
#[command(
    about = "Review or propagate reusable vault configuration back to the canonical source library"
)]
struct Args {
    #[command(flatten)]
    resources: vault_tools::ResourceOptions,

    #[command(flatten)]
    authoring: vault_tools::AuthoringOptions,

    /// Actually write the reviewed changes. Without this flag the command is a dry run.
    #[arg(long, num_args=0..=1, default_missing_value="true", action=clap::ArgAction::Set)]
    apply: Option<bool>,

    /// Permit copying files whose configuration appears to contain credentials or tokens.
    #[arg(long, num_args=0..=1, default_missing_value="true", action=clap::ArgAction::Set)]
    allow_sensitive: Option<bool>,
}

fn main() -> Result<()> {
    let args = Args::parse_from(vault_tools::keyword_args(std::env::args_os()));
    let source = std::env::current_dir()?;
    ensure_vault_root(&source)?;

    vault_tools::configure_authoring(&source, &args.authoring, false)?;
    let config = vault_tools::load_resource_config(args.resources, None)?;
    let master = resolve_master(config.master)?;
    if args.apply.unwrap_or(false) {
        vault_tools::ensure_git_clean_for_path(&master)?;
        if master.join("config/authoring.json").exists() {
            let root = args
                .authoring
                .templates_root
                .clone()
                .or_else(|| std::env::var_os("TEMPLATES_ROOT").map(std::path::PathBuf::from))
                .ok_or_else(|| anyhow::anyhow!("missing templates root"))?;
            vault_tools::ensure_git_clean_for_path(&root)?;
        }
    }
    let template_changes = vault_tools::propagate_shared_templates(
        &source,
        &master,
        args.apply.unwrap_or(false),
        args.allow_sensitive.unwrap_or(false),
    )?;
    print_changes("shared authoring templates", &template_changes);
    let changes = propagate_vault_to_master(
        &source,
        &master,
        args.apply.unwrap_or(false),
        args.allow_sensitive.unwrap_or(false),
    )?;
    print_changes(
        if args.apply.unwrap_or(false) {
            "master changes applied"
        } else {
            "master changes proposed (dry run)"
        },
        &changes,
    );

    if !args.apply.unwrap_or(false) && !changes.is_empty() {
        println!("No files were changed. Re-run with --apply after reviewing the list.");
    }

    Ok(())
}
