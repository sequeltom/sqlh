//! The execution core (PLAN.md 3.1): from the input to the batches' results. `RunPolicy` says
//! what applies to a run (the guard, the wrapper transaction, `--check`), `QueryOrigin` whether
//! the SQL is the user's or sqlh's own, and `RunOutcome` how it ended, apart from any sink.

use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::time::Instant;

use crate::app::Out;
use crate::cli::Opts;
use crate::config::{self, Settings};
use crate::console;
use crate::db::{self, Cancel, Counting, DbError, Session};
use crate::error::{Exit, Failure};
use crate::guard;
use crate::output::{self, BatchInfo};
use crate::script::{self, Batch, Source};
use crate::signals::{Signal, Signals};

/// Session options matching SSMS, so plans and filtered/computed-column indexes behave as
/// people expect (DESIGN.md §4.3). `XACT_ABORT` is set OFF explicitly, as SSMS has it: a
/// server whose `user options` setting turns it on for every session would otherwise hand sqlh
/// an ON session, and then the attention that stops a count (or `-t`) rolls the read-only
/// wrapper back (R14; DESIGN.md §4.4 and §7). It is in the same batch as the wrapper's
/// `BEGIN TRANSACTION` and ahead of it, so it costs no round trip.
const SESSION_SETUP: &str = "SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; \
    SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; SET QUOTED_IDENTIFIER ON; \
    SET NUMERIC_ROUNDABORT OFF; SET XACT_ABORT OFF;";

/// The failure for a read-only run whose wrapper transaction is gone after the last batch with
/// no batch having failed: nothing else says the run went wrong, so it must not be a success
/// (R14). `None` when the wrapper is there, when the run has no wrapper, or when a batch
/// failed and said so (a deadlock victim's rollback needs no second message).
fn wrapper_lost_unreported(
    wrap: bool,
    in_transaction: bool,
    a_batch_failed: bool,
) -> Option<Failure> {
    (wrap && !in_transaction && !a_batch_failed).then(|| {
        Failure::new(
            Exit::SqlError,
            "read-only guard: the wrapper transaction was gone after the last batch (something \
             rolled it back without an error); the run is not reported as a success",
        )
    })
}

/// Agent defaults on every connection, writes allowed or not (PLAN.md 2.8, owner decision
/// 7): sqlh's session is the one that loses a deadlock (error 1205) and gives up on a lock
/// after `--lock-timeout` (error 1222), so it gives way to production work.
const AGENT_DEFAULTS: &str = "SET DEADLOCK_PRIORITY LOW;";

/// Said once, as a note, in a run whose reads were uncommitted (DESIGN.md §4.3).
const READ_UNCOMMITTED_NOTE: &str = "reads were uncommitted (--read-uncommitted): rows may include uncommitted, duplicated or missing data";

/// The batch sent first on every connection: the SSMS options, the agent defaults, the lock
/// timeout, READ UNCOMMITTED if this run reads that way, and last the wrapper transaction of
/// a read-only run. The isolation level is set before the transaction begins, so it is the
/// transaction's from its first statement.
fn session_setup(lock_timeout: u32, read_uncommitted: bool, wrap: bool) -> String {
    let mut setup = format!(
        "{SESSION_SETUP} {AGENT_DEFAULTS} SET LOCK_TIMEOUT {};",
        config::lock_timeout_ms(lock_timeout)
    );
    if read_uncommitted {
        setup.push_str(" SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED;");
    }
    if wrap {
        setup.push_str(" BEGIN TRANSACTION;");
    }
    setup
}

/// What a run did, as far as its batches go.
#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) batches_run: usize,
    pub(crate) batches_skipped: usize,
    pub(crate) first_failed: Option<usize>,
}

