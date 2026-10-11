mod cli;
mod config;
mod console;
mod credman;
mod db;
mod error;
mod files;
mod guard;
mod output;
mod profile;
mod script;
mod signals;

use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cli::{Command, Opts};
use config::Settings;
use db::{Cancel, DbError, Session};
use error::{Exit, Failure};
use output::{BatchInfo, Format, Limits, Summary};
use script::{Batch, Source};
use signals::{Signal, Signals};

const HELP: &str = include_str!("help.txt");

/// How long the closing lines get to reach the reader once a run has been cancelled, or a
/// signal arrives while they are being delivered: the end-to-end bound on a cancel is the
/// signal's grace plus this plus `DIAG_WAIT` (DESIGN.md §4.3).
const DELIVERY_GRACE: Duration = Duration::from_secs(1);

/// How long a diagnostic gets to reach its stream before sqlh gives up on it (R09).
const DIAG_WAIT: Duration = Duration::from_millis(500);

/// The longest variable field in a diagnostic, in characters before escaping. Longer ones
/// are cut and marked.
const DIAG_FIELD_CHARS: usize = 1000;

/// Session options matching SSMS, so plans and filtered/computed-column indexes behave as
/// people expect (DESIGN.md §4.3).
const SESSION_SETUP: &str = "SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; \
    SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; SET QUOTED_IDENTIFIER ON; \
    SET NUMERIC_ROUNDABORT OFF;";

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

type Out = output::Stream;

/// Whether stdout carries the text format, so the panic hook may append a status line.
/// JSON and CSV can't be completed by a stray line, so they only get the stderr message.
static TEXT_ON_STDOUT: AtomicBool = AtomicBool::new(true);

#[derive(Default)]
struct Stats {
    batches_run: usize,
    batches_skipped: usize,
    first_failed: Option<usize>,
}

fn main() -> ExitCode {
    let started = Instant::now();
    install_panic_hook();
    let exit = match cli::parse() {
        Ok(Command::Run(opts)) => run_command(*opts, started),
        Ok(Command::Profile(command)) => profile::run(command, started),
        Ok(Command::Help) => print_plain(HELP),
        Ok(Command::Version) => print_plain(&version_line()),
        Err(message) => fail(Failure::usage(message), Format::Text, started),
    };
    exit.into()
}

/// What `--version` prints: the program's version, and the version of the JSON document it
/// writes (DESIGN.md §5.2), which the document itself starts with. Text and csv carry no
/// version of their own, so a tool that wants to know what to expect asks here.
fn version_line() -> String {
    format!(
        "sqlh {} (json contract {})\n",
        env!("CARGO_PKG_VERSION"),
        output::json::CONTRACT_VERSION
    )
}

/// A panic is a bug, but it must still end with the documented exit code 10 rather than an
/// abort (0xC0000409). The rest is best effort: a stderr line, and a status line on stdout
/// in text output, each given `DIAG_WAIT` and then abandoned if its stream is stalled (R09).
/// Output still buffered is lost, a JSON document stays unfinished, and `-o` output is
/// never published. Expected failures must not rely on this: they are `Result`s, and
/// nothing here may panic again.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = brief(info.payload_as_str().unwrap_or("unknown panic"));
        let location = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        console::restore_echo();
        let mut writes = vec![(
            Target::Stderr,
            format!("sqlh: internal error: {message}{location}\n"),
        )];
        if TEXT_ON_STDOUT.load(Ordering::Relaxed) {
            writes.push((
                Target::Stdout,
                format!(
                    "\n-- error: internal error: {message}\n-- failed (exit 10): internal error\n"
                ),
            ));
        }
        write_bounded(writes);
        std::process::exit(Exit::Internal as i32);
    }));
}

