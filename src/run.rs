//! The execution core (plan-history.md 3.1): from the input to the batches' results. A `Request`
//! says what to run, `RunPolicy` what applies to it (the guard, the wrapper transaction,
//! `--check`), `QueryOrigin` whether the SQL is the user's or sqlh's own, and `RunOutcome` how it
//! ended. Nothing here depends on the command line or on how output is delivered: the CLI (`app`)
//! builds the `Request`, and the run writes to any `Output`, waits on any `Backpressure`, and
//! counts its errors itself (`Accounting`), so other callers (Phase 4's schema commands, Phase
//! 5's connection reuse) can drive it the same way.

use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::time::Instant;

use crate::config::{self, Settings};
use crate::console;
use crate::db::{
    self, Backpressure, BatchSink, Cancel, Column, Counting, DbError, ServerMessage, Session, Stop,
    Value,
};
use crate::error::{Exit, Failure};
use crate::guard;
use crate::output::{self, BatchInfo, Output, Section, Summary};
use crate::script::{self, Batch, Source};
use crate::signals::{Signal, Signals};

/// Where a run's SQL comes from.
pub(crate) enum Input {
    /// The SQL itself (`-Q`).
    Query(String),
    /// Files, in order (`-i`); `-` is stdin.
    Files(Vec<String>),
    /// Nothing named: stdin, unless it is a console.
    Stdin,
    /// A schema command's own SQL (schema-commands.md); `name` is what errors and the batch
    /// lines call its source.
    Command { name: &'static str, plan: Plan },
}

/// One batch of sqlh's own SQL, and the sections its result sets fill, in order.
#[derive(Debug)]
pub(crate) struct Step {
    pub(crate) sql: String,
    pub(crate) sections: Vec<Section>,
}

/// The rows a lookup returned: for each of its result sets, its rows.
pub(crate) type Found = Vec<Vec<Vec<Value>>>;

/// What decides a lookup's steps from its rows, or why there are none.
pub(crate) type Then = Box<dyn Fn(&Found) -> Result<Vec<Step>, Failure>>;

/// A command's SQL: steps known in advance (`tables`), or a lookup whose rows decide them
/// (`describe` resolves a name first, and what it describes depends on the object's type).
/// The lookup runs on the run's own connection, inside its wrapper transaction, and shows
/// nothing: its rows go to `then`, and an error in it ends the run as a failure that `label`
/// words.
///
/// A lookup may hold several statements and fill `#temp` tables, which its steps read: same
/// session, same wrapper. A command that does its server work there and whose step is one
/// `SELECT` keeps `guard::several_outputs` false for the step, so a capped result gets the
/// brief count and no `-- partial:` advice, which a command's user can't act on (it has no
/// `GO` to add; decision 45).
pub(crate) enum Plan {
    Steps(Vec<Step>),
    Lookup {
        sql: String,
        then: Then,
        label: LookupLabel,
    },
}

/// What a lookup does, as its failures say it: a timeout, the server's error, a reply past
/// the `CAPTURE_` bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LookupLabel {
    /// `describe`, `def` and `deps` resolving a name.
    Name,
    /// `activity` copying the server's sessions.
    Activity,
    /// `errorlog` copying the log; `since` is whether `--since` chose the archives.
    ErrorLog { since: bool },
}

impl LookupLabel {
    /// The lookup as something being done: `while looking the name up`, `... failed`.
    fn doing(self) -> &'static str {
        match self {
            LookupLabel::Name => "looking the name up",
            LookupLabel::Activity => "reading the server's sessions",
            LookupLabel::ErrorLog { .. } => "reading the error log",
        }
    }

    /// The lookup as a thing: `the name lookup returned more than it should`.
    fn noun(self) -> &'static str {
        match self {
            LookupLabel::Name => "the name lookup",
            LookupLabel::Activity => "the read of the server's sessions",
            LookupLabel::ErrorLog { .. } => "the error log read",
        }
    }

    /// What a timeout's advice adds to raising the timeout. The error log is read an archive at
    /// a time, each whole, and a search filters only what was copied (decision 89), so a
    /// narrower search doesn't help; a later `--since` reads fewer archives.
    fn after_timeout(self, advice: output::Advice) -> Option<&'static str> {
        use output::Advice::{Cli, Mcp};
        match (self, advice) {
            (LookupLabel::Name | LookupLabel::Activity, _) => None,
            (LookupLabel::ErrorLog { since: true }, Cli) => {
                Some("a later --since reads fewer archives, each one whole even with --search")
            }
            (LookupLabel::ErrorLog { since: true }, Mcp) => {
                Some("a later since reads fewer archives, each one whole even with search")
            }
            (LookupLabel::ErrorLog { since: false }, Cli) => {
                Some("the archive is read whole, even with --search")
            }
            (LookupLabel::ErrorLog { since: false }, Mcp) => {
                Some("the archive is read whole, even with search")
            }
        }
    }
}

/// Where the line about the connection goes: nowhere, to stderr as a diagnostic
/// (`--verbose`), or into the output as a note (`profiles test`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    Off,
    Diagnostic,
    Note,
}

/// What to run and how, apart from the connection settings: everything the run core takes from
/// the command line, which builds it (`app`).
pub(crate) struct Request {
    pub(crate) input: Input,
    /// Values for `$(var)` (`-v`; environment variables resolve too). `None` turns substitution
    /// off (`-x`). sqlh's own SQL is never substituted, whatever this says.
    pub(crate) vars: Option<Vec<(String, String)>>,
    /// The batch terminator (`-c`).
    pub(crate) terminator: String,
    /// `--check`: compile only.
    pub(crate) check: bool,
    /// `--continue-on-error`.
    pub(crate) continue_on_error: bool,
    pub(crate) report: Report,
    /// A database to switch to for this run (`USE`), after the session setup: an MCP call's
    /// `database`. The CLI connects to its database instead (`-d`), so it passes `None`.
    pub(crate) database: Option<String>,
    /// Count every hidden row, on top of the settings' `exact_count`: an MCP call's
    /// `exact_count` (decision 45). The CLI's `--exact-count` is in the settings, so it passes
    /// `false`.
    pub(crate) exact_count: bool,
}

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

/// Agent defaults on every connection, writes allowed or not (plan-history.md 2.8, owner decision
/// 7): sqlh's session is the one that loses a deadlock (error 1205) and gives up on a lock
/// after `--lock-timeout` (error 1222), so it gives way to production work.
const AGENT_DEFAULTS: &str = "SET DEADLOCK_PRIORITY LOW;";

/// What a dirty read can do to the rows, in both of the notes below.
const DIRTY_ROWS: &str = "rows may include uncommitted, duplicated or missing data";

/// Said once, as a note, in a run whose reads were uncommitted (DESIGN.md §4.3).
fn read_uncommitted_note(advice: output::Advice) -> String {
    let asked = match advice {
        output::Advice::Cli => "--read-uncommitted",
        output::Advice::Mcp => "the profile's read_uncommitted = true",
    };
    format!("reads were uncommitted ({asked}): {DIRTY_ROWS}")
}

/// The note for a run whose SQL itself reads uncommitted (decision 30(a), DESIGN.md §4.3), naming
/// each thing that does, so the reader knows what to take out. The CLI and `sqlh mcp` word it
/// alike, since it names no setting. `None` when nothing does.
fn uncommitted_in_sql_note(reads: guard::UncommittedReads) -> Option<String> {
    let sources = [
        (reads.nolock, "NOLOCK"),
        (reads.readuncommitted, "READUNCOMMITTED"),
        (
            reads.isolation,
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        ),
    ]
    .iter()
    .filter(|(found, _)| *found)
    .map(|(_, name)| *name)
    .collect::<Vec<_>>()
    .join(", ");
    (!sources.is_empty())
        .then(|| format!("some reads were uncommitted ({sources} in the SQL): {DIRTY_ROWS}"))
}