/// What applies to a run, decided once from the flags and the settings: the read-only guard
/// (§7), the wrapper transaction, compile-only mode and whether later batches run after a
/// failed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunPolicy {
    /// `--allow-write`, or a profile whose `write_policy` is `allow`.
    pub(crate) allow_write: bool,
    /// `--check`: compile only (`SET NOEXEC ON`).
    pub(crate) check: bool,
    /// `--continue-on-error`.
    pub(crate) continue_on_error: bool,
}

impl RunPolicy {
    pub(crate) fn of(opts: &Opts, settings: &Settings) -> RunPolicy {
        RunPolicy {
            allow_write: settings.allow_write,
            check: opts.check,
            continue_on_error: opts.continue_on_error,
        }
    }

    /// What the guard checks in the user's SQL.
    fn guard(self) -> guard::Policy {
        guard::Policy {
            writes: self.allow_write,
            compile_only: self.check,
        }
    }

    /// Layer 2 of the read-only guard: everything runs in a transaction that is rolled back.
    /// `--check` always gets it too, as a second line of defense behind NOEXEC.
    fn wrap(self) -> bool {
        !self.allow_write || self.check
    }
}

/// Where a run's SQL comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueryOrigin {
    /// The user's: `-Q`, `-i` or stdin. `$(var)` is substituted and the guard checks it.
    User,
    /// sqlh's own, written for a command (`sqlh profile test`, and Phase 4's schema
    /// commands): taken as it is, with no `$(var)` and no guard, since sqlh knows what it
    /// wrote. It still runs inside the wrapper transaction, so a mistake in it can't write.
    Generated,
}

/// How a run ended, apart from the sink that showed it: the run's own result, and the counts
/// the closing lines need (DESIGN.md §5.1).
pub(crate) struct RunOutcome {
    pub(crate) result: Result<Stats, Failure>,
    /// The SQL errors the batches reported.
    pub(crate) errors: u32,
    /// The batches whose count of hidden rows was stopped (DESIGN.md §4.4).
    pub(crate) stopped_batches: usize,
}

impl RunOutcome {
    /// A result with no batch behind it: a failure before the run, or a command's own.
    pub(crate) fn new(result: Result<Stats, Failure>) -> RunOutcome {
        RunOutcome {
            result,
            errors: 0,
            stopped_batches: 0,
        }
    }

    /// The result of a run, with the counts its output's tally kept as the events went by.
    pub(crate) fn observed(result: Result<Stats, Failure>, tally: &output::Tally) -> RunOutcome {
        RunOutcome {
            result,
            errors: tally.errors(),
            stopped_batches: tally.stopped_batches(),
        }
    }

    /// The exit code (DESIGN.md §6): the failure's, else 1 if a batch reported a SQL error.
    pub(crate) fn exit(&self) -> Exit {
        match &self.result {
            Ok(_) if self.errors == 0 => Exit::Ok,
            Ok(_) => Exit::SqlError,
            Err(failure) => failure.exit,
        }
    }
}

/// Input, batches and the guard's verdict: everything decided before any output is written.
pub(crate) struct Prepared {
    pub(crate) sources: Vec<Source>,
    batches: Vec<Batch>,
    policy: RunPolicy,
    /// Where and why the guard refused the input, if it did.
    blocked: Option<(String, guard::Blocked)>,
}

pub(crate) fn prepare(
    opts: &Opts,
    settings: &Settings,
    origin: QueryOrigin,
) -> Result<Prepared, Failure> {
    let sources = load_sources(opts)?;
    let user = origin == QueryOrigin::User;
    let vars = (user && !opts.no_substitution).then_some(opts.vars.as_slice());
    let terminator = opts.terminator.as_deref().unwrap_or("GO");
    let batches = script::prepare(&sources, terminator, vars).map_err(Failure::usage)?;
    let policy = RunPolicy::of(opts, settings);
    let blocked = if user {
        batches.iter().find_map(|batch| {
            guard::check(&batch.sql, policy.guard())
                .err()
                .map(|blocked| {
                    let line = batch.source_line(blocked.line);
                    (location(&sources, batch.source, line), blocked)
                })
        })
    } else {
        None
    };
    Ok(Prepared {
        sources,
        batches,
        policy,
        blocked,
    })
}