fn run_command(opts: Opts, started: Instant) -> Exit {
    let format = opts.format.unwrap_or(Format::Text);
    TEXT_ON_STDOUT.store(
        format == Format::Text && opts.output.is_none(),
        Ordering::Relaxed,
    );
    #[cfg(debug_assertions)]
    if std::env::var_os("SQLH_DEBUG_PANIC").is_some() {
        panic!("forced by SQLH_DEBUG_PANIC");
    }
    // A failure before the settings are resolved still keeps to the flags' own limits, and
    // one after keeps to the run's (R08: the cap covers the whole response).
    let flagged = flag_limits(&opts);
    if opts.write_policy.is_some() || opts.new_password {
        return fail_within(
            Failure::usage("--write-policy and --new-password only apply to sqlh profile add"),
            format,
            flagged,
            started,
        );
    }
    let settings = match config::resolve(&opts) {
        Ok(settings) => settings,
        Err(failure) => return fail_within(failure, format, flagged, started),
    };
    // Everything that can fail before the run happens before `-o` is opened, so bad input
    // never truncates or replaces an existing output file.
    let prepared = match prepare(&opts, &settings) {
        Ok(prepared) => prepared,
        Err(failure) => return fail_within(failure, format, settings.limits, started),
    };
    let (destination, writer) = match Destination::open(opts.output.as_deref(), &prepared.sources) {
        Ok(opened) => opened,
        Err(failure) => return fail_within(failure, format, settings.limits, started),
    };
    let mut out = output::create(format, writer, settings.limits);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            // Nothing to wait with, so deliver the way a command without a runtime does.
            let failure = Failure::new(
                Exit::Internal,
                format!("cannot start the async runtime: {e}"),
            );
            let exit = finish(&mut out, Err(failure), started);
            drop(out);
            return destination.commit(exit);
        }
    };
    runtime.block_on(run_and_deliver(
        &opts,
        &settings,
        &prepared,
        out,
        destination,
        started,
    ))
}

/// The run, its closing lines and the publication of `-o`, on one runtime with the console
/// event listeners alive throughout. Any earlier end for either one lets Ctrl+C and
/// Ctrl+Break fall through to Windows' default handler, which ends the process with
/// 0xC000013A instead of the run's documented outcome (R10; DESIGN.md §4.3).
async fn run_and_deliver(
    opts: &Opts,
    settings: &Settings,
    prepared: &Prepared,
    mut out: Out,
    destination: Destination,
    started: Instant,
) -> Exit {
    // Registered only now. While `prepare` read stdin, a listener would have swallowed
    // Ctrl+C and left that read blocked.
    let mut signals = Signals::new()
        .map_err(|e| format!("cannot watch for Ctrl+C and other console events: {e}"));
    let noted = if opts.ignored.is_empty() {
        Ok(())
    } else {
        out.note(&format!(
            "ignored sqlcmd flags {} (sqlh output is always compact)",
            opts.ignored.join(" ")
        ))
        .map_err(output_failure)
    };
    let (result, late) = match noted {
        Ok(()) => execute(opts, settings, prepared, &mut out, &mut signals, started).await,
        Err(failure) => (Err(failure), None),
    };
    let exit = finish_async(&mut out, result, late, started, signals.as_mut().ok()).await;
    drop(out);
    // `signals` lives to the end of the function, so the publication is covered too.
    destination.commit(exit)
}

fn stdout_output(format: Format, limits: Limits) -> Out {
    output::create(format, Box::new(io::stdout()), limits)
}

/// Text output on stdout, for commands that have no --format (`sqlh profile ...`).
fn stdout_sink(limits: Limits) -> Out {
    stdout_output(Format::Text, limits)
}

/// Reports a failure that happened before any output was set up, before even the options
/// were understood: the default limits apply.
fn fail(failure: Failure, format: Format, started: Instant) -> Exit {
    fail_within(failure, format, Limits::default(), started)
}

/// Reports a failure that happened before any output was set up, within `limits`.
fn fail_within(failure: Failure, format: Format, limits: Limits, started: Instant) -> Exit {
    finish(&mut stdout_output(format, limits), Err(failure), started)
}

/// The output limits the flags give on their own (`--max-output`, `-t`), for a failure found
/// before a profile could add its own.
fn flag_limits(opts: &Opts) -> Limits {
    let defaults = Limits::default();
    Limits {
        max_output: opts.max_output.unwrap_or(defaults.max_output),
        stall_secs: opts.query_timeout.unwrap_or(defaults.stall_secs),
        ..defaults
    }
}

/// How a run ends: its exit code, the status line, and the error that stopped it, if any.
struct Closing {
    exit: Exit,
    status: String,
    error: Option<String>,
    elapsed_ms: u128,
}