/// The batch sent first on every connection: the SSMS options, the agent defaults, the lock
/// timeout, the isolation level, and last the wrapper transaction of a read-only run. The
/// isolation level is set before the transaction begins, so it is the transaction's from its
/// first statement.
///
/// The level is set every time, not only for READ UNCOMMITTED: on SQL Server (all but Azure SQL
/// Database) the connection reset a reused connection gets (`Session::reset_before_next`,
/// `sp_reset_connection`) restores the login's options but not the isolation level, so a call
/// that ran `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` (which the guard allows) would
/// otherwise hand that level to the next call on its connection. READ COMMITTED is a fresh
/// login's level, and still reads row versions where the database has READ_COMMITTED_SNAPSHOT
/// on, so a run on a new connection reads as it did before.
pub(crate) fn session_setup(lock_timeout: u32, read_uncommitted: bool, wrap: bool) -> String {
    let mut setup = format!(
        "{SESSION_SETUP} {AGENT_DEFAULTS} SET LOCK_TIMEOUT {};",
        config::lock_timeout_ms(lock_timeout)
    );
    if read_uncommitted {
        setup.push_str(" SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED;");
    } else {
        setup.push_str(" SET TRANSACTION ISOLATION LEVEL READ COMMITTED;");
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
    /// `sqlh mcp`: the session goes on to the next call (`guard::Policy::reused_session`).
    pub(crate) reused_session: bool,
}

impl RunPolicy {
    pub(crate) fn of(request: &Request, settings: &Settings, origin: QueryOrigin) -> RunPolicy {
        RunPolicy {
            // sqlh's own SQL only reads, so it never runs with writes allowed, whatever the
            // profile's `write_policy`: it keeps the wrapper transaction even under `allow`.
            allow_write: settings.allow_write && origin == QueryOrigin::User,
            check: request.check,
            continue_on_error: request.continue_on_error,
            reused_session: settings.writes_off == Some(guard::WritesOff::Mcp),
        }
    }

    /// What the guard checks in the user's SQL.
    fn guard(self) -> guard::Policy {
        guard::Policy {
            writes: self.allow_write,
            compile_only: self.check,
            reused_session: self.reused_session,
        }
    }

    /// Layer 2 of the read-only guard: everything runs in a transaction that is rolled back.
    /// `--check` always gets it too, as a second line of defense behind NOEXEC.
    fn wrap(self) -> bool {
        !self.allow_write || self.check
    }

    /// The login's name for a CLI run (decision 30(c)), by what it may do rather than by the
    /// flag: under the wrapper nothing it does is kept, so `--check --allow-write`, and sqlh's
    /// own SQL under a profile that allows writes, are read-only runs.
    fn application_name(self) -> &'static str {
        if self.wrap() {
            db::APP_READ_ONLY
        } else {
            db::APP_WRITES
        }
    }
}

/// Where a run's SQL comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueryOrigin {
    /// The user's: `-Q`, `-i` or stdin. `$(var)` is substituted and the guard checks it.
    User,
    /// sqlh's own, written for a command (`sqlh profiles test`, and Phase 4's schema
    /// commands): taken as it is, with no `$(var)` and no guard, since sqlh knows what it
    /// wrote. It always runs as a read-only run, inside the wrapper transaction, even under a
    /// profile that allows writes (`RunPolicy::of`), so a mistake in it can't write.
    Generated,
}

/// How a run ended, apart from the sink that showed it: the run's own result, and the counts
/// the closing lines need (DESIGN.md §5.1), which the run kept itself (`Accounting`).
pub(crate) struct RunOutcome {
    pub(crate) result: Result<Stats, Failure>,
    /// The SQL errors the batches reported.
    pub(crate) errors: u32,
    /// The batches whose count of hidden rows was stopped (DESIGN.md §4.4).
    pub(crate) stopped_batches: usize,
    /// Of those, the batches whose later statements the stop cancelled (decision 45), by number.
    pub(crate) cut_batches: Vec<usize>,
}

impl RunOutcome {
    /// A result with no batch behind it: a failure before the run, or a command's own.
    pub(crate) fn new(result: Result<Stats, Failure>) -> RunOutcome {
        RunOutcome {
            result,
            errors: 0,
            stopped_batches: 0,
            cut_batches: Vec::new(),
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

/// How a run ends: its exit code, the status line, and the error that stopped it, if any.
pub(crate) struct Closing {
    pub(crate) exit: Exit,
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    elapsed_ms: u128,
}

impl Closing {
    /// `ok: <detail>, N ms`.
    pub(crate) fn ok(detail: &str, started: Instant) -> Closing {
        let elapsed_ms = started.elapsed().as_millis();
        Closing {
            exit: Exit::Ok,
            status: format!("ok: {detail}, {elapsed_ms} ms"),
            error: None,
            elapsed_ms,
        }
    }

    /// The closing lines of `outcome`; `advice` words what they suggest (`output::Advice`).
    pub(crate) fn of(outcome: RunOutcome, started: Instant, advice: output::Advice) -> Closing {
        let ms = started.elapsed().as_millis();
        let exit = outcome.exit();
        // A stopped count is in the status line, and so are the batches whose rest it
        // cancelled, so that a reader of that line alone learns that part of the input didn't
        // run (DESIGN.md §5.1, decision 45).
        let mut stopped = match outcome.stopped_batches {
            0 => String::new(),
            n => format!(", counting stopped in {n}"),
        };
        stopped.push_str(&rest_not_run(&outcome.cut_batches));
        let errors = outcome.errors;
        match outcome.result {
            // Exit 0, but the input didn't all run: `partial`, never `ok` (decision 45).
            Ok(stats) if exit == Exit::Ok && !outcome.cut_batches.is_empty() => {
                let s = if stats.batches_run == 1 { "" } else { "es" };
                Closing {
                    exit,
                    status: format!("partial: {} batch{s}{stopped}, {ms} ms", stats.batches_run),
                    error: None,
                    elapsed_ms: ms,
                }
            }
            Ok(stats) if exit == Exit::Ok => {
                let s = if stats.batches_run == 1 { "" } else { "es" };
                Closing::ok(&format!("{} batch{s}{stopped}", stats.batches_run), started)
            }
            Ok(stats) => {
                let s = if errors == 1 { "" } else { "s" };
                let mut status = format!("failed (exit 1): {errors} error{s}");
                if let Some(batch) = stats.first_failed {
                    status.push_str(&format!(", first in batch {batch}"));
                }
                if stats.batches_skipped > 0 {
                    status.push_str(&format!(
                        "; {} later batches not run{}",
                        stats.batches_skipped,
                        match advice {
                            output::Advice::Cli => " (--continue-on-error runs them)",
                            output::Advice::Mcp => "",
                        }
                    ));
                }
                status.push_str(&format!("{stopped}, {ms} ms"));
                Closing {
                    exit,
                    status,
                    error: None,
                    elapsed_ms: ms,
                }
            }
            Err(failure) => {
                let code = exit as u8;
                Closing {
                    exit,
                    status: format!(
                        "failed (exit {code}): {}{stopped}, {ms} ms",
                        match (failure.exit, advice) {
                            (Exit::Usage, output::Advice::Mcp) => "usage error",
                            (exit, _) => exit.label(),
                        }
                    ),
                    error: (!failure.message.is_empty()).then_some(failure.message),
                    elapsed_ms: ms,
                }
            }
        }
    }

    pub(crate) fn summary(&self) -> Summary<'_> {
        Summary {
            exit: self.exit as u8,
            status: &self.status,
            error: self.error.as_deref(),
            elapsed_ms: self.elapsed_ms,
        }
    }
}

/// The most batches the status line names whose rest a stopped count cancelled; the rest are
/// counted. The status line is never shortened, so a run of a thousand repeats must not make
/// it long.
const CUT_BATCHES_NAMED: usize = 5;

/// `, the rest of batch 2 not run`, `, the rest of batches 1, 3 not run`, or nothing.
fn rest_not_run(cut: &[usize]) -> String {
    let named: Vec<String> = cut
        .iter()
        .take(CUT_BATCHES_NAMED)
        .map(usize::to_string)
        .collect();
    match named.len() {
        0 => String::new(),
        1 => format!(", the rest of batch {} not run", named[0]),
        _ => {
            let more = cut.len() - named.len();
            let more = if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            };
            format!(", the rest of batches {}{more} not run", named.join(", "))
        }
    }
}

/// Ends `out` with the outcome's closing lines (the error, the limit notes and the status
/// line), for an output with no delivery to wait for: an MCP call's, in memory. Returns the
/// exit code; an output that refused the closing lines out of order is an internal error.
pub(crate) fn close(
    out: &mut dyn Output,
    outcome: RunOutcome,
    started: Instant,
    advice: output::Advice,
) -> Exit {
    let closing = Closing::of(outcome, started, advice);
    match out.finish(&closing.summary()).and_then(|()| out.flush()) {
        Ok(()) => closing.exit,
        Err(error) => output_failure(error).exit,
    }
}

/// What the run counts for itself as its batches' events go by: the SQL errors, the worst
/// severity in the batch being run (which decides whether it failed), and the batches whose
/// count of hidden rows was stopped. The exit code and the closing lines come from these, so
/// they don't depend on what the output kept.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Accounting {
    errors: u32,
    batch_max_severity: i32,
    batch_stopped: bool,
    /// The batch being run: its number, and whether a stop would cancel a later statement's
    /// output (`guard::several_outputs`).
    batch_number: usize,
    batch_later_statements: bool,
    stopped_batches: usize,
    /// The batches whose stopped count cancelled the rest (decision 45), by number.
    cut_batches: Vec<usize>,
}

