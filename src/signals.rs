//! The console events that cancel a run (PLAN.md 2.4, review F08): Ctrl+C, Ctrl+Break, the
//! console window closing, logoff and shutdown.

use std::io;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    CtrlC,
    CtrlBreak,
    Close,
    Logoff,
    Shutdown,
}

impl Signal {
    pub fn name(self) -> &'static str {
        match self {
            Signal::CtrlC => "Ctrl+C",
            Signal::CtrlBreak => "Ctrl+Break",
            Signal::Close => "the console closing",
            Signal::Logoff => "logoff",
            Signal::Shutdown => "shutdown",
        }
    }

    /// How long the cancelled work gets to finish. Windows ends the process about 5 s after
    /// a close, logoff or shutdown event, so those get less, leaving time to report and exit.
    pub fn grace(self) -> Duration {
        match self {
            Signal::CtrlC | Signal::CtrlBreak => Duration::from_secs(2),
            Signal::Close | Signal::Logoff | Signal::Shutdown => Duration::from_secs(1),
        }
    }
}

/// Listens for all of them. Needs a runtime with signals enabled.
#[cfg(windows)]
pub struct Signals {
    c: tokio::signal::windows::CtrlC,
    brk: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    logoff: tokio::signal::windows::CtrlLogoff,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl Signals {
    pub fn new() -> io::Result<Signals> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
        Ok(Signals {
            c: ctrl_c()?,
            brk: ctrl_break()?,
            close: ctrl_close()?,
            logoff: ctrl_logoff()?,
            shutdown: ctrl_shutdown()?,
        })
    }

    pub async fn recv(&mut self) -> Signal {
        tokio::select! {
            Some(()) = self.c.recv() => Signal::CtrlC,
            Some(()) = self.brk.recv() => Signal::CtrlBreak,
            Some(()) = self.close.recv() => Signal::Close,
            Some(()) = self.logoff.recv() => Signal::Logoff,
            Some(()) = self.shutdown.recv() => Signal::Shutdown,
            else => std::future::pending().await,
        }
    }
}

#[cfg(not(windows))]
pub struct Signals;

#[cfg(not(windows))]
impl Signals {
    pub fn new() -> io::Result<Signals> {
        Ok(Signals)
    }

    pub async fn recv(&mut self) -> Signal {
        match tokio::signal::ctrl_c().await {
            Ok(()) => Signal::CtrlC,
            Err(_) => std::future::pending().await,
        }
    }
}
