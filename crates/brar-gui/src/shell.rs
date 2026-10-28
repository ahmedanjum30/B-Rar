//! Thin wrappers over the Windows shell: file and folder pickers, message box, opening Explorer.

use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use windows::{
    Win32::{
        Foundation::{ERROR_CANCELLED, HWND},
        System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoTaskMemFree},
        UI::{
            Shell::{
                Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
                IFileOpenDialog, IShellItem, IsUserAnAdmin, SEE_MASK_NOASYNC, SHCreateItemFromParsingName, SHELLEXECUTEINFOW, SIGDN_FILESYSPATH,
                ShellExecuteExW, ShellExecuteW,
            },
            WindowsAndMessaging::{AllowSetForegroundWindow, GetForegroundWindow, MB_ICONERROR, MB_OK, MessageBoxW, SW_SHOWNORMAL},
        },
    },
    core::{PCWSTR, w},
};

/// NUL-terminated UTF-16 for Win32 calls.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn init_com() {
    // SAFETY: called once on the UI thread before any COM use.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
}

pub fn error_box(title: &str, message: &str) {
    let (title, message) = (wide(title), wide(message));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(None, PCWSTR(message.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
    }
}

/// Opens a folder in Explorer, or a file in its default app.
pub fn open_path(path: &Path) {
    let path = wide(path);
    // SAFETY: NUL-terminated string that outlives the call.
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Opens the folder `path` is in, in Explorer, with `path` selected.
pub fn show_in_folder(path: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

/// Passes this window's right to take the foreground to process `pid`, so the window it opens
/// comes to the front instead of flashing on the taskbar.
pub fn allow_foreground(pid: u32) {
    // SAFETY: plain Win32 call; failure just means Windows keeps its default behavior.
    unsafe {
        let _ = AllowSetForegroundWindow(pid);
    }
}

/// Is this process running with administrator rights?
pub fn is_elevated() -> bool {
    // SAFETY: no arguments; reads the current process token.
    unsafe { IsUserAnAdmin().as_bool() }
}

/// Starts this program again with administrator rights; Windows shows its UAC prompt.
/// `Ok(false)` if the user said no.
pub fn run_elevated(args: &[OsString]) -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (exe, params) = (wide(&exe), wide(command_line(args)));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` and the strings it points to outlive the call.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(true),
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Ok(false),
        Err(e) => Err(format!("couldn't start B-Rar as administrator: {e}")),
    }
}

/// Joins arguments the way the C runtime splits them again: quotes around anything with
/// spaces, backslashes doubled only where they precede a quote.
fn command_line(args: &[OsString]) -> OsString {
    let mut line = String::new();
    for arg in args {
        let arg = arg.to_string_lossy();
        if !line.is_empty() {
            line.push(' ');
        }
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            line.push_str(&arg);
            continue;
        }
        line.push('"');
        let mut slashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                    line.push('"');
                    slashes = 0;
                }
                _ => {
                    line.extend(std::iter::repeat_n('\\', slashes));
                    line.push(c);
                    slashes = 0;
                }
            }
        }
        line.extend(std::iter::repeat_n('\\', slashes * 2));
        line.push('"');
    }
    line.into()
}

/// The standard Windows folder picker, opened in the archive's folder. `None` if cancelled.
/// `owner` is the window the picker opens over (and blocks); 0 means the window in front, which
/// is Explorer when it has just started B-Rar.
pub fn pick_folder(archive: &Path, owner: isize) -> Result<Option<PathBuf>, String> {
    let fail = |e: windows::core::Error| format!("folder picker failed: {e}");
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    let title = wide(format!("Extract {name} to"));
    // SAFETY: plain COM calls on an STA thread; every string passed is NUL-terminated and
    // outlives its call, and the returned path buffer is freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(fail)?;
        let options = dialog.GetOptions().map_err(fail)?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .map_err(fail)?;
        dialog.SetTitle(PCWSTR(title.as_ptr())).map_err(fail)?;
        dialog.SetOkButtonLabel(w!("Extract here")).map_err(fail)?;
        if let Some(dir) = archive.parent() {
            let dir = wide(dir);
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(dir.as_ptr()), None) {
                let _ = dialog.SetFolder(&item);
            }
        }
        let owner = if owner != 0 { HWND(owner as *mut _) } else { GetForegroundWindow() };
        match dialog.Show(Some(owner)) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => return Ok(None),
            Err(e) => return Err(fail(e)),
        }
        let item = dialog.GetResult().map_err(fail)?;
        let raw = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(fail)?;
        let path = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        Ok(Some(PathBuf::from(path.map_err(|e| e.to_string())?)))
    }
}

/// The standard Windows file picker for opening an archive, over window `owner`. `None` if
/// cancelled.
pub fn pick_archive(owner: isize) -> Result<Option<PathBuf>, String> {
    let fail = |e: windows::core::Error| format!("file picker failed: {e}");
    let archives = w!("*.zip;*.rar;*.7z;*.tar;*.gz;*.tgz;*.xz;*.txz;*.zst;*.tzst;*.bz2;*.tbz;*.tbz2;*.001;*.r00");
    let types = [
        COMDLG_FILTERSPEC {
            pszName: w!("Archives"),
            pszSpec: archives,
        },
        COMDLG_FILTERSPEC {
            pszName: w!("All files"),
            pszSpec: w!("*.*"),
        },
    ];
    // SAFETY: plain COM calls on an STA thread; the filter strings are static, and the
    // returned path buffer is freed with CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(fail)?;
        let options = dialog.GetOptions().map_err(fail)?;
        dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST).map_err(fail)?;
        dialog.SetFileTypes(&types).map_err(fail)?;
        dialog.SetTitle(w!("Open an archive")).map_err(fail)?;
        let owner = if owner != 0 { HWND(owner as *mut _) } else { GetForegroundWindow() };
        match dialog.Show(Some(owner)) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => return Ok(None),
            Err(e) => return Err(fail(e)),
        }
        let item = dialog.GetResult().map_err(fail)?;
        let raw = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(fail)?;
        let path = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        Ok(Some(PathBuf::from(path.map_err(|e| e.to_string())?)))
    }
}
