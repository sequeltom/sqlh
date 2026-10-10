//! Files behind `-o`: file identity, so `-o` can refuse to overwrite an input file however
//! the two paths are spelled (`a.sql` vs `.\A.SQL`, a symbolic link, or an NTFS hard link),
//! and the run's own `.partial` file, which replaces the destination only when complete.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

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

/// True for a character device or a pipe (`NUL`, `CON`, `\\.\pipe\name`): output goes
/// straight to those, since there is no file to replace.
#[cfg(windows)]
pub fn is_stream(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_PIPE, GetFileType};
    // SAFETY: the handle belongs to `file`, which outlives the call.
    let kind = unsafe { GetFileType(file.as_raw_handle()) };
    kind == FILE_TYPE_CHAR || kind == FILE_TYPE_PIPE
}

#[cfg(not(windows))]
pub fn is_stream(file: &File) -> bool {
    file.metadata().is_ok_and(|m| !m.is_file())
}

/// A new file next to a destination that only this run writes. `-o` output goes here and
/// replaces the destination once it's complete (DESIGN.md §4.2).
/// - The name is unique (`out.txt.<pid>-<n>.partial`) and the file is created new, so it
///   never truncates or takes over an existing file, such as another run's.
/// - No other process can write, rename or delete it while it's open.
/// - Until `publish`, it is marked for deletion: Windows removes it when the last handle
///   closes, so a failed run, or a process killed while writing, leaves nothing behind.
///   While marked, others can't open it at all.
/// - That holds only once the mark is set. When setting it fails (not expected), `armed` is
///   false and `Drop` removes the file by name instead. That is best effort: it covers a
///   run that ends normally, but not a kill, and an error removing the file is ignored.
pub struct Partial {
    file: Option<File>,
    path: PathBuf,
    /// The deletion mark took. When it didn't, `Drop` removes the file by name instead.
    armed: bool,
    published: bool,
}

impl Partial {
    pub fn create(target: &Path) -> io::Result<Partial> {
        let pid = std::process::id();
        let mut denied = None;
        for n in 0..100 {
            let mut name = target.as_os_str().to_owned();
            name.push(format!(".{pid}-{n}.partial"));
            let path = PathBuf::from(name);
            match open_new_exclusive(&path) {
                Ok(file) => {
                    let armed = set_delete_on_close(&file, true).is_ok();
                    return Ok(Partial {
                        file: Some(file),
                        path,
                        armed,
                        published: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                // A file that is being deleted also reports access denied; so does a folder
                // we can't write to, which every name will then report.
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    denied.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(denied.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "no free name for the .partial file",
            )
        }))
    }

    /// A handle to write the output through. It must be closed before `publish`.
    pub fn writer(&self) -> io::Result<File> {
        match &self.file {
            Some(file) => file.try_clone(),
            None => Err(io::Error::other("the .partial file is closed")),
        }
    }

    /// Moves the finished file over `target`, replacing it whole. On failure the `.partial`
    /// file is removed and `target` stays as it was. A process killed between clearing the
    /// deletion mark and the rename leaves the `.partial` file behind.
    pub fn publish(mut self, target: &Path) -> io::Result<()> {
        if self.armed {
            let file = self
                .file
                .as_ref()
                .ok_or_else(|| io::Error::other("closed"))?;
            // On failure the mark stays, and dropping `self` deletes the file.
            set_delete_on_close(file, false)?;
            self.armed = false;
        }
        // Close first: the handle doesn't share delete access, so a rename needs it gone.
        drop(self.file.take());
        std::fs::rename(&self.path, target)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        // Closing the last handle of a file marked for deletion deletes it.
        drop(self.file.take());
        if !self.armed && !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Creates `path`, failing if it exists. Others may only open it to read: not to write,
/// rename or delete it. DELETE access lets this handle set the deletion mark.
#[cfg(windows)]
fn open_new_exclusive(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_SHARE_READ};
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

#[cfg(not(windows))]
fn open_new_exclusive(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Marks the file to be deleted when its last handle closes, or clears the mark.
#[cfg(windows)]
fn set_delete_on_close(file: &File, delete: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };
    let info = FILE_DISPOSITION_INFO {
        DeleteFile: u8::from(delete),
    };
    // SAFETY: the handle belongs to `file`, which outlives the call and was opened with
    // DELETE access; `info` is a valid FILE_DISPOSITION_INFO and the size passed is its size.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_delete_on_close(_file: &File, _delete: bool) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Windows only"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sqlh-files-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn identifies_aliases_and_hard_links() {
        let dir = dir("identity");
        let a = dir.join("a.sql");
        let b = dir.join("b.sql");
        let link = dir.join("link.sql");
        std::fs::write(&a, "SELECT 1").unwrap();
        std::fs::write(&b, "SELECT 1").unwrap();
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

    // R01: two runs never share a .partial file, and neither takes over a file it didn't create.
    #[cfg(windows)]
    #[test]
    fn partial_files_are_new_and_private() {
        let dir = dir("partial");
        let target = dir.join("out.txt");
        let pid = std::process::id();
        let squatter = dir.join(format!("out.txt.{pid}-0.partial"));
        std::fs::write(&squatter, "not ours").unwrap();

        let first = Partial::create(&target).unwrap();
        let second = Partial::create(&target).unwrap();
        assert_ne!(first.path, second.path);
        assert_ne!(first.path, squatter);
        assert_eq!(std::fs::read_to_string(&squatter).unwrap(), "not ours");
        // Nobody else can write to, or rename, a run's .partial file while it's open.
        assert!(OpenOptions::new().write(true).open(&first.path).is_err());
        assert!(std::fs::rename(&first.path, dir.join("stolen.txt")).is_err());

        let mut writer = first.writer().unwrap();
        writer.write_all(b"first").unwrap();
        drop(writer);
        first.publish(&target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");

        // An abandoned run leaves no file behind and doesn't touch the destination.
        let mut writer = second.writer().unwrap();
        writer.write_all(b"second").unwrap();
        drop(writer);
        drop(second);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        assert_eq!(
            names(&dir),
            ["out.txt".into(), format!("out.txt.{pid}-0.partial")]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn devices_are_streams() {
        let nul = OpenOptions::new().write(true).open("NUL").unwrap();
        assert!(is_stream(&nul));
        let dir = dir("stream");
        let file = File::create(dir.join("f.txt")).unwrap();
        assert!(!is_stream(&file));
        drop(file);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
