//! File identity, so `-o` can refuse to overwrite an input file however the two paths are
//! spelled (`a.sql` vs `.\A.SQL`, a symbolic link, or an NTFS hard link).

use std::fs::File;
use std::path::Path;

/// True when both paths exist and are the same file.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (identity(a), identity(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Volume serial number and file index: unique per file on a running system.
#[cfg(windows)]
fn identity(path: &Path) -> Option<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = File::open(path).ok()?;
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value of this plain C struct.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle belongs to `file`, which outlives the call, and `info` is a valid
    // out-pointer for the duration of the call.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    (ok != 0).then_some((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

#[cfg(not(windows))]
fn identity(path: &Path) -> Option<(u64, u64, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = File::open(path).ok()?.metadata().ok()?;
    Some((meta.dev(), meta.ino(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_aliases_and_hard_links() {
        let dir = std::env::temp_dir().join(format!("sqlh-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.sql");
        let b = dir.join("b.sql");
        let link = dir.join("link.sql");
        std::fs::write(&a, "SELECT 1").unwrap();
        std::fs::write(&b, "SELECT 1").unwrap();
        let _ = std::fs::remove_file(&link);
        std::fs::hard_link(&a, &link).unwrap();
        let alias = dir.join(".").join("A.SQL");
        assert!(same_file(&a, &a));
        assert!(
            same_file(&a, &alias),
            "a differently spelled path is the same file"
        );
        assert!(same_file(&a, &link), "a hard link is the same file");
        assert!(!same_file(&a, &b), "same contents, different file");
        assert!(!same_file(&a, &dir.join("missing.sql")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
