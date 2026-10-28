//! Explorer context-menu handler (`IExplorerCommand`) for B-Rar, registered by the MSIX
//! package for every file and folder. On Windows 11 this is what puts B-Rar in the short
//! right-click menu: the extract commands for archives, and "Add to archive" for anything.
//!
//! It runs inside Explorer, so it stays tiny: it only builds the menu and launches
//! `B-Rar-GUI.exe` (installed next to this DLL), and it never lets a panic cross into Explorer.

use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use windows::{
    Win32::{
        Foundation::{CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOTIMPL, HMODULE, S_FALSE, S_OK},
        System::{
            Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl},
            LibraryLoader::{
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW,
            },
        },
        UI::Shell::{
            ECF_DEFAULT, ECF_HASSUBCOMMANDS, ECS_ENABLED, ECS_HIDDEN, IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand,
            IExplorerCommand_Impl, IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
        },
    },
    core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement},
};

/// Must match the CLSID in the MSIX manifest; never change it after release.
pub const CLSID_BRAR: GUID = GUID::from_u128(0x91F265B3_8846_4B4F_9721_DA1F07BEC959);

/// The window app, installed next to this DLL.
const GUI_EXE: &str = "B-Rar-GUI.exe";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verb {
    Open,
    Ask,
    ToFolder,
    Here,
    Test,
    /// "Add to archive...": the settings page first.
    Add,
    /// "Compress to name.zip", straight away.
    Compress,
}

const VERBS: [Verb; 7] = [Verb::Open, Verb::Ask, Verb::ToFolder, Verb::Here, Verb::Test, Verb::Add, Verb::Compress];

/// The archive types B-Rar opens (the same as the installer's list).
const ARCHIVES: [&str; 15] = [
    "zip", "7z", "rar", "tar", "gz", "tgz", "xz", "txz", "zst", "tzst", "bz2", "tbz", "tbz2", "001", "r00",
];

fn is_archive(path: &std::path::Path) -> bool {
    path.is_file() && path.extension().is_some_and(|e| ARCHIVES.iter().any(|a| e.eq_ignore_ascii_case(a)))
}

impl Verb {
    fn extracts(self) -> bool {
        !matches!(self, Verb::Add | Verb::Compress)
    }
}

/// The "B-Rar" entry; its submenu holds the verbs.
#[implement(IExplorerCommand)]
struct Root;

impl IExplorerCommand_Impl for Root_Impl {
    fn GetTitle(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        co_string("B-Rar")
    }

    fn GetIcon(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        co_string(&icon())
    }

    fn GetToolTip(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(CLSID_BRAR)
    }

    fn GetState(&self, _: Ref<IShellItemArray>, _: BOOL) -> Result<u32> {
        Ok(ECS_ENABLED.0 as u32)
    }

    fn Invoke(&self, _: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> Result<()> {
        Ok(())
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Ok(Verbs { next: AtomicUsize::new(0) }.into())
    }
}

#[implement(IEnumExplorerCommand)]
struct Verbs {
    next: AtomicUsize,
}

impl IEnumExplorerCommand_Impl for Verbs_Impl {
    fn Next(&self, celt: u32, out: *mut Option<IExplorerCommand>, fetched: *mut u32) -> HRESULT {
        let mut n = 0;
        while n < celt {
            let i = self.next.load(Ordering::Relaxed);
            let Some(&verb) = VERBS.get(i) else { break };
            // SAFETY: the caller provides room for `celt` uninitialized interface pointers.
            unsafe { out.add(n as usize).write(Some(VerbCommand(verb).into())) };
            self.next.store(i + 1, Ordering::Relaxed);
            n += 1;
        }
        if !fetched.is_null() {
            // SAFETY: checked non-null; points to the caller's counter.
            unsafe { *fetched = n };
        }
        if n == celt { S_OK } else { S_FALSE }
    }

    fn Skip(&self, celt: u32) -> Result<()> {
        self.next.fetch_add(celt as usize, Ordering::Relaxed);
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.next.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        Ok(Verbs {
            next: AtomicUsize::new(self.next.load(Ordering::Relaxed)),
        }
        .into())
    }
}

#[implement(IExplorerCommand)]
struct VerbCommand(Verb);

impl IExplorerCommand_Impl for VerbCommand_Impl {
    fn GetTitle(&self, items: Ref<IShellItemArray>) -> Result<PWSTR> {
        let verb = self.0;
        guard(|| {
            let title = match verb {
                Verb::Open => "Open in B-Rar".to_owned(),
                Verb::Ask => "Extract files...".to_owned(),
                Verb::ToFolder => match selected(&items).as_slice() {
                    [one] => format!("Extract to \"{}\\\"", folder_name(one)),
                    _ => "Extract each to separate folder".to_owned(),
                },
                Verb::Here => "Extract here".to_owned(),
                Verb::Test => "Test archive".to_owned(),
                Verb::Add => "Add to archive...".to_owned(),
                Verb::Compress => format!("Compress to \"{}\"", zip_name(&selected(&items))),
            };
            co_string(&title)
        })
    }

    fn GetIcon(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        co_string(&icon())
    }

    fn GetToolTip(&self, _: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Err(E_NOTIMPL.into())
    }

