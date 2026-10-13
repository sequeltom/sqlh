//! The console and the shell around it: the password prompt, and the hint for a Git Bash path.
//!
//! sqlh never changes a console's code page. When stdout is a console, Rust's standard
//! library writes it as UTF-16 (`WriteConsoleW`), so sqlh's UTF-8 text shows correctly under
//! any code page. Switching the console to UTF-8 (`SetConsoleOutputCP`), as sqlh once did,
//! was not needed, and the change stayed behind in the shell's console for every program run
//! after sqlh, with no way to put it back after a kill or an early Ctrl+C. Measured in
//! docs/notes/windows-shells.md; DESIGN.md §4.1 has the rules for the bytes of a pipe or file.

use std::io::{self, BufRead, IsTerminal, Write};

use crate::secret::Secret;

/// The Windows spelling of a path written the way Git Bash writes drive paths: `/c/code/x.sql`
/// is `C:\code\x.sql`. Git Bash converts such arguments before a program sees them; cmd.exe
/// and PowerShell don't, and Windows reads them as paths on the current drive
/// (`\c\code\x.sql`). None for any other path, including a UNC path written with forward
/// slashes (`//server/share/x.sql`).
pub fn git_bash_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let mut chars = rest.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let tail = chars.as_str();
    if !tail.is_empty() && !tail.starts_with('/') {
        return None;
    }
    let tail = tail.replace('/', "\\");
    let tail = if tail.is_empty() { "\\" } else { &tail };
    Some(format!("{}:{tail}", drive.to_ascii_uppercase()))
}

/// What to add to the message for a path that wasn't found when it is written like a Git Bash
/// path, so the caller can name the spellings that work (PLAN.md 2.13). The forward-slash one
/// is there because the text format doubles a backslash (DESIGN.md §5.1), and `C:/code/x.sql`
/// reads the same in every format.
pub fn git_bash_hint(path: &str) -> Option<String> {
    git_bash_path(path).map(|windows| {
        format!(
            "that is a Git Bash path, which only Git Bash converts; in cmd.exe and PowerShell \
             write {windows} or {}",
            windows.replace('\\', "/")
        )
    })
}

/// `message` for a path that couldn't be opened, with the Git Bash hint after it when the path
/// wasn't found and is written like one.
pub fn path_error(message: String, path: &str, error: &io::Error) -> String {
    match git_bash_hint(path) {
        Some(hint) if error.kind() == io::ErrorKind::NotFound => format!("{message}; {hint}"),
        _ => message,
    }
}

/// Reads one line from the console with echo off. The prompt goes to stderr. Callers
/// must check that stdin is a console first; this is the tool's only interactive read.
///
/// Refuses to read at all unless echo was really turned off, and the handler that puts it
/// back on Ctrl+C, Ctrl+Break or a close is in place. Terminals such as Git Bash's mintty
/// look like a console to `IsTerminal` but have no console mode, so the password would be
/// shown as it's typed.
pub fn read_password(prompt: &str) -> io::Result<Secret> {
    let _echo = EchoOff::new().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "can't hide the password in this terminal ({e}). In Git Bash (mintty), which has \
                 no Windows console, run the command from PowerShell or cmd, or prefix it with \
                 winpty"
            ),
        )
    })?;
    // Fallible writes, not eprint!: a panic here would exit without restoring echo.
    let mut stderr = io::stderr();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    let line = read_line_within(io::stdin().lock(), PASSWORD_LINE_BYTES);
    let _ = writeln!(stderr);
    line
}

/// The longest line the password prompt reads, in bytes, its line break included. SQL Server
/// allows passwords of up to 128 characters, at most 4 bytes each in UTF-8, so this is room
/// for any of them.
const PASSWORD_LINE_BYTES: usize = 1024;