impl Closing {
    /// `ok: <detail>, N ms`.
    fn ok(detail: &str, started: Instant) -> Closing {
        let elapsed_ms = started.elapsed().as_millis();
        Closing {
            exit: Exit::Ok,
            status: format!("ok: {detail}, {elapsed_ms} ms"),
            error: None,
            elapsed_ms,
        }
    }

    fn of(out: &Out, result: Result<Stats, Failure>, started: Instant) -> Closing {
        let ms = started.elapsed().as_millis();
        match result {
            Ok(stats) if out.tally().errors() == 0 => {
                let s = if stats.batches_run == 1 { "" } else { "es" };
                Closing::ok(&format!("{} batch{s}", stats.batches_run), started)
            }
            Ok(stats) => {
                let errors = out.tally().errors();
                let s = if errors == 1 { "" } else { "s" };
                let mut status = format!("failed (exit 1): {errors} error{s}");
                if let Some(batch) = stats.first_failed {
                    status.push_str(&format!(", first in batch {batch}"));
                }
                if stats.batches_skipped > 0 {
                    status.push_str(&format!(
                        "; {} later batches not run (--continue-on-error runs them)",
                        stats.batches_skipped
                    ));
                }
                status.push_str(&format!(", {ms} ms"));
                Closing {
                    exit: Exit::SqlError,
                    status,
                    error: None,
                    elapsed_ms: ms,
                }
            }
            Err(failure) => {
                let code = failure.exit as u8;
                Closing {
                    exit: failure.exit,
                    status: format!("failed (exit {code}): {}, {ms} ms", failure.exit.label()),
                    error: (!failure.message.is_empty()).then_some(failure.message),
                    elapsed_ms: ms,
                }
            }
        }
    }

    fn summary(&self) -> Summary<'_> {
        Summary {
            exit: self.exit as u8,
            status: &self.status,
            error: self.error.as_deref(),
            elapsed_ms: self.elapsed_ms,
        }
    }
}

/// Ends the output with `ok: <detail>, N ms` and returns success, or exit 7 if the output
/// couldn't be delivered. For commands without a runtime (`sqlh profile ...`).
fn finish_ok(out: &mut Out, detail: &str, started: Instant) -> Exit {
    let closing = Closing::ok(detail, started);
    deliver(out, &closing.summary(), closing.exit)
}

/// Ends the output with the run's status and returns the exit code. The wait for the reader
/// blocks the thread and nothing can cut it short but its limit, so this is for what runs
/// without a runtime: early usage errors and `sqlh profile ...`. A run uses `finish_async`.
fn finish(out: &mut Out, result: Result<Stats, Failure>, started: Instant) -> Exit {
    let closing = Closing::of(out, result, started);
    deliver(out, &closing.summary(), closing.exit)
}

/// `finish` for a run on the runtime (R10): the wait for the reader can be cut short by a
/// console event. `late` is a signal that came too late to change the run's outcome.
async fn finish_async(
    out: &mut Out,
    result: Result<Stats, Failure>,
    late: Option<Signal>,
    started: Instant,
    signals: Option<&mut Signals>,
) -> Exit {
    let closing = Closing::of(out, result, started);
    deliver_async(out, &closing.summary(), closing.exit, late, signals).await
}

/// How long the reader may stall while the closing lines are written (`None`: the stall
/// limit, -t). After a cancel the console may be gone, so don't wait long; after a failure
/// mid-run, don't wait at all.
fn stall_for(exit: Exit) -> Option<Duration> {
    match exit {
        Exit::Undelivered => Some(Duration::ZERO),
        Exit::Cancelled => Some(DELIVERY_GRACE),
        _ => None,
    }
}

/// Writes the closing lines and flushes. If that fails, the reader didn't get the complete
/// output, so the exit code becomes 7 and stderr says what the run itself ended with.
fn deliver(out: &mut Out, summary: &Summary, exit: Exit) -> Exit {
    let delivered = out
        .finish(summary)
        .and_then(|()| out.flush())
        .and_then(|()| out.wait_delivered(stall_for(exit)));
    settle(summary, exit, delivered, None)
}