impl Accounting {
    /// The run's outcome: `result`, with the counts the run kept.
    pub(crate) fn outcome(self, result: Result<Stats, Failure>) -> RunOutcome {
        RunOutcome {
            result,
            errors: self.errors,
            stopped_batches: self.stopped_batches,
            cut_batches: self.cut_batches,
        }
    }

    fn begin_batch(&mut self, number: usize, later_statements: bool) {
        self.batch_max_severity = 0;
        self.batch_stopped = false;
        self.batch_number = number;
        self.batch_later_statements = later_statements;
    }

    /// Whether the batch being run reported an error (severity 11 or more).
    fn batch_failed(&self) -> bool {
        self.batch_max_severity >= 11
    }
}

/// The sink the driver writes a batch to: the output, with the run's `Accounting` on the way.
/// An event is counted once the output has taken it, as the output's own tally counts it: an
/// error the output refused out of order (a bug, which ends the run) isn't counted, and one
/// the output budget withheld is.
struct Counted<'a> {
    out: &'a mut dyn Output,
    counts: &'a mut Accounting,
}

impl BatchSink for Counted<'_> {
    fn columns(&mut self, columns: &[Column]) -> io::Result<()> {
        self.out.columns(columns)
    }
    fn wants_values(&self) -> bool {
        self.out.wants_values()
    }
    fn cell_limit(&self) -> usize {
        self.out.cell_limit()
    }
    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        self.out.row(values)
    }
    fn skipped_row(&mut self) {
        self.out.skipped_row();
    }
    fn counting_stopped(&mut self, by: Stop) {
        self.out.counting_stopped(by);
        let counts = &mut *self.counts;
        if !counts.batch_stopped {
            counts.batch_stopped = true;
            counts.stopped_batches += 1;
            if counts.batch_later_statements {
                counts.cut_batches.push(counts.batch_number);
            }
        }
    }
    fn end_rows(&mut self) -> io::Result<()> {
        self.out.end_rows()
    }
    fn end_rows_incomplete(&mut self) -> io::Result<()> {
        self.out.end_rows_incomplete()
    }
    fn rows_affected(&mut self, count: u64) -> io::Result<()> {
        self.out.rows_affected(count)
    }
    fn message(&mut self, message: &ServerMessage) -> io::Result<()> {
        self.out.message(message)
    }
    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        self.out.error(error)?;
        self.counts.errors += 1;
        self.counts.batch_max_severity = self.counts.batch_max_severity.max(error.severity);
        Ok(())
    }
}

/// Input, batches and the guard's verdict: everything decided before any output is written.
pub(crate) struct Prepared {
    pub(crate) sources: Vec<Source>,
    batches: Vec<Batch>,
    /// The sections each batch's result sets fill; empty for the user's SQL.
    sections: Vec<Vec<Section>>,
    /// A command's lookup (`Plan::Lookup`): its SQL, and what makes the batches of its rows.
    lookup: Option<(String, Then, LookupLabel)>,
    policy: RunPolicy,
    report: Report,
    /// Where and why the guard refused the input, if it did.
    blocked: Option<(String, guard::Blocked)>,
    /// What in the user's SQL reads uncommitted, for the run's note; nothing where no such note
    /// is said (`prepare`).
    uncommitted_reads: guard::UncommittedReads,
    /// `Request::database`.
    database: Option<String>,
    /// `Request::exact_count`.
    exact_count: bool,
}

impl Prepared {
    /// `RunPolicy::application_name`.
    pub(crate) fn application_name(&self) -> &'static str {
        self.policy.application_name()
    }

    /// What its lookup does, for a command that has one.
    pub(crate) fn lookup_label(&self) -> Option<LookupLabel> {
        self.lookup.as_ref().map(|(_, _, label)| *label)
    }

    /// Whether the request asked to count every hidden row (`Request::exact_count`).
    #[cfg(test)]
    pub(crate) fn exact_count(&self) -> bool {
        self.exact_count
    }
}

/// Settings for a run that never connects, for tests that stop before the session.
#[cfg(test)]
pub(crate) fn unconnected_settings(allow_write: bool) -> Settings {
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
            hints: db::Hints::Flags,
            application_name: db::APP_READ_ONLY,
        },
        limits: output::Limits::default(),
        query_timeout: 60,
        allow_write,
        writes_off: None,
        lock_timeout: 10,
        read_uncommitted: false,
        exact_count: false,
        profile: None,
    }
}

/// A command's steps as batches of its one source, with their sections.
fn step_batches(steps: Vec<Step>) -> (Vec<Batch>, Vec<Vec<Section>>) {
    steps
        .into_iter()
        .map(|step| {
            let batch = Batch {
                sql: step.sql,
                source: 0,
                line: 1,
                repeat: 1,
                line_map: Vec::new(),
            };
            (batch, step.sections)
        })
        .unzip()
}

pub(crate) fn prepare(
    request: Request,
    settings: &Settings,
    origin: QueryOrigin,
) -> Result<Prepared, Failure> {
    let policy = RunPolicy::of(&request, settings, origin);
    if let Input::Command { name, plan } = request.input {
        let sources = vec![Source {
            name: name.into(),
            path: None,
            text: String::new(),
        }];
        let (batches, sections, lookup) = match plan {
            Plan::Steps(steps) => {
                let (batches, sections) = step_batches(steps);
                (batches, sections, None)
            }
            Plan::Lookup { sql, then, label } => (Vec::new(), Vec::new(), Some((sql, then, label))),
        };
        return Ok(Prepared {
            sources,
            batches,
            sections,
            lookup,
            policy,
            report: request.report,
            blocked: None,
            uncommitted_reads: guard::UncommittedReads::default(),
            database: request.database,
            exact_count: request.exact_count,
        });
    }
    let sources = load_sources(&request.input)?;
    let user = origin == QueryOrigin::User;
    let vars = request.vars.as_deref().filter(|_| user);
    let batches = script::prepare(&sources, &request.terminator, vars).map_err(Failure::usage)?;
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
    // Apart from the guard, which `--allow-write` skips: a write based on a NOLOCK read is the
    // case the note matters most for. `--check` reads nothing, and a run under READ UNCOMMITTED
    // already says so of every read, so neither needs the pass over the text.
    let mut uncommitted_reads = guard::UncommittedReads::default();
    if user && blocked.is_none() && !policy.check && !settings.read_uncommitted {
        for batch in &batches {
            uncommitted_reads.add(guard::uncommitted_reads(&batch.sql));
        }
    }
    Ok(Prepared {
        sources,
        sections: Vec::new(),
        batches,
        lookup: None,
        policy,
        report: request.report,
        blocked,
        uncommitted_reads,
        database: request.database,
        exact_count: request.exact_count,
    })
}