/// Reads one line of at most `limit` bytes, its line break included, into a buffer allocated
/// once at that size: the read can't grow it, so it never reallocates and leaves a copy of
/// the password behind in memory it gave up. A longer line is refused. The line break is
/// dropped; the buffer, spare capacity included, is wiped when the `Secret` is dropped, and
/// so is what was read when the line is refused.
fn read_line_within(reader: impl BufRead, limit: usize) -> io::Result<Secret> {
    let mut bytes = zeroize::Zeroizing::new(Vec::with_capacity(limit));
    reader.take(limit as u64).read_until(b'\n', &mut bytes)?;
    if bytes.len() == limit && bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the password is longer than {limit} bytes; SQL Server allows 128 characters"),
        ));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    // `from_utf8` takes the buffer over without copying it; on failure it hands it back,
    // to be wiped.
    match String::from_utf8(std::mem::take(&mut *bytes)) {
        Ok(line) => Ok(Secret::new(line)),
        Err(e) => {
            drop(zeroize::Zeroizing::new(e.into_bytes()));
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the password is not valid UTF-8",
            ))
        }
    }
}

pub fn stdin_is_console() -> bool {
    io::stdin().is_terminal()
}

/// The console mode saved by an `EchoOff` that is alive, for `restore_echo`.
#[cfg(windows)]
static SAVED_MODE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(windows)]
static ECHO_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Restores the console mode if a password prompt turned echo off. For the panic hook,
/// which exits without running destructors.
#[cfg(windows)]
pub fn restore_echo() {
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
    if ECHO_OFF.swap(false, Ordering::SeqCst) {
        // SAFETY: restores a mode saved from the same standard handle; both calls take
        // their arguments by value.
        unsafe {
            SetConsoleMode(
                GetStdHandle(STD_INPUT_HANDLE),
                SAVED_MODE.load(Ordering::SeqCst),
            );
        }
    }
}

#[cfg(not(windows))]
pub fn restore_echo() {}

/// Turns console echo off for its lifetime and restores the saved mode when dropped, on
/// every return path. A panic restores it through `restore_echo`, and so do Ctrl+C,
/// Ctrl+Break and the console closing (`restore_on_signal`). Only a process killed outright
/// can leave echo off in the console it shares with the shell.
struct EchoOff {
    #[cfg_attr(not(windows), allow(dead_code))]
    saved: u32,
}

#[cfg(windows)]
impl EchoOff {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
            SetConsoleCtrlHandler, SetConsoleMode,
        };
        let mut saved = 0;
        // SAFETY: GetStdHandle has no preconditions. GetConsoleMode writes one u32 to
        // `saved` and fails cleanly when the handle isn't a console; SetConsoleMode takes
        // the handle and a mode by value.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if GetConsoleMode(handle, &mut saved) == 0 {
                return Err(io::Error::last_os_error());
            }
            if SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut now = 0;
            if GetConsoleMode(handle, &mut now) == 0 || now & ENABLE_ECHO_INPUT != 0 {
                SetConsoleMode(handle, saved);
                return Err(io::Error::other("echo is still on"));
            }
        }
        // Both are set before the handler is registered, so a signal that arrives right after
        // the registration finds what it needs to put echo back.
        SAVED_MODE.store(saved, std::sync::atomic::Ordering::SeqCst);
        ECHO_OFF.store(true, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: registers a handler that stays valid for the life of the process; it is
        // removed again in `drop`. The call takes the function pointer and a flag by value.
        if unsafe { SetConsoleCtrlHandler(Some(restore_on_signal), 1) } == 0 {
            // Without the handler, Ctrl+C or a close at the prompt would leave echo off in the
            // shell's console. The prompt must not run: put the mode back and fail. The
            // handler isn't registered, so nothing else can be using `SAVED_MODE`.
            let error = io::Error::last_os_error();
            restore_echo();
            return Err(io::Error::new(
                error.kind(),
                format!("can't register the handler that puts echo back on Ctrl+C: {error}"),
            ));
        }
        Ok(EchoOff { saved })
    }
}

#[cfg(windows)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_INPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode,
        };
        ECHO_OFF.store(false, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: restores the mode saved in `new` on the same standard handle, and removes
        // the handler `new` added. Failure can't be reported from a destructor, and the
        // prompt has already returned.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.saved);
            SetConsoleCtrlHandler(Some(restore_on_signal), 0);
        }
    }
}