/// `deliver` on the runtime, with the console event listeners alive (R10). A signal while
/// the closing lines wait for the reader gives the rest `DELIVERY_GRACE` more, even with
/// `-t 0`. If it arrives in time the run keeps its own exit code, since its SQL had
/// finished; if not, the exit is 7 and stderr says what the run itself ended with and that
/// the signal cut the delivery short. `late` is such a signal that arrived while the last
/// batches finished, too late to change the outcome: it bounds the wait from the start.
async fn deliver_async(
    out: &mut Out,
    summary: &Summary<'_>,
    exit: Exit,
    late: Option<Signal>,
    signals: Option<&mut Signals>,
) -> Exit {
    let mut cut = late;
    let queued = out.finish(summary).and_then(|()| out.flush());
    let delivered = match queued {
        Err(e) => Err(e),
        Ok(()) => {
            let (stall, within) = if late.is_some() || exit == Exit::Cancelled {
                (Some(DELIVERY_GRACE), Some(DELIVERY_GRACE))
            } else {
                (stall_for(exit), None)
            };
            let waiting = out.wait_delivered_async(stall, within);
            match signals {
                // Already held to `DELIVERY_GRACE`: another signal can't shorten that, and
                // must not start a new one.
                Some(signals) if within.is_none() => tokio::select! {
                    delivered = waiting => delivered,
                    signal = signals.recv() => {
                        cut = Some(signal);
                        out.wait_delivered_async(Some(DELIVERY_GRACE), Some(DELIVERY_GRACE))
                            .await
                    }
                },
                _ => waiting.await,
            }
        }
    };
    settle(summary, exit, delivered, cut)
}

/// The exit code once delivery has been tried: the run's own if all of it was delivered,
/// else 7. Says on stderr, best effort, what the run itself ended with; `cut` is the signal
/// that ended the wait early, if one did.
fn settle(summary: &Summary, exit: Exit, delivered: io::Result<()>, cut: Option<Signal>) -> Exit {
    if exit == Exit::Undelivered {
        // Writing failed mid-run (`output_failure`), and sqlh stopped at that point.
        diag(format_args!(
            "sqlh: output could not be delivered ({}); sqlh stopped there, so the outcome is \
             unknown: the batch being written may have run, and later batches did not",
            brief(summary.error.unwrap_or("writing output failed"))
        ));
        return exit;
    }
    match delivered {
        Ok(()) => exit,
        Err(e) => {
            let cause = cut
                .map(|s| format!("; cancelled by {} while delivering", s.name()))
                .unwrap_or_default();
            let error = summary
                .error
                .map(|e| format!(" ({})", brief(e)))
                .unwrap_or_default();
            diag(format_args!(
                "sqlh: output could not be delivered ({}{cause}); the run itself ended with \
                 exit {}: {}{error}",
                brief(&e.to_string()),
                exit as u8,
                summary.status
            ));
            Exit::Undelivered
        }
    }
}

/// `text` escaped like text-format metadata and cut after `DIAG_FIELD_CHARS` characters,
/// for a variable field in a one-line diagnostic.
fn brief(text: &str) -> String {
    let mut out = String::new();
    output::push_escaped(&mut out, text, DIAG_FIELD_CHARS);
    out
}

/// A path as shown in a diagnostic: escaped like any metadata field and cut (`brief`). A
/// Windows file name may hold U+2028 or U+2029, which some readers split lines at (review
/// R13), and a backslash is doubled, as in every `-- ` line.
fn shown(path: &Path) -> String {
    brief(&path.display().to_string())
}

/// The diagnostic for an `-o` file left as it was, because the output wasn't complete.
fn left_alone(target: &Path) -> String {
    format!("sqlh: -o {} was left as it was", shown(target))
}

/// The diagnostic for complete output that couldn't be moved into place.
fn not_published(target: &Path, error: &io::Error, exit: Exit) -> String {
    format!(
        "sqlh: output could not be delivered: moving it into {}: {}; the run itself ended with \
         exit {}",
        shown(target),
        brief(&error.to_string()),
        exit as u8
    )
}

/// Writes one line to stderr, best effort and bounded (R09). The write happens on a helper
/// thread that sqlh waits for no longer than `DIAG_WAIT`: taking Rust's stderr lock and the
/// OS write can each block for good when the reader has stopped reading, and the delivery
/// thread may be holding that lock itself (csv's metadata stream). A diagnostic that can't
/// be delivered is dropped, and never delays the exit or changes its code. Unlike
/// `eprintln!`, nothing here panics when stderr is closed.
fn diag(message: std::fmt::Arguments) {
    let mut line = message.to_string();
    line.push('\n');
    write_bounded(vec![(Target::Stderr, line)]);
}

