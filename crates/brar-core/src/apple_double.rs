//! AppleDouble entries: zips made on a Mac (Finder's Compress, `ditto -c -k`) keep each item's
//! extended attributes and resource fork in a partner entry, `dir/._name` or
//! `__MACOSX/dir/._name`. On macOS they're put back on the item, as Archive Utility does,
//! instead of being written as files: stray `._` files inside an app break its code signature.
//! Elsewhere they stay ordinary files, as other extractors leave them.
//!
//! The format: a header (magic 0x00051607, version 0x00020000), then entries (id, offset,
//! length); entry 9 is the Finder info, which Mac OS X extends with the extended attributes
//! (an "ATTR" block), and entry 2 is the resource fork.

/// The entry this AppleDouble entry belongs to, if `name` is one (on macOS).
pub(crate) fn partner(name: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let name = crate::normalize_name(name);
    let name = name.strip_prefix("__MACOSX/").unwrap_or(&name);
    let (dir, file) = name.rsplit_once('/').map_or(("", name), |(d, f)| (d, f));
    let real = file.strip_prefix("._").filter(|r| !r.is_empty())?;
    Some(if dir.is_empty() { real.to_owned() } else { format!("{dir}/{real}") })
}

/// The `__MACOSX` folder itself, which holds only AppleDouble entries (on macOS).
pub(crate) fn is_resource_folder(name: &str) -> bool {
    cfg!(target_os = "macos") && {
        let name = crate::normalize_name(name);
        name == "__MACOSX" || name.starts_with("__MACOSX/")
    }
}

/// The extended attributes an AppleDouble file holds (with the Finder info and resource fork
/// as `com.apple.FinderInfo` and `com.apple.ResourceFork`), or `None` if it isn't one.
pub(crate) fn attributes(data: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let u16_at = |at: usize| Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    if u32_at(0)? != 0x0005_1607 || u32_at(4)? != 0x0002_0000 {
        return None;
    }
    let mut attrs = Vec::new();
    for i in 0..usize::from(u16_at(24)?) {
        let at = 26 + i * 12;
        let (id, offset, length) = (u32_at(at)?, u32_at(at + 4)? as usize, u32_at(at + 8)? as usize);
        let body = data.get(offset..offset.checked_add(length)?)?;
        match id {
            // The resource fork.
            2 if !body.is_empty() => attrs.push(("com.apple.ResourceFork".to_owned(), body.to_vec())),
            // Finder info (32 bytes), then the extended attributes.
            9 => {
                if let Some(info) = body.get(..32).filter(|b| b.iter().any(|&x| x != 0)) {
                    attrs.push(("com.apple.FinderInfo".to_owned(), info.to_vec()));
                }
                // Offsets in the ATTR block count from the start of the file.
                attrs.extend(xattrs(data, offset + 32)?);
            }
            _ => {}
        }
    }
    Some(attrs)
}

/// The "ATTR" block at `at` (after the Finder info, 4-byte aligned): its attributes. Missing
/// is fine (no attributes); damaged isn't.
fn xattrs(data: &[u8], at: usize) -> Option<Vec<(String, Vec<u8>)>> {
    let u16_at = |at: usize| Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    // Two bytes of padding come first.
    let start = at + 2;
    if data.get(start..start + 4) != Some(b"ATTR") {
        return Some(Vec::new());
    }
    let count = usize::from(u16_at(start + 34)?);
    let mut entry = start + 36;
    let mut attrs = Vec::with_capacity(count);
    for _ in 0..count {
        let (offset, length) = (u32_at(entry)? as usize, u32_at(entry + 4)? as usize);
        let name_len = usize::from(*data.get(entry + 10)?);
        let name = data.get(entry + 11..entry + 11 + name_len)?;
        let name = std::str::from_utf8(name.strip_suffix(&[0]).unwrap_or(name)).ok()?.to_owned();
        let value = data.get(offset..offset.checked_add(length)?)?.to_vec();
        attrs.push((name, value));
        // Entries are 4-byte aligned.
        entry = (entry + 11 + name_len + 3) & !3;
    }
    Some(attrs)
}

/// Puts the attributes on `path` (not following a link there). Best effort, as times and
/// permissions are: one the system refuses isn't worth failing the extraction for.
#[cfg(target_os = "macos")]
pub(crate) fn apply(path: &std::path::Path, attrs: &[(String, Vec<u8>)]) {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else { return };
    for (name, value) in attrs {
        // The sandbox's access marker belongs to the Mac that made the zip. (Quarantine
        // stays: it's what has Gatekeeper check a downloaded app before it first runs.)
        if name.starts_with("com.apple.macl") {
            continue;
        }
        let Ok(name) = CString::new(name.as_str()) else { continue };
        // SAFETY: valid C strings and a buffer of `value.len()` bytes.
        unsafe {
            libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, libc::XATTR_NOFOLLOW);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn partners() {
        assert_eq!(partner("dir/._a.txt").as_deref(), Some("dir/a.txt"));
        assert_eq!(partner("__MACOSX/dir/._a.txt").as_deref(), Some("dir/a.txt"));
        assert_eq!(partner("._Contents").as_deref(), Some("Contents"));
        assert_eq!(partner("dir/a.txt"), None);
        assert_eq!(partner("dir/._"), None);
        assert!(is_resource_folder("__MACOSX/") && is_resource_folder("__MACOSX/dir/") && !is_resource_folder("dir"));
    }

    #[test]
    fn not_apple_double() {
        assert!(attributes(b"just a file called ._something").is_none());
    }
}
