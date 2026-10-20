//! Thin wrappers over the Windows shell: folder picker, message box, opening Explorer.

use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use windows::{
    Win32::{
        Foundation::ERROR_CANCELLED,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
            CoInitializeEx, CoTaskMemFree,
        },
        UI::{
            Shell::{
                FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
                IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH, ShellExecuteW,
            },
            WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW, SW_SHOWNORMAL},
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

pub fn open_folder(path: &Path) {
    let path = wide(path);
    // SAFETY: NUL-terminated string that outlives the call.
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// The standard Windows folder picker, opened in the archive's folder. `None` if cancelled.
pub fn pick_folder(archive: &Path) -> Result<Option<PathBuf>, String> {
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
        match dialog.Show(None) {
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
