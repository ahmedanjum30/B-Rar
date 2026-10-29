use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Joins an untrusted entry name onto `dest`, refusing anything that could escape it.
///
/// Both `/` and `\` separate components. Leading separators are dropped, so absolute names
/// land inside `dest`, matching 7-Zip. Rejected: `..` and NUL; on Windows also drive letters
/// and NTFS streams (`:`) and components that Windows would normalize into `..` (trailing
/// dots or spaces, e.g. `.. ` or `...`), which are ordinary names elsewhere.
pub(crate) fn safe_join(dest: &Path, name: &str) -> Result<PathBuf> {
    let mut out = dest.to_path_buf();
    for part in name.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        let unsafe_part = if cfg!(windows) {
            part.trim_end_matches(['.', ' ']).is_empty() || part.contains([':', '\0'])
        } else {
            part == ".." || part.contains('\0')
        };
        if unsafe_part {
            return Err(Error::UnsafePath(name.to_owned()));
        }
        out.push(part);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_normal_paths() {
        let p = safe_join(Path::new("out"), "a/b\\c.txt").unwrap();
        assert_eq!(p, Path::new("out").join("a").join("b").join("c.txt"));
        assert_eq!(
            safe_join(Path::new("out"), "/etc/passwd").unwrap(),
            Path::new("out").join("etc").join("passwd")
        );
        assert!(safe_join(Path::new("out"), "a/.hidden").is_ok());
        assert!(safe_join(Path::new("out"), "a/x..y").is_ok());
    }

    #[test]
    fn rejects_escapes() {
        for bad in ["../x", "a/../../x", "..\\x", "a\0b"] {
            assert!(safe_join(Path::new("out"), bad).is_err(), "{bad}");
        }
        // Only Windows turns these into `..`, a drive or a stream; elsewhere they're names.
        for name in ["a/.. /x", "a/.../x", "C:/x", "a/b:ads"] {
            assert_eq!(safe_join(Path::new("out"), name).is_err(), cfg!(windows), "{name}");
        }
    }
}