#[cfg(not(windows))]
impl EchoOff {
    fn new() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hidden password input needs Windows",
        ))
    }
}

/// Runs on its own thread when Ctrl+C, Ctrl+Break or a close arrives at the prompt. It puts
/// echo back, then returns FALSE so the default handling still ends the process: a handled
/// Ctrl+C would only restart the read, leaving no way out of the prompt.
#[cfg(windows)]
unsafe extern "system" fn restore_on_signal(_event: u32) -> windows_sys::Win32::Foundation::BOOL {
    restore_echo();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // PLAN.md 2.13: only `/<drive letter>` followed by `/` or the end is a Git Bash drive path.
    #[test]
    fn git_bash_drive_paths_have_a_windows_spelling() {
        let cases = [
            ("/c/code/x.sql", Some(r"C:\code\x.sql")),
            ("/C/Code/X.sql", Some(r"C:\Code\X.sql")),
            ("/d/a b/c.sql", Some(r"D:\a b\c.sql")),
            ("/c/code\\x.sql", Some(r"C:\code\x.sql")),
            ("/c", Some(r"C:\")),
            ("/c/", Some(r"C:\")),
            // Not drive paths: UNC with forward slashes, other first components, and the
            // forms Windows reads correctly as they are.
            ("//c/code", None),
            ("//server/share/x.sql", None),
            ("/cd/x", None),
            ("/1/x", None),
            ("/\u{e9}/x", None),
            ("/c:/x", None),
            ("/tmp/x.sql", None),
            ("/", None),
            ("", None),
            ("c/x", None),
            ("C:/x/y.sql", None),
            (r"\c\x.sql", None),
            (r"C:\x\y.sql", None),
            (r"\\server\share\x.sql", None),
        ];
        for (path, windows) in cases {
            assert_eq!(git_bash_path(path).as_deref(), windows, "{path}");
        }
    }

    // The hint is added to a path that wasn't found, and to nothing else.
    #[test]
    fn the_hint_follows_only_a_not_found_git_bash_path() {
        let missing = io::Error::from(io::ErrorKind::NotFound);
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let message = || "-i /c/code/x.sql: gone".to_owned();
        let hinted = path_error(message(), "/c/code/x.sql", &missing);
        assert_eq!(
            hinted,
            r"-i /c/code/x.sql: gone; that is a Git Bash path, which only Git Bash converts; in cmd.exe and PowerShell write C:\code\x.sql or C:/code/x.sql"
        );
        assert_eq!(path_error(message(), "/c/code/x.sql", &denied), message());
        assert_eq!(path_error(message(), "/tmp/x.sql", &missing), message());
        assert_eq!(path_error(message(), r"C:\x.sql", &missing), message());
    }

    // The Phase 3 review's design follow-up: the password's line is read into a buffer that
    // never grows past the size it was allocated at, so no reallocation leaves a copy
    // behind; a longer line is refused, and the line break is dropped.
    #[test]
    fn the_password_line_is_read_without_reallocating() {
        let read = |input: &[u8], limit| read_line_within(io::Cursor::new(input.to_vec()), limit);
        let line = read("pässwörd\r\nnext line".as_bytes(), 64).unwrap();
        assert_eq!(line.as_str(), "pässwörd");
        assert_eq!(line.capacity(), 64, "the buffer kept its size");
        // Exactly the limit, line break included; and the last line, with no break.
        assert_eq!(read(b"1234567\n", 8).unwrap().as_str(), "1234567");
        assert_eq!(
            read(b"12345678", 8).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(read(b"123", 8).unwrap().as_str(), "123");
        assert_eq!(read(b"", 8).unwrap().as_str(), "");
        let long = read(&[b'x'; 2000], PASSWORD_LINE_BYTES).unwrap_err();
        assert!(
            long.to_string().contains("longer than 1024 bytes"),
            "{long}"
        );
        let bad = read(b"\xff\xfe\n", 8).unwrap_err();
        assert_eq!(bad.kind(), io::ErrorKind::InvalidData);
    }
}