#[derive(Clone, Copy)]
enum Target {
    Stdout = 0,
    Stderr = 1,
}

/// Streams a write has already timed out on: stalled, so nothing more is tried on them and
/// all the diagnostics of a run together cost at most one `DIAG_WAIT`.
static STALLED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

/// Writes each text to its stream on a helper thread, giving all of them together
/// `DIAG_WAIT`. Writes still blocked then are abandoned: the thread stays behind, and the
/// process ends without it.
fn write_bounded(writes: Vec<(Target, String)>) {
    let deadline = Instant::now() + DIAG_WAIT;
    let started: Vec<(Target, Abandonable)> = writes
        .into_iter()
        .filter(|(target, _)| !STALLED[*target as usize].load(Ordering::Relaxed))
        .filter_map(|(target, text)| {
            let job = Abandonable::spawn(move || match target {
                Target::Stdout => {
                    let mut stdout = io::stdout().lock();
                    let _ = stdout
                        .write_all(text.as_bytes())
                        .and_then(|()| stdout.flush());
                }
                Target::Stderr => {
                    let mut stderr = io::stderr().lock();
                    let _ = stderr
                        .write_all(text.as_bytes())
                        .and_then(|()| stderr.flush());
                }
            })?;
            Some((target, job))
        })
        .collect();
    for (target, job) in started {
        if !job.finished_by(deadline) {
            STALLED[target as usize].store(true, Ordering::Relaxed);
        }
    }
}

/// Work on a helper thread that the caller may walk away from: for writes that can block
/// for good.
struct Abandonable(mpsc::Receiver<()>);

impl Abandonable {
    /// Starts `job`. None if the thread couldn't be started; the job is then dropped.
    fn spawn(job: impl FnOnce() + Send + 'static) -> Option<Abandonable> {
        let (done, finished) = mpsc::channel();
        std::thread::Builder::new()
            .name("sqlh-diag".into())
            .spawn(move || {
                job();
                let _ = done.send(());
            })
            .ok()?;
        Some(Abandonable(finished))
    }

    /// Whether the job finished by `deadline`. If not, it is left running.
    fn finished_by(self, deadline: Instant) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        !matches!(
            self.0.recv_timeout(left),
            Err(mpsc::RecvTimeoutError::Timeout)
        )
    }
}

/// Output that has no status line (`--help`, `--version`): exit 0, or 7 if stdout is gone or
/// its reader stops reading for the default stall limit. It goes through the delivery thread
/// like any other output, so a reader that keeps the pipe open and reads nothing can't hold
/// sqlh forever (R09).
fn print_plain(text: &str) -> Exit {
    let stall = Duration::from_secs(Limits::default().stall_secs.into());
    let mut delivery = output::delivery::Delivery::start(Box::new(io::stdout()), None, Some(stall));
    let mut writer = delivery.writer(output::delivery::Dest::Main);
    let queued = writer
        .write_all(text.as_bytes())
        .and_then(|()| writer.flush());
    drop(writer);
    match queued.and_then(|()| delivery.finish(None)) {
        Ok(()) => Exit::Ok,
        Err(e) => {
            diag(format_args!(
                "sqlh: output could not be delivered ({})",
                brief(&e.to_string())
            ));
            Exit::Undelivered
        }
    }
}

/// Where the output goes (DESIGN.md §4.2, `-o`).
enum Destination {
    Stdout,
    /// A device or pipe, such as `-o NUL`: written directly, as there is no file to replace.
    Stream,
    /// A file. The run writes its own `.partial` file, which replaces the target whole when
    /// the output is complete, or is deleted when it isn't.
    File {
        partial: files::Partial,
        target: PathBuf,
    },
}

