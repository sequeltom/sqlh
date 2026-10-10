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
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    let result = io::stdin().read_line(&mut line);
    eprintln!();
    result?;
    let trimmed = line.trim_end_matches(['\r', '\n']).len();
    line.truncate(trimmed);
    Ok(line)
}

pub fn stdin_is_console() -> bool {
    io::stdin().is_terminal()
}

/// Turns console echo off for its lifetime and restores the saved mode when dropped,
/// on every return path.
struct EchoOff {
    #[cfg_attr(not(windows), allow(dead_code))]
    saved: u32,
}

#[cfg(windows)]
impl EchoOff {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
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
        Ok(EchoOff { saved })
    }
}

#[cfg(windows)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
        // SAFETY: restores the mode saved in `new` on the same standard handle. Failure
        // can't be reported from a destructor, and the prompt has already returned.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.saved);
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
