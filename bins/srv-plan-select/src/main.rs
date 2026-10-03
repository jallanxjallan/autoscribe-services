use anyhow::{bail, Context, Result};
use clap::Parser;
use rusqlite::Connection;
use srv_common::load_policy;
use std::io::{self, Read, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "Select an AutoScribe plan")]
struct Args {
    #[arg(long, default_value = "/etc/autoscribe/services.toml")]
    policy: PathBuf,
}

#[derive(Debug)]
struct Plan {
    id: String,
    label: String,
}

struct RawMode {
    fd: libc::c_int,
    original: libc::termios,
}

impl RawMode {
    fn enter() -> Result<Self> {
        let fd = libc::STDIN_FILENO;
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error()).context("failed to read terminal mode");
        }

        let mut raw = unsafe { std::ptr::read(&original) };
        // We only need character-at-a-time keyboard input. Keep the terminal's
        // normal output processing intact so newlines still return to column 1.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;

        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("failed to enter raw terminal mode");
        }

        print!("\x1b[?25l");
        io::stdout().flush()?;

        Ok(Self { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
        print!("\x1b[?25h\x1b[0m");
        let _ = io::stdout().flush();
    }
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

    let mut stmt =
        conn.prepare("SELECT id, label FROM plans ORDER BY label COLLATE NOCASE, id")?;

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

fn render(plans: &[Plan], selected: usize) -> Result<()> {
    print!("\x1b[2J\x1b[H");
    println!("AutoScribe plans\n");

    let label_width = plans
        .iter()
        .map(|plan| clean_label(&plan.label).chars().count())
        .max()
        .unwrap_or(0);

    for (index, plan) in plans.iter().enumerate() {
        let label = clean_label(&plan.label);
        if index == selected {
            println!(
                "\x1b[7m> {:width$}  {}\x1b[0m",
                label,
                plan.id,
                width = label_width
            );
        } else {
            println!("  {:width$}  {}", label, plan.id, width = label_width);
        }
    }

    println!("\n↑/↓ select   Enter choose   Esc exit");
    io::stdout().flush()?;
    Ok(())
}

fn read_byte(stdin: &mut impl Read) -> Result<Option<u8>> {
    let mut byte = [0u8; 1];
    match stdin.read(&mut byte)? {
        0 => Ok(None),
        _ => Ok(Some(byte[0])),
    }
}

fn select_plan(plans: &[Plan]) -> Result<Option<usize>> {
    let _raw = RawMode::enter()?;
    let mut selected = 0usize;
    let mut stdin = io::stdin().lock();

    render(plans, selected)?;

    loop {
        let Some(byte) = read_byte(&mut stdin)? else {
            continue;
        };

        match byte {
            b'\r' | b'\n' => {
                print!("\x1b[2J\x1b[H");
                io::stdout().flush()?;
                return Ok(Some(selected));
            }
            0x1b => {
                let second = read_byte(&mut stdin)?;
                if second.is_none() {
                    print!("\x1b[2J\x1b[H");
                    io::stdout().flush()?;
                    return Ok(None);
                }
                if second == Some(b'[') {
                    match read_byte(&mut stdin)? {
                        Some(b'A') => {
                            selected = if selected == 0 {
                                plans.len() - 1
                            } else {
                                selected - 1
                            };
                            render(plans, selected)?;
                        }
                        Some(b'B') => {
                            selected = (selected + 1) % plans.len();
                            render(plans, selected)?;
                        }
                        _ => {}
                    }
                }
            }
            b'k' => {
                selected = if selected == 0 {
                    plans.len() - 1
                } else {
                    selected - 1
                };
                render(plans, selected)?;
            }
            b'j' => {
                selected = (selected + 1) % plans.len();
                render(plans, selected)?;
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let policy = load_policy(&args.policy)?;
    let plans = list_plans(&policy.paths.control_db)?;

    if plans.is_empty() {
        bail!("no plans found in {}", policy.paths.control_db.display());
    }

    let Some(index) = select_plan(&plans)? else {
        return Ok(());
    };

    let plan = &plans[index];
    println!("{}\t{}", clean_label(&plan.label), plan.id);
    Ok(())
}
