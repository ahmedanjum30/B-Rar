//! B-Rar-GUI: the window behind B-Rar's Explorer menu (browser, progress, prompts, results;
//! no console).
//!
//! ```text
//! B-Rar-GUI                         the home page ("Open archive...")
//! B-Rar-GUI <archive>               browse the archive
//! B-Rar-GUI x <archive>             extract to a folder named after the archive
//! B-Rar-GUI x <archive> --here      extract next to the archive
//! B-Rar-GUI x <archive> --ask       pick the destination folder first
//! B-Rar-GUI x <archive> -o <dir>    extract to <dir>
//! B-Rar-GUI t <archive>             test the archive
//! B-Rar-GUI a <paths>...            compress files and folders to "name.zip" next to them
//! B-Rar-GUI a --ask <paths>...      the same, after the "Add to archive" page
//! ```
//!
//! `a` from the installer's right-click menu runs once per selected item; those runs hand
//! their paths to the first, which opens one window for all of them ([`gather`]). The Store
//! version's menu passes them all at once, with `--all`.
//!
//! `x` also takes `--items <file>` (archive paths to extract, one per line; the file is
//! deleted after reading) and `--base <folder>` (write them relative to that archive folder).
#![windows_subsystem = "windows"]

mod app;
mod browse;
mod shell;
mod style;
mod update;

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::ExitCode,
};

use brar_core::Selection;

/// The browser hands its password to the extraction it starts through this variable.
pub const PASSWORD_VAR: &str = "BRAR_PASSWORD";

#[derive(Clone)]
pub enum Mode {
    Extract {
        dest: PathBuf,
        selection: Option<Selection>,
    },
    Test,
    /// Create the archive from these files and folders; `ask`: the "Add to archive" page
    /// first.
    Create {
        inputs: Vec<PathBuf>,
        level: u32,
        password: Option<String>,
        overwrite: bool,
        ask: bool,
    },
}

fn main() -> ExitCode {
    let password = env::var(PASSWORD_VAR).ok();
    // SAFETY: no other threads exist yet.
    unsafe { env::remove_var(PASSWORD_VAR) };
    shell::init_com();
    update::clean_up();
    match run(password) {
        Ok(code) => code,
        Err(msg) => {
            shell::error_box("B-Rar", &msg);
            ExitCode::FAILURE
        }
    }
}

fn run(password: Option<String>) -> Result<ExitCode, String> {
    let mut args = env::args_os().skip(1);
    let usage = || {
        "usage:\n  B-Rar-GUI <archive>\n  B-Rar-GUI x <archive> [--here | --ask | -o <dir>] [--items <file> [--base <folder>]]\n  B-Rar-GUI t <archive>\n  B-Rar-GUI a [--ask] <paths>..."
            .to_owned()
    };
    // From the Start menu: the browser's home page.
    let Some(command) = args.next() else {
        return Ok(browse::run(None, password));
    };
    let absolute = |p: OsString| {
        let p = PathBuf::from(p);
        std::path::absolute(&p).map_err(|e| format!("{}: {e}", p.display()))
    };
    if command == "a" {
        let (mut ask, mut all, mut inputs) = (false, false, Vec::new());
        for arg in args {
            match arg.to_str() {
                Some("--ask") => ask = true,
                Some("--all") => all = true,
                _ => inputs.push(absolute(arg)?),
            }
        }
        if inputs.is_empty() {
            return Err(usage());
        }
        if !all {
            match gather(inputs, ask) {
                Some(everything) => inputs = everything,
                // Handed over to the run that opens the window.
                None => return Ok(ExitCode::SUCCESS),
            }
        }
        let archive = archive_for(&inputs);
        let mode = Mode::Create {
            inputs,
            level: 6,
            password: None,
            overwrite: false,
            ask,
        };
        return Ok(app::run(archive, mode, None));
    }
    if !matches!(command.to_str(), Some("x" | "t")) {
        // "Open with" passes just the archive.
        return Ok(browse::run(Some(absolute(command)?), password));
    }
    let archive = absolute(args.next().ok_or_else(usage)?)?;
    let mut rest = args.map(|a| a.to_string_lossy().into_owned());

    let mode = if command == "t" {
        Mode::Test
    } else {
        let (mut dest, mut items, mut base) = (None, None, String::new());
        while let Some(arg) = rest.next() {
            let mut value = || rest.next().ok_or_else(usage);
            match arg.as_str() {
                "--here" => dest = Some(archive.parent().map(Path::to_path_buf).unwrap_or_default()),
                "--ask" => match shell::pick_folder(&archive, 0)? {
                    Some(dir) => dest = Some(dir),
                    None => return Ok(ExitCode::SUCCESS),
                },
                "-o" => dest = Some(PathBuf::from(value()?)),
                "--items" => items = Some(read_items(Path::new(&value()?))?),
                "--base" => base = value()?,
                other => return Err(format!("unknown option {other}\n\n{}", usage())),
            }
        }
        Mode::Extract {
            dest: dest.unwrap_or_else(|| brar_core::suggested_dir(&archive)),
            selection: items.map(|items| Selection { items, base }),
        }
    };
    // A folder only administrators can write to (Program Files, another user's folder):
    // hand the job to an elevated copy straight away, before anything is extracted.
    if let Mode::Extract { dest, .. } = &mode
        && needs_admin(dest)
    {
        return match elevate(&archive, &mode)? {
            true => Ok(ExitCode::SUCCESS),
            false => Err(declined(dest)),
        };
    }
    Ok(app::run(archive, mode, password))
}