/// Runs the input, or reports why it won't run. The signal, if any, is one that arrived
/// while the last batches finished, too late to change the outcome (`run_cancellable`).
/// `started` is the run clock: the batches are timed on it, as the closing lines are.
pub(crate) async fn execute(
    opts: &Opts,
    settings: &Settings,
    prepared: &Prepared,
    out: &mut Out,
    signals: &mut Result<Signals, String>,
    started: Instant,
) -> (RunOutcome, Option<Signal>) {
    let (result, late) = execute_inner(opts, settings, prepared, out, signals, started).await;
    (RunOutcome::observed(result, out.tally()), late)
}

async fn execute_inner(
    opts: &Opts,
    settings: &Settings,
    prepared: &Prepared,
    out: &mut Out,
    signals: &mut Result<Signals, String>,
    started: Instant,
) -> (Result<Stats, Failure>, Option<Signal>) {
    if let Some((location, blocked)) = &prepared.blocked {
        let advice = blocked.advice_for(settings.write_denied_by.as_deref());
        if let Err(e) = out.blocked(location, &blocked.reason, &advice) {
            return (Err(output_failure(e)), None);
        }
        return (Err(Failure::new(Exit::Blocked, "")), None);
    }
    if prepared.batches.is_empty() {
        let noted = out
            .note("no SQL to run")
            .map(|()| Stats::default())
            .map_err(output_failure);
        return (noted, None);
    }
    // Running without being able to cancel isn't an option, so a failure to listen ends the
    // run here (its output still gets delivered).
    let signals = match signals {
        Ok(signals) => signals,
        Err(message) => return (Err(Failure::new(Exit::Internal, message.clone())), None),
    };
    run_cancellable(opts, settings, prepared, out, signals, started).await
}

fn load_sources(opts: &Opts) -> Result<Vec<Source>, Failure> {
    if opts.query.is_some() && !opts.inputs.is_empty() {
        return Err(Failure::usage("give SQL with -Q or with -i, not both"));
    }
    if let Some(query) = &opts.query {
        return Ok(vec![Source {
            name: "-Q".into(),
            path: None,
            text: query.clone(),
        }]);
    }
    let mut sources = Vec::new();
    let mut left = script::MAX_INPUT_BYTES;
    for input in &opts.inputs {
        let (name, path, bytes) = if input == "-" {
            let bytes = read_capped(io::stdin().lock(), "stdin", &mut left)?;
            ("stdin".to_owned(), None, bytes)
        } else {
            let label = format!("-i {input}");
            let file = std::fs::File::open(input).map_err(|e| {
                Failure::usage(console::path_error(format!("{label}: {e}"), input, &e))
            })?;
            let bytes = read_capped(file, &label, &mut left)?;
            (input.clone(), Some(PathBuf::from(input)), bytes)
        };
        let text = decode_source(bytes, &name)?;
        sources.push(Source { name, path, text });
    }
    if sources.is_empty() {
        if io::stdin().is_terminal() {
            return Err(Failure::usage(
                "no SQL given: pass -Q \"...\", -i file.sql, or pipe SQL on stdin",
            ));
        }
        let bytes = read_capped(io::stdin().lock(), "stdin", &mut left)?;
        let text = decode_source(bytes, "stdin")?;
        sources.push(Source {
            name: "stdin".into(),
            path: None,
            text,
        });
    }
    Ok(sources)
}

/// Decodes one source (DESIGN.md §4.1). Piped text that isn't UTF-8 was encoded by the sending
/// shell, not saved by the user, so "save it as UTF-8" alone doesn't help: say what to change.
fn decode_source(bytes: Vec<u8>, name: &str) -> Result<String, Failure> {
    script::decode(bytes, name).map_err(|message| {
        if name == "stdin" && message.contains("not valid UTF-8") {
            Failure::usage(format!(
                "{message}. Piped text is encoded by the shell: in cmd.exe run chcp 65001 first, \
                 in Windows PowerShell run $OutputEncoding = [Text.Encoding]::UTF8 first, or pass \
                 the SQL with -Q or -i"
            ))
        } else {
            Failure::usage(message)
        }
    })
}

