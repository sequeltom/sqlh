//! Exit codes (DESIGN.md §6) and the failure type that carries one.

use std::process::ExitCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    Ok = 0,
    SqlError = 1,
    Connect = 2,
    Usage = 3,
    Timeout = 4,
    Blocked = 5,
    Cancelled = 6,
    /// The complete output, status line included, didn't reach its destination: the reader
    /// closed stdout early, or a write, flush or the final rename of `-o` failed. The SQL
    /// work may have run. Takes precedence over the run's own exit code (DESIGN.md §6).
    Undelivered = 7,
    Internal = 10,
}

impl Exit {
    /// Short reason used in the final `-- failed (exit N): ...` line.
    pub fn label(self) -> &'static str {
        match self {
            Exit::Ok => "ok",
            Exit::SqlError => "SQL error",
            Exit::Connect => "connection failed",
            Exit::Usage => "usage error (see sqlh --help)",
            Exit::Timeout => "timeout",
            Exit::Blocked => "blocked by the read-only guard",
            Exit::Cancelled => "cancelled",
            Exit::Undelivered => "output could not be delivered",
            Exit::Internal => "internal error",
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}

/// A run that stopped early. `message` is printed as `-- error: ...` before the summary.
#[derive(Debug)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Failure {
            exit,
            message: message.into(),
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Failure::new(Exit::Usage, message)
    }
}
