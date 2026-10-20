use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Instant,
};

use anyhow::Context;
use brar_core::{Conflict, ConflictPolicy, Decision, Error, Options, Resolution, Summary};
use clap::{Args, Parser, Subcommand};

/// brar: a fast, parallel archive extractor (zip, 7z, rar, tar, gz, xz, zst, bz2).
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract an archive.
    #[command(visible_alias = "x")]
    Extract {
        archive: PathBuf,
        /// Output directory [default: a folder named after the archive].
        #[arg(short, long, conflicts_with = "here")]
        output: Option<PathBuf>,
        /// Extract into the folder that contains the archive.
        #[arg(long)]
        here: bool,
        /// Overwrite existing files without asking.
        #[arg(short = 'y', long = "yes", group = "conflict")]
        overwrite: bool,
        /// Keep existing files without asking.
        #[arg(long, group = "conflict")]
        skip_existing: bool,
        /// Extract under a new name ("file (1).txt") when a file exists.
        #[arg(long, group = "conflict")]
        rename_existing: bool,
        #[command(flatten)]
        common: Common,
    },
    /// List an archive's contents.
    #[command(visible_alias = "l")]
    List {
        archive: PathBuf,
        #[command(flatten)]
        common: Common,
    },
    /// Decode everything and verify checksums without writing files.
    #[command(visible_alias = "t")]
    Test {
        archive: PathBuf,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Args)]
struct Common {
    /// Worker threads [default: all logical CPUs].
    #[arg(short, long, default_value_t = 0)]
    threads: usize,
    /// Password for encrypted archives [default: ask when needed].
    #[arg(short, long)]
    password: Option<String>,
    /// On failure, keep the console open until Enter is pressed.
    #[arg(long)]
    pause_on_error: bool,
    /// Always keep the console open until Enter is pressed.
    #[arg(long)]
    pause: bool,
}

impl Common {
    fn options(&self) -> Options {
        Options {
            threads: self.threads,
            password: self.password.clone(),
            ..Options::default()
        }
    }
}

impl Command {
    fn common(&self) -> &Common {
        match self {
            Command::Extract { common, .. } | Command::List { common, .. } | Command::Test { common, .. } => common,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let common = cli.command.common();
    let (pause, pause_on_error) = (common.pause, common.pause_on_error);
    let result = run(cli);
    if let Err(e) = &result {
        eprintln!("error: {e:#}");
    }
    if pause || (pause_on_error && result.is_err()) {
        eprintln!("\nPress Enter to close...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Extract { archive, output, here, overwrite, skip_existing, rename_existing, common } => {
            let dest = match (output, here) {
                (Some(dir), _) => dir,
                (None, true) => archive_dir(&archive),
                (None, false) => brar_core::suggested_dir(&archive),
            };
            let mut opts = common.options();
            opts.on_conflict = match (overwrite, skip_existing, rename_existing) {
                (true, _, _) => ConflictPolicy::Always(Resolution::Overwrite),
                (_, true, _) => ConflictPolicy::Always(Resolution::Skip),
                (_, _, true) => ConflictPolicy::Always(Resolution::Rename),
                // Scripts can't answer; never replace files behind their back.
                _ if !std::io::stdin().is_terminal() => ConflictPolicy::Always(Resolution::Skip),
                _ => ConflictPolicy::Ask(Arc::new(ask_conflict)),
            };
            let started = Instant::now();
            let summary = with_password(&archive, opts, |o| brar_core::extract(&archive, &dest, o))
                .with_context(|| format!("extracting {}", archive.display()))?;
            report("Extracted", &summary, started);
            println!("  -> {}", dest.display());
        }
        Command::Test { archive, common } => {
            let started = Instant::now();
            let summary = with_password(&archive, common.options(), |o| brar_core::test(&archive, o))
                .with_context(|| format!("testing {}", archive.display()))?;
            report("OK", &summary, started);
        }
        Command::List { archive, common } => {
            let entries = with_password(&archive, common.options(), |o| brar_core::list(&archive, o))
                .with_context(|| format!("listing {}", archive.display()))?;
            let (mut files, mut total) = (0u64, 0u64);
            println!("{:>12}  {:>12}  name", "size", "packed");
            for e in &entries {
                let packed = e.packed.map(human).unwrap_or_default();
                let size = if e.is_dir { "<dir>".to_owned() } else { human(e.size) };
                println!("{size:>12}  {packed:>12}  {}", e.name);
                if !e.is_dir {
                    files += 1;
                    total += e.size;
                }
            }
            println!("{files} files, {}", human(total));
        }
    }
    Ok(())
}

/// Runs `op`, asking for a password (hidden input) when the archive needs one.
fn with_password<T>(
    archive: &Path,
    mut opts: Options,
    op: impl Fn(&Options) -> brar_core::Result<T>,
) -> brar_core::Result<T> {
    let mut attempts = 0;
    loop {
        match op(&opts) {
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) if std::io::stdin().is_terminal() => {
                attempts += 1;
                if attempts > 3 {
                    return Err(e);
                }
                if matches!(e, Error::WrongPassword) {
                    eprintln!("Wrong password.");
                }
                let name = archive.file_name().unwrap_or_default().to_string_lossy();
                let password = rpassword::prompt_password(format!("Password for {name}: "))?;
                if password.is_empty() {
                    return Err(Error::Cancelled);
                }
                opts.password = Some(password);
            }
            other => return other,
        }
    }
}

fn ask_conflict(c: &Conflict<'_>) -> Decision {
    let existing = std::fs::metadata(c.path).map(|m| human(m.len())).unwrap_or_default();
    let new = c.new_size.map(human).unwrap_or_else(|| "?".into());
    loop {
        eprint!(
            "\n{} already exists ({existing}; new: {new}).\n[y]es, [n]o, [a]ll, [s]kip all, [r]ename, [q]uit: ",
            c.path.display()
        );
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return Decision { resolution: Resolution::Cancel, apply_to_all: false };
        }
        let (resolution, apply_to_all) = match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => (Resolution::Overwrite, false),
            "n" | "no" => (Resolution::Skip, false),
            "a" | "all" => (Resolution::Overwrite, true),
            "s" => (Resolution::Skip, true),
            "r" => (Resolution::Rename, false),
            "q" => (Resolution::Cancel, false),
            _ => continue,
        };
        return Decision { resolution, apply_to_all };
    }
}

fn archive_dir(archive: &Path) -> PathBuf {
    match archive.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn report(verb: &str, s: &Summary, started: Instant) {
    let secs = started.elapsed().as_secs_f64();
    let rate = s.bytes as f64 / secs.max(1e-9);
    print!(
        "{verb} {} files, {} dirs, {} in {secs:.2}s ({}/s)",
        s.files,
        s.dirs,
        human(s.bytes),
        human(rate as u64)
    );
    if s.kept_existing > 0 {
        print!(", kept {} existing", s.kept_existing);
    }
    if s.skipped > 0 {
        print!(", skipped {} links/special entries", s.skipped);
    }
    println!();
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}
