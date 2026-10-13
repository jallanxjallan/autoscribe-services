use std::{
    collections::{hash_map::DefaultHasher, BTreeMap, HashMap},
    fs,
    hash::{Hash, Hasher},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn settings(path: &Path) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    if !path.exists() {
        return Ok(values);
    }
    for (n, line) in fs::read_to_string(path)?.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(format!(
            "{}:{}: expected key=value",
            path.display(),
            n + 1
        ))?;
        if values
            .insert(key.trim().to_string(), value.trim().to_string())
            .is_some()
        {
            return Err(format!("duplicate key: {key}").into());
        }
    }
    Ok(values)
}
fn relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn project_name(value: &str) -> bool {
    relative(value) && !value.contains('/') && !value.starts_with('.') && value.len() <= 180
}
struct Project {
    entry: String,
    output: String,
    inputs: BTreeMap<String, String>,
}
fn project(path: &Path) -> Result<Project> {
    let mut cfg = settings(&path.join("typst.conf"))?;
    let entry = cfg.remove("entrypoint").unwrap_or("main.typ".into());
    let output = cfg.remove("output").unwrap_or("main.pdf".into());
    if !relative(&entry) || !entry.ends_with(".typ") {
        return Err("entrypoint must be a relative .typ path without traversal".into());
    }
    if !relative(&output) || output.contains('/') || !output.ends_with(".pdf") {
        return Err("output must be a PDF filename in the project root".into());
    }
    let mut inputs = BTreeMap::new();
    for (key, value) in cfg {
        let key = key
            .strip_prefix("input.")
            .filter(|k| !k.is_empty())
            .ok_or(format!("unknown project setting: {key}"))?;
        inputs.insert(key.to_string(), value);
    }
    if !path.join(&entry).is_file() {
        return Err(format!("missing entrypoint: {entry}").into());
    }
    Ok(Project {
        entry,
        output,
        inputs,
    })
}
fn fingerprint(root: &Path, output: &str) -> Result<String> {
    fn walk(root: &Path, path: &Path, output: &str, hash: &mut DefaultHasher) -> Result<()> {
        let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let rel = path.strip_prefix(root)?;
            if rel == Path::new(output) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err("project symlinks are not supported".into());
            }
            rel.hash(hash);
            if kind.is_dir() {
                walk(root, &path, output, hash)?;
            } else if kind.is_file() {
                entry.metadata()?.len().hash(hash);
                let mut file = fs::File::open(path)?;
                let mut buf = [0u8; 65536];
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hash.write(&buf[..n]);
                }
            } else {
                return Err("unsupported project file type".into());
            }
        }
        Ok(())
    }
    let mut hash = DefaultHasher::new();
    walk(root, root, output, &mut hash)?;
    Ok(format!("{:016x}", hash.finish()))
}
struct Service {
    remote: String,
    state: PathBuf,
    rclone: String,
    typst: String,
    poll: u64,
}
impl Service {
    fn load(path: &Path) -> Result<Self> {
        let mut cfg = settings(path)?;
        let remote = cfg.remove("remote").ok_or("missing remote")?;
        let state = PathBuf::from(cfg.remove("state_dir").ok_or("missing state_dir")?);
        let rclone = cfg.remove("rclone").ok_or("missing rclone")?;
        let typst = cfg.remove("typst").ok_or("missing typst")?;
        let poll = cfg
            .remove("poll_seconds")
            .unwrap_or("30".into())
            .parse::<u64>()?;
        if !cfg.is_empty()
            || poll < 5
            || !state.is_absolute()
            || !Path::new(&rclone).is_absolute()
            || !Path::new(&typst).is_absolute()
        {
            return Err("invalid service configuration".into());
        }
        fs::create_dir_all(&state)?;
        Ok(Self {
            remote,
            state,
            rclone,
            typst,
            poll,
        })
    }
    fn run(&self, executable: &str, args: &[String]) -> Result<String> {
        let result = Command::new("/usr/bin/timeout")
            .args(["--kill-after=10s", "180s", executable])
            .args(args)
            .output()?;
        if !result.status.success() {
            return Err(format!(
                "{} failed ({}): {}",
                executable,
                result.status,
                String::from_utf8_lossy(&result.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(result.stdout)?)
    }
    fn rclone(&self, args: Vec<String>) -> Result<String> {
        let mut args = args;
        args.extend([
            "--retries".into(),
            "2".into(),
            "--low-level-retries".into(),
            "2".into(),
            "--contimeout".into(),
            "15s".into(),
            "--timeout".into(),
            "60s".into(),
        ]);
        self.run(&self.rclone, &args)
    }
    fn sync(&self, remote: &str, local: &Path) -> Result<()> {
        self.rclone(vec![
            "sync".into(),
            remote.into(),
            local.display().to_string(),
            "--checksum".into(),
        ])?;
        Ok(())
    }
    fn process(&self, name: &str, seen: &mut HashMap<String, String>, once: bool) -> Result<()> {
        let remote = format!("{}/{name}", self.remote.trim_end_matches('/'));
        let base = self.state.join(name);
        let local = base.join("source");
        fs::create_dir_all(&local)?;
        self.sync(&remote, &local)?;
        let cfg = project(&local)?;
        let digest = fingerprint(&local, &cfg.output)?;
        let receipt = base.join("success");
        if fs::read_to_string(&receipt).unwrap_or_default() == digest
            && local.join(&cfg.output).is_file()
        {
            return Ok(());
        }
        let previous = seen.insert(name.to_string(), digest.clone());
        if !once && previous.as_ref() != Some(&digest) {
            return Ok(());
        }
        let pdf = base.join("compiled.pdf");
        if pdf.exists() {
            fs::remove_file(&pdf)?;
        }
        let mut args = vec![
            "compile".into(),
            "--root".into(),
            local.display().to_string(),
            "--ignore-system-fonts".into(),
            "--font-path".into(),
            local.display().to_string(),
        ];
        for (key, value) in cfg.inputs {
            args.extend(["--input".into(), format!("{key}={value}")]);
        }
        args.extend([
            local.join(&cfg.entry).display().to_string(),
            pdf.display().to_string(),
        ]);
        self.run(&self.typst, &args)?;
        let mut header = [0u8; 5];
        fs::File::open(&pdf)?.read_exact(&mut header)?;
        if &header != b"%PDF-" {
            return Err("compiler did not produce a PDF".into());
        }
        // Recheck Dropbox after compilation to avoid publishing a known stale snapshot.
        self.sync(&remote, &local)?;
        if fingerprint(&local, &cfg.output)? != digest {
            return Err("inputs changed during compilation; retrying next poll".into());
        }
        self.rclone(vec![
            "copyto".into(),
            pdf.display().to_string(),
            format!("{remote}/{}", cfg.output),
        ])?;
        // Record success only after Dropbox confirms the upload.
        fs::write(base.join("success.tmp"), &digest)?;
        fs::rename(base.join("success.tmp"), receipt)?;
        println!(
            "compiled {name}/{} -> {name}/{} ({digest})",
            cfg.entry, cfg.output
        );
        Ok(())
    }
    fn tick(&self, seen: &mut HashMap<String, String>, once: bool) -> Result<()> {
        let folders = self.rclone(vec![
            "lsf".into(),
            self.remote.clone(),
            "--dirs-only".into(),
        ])?;
        let mut failed = false;
        for folder in folders.lines() {
            let name = folder.trim_end_matches('/');
            if !project_name(name) {
                eprintln!("skipping unsupported project folder {name:?}");
                continue;
            }
            if let Err(error) = self.process(name, seen, once) {
                eprintln!("project {name}: {error}");
                failed = true;
            }
        }
        if once && failed {
            return Err("one or more projects failed".into());
        }
        Ok(())
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 1 || args.len() > 2 || (args.len() == 2 && args[1] != "--once") {
        return Err("usage: srv-typst /absolute/service.conf [--once] (run under flock)".into());
    }
    let service = Service::load(Path::new(&args[0]))?;
    let once = args.len() == 2;
    let mut seen = HashMap::new();
    loop {
        let result = service.tick(&mut seen, once);
        if once {
            return result;
        }
        if let Err(error) = result {
            eprintln!("poll: {error}");
        }
        thread::sleep(Duration::from_secs(service.poll));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_path_escape() {
        for value in [
            "../other.typ",
            "/tmp/main.typ",
            "a/../../main.typ",
            "",
            "a\\b",
            "bad\nname",
        ] {
            assert!(!relative(value), "{value}");
        }
        assert!(relative("chapters/main.typ"));
        assert!(project_name("Sample project-01"));
        assert!(!project_name("../other"));
    }
    #[test]
    fn contract_and_fingerprint() -> Result<()> {
        let root = std::env::temp_dir().join(format!("srv-typst-test-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        fs::write(root.join("main.typ"), "hello")?;
        let cfg = project(&root)?;
        assert_eq!(cfg.output, "main.pdf");
        let first = fingerprint(&root, &cfg.output)?;
        fs::write(root.join("main.pdf"), "%PDF-output")?;
        assert_eq!(first, fingerprint(&root, &cfg.output)?);
        fs::write(root.join("image.svg"), "asset")?;
        assert_ne!(first, fingerprint(&root, &cfg.output)?);
        fs::write(root.join("typst.conf"), "output=../escape.pdf")?;
        assert!(project(&root).is_err());
        fs::write(
            root.join("typst.conf"),
            "output=report.pdf\ninput.title=Sample",
        )?;
        assert_eq!(project(&root)?.inputs["title"], "Sample");
        fs::write(root.join("typst.conf"), "output=report.pdf\nunknown=true")?;
        assert!(project(&root).is_err());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