/// Reads all of `reader`, refusing to go past `left` bytes: the input limit is shared by all
/// of a run's sources (DESIGN.md §4.4). Reads at most one byte more than allowed.
fn read_capped(reader: impl Read, label: &str, left: &mut usize) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    reader
        .take(*left as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::usage(format!("{label}: {e}")))?;
    if bytes.len() > *left {
        return Err(Failure::usage(format!(
            "{label}: the SQL input is over the {} MB limit; run it in parts",
            script::MAX_INPUT_BYTES / 1_000_000
        )));
    }
    *left -= bytes.len();
    Ok(bytes)
}

/// `line 14` for a single source, `file.sql:14` when there are several.
fn location(sources: &[Source], source: usize, line: u32) -> String {
    if sources.len() > 1 {
        format!("{}:{line}", sources[source].name)
    } else {
        format!("line {line}")
    }
}

/// Runs the session, cancelling it on Ctrl+C, Ctrl+Break, the console closing, logoff or
/// shutdown. The error then says what sqlh observed: the statement was cancelled and the
/// connection stayed usable, or the connection was closed instead (DESIGN.md §4.3). The
/// listeners are the caller's, and outlive this: the closing lines still need them (R10).
///
/// The second value is the signal when it came too late to change the outcome (every batch
/// had run), so the caller can still hold the delivery to `DELIVERY_GRACE`.
async fn run_cancellable(
    opts: &Opts,
    settings: &Settings,
    prepared: &Prepared,
    out: &mut Out,
    signals: &mut Signals,
    started: Instant,
) -> (Result<Stats, Failure>, Option<Signal>) {
    let root = Cancel::new();
    let cancel = root.child();
    let work = run_session(opts, settings, prepared, out, &cancel, started);
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => (result, None),
        signal = signals.recv() => {
            // The cancel reaches the connect, every statement and sqlh's own output wait.
            // Whatever is still running when the grace ends is dropped, closing the
            // connection, which makes the server stop too.
            root.cancel();
            let grace = signal.grace();
            let detail = match tokio::time::timeout(grace, &mut work).await {
                // Every batch had run; the cancel came too late to change the outcome.
                Ok(Ok(stats)) => return (Ok(stats), Some(signal)),
                Ok(Err(failure)) if failure.exit == Exit::Cancelled => failure.message,
                Ok(Err(failure)) => format!("the run stopped: {}", failure.message),
                Err(_) => format!(
                    "nothing confirmed the cancel within {} s, so the connection was closed",
                    grace.as_secs()
                ),
            };
            let failure = Failure::new(
                Exit::Cancelled,
                format!("cancelled by {}: {detail}", signal.name()),
            );
            (Err(failure), None)
        }
    }
}

/// How a run counts the rows it does not show (DESIGN.md §4.4): for a bounded time, except
/// where a stopped count would lose something, and on request.
/// - **Writes allowed:** a stopped count skips the statements after it, and one can be the
///   `COMMIT`. Exact counting, and `-t` stays a failure (exit 4), since stopping there skips
///   the same statements.
/// - **`XACT_ABORT` in the input:** with it on, the attention rolls the transaction back, and
///   the read-only wrapper with it, which would end the run. Exact counting, and `-t` stays a
///   failure, since it sends the same attention.
/// - **`--exact-count`** or the profile's `exact_count`: exact counting. `-t` still ends a
///   count of rows nobody sees, in a run that can lose nothing by it.
fn counting_for(allow_write: bool, exact_requested: bool, xact_abort: bool) -> Counting {
    let base = if allow_write || exact_requested || xact_abort {
        Counting::EXACT
    } else {
        Counting::BOUNDED
    };
    Counting {
        timeout_stops: !allow_write && !xact_abort,
        ..base
    }
}

