//! Updates from GitHub releases, for the copies the Microsoft Store doesn't update: the
//! installed version (B-Rar-Setup), the portable files and the sideloaded MSIX (signed with
//! B-Rar's own certificate). B-Rar only
//! tells about a new version; nothing is downloaded until the update is asked for.
//!
//! The newest version comes from the release pages, not GitHub's API: the API allows only 60
//! requests an hour per internet address without an account, which people sharing an address
//! run out of. Downloads are checked against the release's `SHA256SUMS.txt` before use.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};

const REPO: &str = "https://github.com/ahmedanjum30/B-Rar";
/// The sideloaded package (publisher `CN=Ahmad Anjum`); the Store's has another publisher, so
/// another family name.
const SIDELOADED_FAMILY: &str = "AhmadAnjum.B-Rar_2kp6fvgcfm1dm";

/// What kind of copy this is, which decides how (and whether) it updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// From the Microsoft Store, which updates it.
    Store,
    /// The MSIX package signed with B-Rar's certificate: updated by opening the new package
    /// in App Installer.
    Sideloaded,
    /// Installed by B-Rar-Setup: updated by running the new setup.
    Installed,
    /// The portable zip's files: replaced in place.
    Portable,
    /// Anything else (a development build): never updated.
    Other,
}

pub fn kind() -> Kind {
    match package_family() {
        Some(family) if family == SIDELOADED_FAMILY => Kind::Sideloaded,
        Some(_) => Kind::Store,
        // The setup leaves its uninstaller next to the programs.
        None if exe_dir().is_some_and(|d| d.join("unins000.exe").exists()) => Kind::Installed,
        // The portable zip ships the unRAR license next to the programs; a build folder
        // doesn't have it.
        None if exe_dir().is_some_and(|d| d.join("unrar-license.txt").exists()) => Kind::Portable,
        None => Kind::Other,
    }
}

/// This process's package family name, if it runs from an MSIX package.
fn package_family() -> Option<String> {
    use windows::{Win32::Storage::Packaging::Appx::GetCurrentPackageFamilyName, core::PWSTR};
    let mut buf = [0u16; 128];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer and its length in characters, as the call expects.
    let result = unsafe { GetCurrentPackageFamilyName(&mut len, Some(PWSTR(buf.as_mut_ptr()))) };
    result.is_ok().then(|| String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]))
}

fn exe_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// The release page, for "What's new".
    pub page: String,
    pub url: String,
    pub file_name: String,
    /// From the release's `SHA256SUMS.txt`.
    pub sha256: String,
}

/// The newest release if it's newer than this copy (`None`: up to date).
pub fn check(kind: Kind) -> Result<Option<Release>, String> {
    check_against(kind, env!("CARGO_PKG_VERSION"))
}

fn check_against(kind: Kind, current: &str) -> Result<Option<Release>, String> {
    // `releases/latest` redirects to the newest release's page, `.../releases/tag/v1.2.3`.
    let page = http::redirect_target(&format!("{REPO}/releases/latest"))?;
    let tag = page
        .rsplit_once("/releases/tag/")
        .map(|(_, tag)| tag.trim_end_matches('/'))
        .ok_or("no release found on GitHub")?
        .to_owned();
    let version = tag.trim_start_matches('v').to_owned();
    if !is_newer(&version, current) {
        return Ok(None);
    }
    let file_name = match kind {
        Kind::Sideloaded => format!("B-Rar-{version}-x64.msix"),
        Kind::Installed => format!("B-Rar-Setup-{version}.exe"),
        _ => format!("B-Rar-{version}-portable-x64.zip"),
    };
    let sums = http::get(&format!("{REPO}/releases/download/{tag}/SHA256SUMS.txt"), 64 * 1024)?;
    let sha256 = checksum_for(&String::from_utf8_lossy(&sums), &file_name).ok_or("the update is missing from the release")?;
    Ok(Some(Release {
        url: format!("{REPO}/releases/download/{tag}/{file_name}"),
        file_name,
        version,
        page,
        sha256,
    }))
}