/// Runs the input into `out`, or reports why it won't run. `gate` holds the run back while
/// the output's reader is behind. The signal, if any, is one that arrived while the last
/// batches finished, too late to change the outcome (`run_cancellable`). `started` is the run
/// clock: the batches are timed on it, as the closing lines are.
pub(crate) async fn execute(
    settings: &Settings,
    prepared: &Prepared,
    out: &mut dyn Output,
    gate: &dyn Backpressure,
    signals: &mut Result<Signals, String>,
    started: Instant,
) -> (RunOutcome, Option<Signal>) {
    let mut counts = Accounting::default();
    let (result, late) =
        execute_inner(settings, prepared, out, gate, signals, &mut counts, started).await;
    (counts.outcome(result), late)
}

/// What a run ends with before it needs a connection, if it does: the guard's refusal (exit 5),
/// or a note that there is no SQL to run. Shared by the CLI and the engine.
pub(crate) fn preflight(
    settings: &Settings,
    prepared: &Prepared,
    out: &mut dyn Output,
) -> Option<Result<Stats, Failure>> {
    if let Some((location, blocked)) = &prepared.blocked {
        let advice = blocked.advice_for(settings.writes_off.as_ref());
        if let Err(e) = out.blocked(location, &blocked.reason, &advice) {
            return Some(Err(output_failure(e)));
        }
        return Some(Err(Failure::new(Exit::Blocked, "")));
    }
    if prepared.batches.is_empty() && prepared.lookup.is_none() {
        let noted = out
            .note("no SQL to run")
            .map(|()| Stats::default())
            .map_err(output_failure);
        return Some(noted);
    }
    None
}

async fn execute_inner(
    settings: &Settings,
    prepared: &Prepared,
    out: &mut dyn Output,
    gate: &dyn Backpressure,
    signals: &mut Result<Signals, String>,
    counts: &mut Accounting,
    started: Instant,
) -> (Result<Stats, Failure>, Option<Signal>) {
    if let Some(early) = preflight(settings, prepared, out) {
        return (early, None);
    }
    // Running without being able to cancel isn't an option, so a failure to listen ends the
    // run here (its output still gets delivered).
    let signals = match signals {
        Ok(signals) => signals,
        Err(message) => return (Err(Failure::new(Exit::Internal, message.clone())), None),
    };
    run_cancellable(settings, prepared, out, gate, signals, counts, started).await
}