async fn run_session(
    opts: &Opts,
    settings: &Settings,
    prepared: &Prepared,
    out: &mut Out,
    cancel: &Cancel,
    started: Instant,
) -> Result<Stats, Failure> {
    let (sources, batches, policy) = (&prepared.sources, &prepared.batches, prepared.policy);
    let timeout = settings.query_timeout;
    // --verbose and `profile test` say where the time to the first batch went (PLAN.md 2.1).
    let report = opts.verbose || opts.show_connection;
    let connect_started = Instant::now();
    if report {
        db::phases::start();
    }
    let connected = Session::connect(&settings.connect, cancel).await;
    let steps = if report {
        db::phases::stop(connect_started)
    } else {
        Vec::new()
    };
    let mut session = connected.map_err(db_failure)?;
    let connect_ms = connect_started.elapsed().as_millis();
    // The output says where the run went, from what the login already told us: JSON in its
    // head, text and csv at the end of the status line (owner decision 10). Not said when
    // nothing connected, which is every path that returns before this point.
    out.connected(&session.connection(), connect_ms)
        .map_err(output_failure)?;
    let wrap = policy.wrap();
    // `--check` reads nothing, so there is no read to make uncommitted and none to report.
    let read_uncommitted = settings.read_uncommitted && !policy.check;
    let setup = session_setup(settings.lock_timeout, read_uncommitted, wrap);
    // Debug builds only: SQL sent ahead of the setup in the same batch, so that a test can
    // start from a session whose `XACT_ABORT` is ON (as a server's `user options` would make
    // it) without touching the server's configuration. The setup's OFF has to undo it (R14).
    #[cfg(debug_assertions)]
    let setup = match std::env::var("SQLH_DEBUG_SESSION_PREFIX") {
        Ok(prefix) => format!("{prefix} {setup}"),
        Err(_) => setup,
    };
    let setup_started = Instant::now();
    session
        .exec_internal(&setup, timeout, cancel)
        .await
        .map_err(db_failure)?;
    if read_uncommitted {
        // Said once, up front, where an output cut short by --max-output still has it.
        out.note(READ_UNCOMMITTED_NOTE).map_err(output_failure)?;
    }
    if report {
        let profile = settings.profile.as_deref().unwrap_or("(none)");
        let line = format!(
            "profile {profile}; connected in {connect_ms} ms{}, session setup {} ms; {}",
            db::phases::describe(&steps),
            setup_started.elapsed().as_millis(),
            session.describe()
        );
        if opts.show_connection {
            out.note(&line).map_err(output_failure)?;
        } else {
            out.diagnostic(&line).map_err(output_failure)?;
        }
    }

    let xact_abort = batches
        .iter()
        .any(|batch| guard::mentions_xact_abort(&batch.sql));
    let counting = counting_for(policy.allow_write, settings.exact_count, xact_abort);
    let mut stats = Stats::default();
    let gate = out.gate();
    let total: usize = batches.iter().map(|b| b.repeat as usize).sum();
    let mut number = 0;
    'batches: for batch in batches {
        for run in 1..=batch.repeat {
            number += 1;
            // A cancel ends the batch it reaches; this makes sure no later one starts.
            if cancel.is_cancelled() {
                return Err(Failure::new(
                    Exit::Cancelled,
                    format!("cancelled before batch {number} ran"),
                ));
            }
            if wrap && !session.in_transaction() {
                return Err(Failure::new(
                    Exit::SqlError,
                    "read-only guard: an error rolled back the wrapper transaction; stopping so nothing runs outside it",
                ));
            }
            if policy.check {
                // Re-armed before every batch. The guard already refuses input that mentions
                // NOEXEC; this keeps compile-only mode in force regardless.
                session
                    .exec_internal("SET NOEXEC ON;", timeout, cancel)
                    .await
                    .map_err(db_failure)?;
            }
            out.begin_batch(&BatchInfo {
                number,
                total,
                source: &sources[batch.source].name,
                multi_source: sources.len() > 1,
                line: batch.line,
                line_map: &batch.line_map,
                run,
                runs: batch.repeat,
                started_ms: started.elapsed().as_millis(),
            })
            .map_err(output_failure)?;
            session
                .run(
                    batch.sql.clone(),
                    timeout,
                    counting,
                    cancel,
                    &mut **out,
                    &gate,
                )
                .await
                .map_err(|e| match e {
                    DbError::Timeout(_) => Failure::new(
                        Exit::Timeout,
                        format!(
                            "query timeout after {timeout} s in batch {number}; raise -t or narrow the query"
                        ),
                    ),
                    e => db_failure(e),
                })?;
            stats.batches_run += 1;
            out.end_batch(started.elapsed().as_millis())
                .map_err(output_failure)?;
            out.flush().map_err(output_failure)?;
            if out.tally().batch_max_severity() >= 11 {
                stats.first_failed.get_or_insert(number);
                if !policy.continue_on_error {
                    stats.batches_skipped = total - number;
                    break 'batches;
                }
            }
        }
    }

    // The wrapper has to be there after the last batch too. It is checked before each batch
    // (above), which leaves the last one: a run whose wrapper was rolled back without an error
    // (an attention under XACT_ABORT ON) must not end as a success (R14). A batch that failed
    // already says so, and its rollback (a deadlock victim) needs no second message.
    if let Some(failure) =
        wrapper_lost_unreported(wrap, session.in_transaction(), stats.first_failed.is_some())
    {
        return Err(failure);
    }

    if policy.check {
        out.note(
            "--check compiled each batch without running it. Missing tables are only reported \
             when a query runs (SQL Server defers their name resolution)",
        )
        .map_err(output_failure)?;
        let _ = session
            .exec_internal("SET NOEXEC OFF;", timeout, cancel)
            .await;
    }
    if wrap && session.in_transaction() {
        let _ = session
            .exec_internal("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;", timeout, cancel)
            .await;
    }
    session.close().await;
    Ok(stats)
}