/// Compares dotted version numbers: `1.0.10` is newer than `1.0.9`.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|part| part.parse().ok()).collect() };
    match (parse(candidate), parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// Finds `name` in a `sha256sum`-style file (`<hex>  <file name>` per line).
fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name && hex.len() == 64).then(|| hex.to_ascii_lowercase())
    })
}

/// Where downloads go.
fn download_dir() -> PathBuf {
    std::env::temp_dir().join("B-Rar-update")
}

/// Downloads the release's file and checks its SHA-256. `done` and `total` follow the bytes.
pub fn download(release: &Release, done: &AtomicU64, total: &AtomicU64) -> Result<PathBuf, String> {
    let dir = download_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&release.file_name);
    let mut file = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    let result = http::download(&release.url, total, &mut |chunk| {
        hash.update(chunk);
        done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        file.write_all(chunk).map_err(|e| format!("{}: {e}", path.display()))
    });
    drop(file);
    let checked = result.and_then(|()| {
        let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got == release.sha256 {
            Ok(())
        } else {
            Err("the download doesn't match the release's checksum".to_owned())
        }
    });
    match checked {
        Ok(()) => Ok(path),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

/// Installs a downloaded update. Installed: runs the new setup silently (it closes B-Rar,
/// replaces it and opens it again). Sideloaded: opens the package in App Installer (which
/// closes B-Rar to update it). Portable: puts the new files in place of these and starts the
/// new B-Rar-GUI. Either way this window should close afterwards.
pub fn install(kind: Kind, file: &Path) -> Result<(), String> {
    match kind {
        Kind::Sideloaded => {
            crate::shell::open_path(file);
            Ok(())
        }
        // Read by the `UpdateMode` check in installer\b-rar.iss: open B-Rar again afterwards.
        Kind::Installed => std::process::Command::new(file)
            .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/UPDATE=1"])
            .spawn()
            .map(drop)
            .map_err(|e| format!("couldn't start the update: {e}")),
        Kind::Portable => replace_files(file),
        _ => Err("this copy of B-Rar isn't updated from GitHub".to_owned()),
    }
}

/// Unpacks the portable zip next to the running programs. A running program can't be
/// overwritten, but it can be renamed: each file there now becomes `<name>.old` (removed by
/// [`clean_up`] on the next start), then the new one moves in.
fn replace_files(zip: &Path) -> Result<(), String> {
    let dir = exe_dir().ok_or("can't find B-Rar's folder")?;
    put_in_place(zip, &dir, &download_dir().join("unpacked"))?;
    let _ = std::fs::remove_file(zip);
    std::process::Command::new(dir.join("B-Rar-GUI.exe"))
        .spawn()
        .map(drop)
        .map_err(|e| format!("couldn't start the new B-Rar: {e}"))
}

/// [`replace_files`] for the files in `dir`, unpacking through `unpacked`.
fn put_in_place(zip: &Path, dir: &Path, unpacked: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(unpacked);
    brar_core::extract(zip, unpacked, &brar_core::Options::default()).map_err(|e| e.to_string())?;
    let entries = std::fs::read_dir(unpacked).map_err(|e| format!("{}: {e}", unpacked.display()))?;
    let files: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect();
    if !files.iter().any(|f| f.file_name().is_some_and(|n| n == "B-Rar-GUI.exe")) {
        return Err("the update has no B-Rar-GUI.exe".to_owned());
    }
    let denied = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "B-Rar can't write to {}: move it to a folder of yours, or download the update",
            dir.display()
        ),
        _ => format!("{}: {e}", dir.display()),
    };
    for new in &files {
        let name = new.file_name().unwrap_or_default();
        let target = dir.join(name);
        if target.exists() {
            let mut old = target.clone().into_os_string();
            old.push(".old");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&target, &old).map_err(denied)?;
        }
        // Across drives a rename fails: copy instead.
        if std::fs::rename(new, &target).is_err() {
            std::fs::copy(new, &target).map_err(denied)?;
        }
    }
    let _ = std::fs::remove_dir_all(unpacked);
    Ok(())
}

