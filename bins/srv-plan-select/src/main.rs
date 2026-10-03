use anyhow::{bail, Context, Result};
use clap::Parser;
use rusqlite::Connection;
use srv_common::load_policy;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "Select an AutoScribe plan and copy its human label plus identity via OSC 52")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,
}

#[derive(Debug)]
struct Plan {
    id: String,
    label: String,
}

fn clean_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn list_plans(db_path: &PathBuf) -> Result<Vec<Plan>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open control DB {}", db_path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT id, label FROM plans ORDER BY label COLLATE NOCASE, id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(Plan {
            id: row.get(0)?,
            label: row.get(1)?,
        })
    })?;

    let mut plans = Vec::new();
    for row in rows {
        let plan = row?;
        if !plan.id.starts_with("pln_") {
            bail!("control DB contains invalid plan identity: {}", plan.id);
        }
        plans.push(plan);
    }
    Ok(plans)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0usize;

    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16)
            | ((bytes[i + 1] as u32) << 8)
            | bytes[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }

    match bytes.len() - i {
        1 => {
            let n = (bytes[i] as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }

    out
}

fn copy_osc52(text: &str) -> Result<()> {
    let encoded = base64(text.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]52;c;{}\x07", encoded)?;
    stdout.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let plans = list_plans(&policy.paths.control_db)?;

    if plans.is_empty() {
        bail!("no plans found in {}", policy.paths.control_db.display());
    }

    println!("AutoScribe plans:\n");
    for (index, plan) in plans.iter().enumerate() {
        println!(
            "{:>3}. {}\t{}",
            index + 1,
            clean_label(&plan.label),
            plan.id
        );
    }
    println!("  q. Bail");

    loop {
        print!("\nSelect plan: ");
        io::stdout().flush()?;

        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            return Ok(());
        }
        let choice = input.trim();

        if choice.eq_ignore_ascii_case("q") || choice.eq_ignore_ascii_case("quit") {
            return Ok(());
        }

        let index: usize = match choice.parse::<usize>() {
            Ok(value) if value >= 1 && value <= plans.len() => value - 1,
            _ => {
                eprintln!("Choose 1-{} or q.", plans.len());
                continue;
            }
        };

        let plan = &plans[index];
        let paste = format!("{}\t{}", clean_label(&plan.label), plan.id);

        copy_osc52(&paste)?;
        println!("\n{}", paste);
        println!("Sent to terminal clipboard via OSC 52.");
        return Ok(());
    }
}
