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
pub fn read_password(prompt: &str) -> io::Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let restore = echo_off();
    let mut line = String::new();
    let result = io::stdin().read_line(&mut line);
    restore_echo(restore);
    eprintln!();
    result?;
    let trimmed = line.trim_end_matches(['\r', '\n']).len();
    line.truncate(trimmed);
    Ok(line)
}

pub fn stdin_is_console() -> bool {
    io::stdin().is_terminal()
}

#[cfg(windows)]
fn echo_off() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };
    let mut mode = 0;
    // SAFETY: GetStdHandle has no preconditions; GetConsoleMode writes one u32 to `mode`
    // and fails cleanly when the handle isn't a console.
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if GetConsoleMode(handle, &mut mode) == 0 {
            return None;
        }
        SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT);
    }
    Some(mode)
}

#[cfg(windows)]
fn restore_echo(mode: Option<u32>) {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
    if let Some(mode) = mode {
        // SAFETY: restores the mode read in `echo_off` on the same standard handle.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
        }
    }
}

#[cfg(not(windows))]
fn echo_off() -> Option<u32> {
    None
}

#[cfg(not(windows))]
fn restore_echo(_mode: Option<u32>) {}
