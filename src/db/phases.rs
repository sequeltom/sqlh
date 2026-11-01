//! How long each step of a connect took, for `--verbose` and `profiles test` (plan-history.md 2.1):
//! the TCP connection, the prelogin reply, the TLS handshake and the login, taken from the
//! driver's own log events. They arrive through the `log` bridge of the driver's `tracing`.
//!
//! Built only with `--features timing`. Listening to the driver's logging keeps all of its
//! log formatting in the exe (+270 KB, about 12%, measured 2026-10-10), so default builds
//! report the total connect time without the steps.
//!
//! Only the time each step ended is kept. Never print or store the events' text: at debug
//! level the driver dumps TDS packets before encryption, and the login packet carries the
//! password. The texts matched below are mssql-tds 0.3.0's (unchanged from 0.2.0); recheck them when bumping it.

use std::time::{Duration, Instant};

#[cfg(feature = "timing")]
pub use recorder::{start, stop};

/// Default builds don't time the steps.
#[cfg(not(feature = "timing"))]
pub fn start() {}

#[cfg(not(feature = "timing"))]
pub fn stop(_started: Instant) -> Vec<(&'static str, Duration)> {
    Vec::new()
}

/// ` (tcp 51, prelogin 4, tls 19, login 126)`, in milliseconds; empty when no step was timed
/// (a default build, or a local connection over shared memory or a named pipe).
pub fn describe(steps: &[(&str, Duration)]) -> String {
    if steps.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = steps
        .iter()
        .map(|(name, d)| format!("{name} {}", d.as_millis()))
        .collect();
    format!(" ({})", parts.join(", "))
}

#[cfg(any(feature = "timing", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The driver starts a connection attempt; a second one means the first failed.
    Attempt,
    Tcp,
    /// The first packet received after the TCP connection: the prelogin reply.
    Prelogin,
    Tls,
    Login,
}

#[cfg(any(feature = "timing", test))]
fn is_watched(target: &str) -> bool {
    target.ends_with("::network_transport")
        || target.ends_with("::win_tls::handshake")
        || target.ends_with("::tds_connection_provider")
}

/// The step a driver event ends, if any.
#[cfg(any(feature = "timing", test))]
fn classify(target: &str, text: &str) -> Option<Step> {
    if target.ends_with("::tds_connection_provider") {
        if text.starts_with("Attempt ") && text.contains(": connecting with") {
            return Some(Step::Attempt);
        }
        if text.starts_with("Connection successful") {
            return Some(Step::Login);
        }
    } else if target.ends_with("::network_transport") {
        if text.starts_with("Connected to TCP transport") {
            return Some(Step::Tcp);
        }
        if text.starts_with("Received packet of size") {
            return Some(Step::Prelogin);
        }
    } else if target.ends_with("::win_tls::handshake")
        && text.starts_with("win_tls: handshake complete")
    {
        return Some(Step::Tls);
    }
    None
}

/// Each step's duration since the step before it. A repeated step is ignored (only the first
/// packet received is the prelogin reply), and a later attempt means the earlier one failed:
/// its time is reported as `retry`.
#[cfg(any(feature = "timing", test))]
fn durations(started: Instant, marks: &[(Step, Instant)]) -> Vec<(&'static str, Duration)> {
    let mut out = Vec::new();
    let mut seen: Vec<Step> = Vec::new();
    let mut last = started;
    for &(step, at) in marks {
        let name = match step {
            Step::Attempt if seen.is_empty() => continue,
            Step::Attempt => {
                seen.clear();
                "retry"
            }
            _ if seen.contains(&step) => continue,
            Step::Tcp => "tcp",
            Step::Prelogin => "prelogin",
            Step::Tls => "tls",
            Step::Login => "login",
        };
        if step != Step::Attempt {
            seen.push(step);
        }
        out.push((name, at.saturating_duration_since(last)));
        last = at;
    }
    out
}

#[cfg(feature = "timing")]
mod recorder {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::{Step, classify, durations, is_watched};

    static MARKS: Mutex<Vec<(Step, Instant)>> = Mutex::new(Vec::new());
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    struct Recorder;

    impl log::Log for Recorder {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            is_watched(metadata.target())
        }