/// Removes what an earlier update left: the replaced programs (`*.old`, in use until that
/// B-Rar closed) and the download folder.
pub fn clean_up() {
    if let Some(Ok(entries)) = exe_dir().map(std::fs::read_dir) {
        for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
            if path.extension().is_some_and(|e| e == "old") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// HTTPS requests through Windows' own WinHTTP (the system's proxy settings and certificate
/// store, nothing extra in B-Rar).
mod http {
    use std::{
        ffi::c_void,
        sync::atomic::{AtomicU64, Ordering},
    };

    use windows::{
        Win32::Networking::WinHttp::{
            INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_SECURE,
            WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION,
            WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
            WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption,
        },
        core::{PCWSTR, w},
    };

    use crate::shell::wide;

    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle WinHTTP returned, closed once.
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    /// An open request with its response headers received (the session and connection are
    /// kept with it, closed in reverse order).
    struct Response {
        request: Handle,
        _connect: Handle,
        _session: Handle,
        status: u32,
    }

    fn error(what: &str) -> String {
        format!("{what}: {}", windows::core::Error::from_thread())
    }

    /// Sends a GET. `follow`: follow redirects (otherwise the 302 itself is the response).
    fn send(url: &str, follow: bool) -> Result<Response, String> {
        let rest = url.strip_prefix("https://").ok_or("only https addresses")?;
        let (host, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
        let agent = wide(format!("B-Rar/{}", env!("CARGO_PKG_VERSION")));
        let (host, path) = (wide(host), wide(path));
        // SAFETY (all calls): NUL-terminated strings that outlive the calls, handles from the
        // calls before, closed by Handle.
        unsafe {
            let session = Handle(WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ));
            if session.0.is_null() {
                return Err(error("couldn't start WinHTTP"));
            }
            let connect = Handle(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), INTERNET_DEFAULT_HTTPS_PORT, 0));
            if connect.0.is_null() {
                return Err(error("couldn't connect"));
            }
            let request = Handle(WinHttpOpenRequest(
                connect.0,
                w!("GET"),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ));
            if request.0.is_null() {
                return Err(error("couldn't open the request"));
            }
            if !follow {
                let off = WINHTTP_DISABLE_REDIRECTS.to_le_bytes();
                WinHttpSetOption(Some(request.0), WINHTTP_OPTION_DISABLE_FEATURE, Some(&off)).map_err(|e| e.to_string())?;
            }
            WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|e| format!("couldn't reach GitHub: {e}"))?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| format!("no answer from GitHub: {e}"))?;
            let status = number(&request, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
            Ok(Response {
                request,
                _connect: connect,
                _session: session,
                status,
            })
        }
    }

    fn number(request: &Handle, header: u32) -> Option<u32> {
        let (mut value, mut len) = (0u32, 4u32);
        // SAFETY: a 4-byte buffer for a numeric header.
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                header | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut value as *mut u32 as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )
            .ok()?;
        }
        Some(value)
    }

    /// Where `url` redirects to.
    pub fn redirect_target(url: &str) -> Result<String, String> {
        let response = send(url, false)?;
        if !(300..400).contains(&response.status) {
            return Err(format!("GitHub answered {}", response.status));
        }
        let mut buf = [0u16; 2048];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: the buffer and its size in bytes.
        unsafe {
            WinHttpQueryHeaders(
                response.request.0,
                WINHTTP_QUERY_LOCATION,
                PCWSTR::null(),
                Some(buf.as_mut_ptr() as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )
            .map_err(|e| format!("no redirect from GitHub: {e}"))?;
        }
        Ok(String::from_utf16_lossy(&buf[..len as usize / 2]))
    }

    /// The body of `url`, at most `max` bytes.
    pub fn get(url: &str, max: usize) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        download(url, &AtomicU64::new(0), &mut |chunk| {
            body.extend_from_slice(chunk);
            if body.len() > max {
                Err("the answer is too long".to_owned())
            } else {
                Ok(())
            }
        })?;
        Ok(body)
    }

    /// Streams the body of `url` (redirects followed) to `sink`; `total` gets its length if
    /// the server says.
    pub fn download(url: &str, total: &AtomicU64, sink: &mut dyn FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
        let response = send(url, true)?;
        if response.status != 200 {
            return Err(format!("GitHub answered {} for {url}", response.status));
        }
        if let Some(n) = number(&response.request, WINHTTP_QUERY_CONTENT_LENGTH) {
            total.store(u64::from(n), Ordering::Relaxed);
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            // SAFETY: the buffer and its length.
            unsafe { WinHttpReadData(response.request.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut read) }
                .map_err(|e| format!("the download stopped: {e}"))?;
            if read == 0 {
                return Ok(());
            }
            sink(&buf[..read as usize])?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
        assert!(!is_newer("1.1.0-beta", "1.0.0"));
    }

    #[test]
    fn checksums() {
        let sums = "aaaa  other.zip\nB6E72874F8CB5C4ED77F9B03D7F1FDE851B9455A7FF02A1E1119C876318EBC65  B-Rar-1.0.1-portable-x64.zip\n";
        assert_eq!(
            checksum_for(sums, "B-Rar-1.0.1-portable-x64.zip").as_deref(),
            Some("b6e72874f8cb5c4ed77f9b03d7f1fde851b9455a7ff02a1e1119c876318ebc65")
        );
        assert_eq!(checksum_for(sums, "other.zip"), None);
        assert_eq!(checksum_for(sums, "missing.zip"), None);
    }

    /// A portable folder updated from a zip (made with PowerShell's Compress-Archive): every
    /// file replaced, the old ones kept as `.old`, files the zip doesn't have left alone.
    #[test]
    fn portable_files_are_replaced() {
        let root = std::env::temp_dir().join(format!("brar-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (dir, new, unpacked) = (root.join("B-Rar"), root.join("new"), root.join("unpacked"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        for (name, text) in [("B-Rar-GUI.exe", "old gui"), ("B-Rar.exe", "old cli"), ("mine.txt", "user file")] {
            std::fs::write(dir.join(name), text).unwrap();
        }
        for (name, text) in [("B-Rar-GUI.exe", "new gui"), ("B-Rar.exe", "new cli"), ("README.md", "new readme")] {
            std::fs::write(new.join(name), text).unwrap();
        }
        let zip = root.join("update.zip");
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(format!(
                "Compress-Archive -Path '{}\\*' -DestinationPath '{}'",
                new.display(),
                zip.display()
            ))
            .status()
            .unwrap();
        assert!(status.success());
        put_in_place(&zip, &dir, &unpacked).unwrap();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read("B-Rar-GUI.exe"), "new gui");
        assert_eq!(read("B-Rar.exe"), "new cli");
        assert_eq!(read("README.md"), "new readme");
        assert_eq!(read("B-Rar-GUI.exe.old"), "old gui");
        assert_eq!(read("mine.txt"), "user file");
        assert!(!unpacked.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Talks to GitHub, so it only runs when asked:
    /// `cargo test -p brar-gui -- --ignored live_release`. Needs a published release.
    #[test]
    #[ignore]
    fn live_release_is_found() {
        let release = check_against(Kind::Portable, "0.0.1").unwrap().expect("a newer release");
        println!("{release:?}");
        assert_eq!(release.sha256.len(), 64);
        assert_eq!(check_against(Kind::Portable, "999.0.0").unwrap(), None);
    }
}