    fn GetState(&self, items: Ref<IShellItemArray>, _: BOOL) -> Result<u32> {
        let verb = self.0;
        guard(|| {
            // The extract commands only when everything selected is an archive.
            let shown = !verb.extracts() || {
                let items = selected(&items);
                !items.is_empty() && items.iter().all(|p| is_archive(p))
            };
            Ok(if shown { ECS_ENABLED.0 } else { ECS_HIDDEN.0 } as u32)
        })
    }

    fn Invoke(&self, items: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> Result<()> {
        let verb = self.0;
        guard(|| {
            let gui = install_dir().join(GUI_EXE);
            if !verb.extracts() {
                // All the selected items, into one archive, in one window.
                let items = selected(&items);
                let mut cmd = Command::new(&gui);
                cmd.arg("a").arg("--all");
                if verb == Verb::Add {
                    cmd.arg("--ask");
                }
                cmd.args(&items);
                if let Some(dir) = items.first().and_then(|p| p.parent()) {
                    cmd.current_dir(dir);
                }
                let _ = cmd.spawn();
                return Ok(());
            }
            for archive in selected(&items) {
                let mut cmd = Command::new(&gui);
                match verb {
                    Verb::Open => cmd.arg(&archive),
                    Verb::Ask => cmd.arg("x").arg(&archive).arg("--ask"),
                    Verb::ToFolder => cmd.arg("x").arg(&archive),
                    Verb::Here => cmd.arg("x").arg(&archive).arg("--here"),
                    Verb::Test => cmd.arg("t").arg(&archive),
                    // Handled above.
                    Verb::Add | Verb::Compress => continue,
                };
                if let Some(dir) = archive.parent() {
                    cmd.current_dir(dir);
                }
                let _ = cmd.spawn();
            }
            Ok(())
        })
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, iid: *const GUID, object: *mut *mut core::ffi::c_void) -> Result<()> {
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let root: IUnknown = Root.into();
        // SAFETY: COM contract: `iid` and `object` come straight from the caller.
        unsafe { root.query(iid, object).ok() }
    }

    fn LockServer(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
}

/// COM entry point: hands Explorer the factory for [`CLSID_BRAR`].
///
/// # Safety
/// Called by COM with valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, object: *mut *mut core::ffi::c_void) -> HRESULT {
    // SAFETY: COM passes a valid CLSID pointer.
    if clsid.is_null() || unsafe { *clsid } != CLSID_BRAR {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    // SAFETY: COM passes valid `iid` and `object` pointers.
    unsafe { factory.query(iid, object) }
}

/// Keep the DLL loaded; Explorer unloads idle handlers on its own schedule.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

/// Runs `f`, turning a panic into E_FAIL instead of unwinding into Explorer.
fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_FAIL.into()))
}

/// File-system paths of the selected items (non-file items are ignored).
fn selected(items: &Ref<IShellItemArray>) -> Vec<PathBuf> {
    let Ok(items) = items.ok() else { return Vec::new() };
    // SAFETY: plain COM calls; each returned string is freed with CoTaskMemFree.
    unsafe {
        let count = items.GetCount().unwrap_or(0);
        (0..count)
            .filter_map(|i| {
                let raw = items.GetItemAt(i).ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
                let path = raw.to_string().ok();
                CoTaskMemFree(Some(raw.0 as *const _));
                path.map(PathBuf::from)
            })
            .collect()
    }
}

/// The archive "Compress to" makes (as B-Rar-GUI picks it, without the " (2)" it adds when the
/// name is taken): one item's name, or for several their folder's name, with .zip.
fn zip_name(items: &[PathBuf]) -> String {
    let name = match items {
        [one] if one.is_dir() => one.file_name(),
        [one] => one.file_stem(),
        [first, ..] => first.parent().and_then(|p| p.file_name()),
        [] => None,
    };
    let stem = name.map(|n| n.to_string_lossy().into_owned()).filter(|s| !s.is_empty());
    format!("{}.zip", stem.as_deref().unwrap_or("Archive"))
}

/// Same rule as `brar_core::suggested_dir`: the name up to the first dot.
fn folder_name(archive: &std::path::Path) -> String {
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    name.split('.').next().filter(|s| !s.is_empty()).unwrap_or("extracted").to_owned()
}

fn icon() -> String {
    format!("{},0", install_dir().join(GUI_EXE).display())
}

/// The folder this DLL was loaded from (the package's install folder).
fn install_dir() -> PathBuf {
    let mut module = HMODULE::default();
    let mut buf = [0u16; 1024];
    // SAFETY: looks up the module containing this function; `buf` is a valid out buffer.
    let len = unsafe {
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if GetModuleHandleExW(flags, PCWSTR(install_dir as *const u16), &mut module).is_err() {
            return PathBuf::new();
        }
        GetModuleFileNameW(Some(module), &mut buf) as usize
    };
    let path = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
    path.parent().map(PathBuf::from).unwrap_or_default()
}

/// A string Explorer will free with CoTaskMemFree.
fn co_string(s: &str) -> Result<PWSTR> {
    let wide: Vec<u16> = OsStr::new(s).encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; SHStrDupW copies it.
    unsafe { SHStrDupW(PCWSTR(wide.as_ptr())) }
}