/// The archive "Add to name.zip" makes for `inputs`: one item's name, or for several their folder's
/// name, with .zip, next to them; " (2)" and so on if that's taken.
fn archive_for(inputs: &[PathBuf]) -> PathBuf {
    let first = &inputs[0];
    let dir = first.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = match inputs {
        [one] if one.is_dir() => one.file_name().map(|n| n.to_string_lossy().into_owned()),
        [one] => one.file_stem().map(|n| n.to_string_lossy().into_owned()),
        _ => dir.file_name().map(|n| n.to_string_lossy().into_owned()),
    }
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "Archive".to_owned());
    (1..)
        .map(|n| match n {
            1 => dir.join(format!("{stem}.zip")),
            n => dir.join(format!("{stem} ({n}).zip")),
        })
        .find(|p| !p.exists())
        .expect("a free name")
}

/// Explorer runs the installer's "Add to archive" entries once per selected item. The first
/// run takes a lock and collects the paths the others append to a queue file, until none has
/// come for a moment; the others append theirs and return `None`.
fn gather(mine: Vec<PathBuf>, ask: bool) -> Option<Vec<PathBuf>> {
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    let dir = env::temp_dir().join("B-Rar");
    if std::fs::create_dir_all(&dir).is_err() {
        return Some(mine);
    }
    let key = if ask { "add-ask" } else { "add" };
    let (lock, queue) = (dir.join(format!("{key}.lock")), dir.join(format!("{key}.queue")));
    // A lock left by a run that died is taken over.
    if std::fs::metadata(&lock)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_secs(30)))
    {
        let _ = std::fs::remove_file(&lock);
    }
    if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
        let text: String = mine.iter().map(|p| format!("{}\n", p.display())).collect();
        let appended = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&queue)
            .and_then(|mut f| f.write_all(text.as_bytes()));
        return match appended {
            Ok(()) => None,
            Err(_) => Some(mine),
        };
    }
    let mut all = mine;
    let (mut seen, mut quiet) = (u64::MAX, Instant::now());
    while quiet.elapsed() < Duration::from_millis(300) {
        std::thread::sleep(Duration::from_millis(30));
        let len = std::fs::metadata(&queue).map_or(0, |m| m.len());
        if len != seen {
            (seen, quiet) = (len, Instant::now());
        }
    }
    if let Ok(text) = std::fs::read_to_string(&queue) {
        all.extend(text.lines().filter(|l| !l.is_empty()).map(PathBuf::from));
    }
    let _ = std::fs::remove_file(&queue);
    let _ = std::fs::remove_file(&lock);
    all.dedup();
    Some(all)
}

/// Can this process write to `dest`? Only "access denied" counts: anything else is left for
/// the extraction to report.
fn needs_admin(dest: &Path) -> bool {
    if shell::is_elevated() {
        return false;
    }
    let denied = |e: &std::io::Error| e.kind() == std::io::ErrorKind::PermissionDenied;
    if let Err(e) = std::fs::create_dir_all(dest) {
        return denied(&e);
    }
    let probe = dest.join(format!(".B-Rar-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            false
        }
        Err(e) => denied(&e),
    }
}

/// Restarts this job with administrator rights (Windows asks first). `false` if the user
/// declined. The elevated copy asks for a password again: Windows doesn't pass this
/// process's environment to it.
pub fn elevate(archive: &Path, mode: &Mode) -> Result<bool, String> {
    let Mode::Extract { dest, selection } = mode else {
        return Err("only extraction can be elevated".into());
    };
    let mut args: Vec<OsString> = vec!["x".into(), archive.into(), "-o".into(), dest.into()];
    if let Some(s) = selection {
        let dir = env::temp_dir().join("B-Rar");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = dir.join(format!("elevate-{}.txt", std::process::id()));
        std::fs::write(&file, s.items.join("\n")).map_err(|e| format!("{}: {e}", file.display()))?;
        args.extend(["--items".into(), file.into(), "--base".into(), s.base.clone().into()]);
    }
    shell::run_elevated(&args)
}

pub fn declined(dest: &Path) -> String {
    format!(
        "B-Rar needs administrator permission to extract into\n{}\n\nChoose another folder, or allow the Windows prompt.",
        dest.display()
    )
}

fn read_items(file: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let _ = std::fs::remove_file(file);
    Ok(text.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect())
}
