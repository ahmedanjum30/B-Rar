//! brar-gui: the window behind brar's Explorer menu. No console, a native progress dialog.
//!
//! ```text
//! brar-gui x <archive>             extract to a folder named after the archive
//! brar-gui x <archive> --here      extract next to the archive
//! brar-gui x <archive> --ask       pick the destination folder first
//! brar-gui x <archive> -o <dir>    extract to <dir>
//! brar-gui t <archive>             test the archive
//! ```
#![windows_subsystem = "windows"]

mod app;
mod shell;
mod style;

use std::{
    env,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Clone)]
pub enum Mode {
    Extract { dest: PathBuf },
    Test,
}

fn main() -> ExitCode {
    shell::init_com();
    match run() {
        Ok(code) => code,
        Err(msg) => {
            shell::error_box("brar", &msg);
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let mut args = env::args_os().skip(1);
    let usage = || {
        "usage:\n  brar-gui x <archive> [--here | --ask | -o <dir>]\n  brar-gui t <archive>".to_owned()
    };
    let command = args.next().ok_or_else(usage)?;
    let archive = PathBuf::from(args.next().ok_or_else(usage)?);
    let archive = std::path::absolute(&archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let rest: Vec<_> = args.collect();

    let mode = match command.to_str() {
        Some("x") => Mode::Extract {
            dest: match rest.first().and_then(|a| a.to_str()) {
                None => brar_core::suggested_dir(&archive),
                Some("--here") => archive.parent().map(Path::to_path_buf).unwrap_or_default(),
                Some("--ask") => match shell::pick_folder(&archive)? {
                    Some(dir) => dir,
                    None => return Ok(ExitCode::SUCCESS),
                },
                Some("-o") => PathBuf::from(rest.get(1).ok_or_else(usage)?),
                Some(other) => return Err(format!("unknown option {other}\n\n{}", usage())),
            },
        },
        Some("t") => Mode::Test,
        _ => return Err(usage()),
    };
    Ok(app::run(archive, mode))
}