impl Destination {
    fn open(
        target: Option<&Path>,
        sources: &[Source],
    ) -> Result<(Self, Box<dyn Write + Send>), Failure> {
        let Some(target) = target else {
            return Ok((Destination::Stdout, Box::new(io::stdout())));
        };
        let shown = target.display();
        if target.is_dir() {
            return Err(Failure::usage(format!("-o {shown}: is a directory")));
        }
        let clash = sources.iter().find(|s| {
            s.path
                .as_deref()
                .is_some_and(|p| files::same_file(p, target))
        });
        if let Some(source) = clash {
            return Err(Failure::usage(format!(
                "-o {shown} would overwrite the input file {}; write the output somewhere else",
                source.name
            )));
        }
        let cannot = |e: io::Error| {
            let message = format!("-o {shown}: {e}");
            let path = target.to_str().unwrap_or_default();
            Failure::usage(console::path_error(message, path, &e))
        };
        // Opening an existing target (without truncating it) tells a device or pipe from a
        // file, and finds a file that can't be replaced before anything runs.
        match OpenOptions::new().write(true).open(target) {
            Ok(file) if files::is_stream(&file) => {
                return Ok((Destination::Stream, Box::new(file)));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(cannot(e)),
        }
        let partial = files::Partial::create(target).map_err(cannot)?;
        let writer = partial.writer().map_err(cannot)?;
        let target = target.to_path_buf();
        Ok((Destination::File { partial, target }, Box::new(writer)))
    }

    /// Moves `-o` output into place once it's complete. The writer must be closed by now.
    /// Undelivered output leaves the destination as it was.
    fn commit(self, exit: Exit) -> Exit {
        let Destination::File { partial, target } = self else {
            return exit;
        };
        if exit == Exit::Undelivered {
            // Dropping `partial` deletes it.
            diag(format_args!("{}", left_alone(&target)));
            return exit;
        }
        match partial.publish(&target) {
            Ok(()) => exit,
            Err(e) => {
                diag(format_args!("{}", not_published(&target, &e, exit)));
                Exit::Undelivered
            }
        }
    }
}

/// Input, batches and the guard's verdict: everything decided before any output is written.
struct Prepared {
    sources: Vec<Source>,
    batches: Vec<Batch>,
    /// Where and why the guard refused the input, if it did.
    blocked: Option<(String, guard::Blocked)>,
}

fn prepare(opts: &Opts, settings: &Settings) -> Result<Prepared, Failure> {
    let sources = load_sources(opts)?;
    let vars = (!opts.no_substitution).then_some(opts.vars.as_slice());
    let terminator = opts.terminator.as_deref().unwrap_or("GO");
    let batches = script::prepare(&sources, terminator, vars).map_err(Failure::usage)?;
    let policy = guard::Policy {
        writes: settings.allow_write,
        compile_only: opts.check,
    };
    let blocked = batches.iter().find_map(|batch| {
        guard::check(&batch.sql, policy).err().map(|blocked| {
            let line = batch.source_line(blocked.line);
            (location(&sources, batch.source, line), blocked)
        })
    });
    Ok(Prepared {
        sources,
        batches,
        blocked,
    })
}

/// Runs the input, or reports why it won't run. The signal, if any, is one that arrived
/// while the last batches finished, too late to change the outcome (`run_cancellable`).
/// `started` is the run clock: the batches are timed on it, as the closing lines are.
async fn execute(
    opts: &Opts,
    settings: &Settings,
    prepared: &Prepared,
    out: &mut Out,
    signals: &mut Result<Signals, String>,
    started: Instant,
) -> (Result<Stats, Failure>, Option<Signal>) {
    if let Some((location, blocked)) = &prepared.blocked {
        if let Err(e) = out.blocked(location, &blocked.reason, &blocked.advice) {
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
    run_cancellable(
        opts,
        settings,
        &prepared.sources,
        &prepared.batches,
        out,
        signals,
        started,
    )
    .await
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
    sources: &[Source],
    batches: &[Batch],
    out: &mut Out,
    signals: &mut Signals,
    started: Instant,
) -> (Result<Stats, Failure>, Option<Signal>) {
    let root = Cancel::new();
    let cancel = root.child();
    let work = run_session(opts, settings, sources, batches, out, &cancel, started);
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

async fn run_session(
    opts: &Opts,
    settings: &Settings,
    sources: &[Source],
    batches: &[Batch],
    out: &mut Out,
    cancel: &Cancel,
    started: Instant,
) -> Result<Stats, Failure> {
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
    // Layer 2 of the read-only guard: everything runs in a transaction that is rolled back.
    // `--check` always gets it too, as a second line of defense behind NOEXEC.
    let wrap = !settings.allow_write || opts.check;
    // `--check` reads nothing, so there is no read to make uncommitted and none to report.
    let read_uncommitted = settings.read_uncommitted && !opts.check;
    let setup = session_setup(settings.lock_timeout, read_uncommitted, wrap);
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
            if opts.check {
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
                .run(batch.sql.clone(), timeout, cancel, &mut **out, &gate)
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
                if !opts.continue_on_error {
                    stats.batches_skipped = total - number;
                    break 'batches;
                }
            }
        }
    }

    if opts.check {
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

fn db_failure(error: DbError) -> Failure {
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
fn output_failure(error: io::Error) -> Failure {
    if output::OutOfOrder::is(&error) {
        return Failure::new(Exit::Internal, format!("internal error: {error}"));
    }
    Failure::new(Exit::Undelivered, format!("writing output failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // PLAN.md 2.8: the session setup, exactly. The agent defaults are in every connection,
    // writes allowed or not; READ UNCOMMITTED only when asked for, before the wrapper
    // transaction begins.
    #[test]
    fn the_session_setup_carries_the_agent_defaults() {
        let ssms = SESSION_SETUP;
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

    /// A writer that fails after accepting `ok` bytes, or only at flush.
    struct Failing {
        ok: usize,
        fail_flush: bool,
    }

    impl Write for Failing {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.ok == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "reader went away",
                ));
            }
            let n = buf.len().min(self.ok);
            self.ok -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("disk full"))
            } else {
                Ok(())
            }
        }
    }

    fn out(ok: usize, fail_flush: bool) -> Out {
        output::create(
            Format::Text,
            Box::new(Failing { ok, fail_flush }),
            Limits::default(),
        )
    }

    /// A reader that never reads: every write blocks until the test lets go (drops
    /// `release`).
    struct Blocked(Mutex<mpsc::Receiver<()>>);

    impl Write for Blocked {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let _ = self.0.lock().unwrap().recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// An output whose reader is stalled, with no stall limit of its own (`-t 0`).
    fn stalled_out() -> (Out, mpsc::Sender<()>) {
        let (release, blocked) = mpsc::channel();
        let limits = Limits {
            stall_secs: 0,
            ..Limits::default()
        };
        let out = output::create(Format::Text, Box::new(Blocked(Mutex::new(blocked))), limits);
        (out, release)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Delivers the closing lines of a successful run on the runtime, without listeners.
    fn deliver_ok(out: &mut Out, exit: Exit, late: Option<Signal>) -> (Exit, Duration) {
        let closing = Closing::ok("0 batches", Instant::now());
        let started = Instant::now();
        let exit = runtime().block_on(deliver_async(out, &closing.summary(), exit, late, None));
        (exit, started.elapsed())
    }

    // R10: with -t 0 nothing else bounds the wait for a stalled reader, so a signal that came
    // too late to change the run (`late`) must: one second, then exit 7.
    #[test]
    fn a_late_signal_bounds_the_wait_even_with_no_stall_limit() {
        let (mut out, release) = stalled_out();
        let (exit, elapsed) = deliver_ok(&mut out, Exit::Ok, Some(Signal::CtrlBreak));
        assert_eq!(exit, Exit::Undelivered);
        assert!(
            elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(5),
            "{elapsed:?}"
        );
        drop(out);
        drop(release);
    }

    // PLAN.md 2.4: after a cancel the closing lines get one second, whatever -t says, and
    // the delivery is detached once it gives up, so dropping the output doesn't wait.
    #[test]
    fn after_a_cancel_the_closing_lines_get_one_second() {
        let (mut out, release) = stalled_out();
        let (exit, elapsed) = deliver_ok(&mut out, Exit::Cancelled, None);
        assert_eq!(exit, Exit::Undelivered);
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        let dropping = Instant::now();
        drop(out);
        assert!(dropping.elapsed() < Duration::from_secs(1));
        drop(release);
    }

    // The async delivery keeps the run's own exit code when everything is delivered.
    #[test]
    fn async_delivery_keeps_the_runs_exit_code() {
        for exit in [Exit::Ok, Exit::SqlError, Exit::Cancelled] {
            let mut out = out(usize::MAX, false);
            let (delivered, _) = deliver_ok(&mut out, exit, None);
            assert_eq!(delivered, exit);
        }
        let mut broken = out(0, false);
        let (delivered, _) = deliver_ok(&mut broken, Exit::Ok, None);
        assert_eq!(delivered, Exit::Undelivered);
    }

    // PLAN.md 2.12: a sink that refuses an event out of order has found a bug in sqlh or its
    // driver, so the run is an internal error (exit 10), not undeliverable output (exit 7);
    // a failure to write stays exit 7.
    #[test]
    fn an_event_out_of_order_is_an_internal_error_and_a_broken_pipe_is_not() {
        let mut tally = output::Tally::new(Limits::default());
        tally.start_result();
        let refused = tally.outside_rows("a message").unwrap_err();
        let failure = output_failure(refused);
        assert_eq!(failure.exit, Exit::Internal);
        assert!(
            failure
                .message
                .starts_with("internal error: a message arrived while a result set's rows were"),
            "{}",
            failure.message
        );
        let broken = output_failure(io::Error::from(io::ErrorKind::InvalidInput));
        assert_eq!(broken.exit, Exit::Undelivered);
        let broken = output_failure(io::Error::from(io::ErrorKind::BrokenPipe));
        assert_eq!(broken.exit, Exit::Undelivered);
        assert!(broken.message.starts_with("writing output failed: "));
    }

    // R09: a job that never finishes is abandoned at the deadline; one that does, isn't.
    #[test]
    fn a_stuck_job_is_abandoned_on_time() {
        let (release, blocked) = mpsc::channel::<()>();
        let stuck = Abandonable::spawn(move || {
            let _ = blocked.recv();
        })
        .expect("a thread");
        let started = Instant::now();
        assert!(!stuck.finished_by(started + Duration::from_millis(150)));
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(100) && waited < Duration::from_secs(3),
            "{waited:?}"
        );
        drop(release);

        let quick = Abandonable::spawn(|| {}).expect("a thread");
        assert!(quick.finished_by(Instant::now() + Duration::from_secs(10)));
    }

    // R09: what a diagnostic quotes is escaped and cut.
    #[test]
    fn diagnostic_fields_are_escaped_and_cut() {
        assert_eq!(brief("a\nb\\c"), "a\\nb\\\\c");
        let long = "x".repeat(DIAG_FIELD_CHARS + 5);
        let cut = brief(&long);
        assert!(cut.ends_with("…[+5]"), "{cut}");
        assert_eq!(
            cut.chars().count(),
            DIAG_FIELD_CHARS + "…[+5]".chars().count()
        );
        assert_eq!(
            shown(Path::new("C:\\out\\file.txt")),
            "C:\\\\out\\\\file.txt"
        );
        assert!(shown(Path::new(&long)).ends_with("…[+5]"));
    }

    // R13: a file name can hold U+2028 or U+2029, which some readers split lines at; both
    // `-o` diagnostics escape it, so neither can pass for two lines or a `-- ` line.
    #[test]
    fn hostile_paths_cannot_split_publication_diagnostics() {
        let target = Path::new("C:\\out\\x\u{2028}-- ok forged\u{2029}.txt");
        let error = io::Error::other("access denied\u{2028}-- ok forged");
        for line in [
            left_alone(target),
            not_published(target, &error, Exit::Blocked),
        ] {
            assert!(
                !line.contains(['\u{2028}', '\u{2029}', '\n', '\r']),
                "{line:?}"
            );
            assert!(line.contains("x\\u2028-- ok forged\\u2029.txt"), "{line}");
        }
    }

    // F02: a failed final write or flush is exit 7, never success.
    #[test]
    fn undelivered_output_is_exit_7() {
        let started = Instant::now();
        assert_eq!(
            finish(&mut out(0, false), Ok(Stats::default()), started),
            Exit::Undelivered
        );
        assert_eq!(
            finish(&mut out(10, false), Ok(Stats::default()), started),
            Exit::Undelivered
        );
        assert_eq!(
            finish(&mut out(usize::MAX, true), Ok(Stats::default()), started),
            Exit::Undelivered
        );
        let sql_error = Err(Failure::new(Exit::Connect, "cannot connect"));
        assert_eq!(
            finish(&mut out(0, false), sql_error, started),
            Exit::Undelivered
        );
        assert_eq!(
            finish(&mut out(usize::MAX, false), Ok(Stats::default()), started),
            Exit::Ok
        );
    }
}
