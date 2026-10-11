//! Console setup and the password prompt. Output is always UTF-8; a console needs telling.

use std::io::{self, IsTerminal, Write};

#[cfg(windows)]
pub fn use_utf8() {
    const CP_UTF8: u32 = 65001;
    if io::stdout().is_terminal() {
        SetConsoleOutputCP(CP_UTF8);
    }
}

#[cfg(not(windows))]
pub fn use_utf8() {}

// SAFETY: SetConsoleOutputCP takes a code page number by value and has no
// memory-safety preconditions, so it is declared `safe`.
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    safe fn SetConsoleOutputCP(code_page: u32) -> i32;
}

/// Reads one line from the console with echo off. The prompt goes to stderr. Callers
/// must check that stdin is a console first; this is the tool's only interactive read.
///
/// Refuses to read at all unless echo was really turned off. Terminals such as Git Bash's
/// mintty look like a console to `IsTerminal` but have no console mode, so the password
/// would be shown as it's typed.
pub fn read_password(prompt: &str) -> io::Result<String> {
    let _echo = EchoOff::new().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "can't hide the password in this terminal ({e}). Git Bash (mintty) has no \
                 Windows console: run the command from PowerShell or cmd, or prefix it with winpty"
            ),
        )
    })?;
    // Fallible writes, not eprint!: a panic here would exit without restoring echo.
    let mut stderr = io::stderr();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    let mut line = String::new();
    let result = io::stdin().read_line(&mut line);
    let _ = writeln!(stderr);
    result?;
    let trimmed = line.trim_end_matches(['\r', '\n']).len();
    line.truncate(trimmed);
    Ok(line)
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
        SAVED_MODE.store(saved, std::sync::atomic::Ordering::SeqCst);
        ECHO_OFF.store(true, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: registers a handler that stays valid for the life of the process; it is
        // removed again in `drop`.
        unsafe {
            SetConsoleCtrlHandler(Some(restore_on_signal), 1);
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