pub(crate) fn db_failure(error: DbError) -> Failure {
    match error {
        DbError::Connect(message) => Failure::new(Exit::Connect, message),
        DbError::Lost(message) => {
            Failure::new(Exit::Connect, format!("connection lost: {message}"))
        }
        DbError::Timeout(message) => Failure::new(Exit::Timeout, message),
        DbError::Cancelled(detail) => Failure::new(Exit::Cancelled, detail),
        DbError::Sql(message) => Failure::new(Exit::SqlError, message),
        DbError::Output(e) => output_failure(e),
        DbError::Protocol(message) => Failure::new(Exit::Internal, message),
    }
}

/// Writing output failed mid-run: the reader went away, or the `-o` file couldn't be written.
/// A sink that refused an event out of order (DESIGN.md §5.1) is not that: nothing is wrong
/// with the reader, and the order is a promise of sqlh and its driver, so it is a bug.
pub(crate) fn output_failure(error: io::Error) -> Failure {
    if output::OutOfOrder::is(&error) {
        return Failure::new(Exit::Internal, format!("internal error: {error}"));
    }
    Failure::new(Exit::Undelivered, format!("writing output failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // R14: a read-only run whose wrapper is gone after the last batch, with no batch having
    // failed, is a failure (exit 1); every other combination is left alone.
    #[test]
    fn a_wrapper_lost_without_a_failed_batch_is_a_failure() {
        let failure = wrapper_lost_unreported(true, false, false).expect("a failure");
        assert_eq!(failure.exit, Exit::SqlError);
        assert!(
            failure.message.starts_with(
                "read-only guard: the wrapper transaction was gone after the last batch"
            ),
            "{}",
            failure.message
        );
        // The wrapper is there; a batch failed and said so; the run has no wrapper.
        assert!(wrapper_lost_unreported(true, true, false).is_none());
        assert!(wrapper_lost_unreported(true, false, true).is_none());
        assert!(wrapper_lost_unreported(false, false, false).is_none());
        assert!(wrapper_lost_unreported(false, true, false).is_none());
    }

    // PLAN.md 2.8: the session setup, exactly. The agent defaults are in every connection,
    // writes allowed or not; READ UNCOMMITTED only when asked for, before the wrapper
    // transaction begins.
    #[test]
    fn the_session_setup_carries_the_agent_defaults() {
        let ssms = SESSION_SETUP;
        // R14, spelled out: the whole first batch of a read-only run, with XACT_ABORT OFF
        // before the wrapper transaction begins (and in an --allow-write run, which has no
        // wrapper, all the same).
        assert_eq!(
            session_setup(10, false, true),
            "SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; \
             SET CONCAT_NULL_YIELDS_NULL ON; SET QUOTED_IDENTIFIER ON; SET NUMERIC_ROUNDABORT OFF; \
             SET XACT_ABORT OFF; SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000; \
             BEGIN TRANSACTION;"
        );
        for read_uncommitted in [false, true] {
            let setup = session_setup(10, read_uncommitted, true);
            let off = setup
                .find("SET XACT_ABORT OFF;")
                .expect("XACT_ABORT is set");
            let begin = setup.find("BEGIN TRANSACTION;").expect("the wrapper");
            assert!(off < begin, "{setup}");
            assert!(!setup.contains("XACT_ABORT ON"), "{setup}");
        }
        assert!(session_setup(10, false, false).contains(" SET XACT_ABORT OFF; "));
        assert_eq!(
            session_setup(10, false, true),
            format!("{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000; BEGIN TRANSACTION;")
        );
        // --allow-write: no wrapper transaction, same defaults.
        assert_eq!(
            session_setup(10, false, false),
            format!("{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000;")
        );
        // A changed timeout, and 0 = no limit, which is -1 on the server.
        assert!(session_setup(3, false, true).contains(" SET LOCK_TIMEOUT 3000; "));
        assert!(session_setup(0, false, false).ends_with(" SET LOCK_TIMEOUT -1;"));
        assert_eq!(
            session_setup(2_147_483, false, false),
            format!("{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 2147483000;")
        );
        // Dirty reads: after the lock timeout, before the transaction is opened.
        assert_eq!(
            session_setup(10, true, true),
            format!(
                "{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000; \
                 SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; BEGIN TRANSACTION;"
            )
        );
        assert!(!session_setup(10, false, true).contains("ISOLATION"));
    }

    // PLAN.md 2.15, decision 6 (DESIGN.md §4.4): which runs stop their count, and which let `-t`
    // end it. Writes allowed, and `XACT_ABORT` in the input, count exactly and keep `-t` a
    // failure, because both would lose something to a stop (a skipped COMMIT, a rolled-back
    // wrapper); a request for exact counting changes the count only.
    #[test]
    fn the_run_picks_how_it_counts() {
        let bounded = Counting::BOUNDED;
        let exact = Counting::EXACT;
        let exact_but_for_timeouts = Counting {
            timeout_stops: true,
            ..exact
        };
        // (writes allowed, exact requested, XACT_ABORT in the input) and what each gets.
        let table = [
            ((false, false, false), bounded),
            ((false, true, false), exact_but_for_timeouts),
            ((false, false, true), exact),
            ((false, true, true), exact),
            ((true, false, false), exact),
            ((true, true, false), exact),
            ((true, false, true), exact),
            ((true, true, true), exact),
        ];
        for ((allow_write, requested, xact_abort), want) in table {
            assert_eq!(
                counting_for(allow_write, requested, xact_abort),
                want,
                "allow_write {allow_write}, exact_count {requested}, XACT_ABORT {xact_abort}"
            );
        }
        // The two kinds the table separates.
        assert!(bounded.budget.is_some() && bounded.timeout_stops);
        assert!(exact.budget.is_none() && !exact.timeout_stops);
    }

    /// Settings for a run that never connects: the tests here stop before the session.
    fn settings(allow_write: bool) -> Settings {
        Settings {
            connect: db::ConnectOptions {
                server: "127.0.0.1,1".into(),
                database: None,
                auth: db::Auth::Integrated,
                encrypt: db::Encrypt::Mandatory,
                trust_server_certificate: false,
                login_timeout: 1,
                read_only_intent: false,
                multi_subnet_failover: false,
            },
            limits: output::Limits::default(),
            query_timeout: 60,
            allow_write,
            write_denied_by: None,
            lock_timeout: 10,
            read_uncommitted: false,
            exact_count: false,
            profile: None,
        }
    }

    fn query(sql: &str) -> Opts {
        Opts {
            query: Some(sql.into()),
            vars: vec![("t".into(), "dbo.T".into())],
            ..Opts::default()
        }
    }

    // PLAN.md 3.1: the policy is decided once. The guard checks writes unless they are allowed,
    // and NOEXEC under --check; the wrapper is there unless writes are allowed, and always
    // under --check.
    #[test]
    fn the_run_policy_decides_the_guard_and_the_wrapper() {
        let policy = |allow_write, check| RunPolicy {
            allow_write,
            check,
            continue_on_error: false,
        };
        for (allow_write, check, wrap) in [
            (false, false, true),
            (true, false, false),
            (false, true, true),
            (true, true, true),
        ] {
            let p = policy(allow_write, check);
            assert_eq!(p.wrap(), wrap, "{p:?}");
            assert_eq!(p.guard().writes, allow_write, "{p:?}");
            assert_eq!(p.guard().compile_only, check, "{p:?}");
        }
        let opts = Opts {
            check: true,
            continue_on_error: true,
            ..Opts::default()
        };
        assert_eq!(
            RunPolicy::of(&opts, &settings(true)),
            RunPolicy {
                allow_write: true,
                check: true,
                continue_on_error: true,
            }
        );
    }

    // PLAN.md 3.1: the user's SQL gets $(var) and the guard; sqlh's own SQL (QueryOrigin::
    // Generated) is taken as it is, and is still in the policy that puts it in the wrapper.
    #[test]
    fn generated_sql_skips_substitution_and_the_guard_but_not_the_wrapper() {
        let opts = query("UPDATE $(t) SET x = 1");
        let user = prepare(&opts, &settings(false), QueryOrigin::User).unwrap();
        assert_eq!(user.batches[0].sql, "UPDATE dbo.T SET x = 1");
        let (location, blocked) = user.blocked.as_ref().expect("the guard refuses it");
        assert_eq!(location, "line 1");
        assert!(
            blocked.reason.contains("UPDATE dbo.T would write"),
            "{}",
            blocked.reason
        );

        let generated = prepare(&opts, &settings(false), QueryOrigin::Generated).unwrap();
        assert_eq!(generated.batches[0].sql, "UPDATE $(t) SET x = 1");
        assert!(generated.blocked.is_none());
        assert!(
            generated.policy.wrap(),
            "generated SQL still runs in the wrapper"
        );
    }

    // PLAN.md 3.1: the exit code comes from the outcome alone: the failure's, else 1 when a
    // batch reported a SQL error, else 0.
    #[test]
    fn the_outcome_gives_the_exit_code() {
        let ok = RunOutcome::new(Ok(Stats::default()));
        assert_eq!(ok.exit(), Exit::Ok);
        let sql_error = RunOutcome {
            errors: 2,
            ..RunOutcome::new(Ok(Stats::default()))
        };
        assert_eq!(sql_error.exit(), Exit::SqlError);
        let timeout = RunOutcome {
            errors: 2,
            ..RunOutcome::new(Err(Failure::new(Exit::Timeout, "query timeout")))
        };
        assert_eq!(timeout.exit(), Exit::Timeout);
    }
}