        fn log(&self, record: &log::Record) {
            let target = record.target();
            if !is_watched(target) {
                return;
            }
            let text = match record.args().as_str() {
                Some(text) => std::borrow::Cow::Borrowed(text),
                None => std::borrow::Cow::Owned(record.args().to_string()),
            };
            if let Some(step) = classify(target, &text)
                && let Ok(mut marks) = MARKS.lock()
            {
                marks.push((step, Instant::now()));
            }
        }

        fn flush(&self) {}
    }

    /// Starts recording before a connect. Logging stays off otherwise.
    pub fn start() {
        static RECORDER: Recorder = Recorder;
        if !INSTALLED.swap(true, Ordering::SeqCst) && log::set_logger(&RECORDER).is_err() {
            return;
        }
        if let Ok(mut marks) = MARKS.lock() {
            marks.clear();
        }
        log::set_max_level(log::LevelFilter::Debug);
    }

    /// Stops recording, and gives each step that was seen with how long it took, counting
    /// from `started`.
    pub fn stop(started: Instant) -> Vec<(&'static str, Duration)> {
        log::set_max_level(log::LevelFilter::Off);
        let marks = MARKS
            .lock()
            .map(|mut marks| std::mem::take(&mut *marks))
            .unwrap_or_default();
        durations(started, &marks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSPORT: &str = "mssql_tds::connection::transport::network_transport";
    const PROVIDER: &str = "mssql_tds::connection_provider::tds_connection_provider";
    const HANDSHAKE: &str = "mssql_tds::connection::transport::win_tls::handshake";

    #[test]
    fn classifies_the_driver_events() {
        let cases = [
            (
                PROVIDER,
                "Attempt 0: connecting with Tcp { .. }",
                Some(Step::Attempt),
            ),
            (
                TRANSPORT,
                "Connected to TCP transport: db:1433",
                Some(Step::Tcp),
            ),
            (
                TRANSPORT,
                "Received packet of size: 54",
                Some(Step::Prelogin),
            ),
            (
                HANDSHAKE,
                "win_tls: handshake complete kind=NoValidate steps=4",
                Some(Step::Tls),
            ),
            (
                PROVIDER,
                "Connection successful via action chain",
                Some(Step::Login),
            ),
            (TRANSPORT, "Packet content: Length: 54 (0x36) bytes", None),
            (TRANSPORT, "Socket addresses: [db:1433]", None),
            (
                "mssql_tds::io::packet_writer",
                "Received packet of size: 9",
                None,
            ),
        ];
        for (target, text, step) in cases {
            assert_eq!(classify(target, text), step, "{target}: {text}");
        }
        assert!(!is_watched("mssql_tds::io::packet_writer"));
        assert!(!is_watched("mssql_tds::message::login"));
    }

    #[test]
    fn steps_take_the_time_since_the_one_before() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let ms = |d: &[(&str, Duration)]| -> Vec<(String, u128)> {
            d.iter()
                .map(|(n, d)| (n.to_string(), d.as_millis()))
                .collect()
        };
        let marks = [
            (Step::Attempt, at(1)),
            (Step::Tcp, at(52)),
            (Step::Prelogin, at(56)),
            (Step::Tls, at(75)),
            (Step::Prelogin, at(201)),
            (Step::Login, at(201)),
        ];
        assert_eq!(
            ms(&durations(t0, &marks)),
            [
                ("tcp".into(), 52),
                ("prelogin".into(), 4),
                ("tls".into(), 19),
                ("login".into(), 126)
            ]
        );
        // A failed first attempt shows as a retry; steps may be missing (no TLS here).
        let marks = [
            (Step::Attempt, at(0)),
            (Step::Tcp, at(10)),
            (Step::Attempt, at(1000)),
            (Step::Tcp, at(1003)),
            (Step::Login, at(1130)),
        ];
        assert_eq!(
            ms(&durations(t0, &marks)),
            [
                ("tcp".into(), 10),
                ("retry".into(), 990),
                ("tcp".into(), 3),
                ("login".into(), 127)
            ]
        );
    }

    #[test]
    fn describes_the_steps() {
        let ms = Duration::from_millis;
        assert_eq!(
            describe(&[("tcp", ms(51)), ("tls", ms(19)), ("login", ms(126))]),
            " (tcp 51, tls 19, login 126)"
        );
        assert_eq!(describe(&[]), "");
    }
}
