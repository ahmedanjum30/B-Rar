//! B-Rar-GUI: the window behind B-Rar's Explorer menu (browser, progress, prompts, results;
//! no console).
//!
//! ```text
//! B-Rar-GUI <archive>               browse the archive
//! B-Rar-GUI x <archive>             extract to a folder named after the archive
//! B-Rar-GUI x <archive> --here      extract next to the archive
//! B-Rar-GUI x <archive> --ask       pick the destination folder first
//! B-Rar-GUI x <archive> -o <dir>    extract to <dir>
//! B-Rar-GUI t <archive>             test the archive
//! ```
//!
//! `x` also takes `--items <file>` (archive paths to extract, one per line; the file is
//! deleted after reading) and `--base <folder>` (write them relative to that archive folder).
#![windows_subsystem = "windows"]

mod app;
mod browse;
mod shell;
mod style;

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
    Extract { dest: PathBuf, selection: Option<Selection> },
    Test,
}

fn main() -> ExitCode {
    let password = env::var(PASSWORD_VAR).ok();
    // SAFETY: no other threads exist yet.
    unsafe { env::remove_var(PASSWORD_VAR) };
    shell::init_com();
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
        "usage:\n  B-Rar-GUI <archive>\n  B-Rar-GUI x <archive> [--here | --ask | -o <dir>] [--items <file> [--base <folder>]]\n  B-Rar-GUI t <archive>"
            .to_owned()
    };
    let command = args.next().ok_or_else(usage)?;
    let absolute = |p: OsString| {
        let p = PathBuf::from(p);
        std::path::absolute(&p).map_err(|e| format!("{}: {e}", p.display()))
    };
    if !matches!(command.to_str(), Some("x" | "t")) {
        // "Open with" passes just the archive.
        return Ok(browse::run(absolute(command)?, password));
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
                "--ask" => match shell::pick_folder(&archive)? {
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
    Ok(app::run(archive, mode, password))
}

fn read_items(file: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let _ = std::fs::remove_file(file);
    Ok(text.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect())
}