fn load_sources(input: &Input) -> Result<Vec<Source>, Failure> {
    let files = match input {
        Input::Query(query) => {
            return Ok(vec![Source {
                name: "-Q".into(),
                path: None,
                text: query.clone(),
            }]);
        }
        Input::Files(files) => files.as_slice(),
        Input::Stdin => &[],
        // A command's SQL is its plan's (`prepare`); it has no sources to read.
        Input::Command { .. } => return Ok(Vec::new()),
    };
    let mut sources = Vec::new();
    let mut left = script::MAX_INPUT_BYTES;
    for input in files {
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
    settings: &Settings,
    prepared: &Prepared,
    out: &mut dyn Output,
    gate: &dyn Backpressure,
    signals: &mut Signals,
    counts: &mut Accounting,
    started: Instant,
) -> (Result<Stats, Failure>, Option<Signal>) {
    let root = Cancel::new();
    let cancel = root.child();
    // On the heap: the session's future is large, the more so in a debug build, and the main
    // thread's stack is 1 MB.
    let mut work = Box::pin(run_session(
        settings, prepared, out, gate, &cancel, counts, started,
    ));
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

/// The advice for a run's `timeout` that expired: `raise -t` on the command line; in
/// `sqlh mcp`, the call's `timeout`, which can go up to the server's own (`output::Advice`).
/// `None` when the call already had the server's limit, so there is no larger one to offer.
pub(crate) fn raise_timeout(settings: &Settings, timeout: u32) -> Option<String> {
    match (settings.limits.advice, settings.query_timeout) {
        (output::Advice::Cli, _) => Some("raise -t".into()),
        (output::Advice::Mcp, 0) => Some("raise timeout".into()),
        (output::Advice::Mcp, limit) if timeout < limit => {
            Some(format!("raise timeout (the server allows up to {limit} s)"))
        }
        (output::Advice::Mcp, _) => None,
    }
}

/// The CLI's session: connected for this run, `open`ed, run and closed.
async fn run_session(
    settings: &Settings,
    prepared: &Prepared,
    out: &mut dyn Output,
    gate: &dyn Backpressure,
    cancel: &Cancel,
    counts: &mut Accounting,
    started: Instant,
) -> Result<Stats, Failure> {
    let timeout = settings.query_timeout;
    // --verbose and `profiles test` say where the time to the first batch went
    // (plan-history.md 2.1).
    let report = prepared.report != Report::Off;
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
    let advice = settings.limits.advice;
    let mut session = connected.map_err(|e| db_failure(e, advice))?;
    let connect_ms = connect_started.elapsed().as_millis();
    // The output says where the run went, from what the login already told us: JSON in its
    // head, text and csv at the end of the status line (owner decision 10). Not said when
    // nothing connected, which is every path that returns before this point.
    out.connected(&session.connection(), connect_ms)
        .map_err(output_failure)?;
    let setup_ms = open(&mut session, settings, prepared, timeout, cancel)
        .await
        .map_err(|e| db_failure(e, advice))?;
    let opened = Opened {
        connect_ms,
        steps,
        setup_ms,
    };
    let stats = run_opened(
        &mut session,
        &opened,
        settings,
        timeout,
        prepared,
        out,
        gate,
        cancel,
        counts,
        started,
    )
    .await?;
    session.close().await;
    Ok(stats)
}

/// How a session came to be ready for a run, for the `--verbose` line: the connect (0 ms for
/// a pooled connection that was reused at once; after the engine's probe or a replacement,
/// the time from its first attempt) with its steps, and the session setup.
pub(crate) struct Opened {
    pub(crate) connect_ms: u128,
    pub(crate) steps: Vec<(&'static str, std::time::Duration)>,
    pub(crate) setup_ms: u128,
}

/// A run's first requests on a connection, and the only ones before anything is shown: the
/// session setup (`session_setup`), then the call's `USE` when it names a database
/// (`Prepared::database`). On a reused connection the setup carries the reset
/// (`Session::reset_before_next`), so this is where a connection that died while idle, or a
/// reset the server didn't acknowledge, shows up: before any of the run's own work, which the
/// engine relies on to retry (owner decision 19). Returns the setup's milliseconds.
///
/// It is `setup` then `use_database`; the engine runs the two apart, so that its probe of a
/// reused connection (`REUSE_PROBE`) bounds the setup alone.
pub(crate) async fn open(
    session: &mut Session,
    settings: &Settings,
    prepared: &Prepared,
    timeout: u32,
    cancel: &Cancel,
) -> Result<u128, DbError> {
    let setup_ms = setup(session, settings, prepared, timeout, cancel).await?;
    use_database(session, prepared, timeout, cancel).await?;
    Ok(setup_ms)
}

/// A timeout in `open` says where it struck: no query of the run's own was running.
fn timed_out(error: DbError, after: u32, at: &str) -> DbError {
    match error {
        DbError::Timeout(_) => DbError::Timeout(format!("query timeout after {after} s {at}")),
        error => error,
    }
}

/// `open`'s first request, the session setup (`session_setup`), within `timeout`; it carries
/// the reset of a reused connection. Returns its milliseconds.
pub(crate) async fn setup(
    session: &mut Session,
    settings: &Settings,
    prepared: &Prepared,
    timeout: u32,
    cancel: &Cancel,
) -> Result<u128, DbError> {
    let policy = prepared.policy;
    // `--check` reads nothing, so there is no read to make uncommitted and none to report.
    let read_uncommitted = settings.read_uncommitted && !policy.check;
    let setup = session_setup(settings.lock_timeout, read_uncommitted, policy.wrap());
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
        .map_err(|e| timed_out(e, timeout, "in the session setup"))?;
    Ok(setup_started.elapsed().as_millis())
}

/// `open`'s second request, the call's `USE` when it names a database
/// (`Prepared::database`), with `]` doubled; none otherwise.
pub(crate) async fn use_database(
    session: &mut Session,
    prepared: &Prepared,
    timeout: u32,
    cancel: &Cancel,
) -> Result<(), DbError> {
    if let Some(database) = &prepared.database {
        let sql = format!("USE [{}];", database.replace(']', "]]"));
        session
            .exec_internal(&sql, timeout, cancel)
            .await
            .map_err(|e| timed_out(e, timeout, "changing to the call's database"))?;
    }
    Ok(())
}

/// Everything a run does on a session that is `open`: its notes, its lookup and batches, the
/// read-only checks after them and the rollback of the wrapper. The connection stays open
/// for the caller to close (the CLI) or keep (the engine). `timeout` is the run's `-t`,
/// which a call may set below its settings' (an MCP call's `timeout`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_opened(
    session: &mut Session,
    opened: &Opened,
    settings: &Settings,
    timeout: u32,
    prepared: &Prepared,
    out: &mut dyn Output,
    gate: &dyn Backpressure,
    cancel: &Cancel,
    counts: &mut Accounting,
    started: Instant,
) -> Result<Stats, Failure> {
    let (sources, batches, policy) = (&prepared.sources, &prepared.batches, prepared.policy);
    let report = prepared.report != Report::Off;
    let wrap = policy.wrap();
    let read_uncommitted = settings.read_uncommitted && !policy.check;
    // Said once, up front, where an output cut short by --max-output still has it.
    if read_uncommitted {
        out.note(&read_uncommitted_note(settings.limits.advice))
            .map_err(output_failure)?;
    } else if let Some(note) = uncommitted_in_sql_note(prepared.uncommitted_reads) {
        out.note(&note).map_err(output_failure)?;
    }
    if report {
        let profile = settings.profile.as_deref().unwrap_or("(none)");
        let line = format!(
            "profile {profile}; connected in {} ms{}, session setup {} ms; {}",
            opened.connect_ms,
            db::phases::describe(&opened.steps),
            opened.setup_ms,
            session.describe()
        );
        if prepared.report == Report::Note {
            out.note(&line).map_err(output_failure)?;
        } else {
            out.diagnostic(&line).map_err(output_failure)?;
        }
    }

    // A command's lookup decides its batches; it shows nothing (`Plan::Lookup`).
    let looked_up;
    let (batches, sections) = match &prepared.lookup {
        None => (batches, &prepared.sections),
        Some((sql, then, label)) => {
            // Boxed: its state would otherwise add to the session's future, which a debug
            // build lays out big enough to overflow the main thread's stack.
            let found = Box::pin(look_up(
                session, sql, *label, timeout, settings, cancel, gate,
            ))
            .await?;
            looked_up = step_batches(then(&found)?);
            (&looked_up.0, &looked_up.1)
        }
    };
    let xact_abort = batches
        .iter()
        .any(|batch| guard::mentions_xact_abort(&batch.sql));
    let counting = counting_for(
        policy.allow_write,
        settings.exact_count || prepared.exact_count,
        xact_abort,
    );
    let mut stats = Stats::default();
    let total: usize = batches.iter().map(|b| b.repeat as usize).sum();
    let mut number = 0;
    let mut output_modes = false;
    'batches: for (index, batch) in batches.iter().enumerate() {
        out.set_sections(sections.get(index).map_or(&[], Vec::as_slice));
        // A stopped count cancels the rest of the batch; where that could cancel a later
        // statement's output, the count runs longer first, and a stop is reported as the rest
        // of the batch not run (decision 45, DESIGN.md §4.4). An option that adds output
        // (`SET STATISTICS XML ON`) lasts for the session, so it makes this batch and every
        // later one such a batch (R48).
        output_modes = output_modes || guard::sets_output_modes(&batch.sql);
        let later_statements = output_modes || guard::several_outputs(&batch.sql);
        let counting = if later_statements {
            counting.with_later_statements()
        } else {
            counting
        };
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
                    .map_err(|e| db_failure(e, settings.limits.advice))?;
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
                later_statements,
            })
            .map_err(output_failure)?;
            counts.begin_batch(number, later_statements);
            let mut sink = Counted {
                out: &mut *out,
                counts: &mut *counts,
            };
            session
                .run(
                    batch.sql.clone(),
                    timeout,
                    counting,
                    cancel,
                    &mut sink,
                    gate,
                )
                .await
                .map_err(|e| match e {
                    DbError::Timeout(_) => Failure::new(
                        Exit::Timeout,
                        format!(
                            "query timeout after {timeout} s in batch {number}; {}",
                            match raise_timeout(settings, timeout) {
                                Some(raise) => format!("{raise} or narrow the query"),
                                None => {
                                    format!("narrow the query ({timeout} s is the server's limit)")
                                }
                            }
                        ),
                    ),
                    e => db_failure(e, settings.limits.advice),
                })?;
            stats.batches_run += 1;
            out.end_batch(started.elapsed().as_millis())
                .map_err(output_failure)?;
            out.flush().map_err(output_failure)?;
            if counts.batch_failed() {
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
        let checker = match settings.limits.advice {
            output::Advice::Cli => "--check",
            output::Advice::Mcp => "check",
        };
        out.note(&format!(
            "{checker} compiled each batch without running it. Missing tables are only reported \
             when a query runs (SQL Server defers their name resolution)"
        ))
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
    Ok(stats)
}

/// Runs a command's lookup (`Plan::Lookup`) and returns its rows. It shows nothing: an error
/// in it ends the run as a failure that quotes the server's message, and its messages are
/// dropped, so no event reaches the output outside a batch.
async fn look_up(
    session: &mut Session,
    sql: &str,
    label: LookupLabel,
    timeout: u32,
    settings: &Settings,
    cancel: &Cancel,
    gate: &dyn Backpressure,
) -> Result<Found, Failure> {
    let mut capture = Capture::default();
    session
        .run(
            sql.to_owned(),
            timeout,
            // Rows past the bounds already fail the lookup; counting them stops early.
            Counting::BOUNDED,
            cancel,
            &mut capture,
            gate,
        )
        .await
        .map_err(|e| match e {
            DbError::Timeout(_) => Failure::new(
                Exit::Timeout,
                format!(
                    "query timeout after {timeout} s while {}; {}{}",
                    label.doing(),
                    raise_timeout(settings, timeout)
                        .unwrap_or_else(|| format!("{timeout} s is the server's limit")),
                    label
                        .after_timeout(settings.limits.advice)
                        .map_or_else(String::new, |more| format!("; {more}"))
                ),
            ),
            e => db_failure(e, settings.limits.advice),
        })?;
    capture.finish(label)
}

/// The most a lookup keeps (R31): result sets, rows in each, and characters of a value. The
/// lookup's own SQL returns a few short rows (the candidates query has `TOP`, and a total for
/// the rest), so anything past these is a reply sqlh didn't ask for: it fails the run rather
/// than grow, and is never taken for a whole answer.
const CAPTURE_RESULTS: usize = 4;
const CAPTURE_ROWS: usize = 16;
const CAPTURE_CHARS: usize = 4096;

/// The sink of a lookup: keeps the rows of its result sets, within the `CAPTURE_` bounds, and
/// its first error.
#[derive(Default)]
struct Capture {
    found: Found,
    error: Option<(u32, String)>,
    /// The reply went past a bound: what was kept isn't all of it.
    overflow: bool,
}

impl Capture {
    /// What the lookup returned, or why it can't be used.
    fn finish(self, label: LookupLabel) -> Result<Found, Failure> {
        if let Some((number, text)) = self.error {
            return Err(Failure::new(
                Exit::SqlError,
                format!("{} failed: error {number}: {text}", label.doing()),
            ));
        }
        if self.overflow {
            return Err(Failure::new(
                Exit::Internal,
                format!(
                    "{} returned more than it should; nothing was shown",
                    label.noun()
                ),
            ));
        }
        Ok(self.found)
    }
}

impl BatchSink for Capture {
    fn columns(&mut self, _columns: &[Column]) -> io::Result<()> {
        if self.found.len() < CAPTURE_RESULTS {
            self.found.push(Vec::new());
        } else {
            self.overflow = true;
        }
        Ok(())
    }
    fn wants_values(&self) -> bool {
        !self.overflow
            && self
                .found
                .last()
                .is_some_and(|rows| rows.len() < CAPTURE_ROWS)
    }
    fn cell_limit(&self) -> usize {
        CAPTURE_CHARS
    }
    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        let cut = values
            .iter()
            .any(|v| matches!(v, Value::TextCut(..) | Value::BinCut(..)));
        match self.found.last_mut() {
            Some(rows) if !cut && !self.overflow && rows.len() < CAPTURE_ROWS => {
                rows.push(values.to_vec());
            }
            _ => self.overflow = true,
        }
        Ok(())
    }
    fn skipped_row(&mut self) {
        self.overflow = true;
    }
    fn counting_stopped(&mut self, _by: Stop) {
        self.overflow = true;
    }
    fn end_rows(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn end_rows_incomplete(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn rows_affected(&mut self, _count: u64) -> io::Result<()> {
        Ok(())
    }
    fn message(&mut self, _message: &ServerMessage) -> io::Result<()> {
        Ok(())
    }
    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        if self.error.is_none() {
            self.error = Some((error.number, error.text.clone()));
        }
        Ok(())
    }
}

/// A driver error as the run's failure; `advice` words what the run can do about a count stop
/// the server didn't confirm.
pub(crate) fn db_failure(error: DbError, advice: output::Advice) -> Failure {
    match error {
        DbError::Connect(message) => Failure::new(Exit::Connect, message),
        DbError::Lost(message) => {
            Failure::new(Exit::Connect, format!("connection lost: {message}"))
        }
        DbError::Timeout(message) => Failure::new(Exit::Timeout, message),
        DbError::Cancelled(detail) => Failure::new(Exit::Cancelled, detail),
        DbError::CountStopUnconfirmed => {
            let instead = match advice {
                output::Advice::Cli => "--exact-count reads every row instead",
                output::Advice::Mcp => "exact_count = true reads every row instead",
            };
            Failure::new(
                Exit::Cancelled,
                format!("{} ({instead})", crate::db::COUNT_STOP_UNCONFIRMED),
            )
        }
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

    // R31: a lookup keeps a bounded reply and fails on more, never taking what it kept for all
    // of it.
    #[test]
    fn a_lookup_keeps_a_bounded_reply() {
        let row = || vec![Value::Int(1), Value::Text("dbo".into())];
        let mut capture = Capture::default();
        capture.columns(&[]).unwrap();
        for _ in 0..CAPTURE_ROWS {
            assert!(capture.wants_values());
            capture.row(&row()).unwrap();
        }
        assert!(!capture.wants_values());
        assert_eq!(capture.cell_limit(), CAPTURE_CHARS);
        let found = capture.finish(LookupLabel::Name).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].len(), CAPTURE_ROWS);

        let overflowed = |feed: &dyn Fn(&mut Capture)| {
            let mut capture = Capture::default();
            feed(&mut capture);
            let failure = capture.finish(LookupLabel::Name).unwrap_err();
            assert_eq!(failure.exit, Exit::Internal);
            assert!(failure.message.contains("returned more than it should"));
        };
        overflowed(&|c| {
            c.columns(&[]).unwrap();
            for _ in 0..CAPTURE_ROWS {
                c.row(&row()).unwrap();
            }
            c.skipped_row();
        });
        overflowed(&|c| {
            c.columns(&[]).unwrap();
            c.row(&[Value::TextCut("x".repeat(CAPTURE_CHARS), 1)])
                .unwrap();
        });
        overflowed(&|c| {
            for _ in 0..=CAPTURE_RESULTS {
                c.columns(&[]).unwrap();
            }
        });
        overflowed(&|c| {
            c.columns(&[]).unwrap();
            c.counting_stopped(Stop::Budget);
        });

        // The server's error comes first.
        let mut capture = Capture::default();
        capture.skipped_row();
        let error = ServerMessage {
            number: 208,
            severity: 16,
            state: 1,
            line: Some(1),
            procedure: None,
            text: "Invalid object name".into(),
        };
        capture.error(&error).unwrap();
        let failure = capture.finish(LookupLabel::Name).unwrap_err();
        assert_eq!(failure.exit, Exit::SqlError);
        assert_eq!(
            failure.message,
            "looking the name up failed: error 208: Invalid object name"
        );
    }

    // A lookup's failures say what it was doing (`LookupLabel`); the name commands' words are
    // the ones they always had (engine.rs's lookup timeout test has the timeout's).
    #[test]
    fn a_lookups_failures_name_what_it_was_doing() {
        assert_eq!(LookupLabel::Name.doing(), "looking the name up");
        let mut capture = Capture::default();
        capture.columns(&[]).unwrap();
        capture.counting_stopped(Stop::Budget);
        assert_eq!(
            capture.finish(LookupLabel::Name).unwrap_err().message,
            "the name lookup returned more than it should; nothing was shown"
        );
    }

    // `Plan::Lookup`'s rule: the lookup holds a server command's work, its `#temp` tables live
    // on into the step, and a step that is one `SELECT` reading them is not a batch with later
    // statements, so it gets no longer count and no `-- partial:` advice. Checked on the steps
    // `activity` and `errorlog` build from their lookups' rows, as a run builds them (one after
    // common table expressions, a recursive one with `UNION ALL` in it, and a query hint); a
    // second `SELECT` is not one, nor is a lookup (which is never asked).
    #[test]
    fn a_step_of_one_select_after_a_lookup_has_no_later_statements() {
        use crate::output::Advice;
        use crate::schema::{Command, Errorlog};
        let row = |values: &[i64]| -> Found {
            vec![vec![values.iter().map(|&n| Value::Int(n)).collect()]]
        };
        let agent_search = Errorlog {
            since: Some("1h".into()),
            search: Some("x".into()),
            agent: true,
            archive: None,
        };
        for (command, found) in [
            // can_view, major
            (Command::Activity, row(&[1, 17])),
            // can_read, archives, last, read, major
            (
                Command::Errorlog(Errorlog::default()),
                row(&[1, 7, 6, 1, 17]),
            ),
            (Command::Errorlog(agent_search), row(&[1, 10, 9, 2, 13])),
        ] {
            let Ok(Input::Command {
                plan: Plan::Lookup { sql, then, .. },
                ..
            }) = command.input(Advice::Cli)
            else {
                panic!("{command:?} looks up first");
            };
            assert!(guard::several_outputs(&sql), "{command:?}'s lookup");
            let steps = then(&found).unwrap_or_else(|f| panic!("{command:?}: {}", f.message));
            assert_eq!(steps.len(), 1, "{command:?}");
            assert!(
                !guard::several_outputs(&steps[0].sql),
                "{command:?}: {}",
                steps[0].sql
            );
        }
        for not_one in [
            "SELECT 1 AS a FROM #log; SELECT 2 AS b FROM #log;",
            "SET NOCOUNT ON; CREATE TABLE #log (n int); INSERT INTO #log EXEC sys.sp_readerrorlog \
             0, 1; SELECT COUNT(*) AS n FROM #log;",
        ] {
            assert!(guard::several_outputs(not_one), "{not_one}");
        }
    }

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

    // A timeout's advice names what its reader can change: -t on the command line; in sqlh mcp
    // a larger `timeout`, up to the server's limit, and nothing once the call has that limit.
    #[test]
    fn the_timeout_advice_offers_only_what_can_be_raised() {
        let mut settings = settings(false);
        settings.query_timeout = 30;
        assert_eq!(raise_timeout(&settings, 30).as_deref(), Some("raise -t"));
        settings.limits.advice = output::Advice::Mcp;
        assert_eq!(
            raise_timeout(&settings, 10).as_deref(),
            Some("raise timeout (the server allows up to 30 s)")
        );
        assert_eq!(raise_timeout(&settings, 30), None);
        settings.query_timeout = 0;
        assert_eq!(
            raise_timeout(&settings, 10).as_deref(),
            Some("raise timeout")
        );
    }

    // docs/mcp-mode.md §4, exactly and in both forms: the dirty-read note and a count stop the
    // server didn't confirm (exit 6) name what their reader can change, the CLI's flags or, in
    // sqlh mcp, the profile's settings.
    #[test]
    fn the_dirty_read_note_and_an_unconfirmed_count_stop_name_what_can_change() {
        assert_eq!(
            read_uncommitted_note(output::Advice::Cli),
            "reads were uncommitted (--read-uncommitted): rows may include uncommitted, \
             duplicated or missing data"
        );
        assert_eq!(
            read_uncommitted_note(output::Advice::Mcp),
            "reads were uncommitted (the profile's read_uncommitted = true): rows may include \
             uncommitted, duplicated or missing data"
        );
        let none = guard::UncommittedReads::default();
        assert_eq!(uncommitted_in_sql_note(none), None);
        let dirty = "rows may include uncommitted, duplicated or missing data";
        for (reads, sources) in [
            (
                guard::UncommittedReads {
                    nolock: true,
                    ..none
                },
                "NOLOCK",
            ),
            (
                guard::UncommittedReads {
                    readuncommitted: true,
                    ..none
                },
                "READUNCOMMITTED",
            ),
            (
                guard::UncommittedReads {
                    isolation: true,
                    ..none
                },
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            ),
            (
                guard::UncommittedReads {
                    nolock: true,
                    readuncommitted: true,
                    ..none
                },
                "NOLOCK, READUNCOMMITTED",
            ),
            (
                guard::UncommittedReads {
                    nolock: true,
                    readuncommitted: true,
                    isolation: true,
                },
                "NOLOCK, READUNCOMMITTED, SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
            ),
        ] {
            assert_eq!(
                uncommitted_in_sql_note(reads).unwrap(),
                format!("some reads were uncommitted ({sources} in the SQL): {dirty}")
            );
        }
        let stop = "stopped counting the rows that are not shown, but the server didn't confirm \
                    the cancel, so the connection was closed; the rows shown are complete";
        for (advice, instead) in [
            (output::Advice::Cli, "--exact-count reads every row instead"),
            (
                output::Advice::Mcp,
                "exact_count = true reads every row instead",
            ),
        ] {
            let failure = db_failure(DbError::CountStopUnconfirmed, advice);
            assert_eq!(failure.exit, Exit::Cancelled);
            assert_eq!(failure.message, format!("{stop} ({instead})"));
        }
    }

    // plan-history.md 2.8: the session setup, exactly. The agent defaults are in every connection,
    // writes allowed or not; READ UNCOMMITTED only when asked for, and READ COMMITTED
    // otherwise, always before the wrapper transaction begins.
    #[test]
    fn the_session_setup_carries_the_agent_defaults() {
        let ssms = SESSION_SETUP;
        let committed = "SET TRANSACTION ISOLATION LEVEL READ COMMITTED;";
        // R14, spelled out: the whole first batch of a read-only run, with XACT_ABORT OFF
        // before the wrapper transaction begins (and in an --allow-write run, which has no
        // wrapper, all the same).
        assert_eq!(
            session_setup(10, false, true),
            "SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; \
             SET CONCAT_NULL_YIELDS_NULL ON; SET QUOTED_IDENTIFIER ON; SET NUMERIC_ROUNDABORT OFF; \
             SET XACT_ABORT OFF; SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000; \
             SET TRANSACTION ISOLATION LEVEL READ COMMITTED; BEGIN TRANSACTION;"
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
            format!(
                "{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000; {committed} \
                 BEGIN TRANSACTION;"
            )
        );
        // --allow-write: no wrapper transaction, same defaults, the isolation level included.
        assert_eq!(
            session_setup(10, false, false),
            format!("{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000; {committed}")
        );
        // A changed timeout, and 0 = no limit, which is -1 on the server.
        assert!(session_setup(3, false, true).contains(" SET LOCK_TIMEOUT 3000; "));
        assert!(
            session_setup(0, false, false).ends_with(&format!(" SET LOCK_TIMEOUT -1; {committed}"))
        );
        assert_eq!(
            session_setup(2_147_483, false, false),
            format!("{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 2147483000; {committed}")
        );
        // Dirty reads: after the lock timeout, before the transaction is opened.
        assert_eq!(
            session_setup(10, true, true),
            format!(
                "{ssms} SET DEADLOCK_PRIORITY LOW; SET LOCK_TIMEOUT 10000; \
                 SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; BEGIN TRANSACTION;"
            )
        );
        // The isolation level is always set, exactly once, and always before the wrapper:
        // the connection reset doesn't restore it, so a reused connection would otherwise keep
        // whatever level the call before set (`session_setup`).
        for read_uncommitted in [false, true] {
            for wrap in [false, true] {
                let setup = session_setup(10, read_uncommitted, wrap);
                assert_eq!(setup.matches("ISOLATION LEVEL").count(), 1, "{setup}");
                let level = if read_uncommitted {
                    "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED;"
                } else {
                    committed
                };
                let at = setup.find(level).expect("the isolation level is set");
                if let Some(begin) = setup.find("BEGIN TRANSACTION;") {
                    assert!(at < begin, "{setup}");
                }
            }
        }
    }

    // plan-history.md 2.15, decision 6 (DESIGN.md §4.4): which runs stop their count, and which let
    // `-t` end it. Writes allowed, and `XACT_ABORT` in the input, count exactly and keep `-t` a
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

    fn settings(allow_write: bool) -> Settings {
        super::unconnected_settings(allow_write)
    }

    fn query(sql: &str) -> Request {
        Request {
            input: Input::Query(sql.into()),
            vars: Some(vec![("t".into(), "dbo.T".into())]),
            terminator: "GO".into(),
            check: false,
            continue_on_error: false,
            report: Report::Off,
            database: None,
            exact_count: false,
        }
    }

    // plan-history.md 3.1: the policy is decided once. The guard checks writes unless they are
    // allowed, and NOEXEC under --check; the wrapper is there unless writes are allowed, and always
    // under --check.
    #[test]
    fn the_run_policy_decides_the_guard_and_the_wrapper() {
        let policy = |allow_write, check| RunPolicy {
            allow_write,
            check,
            continue_on_error: false,
            reused_session: false,
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
        let request = Request {
            check: true,
            continue_on_error: true,
            ..query("SELECT 1")
        };
        assert_eq!(
            RunPolicy::of(&request, &settings(true), QueryOrigin::User),
            RunPolicy {
                allow_write: true,
                check: true,
                continue_on_error: true,
                reused_session: false,
            }
        );
        // sqlh's own SQL never runs with writes allowed: the wrapper stays (the Phase 3
        // review's design follow-up).
        let generated = RunPolicy::of(&request, &settings(true), QueryOrigin::Generated);
        assert!(!generated.allow_write && generated.wrap(), "{generated:?}");
    }

    // Decision 30(c): `sqlh (writes)` only where nothing would undo a write; --check keeps the
    // wrapper and NOEXEC, whatever --allow-write says.
    #[test]
    fn the_run_policy_names_the_login_for_what_the_run_may_do() {
        for (allow_write, check, name) in [
            (false, false, "sqlh"),
            (true, false, "sqlh (writes)"),
            (false, true, "sqlh"),
            (true, true, "sqlh"),
        ] {
            let policy = RunPolicy {
                allow_write,
                check,
                continue_on_error: false,
                reused_session: false,
            };
            assert_eq!(policy.application_name(), name, "{policy:?}");
        }
    }

    // Decision 30(e): only `sqlh mcp`'s session goes on to another call. A `deny` profile's writes
    // are off too, and its session ends with the run like any other CLI's.
    #[test]
    fn only_sqlh_mcp_reuses_a_session() {
        let sql = "SET AUTOPILOT ON";
        for (writes_off, refused) in [
            (None, false),
            (Some(guard::WritesOff::Profile("p".into())), false),
            (Some(guard::WritesOff::Mcp), true),
        ] {
            let settings = Settings {
                writes_off,
                ..settings(false)
            };
            for check in [false, true] {
                let request = Request {
                    check,
                    ..query(sql)
                };
                let prepared = prepare(request, &settings, QueryOrigin::User).unwrap();
                assert_eq!(prepared.policy.reused_session, refused);
                assert_eq!(prepared.blocked.is_some(), refused, "check {check}");
            }
        }
        // sqlh's own SQL is taken as it is, in a session reused or not.
        let settings = Settings {
            writes_off: Some(guard::WritesOff::Mcp),
            ..settings(false)
        };
        let generated = prepare(query(sql), &settings, QueryOrigin::Generated).unwrap();
        assert!(generated.blocked.is_none());
    }

    // plan-history.md 3.1: the user's SQL gets $(var) and the guard; sqlh's own SQL (QueryOrigin::
    // Generated) is taken as it is, and always runs in the wrapper, even where the profile
    // allows writes.
    #[test]
    fn generated_sql_skips_substitution_and_the_guard_but_not_the_wrapper() {
        let opts = || query("UPDATE $(t) SET x = 1");
        let user = prepare(opts(), &settings(false), QueryOrigin::User).unwrap();
        assert_eq!(user.batches[0].sql, "UPDATE dbo.T SET x = 1");
        let (location, blocked) = user.blocked.as_ref().expect("the guard refuses it");
        assert_eq!(location, "line 1");
        assert!(
            blocked.reason.contains("UPDATE dbo.T would write"),
            "{}",
            blocked.reason
        );

        for allow_write in [false, true] {
            let generated =
                prepare(opts(), &settings(allow_write), QueryOrigin::Generated).unwrap();
            assert_eq!(generated.batches[0].sql, "UPDATE $(t) SET x = 1");
            assert!(generated.blocked.is_none());
            assert!(
                generated.policy.wrap() && !generated.policy.allow_write,
                "generated SQL runs in the wrapper (profile allows writes: {allow_write})"
            );
        }
        // The user's SQL under a profile that allows writes runs without it.
        let user = prepare(query("SELECT 1"), &settings(true), QueryOrigin::User).unwrap();
        assert!(!user.policy.wrap());
    }

    // The Phase 3 review's design follow-up: the run counts SQL errors, each batch's worst
    // severity and the stopped counts itself, as the events pass through to the output: an
    // error is counted once the output has taken it (shown, or withheld by the output budget),
    // not when the output refused it out of order; a batch with two stops is one stopped batch.
    #[test]
    fn the_run_counts_its_errors_and_stops_itself() {
        use crate::output::{Format, Limits};
        let mut out = output::create(Format::Text, Box::new(io::sink()), Limits::default());
        let mut counts = Accounting::default();
        // Batch 3 has later statements a stop cancels (decision 45); batch 2 has none.
        let batch = |number| BatchInfo {
            number,
            total: 3,
            source: "-Q",
            multi_source: false,
            line: 1,
            line_map: &[],
            run: 1,
            runs: 1,
            started_ms: 0,
            later_statements: number == 3,
        };
        let message = |severity| ServerMessage {
            number: if severity > 10 { 208 } else { 0 },
            severity,
            state: 1,
            line: Some(1),
            procedure: None,
            text: "x".into(),
        };
        let mut failed = Vec::new();
        for number in 1..=3 {
            out.begin_batch(&batch(number)).unwrap();
            counts.begin_batch(number, number == 3);
            let mut sink = Counted {
                out: &mut *out,
                counts: &mut counts,
            };
            sink.columns(&[]).unwrap();
            if number > 1 {
                // Stopped twice in one batch: it is still one batch.
                sink.counting_stopped(Stop::Budget);
                sink.counting_stopped(Stop::Timeout);
            }
            sink.end_rows().unwrap();
            sink.message(&message(0)).unwrap();
            if number != 2 {
                sink.error(&message(16)).unwrap();
                sink.error(&message(11)).unwrap();
            }
            // An error in the middle of a result is out of order: refused, and not counted.
            sink.columns(&[]).unwrap();
            assert!(sink.error(&message(16)).is_err());
            sink.end_rows().unwrap();
            failed.push(counts.batch_failed());
            out.end_batch(1).unwrap();
        }
        assert_eq!(failed, [true, false, true]);
        assert_eq!(counts.errors, 4);
        assert_eq!(counts.stopped_batches, 2);
        assert_eq!(counts.cut_batches, [3]);

        // An error the output budget withholds still counts: it happened.
        #[derive(Clone, Default)]
        struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl io::Write for Shared {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let written = Shared::default();
        let limits = Limits {
            max_rows: 0,
            max_cell: 0,
            max_results: 0,
            max_messages: 0,
            max_output: output::OUTPUT_RESERVE + 50,
            stall_secs: 0,
            ..Limits::default()
        };
        let mut out = output::create(Format::Text, Box::new(written.clone()), limits);
        let mut counts = Accounting::default();
        counts.begin_batch(1, false);
        let mut sink = Counted {
            out: &mut *out,
            counts: &mut counts,
        };
        let column = Column {
            name: "v".into(),
            type_name: "varchar(9)".into(),
            nullable: true,
        };
        sink.columns(&[column]).unwrap();
        for _ in 0..5 {
            if sink.wants_values() {
                sink.row(&[Value::Text("aaaaaaaaa".into())]).unwrap();
            } else {
                sink.skipped_row();
            }
        }
        sink.end_rows().unwrap();
        sink.error(&message(16)).unwrap();
        assert_eq!(counts.errors, 1);
        assert!(counts.batch_failed());
        let summary = output::Summary {
            exit: 1,
            status: "failed (exit 1): 1 error, 3 ms",
            error: None,
            elapsed_ms: 3,
        };
        out.finish(&summary).unwrap();
        out.flush().unwrap();
        out.wait_delivered(None).unwrap();
        let text = String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
        assert!(
            !text.contains("error 208"),
            "the error was withheld: {text}"
        );
    }

    // plan-history.md 3.1: the exit code comes from the outcome alone: the failure's, else 1 when a
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
