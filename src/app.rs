//! The command-line application: arguments, the output and its closing lines, `-o`, the
//! panic net and the diagnostics of last resort (DESIGN.md §4–§6). The run itself is
//! `run` (PLAN.md 3.1).

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cli::{self, Command, Opts};
use crate::config::{self, Settings};
use crate::error::{Exit, Failure};
use crate::output::{self, Format, Limits, Summary};
use crate::run::{
    self, Closing, Input, Prepared, QueryOrigin, Report, Request, RunOutcome, output_failure,
};
use crate::script::Source;
use crate::signals::{Signal, Signals};
use crate::{console, files, profile};

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

pub(crate) type Out = output::Stream;

/// Whether stdout carries the text format, so the panic hook may append a status line.
/// JSON and CSV can't be completed by a stray line, so they only get the stderr message.
static TEXT_ON_STDOUT: AtomicBool = AtomicBool::new(true);

pub(crate) fn main() -> ExitCode {
    let started = Instant::now();
    install_panic_hook();
    // `sqlh mcp`'s stdout carries only protocol messages (docs/mcp-mode.md §2): its usage
    // errors, and the panic hook's lines, go to stderr only.
    let mcp = std::env::args_os().nth(1).is_some_and(|a| a == "mcp");
    if mcp {
        TEXT_ON_STDOUT.store(false, Ordering::Relaxed);
    }
    let exit = match cli::parse() {
        Ok(Command::Run(opts)) => run_command(*opts, QueryOrigin::User, started),
        Ok(Command::Profile(command)) => profile::run(command, started),
        Ok(Command::Mcp(opts)) => crate::mcp::run(*opts),
        Ok(Command::Help) => print_plain(HELP),
        Ok(Command::Version) => print_plain(&version_line()),
        Err(message) if mcp => {
            mcp_log(&format!("sqlh mcp: usage error: {message}"));
            Exit::Usage
        }
        Err(message) => fail(Failure::usage(message), Format::Text, started),
    };
    exit.into()
}

/// A line on stderr for `sqlh mcp`, which has no output of its own there: its launch errors
/// (docs/mcp-mode.md §3). Bounded, like every last-resort line, and escaped as any field is
/// (DESIGN.md §5.1), since an argument it names can hold a line break: one error, one line.
pub(crate) fn mcp_log(line: &str) {
    diag(format_args!("{}", output::escaped(line)));
}

/// What `--version` prints: the program's version, and the version of the JSON document it
/// writes (DESIGN.md §5.2), which the document itself starts with. Text and csv carry no
/// version of their own, so a tool that wants to know what to expect asks here.
fn version_line() -> String {
    format!(
        "sqlh {VERSION} (json contract {})\n",
        output::json::CONTRACT_VERSION
    )
}

/// sqlh's version: the release's, given at build time by the release workflow (`SQLH_VERSION`,
/// PLAN.md 3.6: `0.1.0` from the tag `v0.1.0`, or `0.1.0-dev.<run>` for a build without a
/// tag), else `Cargo.toml`'s.
pub(crate) const VERSION: &str = match option_env!("SQLH_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

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

/// Runs SQL and delivers its output: the user's (`sqlh -Q ...`), or a command's own
/// (`sqlh profile test`), which `origin` tells apart (`QueryOrigin`).
pub(crate) fn run_command(opts: Opts, origin: QueryOrigin, started: Instant) -> Exit {
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
    // `--profile add Dev ...` for `profile add Dev ...`: a profile named like a profile
    // command that the profiles file doesn't have is that typo, whatever follows it.
    if let Some(name) = opts.profile.as_deref() {
        let command = ["add", "list", "test", "rm"]
            .into_iter()
            .find(|c| c.eq_ignore_ascii_case(name));
        let saved = config::load().is_ok_and(|c| c.find(name).is_some());
        if let (Some(command), false) = (command, saved) {
            return fail_within(
                Failure::usage(format!(
                    "--profile {name} picks a saved profile named {name}, and there is none; \
                     to manage profiles, leave out the dashes: sqlh profile {command} ..."
                )),
                format,
                flagged,
                started,
            );
        }
    }
    if opts.write_policy.is_some() || opts.new_password || opts.description.is_some() {
        return fail_within(
            Failure::usage(
                "--write-policy, --new-password and --description only apply to sqlh profile add",
            ),
            format,
            flagged,
            started,
        );
    }
    if opts.command.is_some() && opts.check {
        return fail_within(
            Failure::usage("--check compiles your SQL; a schema command runs its own"),
            format,
            flagged,
            started,
        );
    }
    let mut settings = match config::resolve(&opts) {
        Ok(settings) => settings,
        Err(failure) => return fail_within(failure, format, flagged, started),
    };
    // A schema command's SQL is sqlh's own (read-only, unguarded), with its own default limits.
    let origin = match &opts.command {
        Some(command) => {
            command.limits(
                &mut settings.limits,
                opts.max_rows.is_some(),
                opts.max_cell.is_some(),
            );
            QueryOrigin::Generated
        }
        None => origin,
    };
    // Everything that can fail before the run happens before `-o` is opened, so bad input
    // never truncates or replaces an existing output file.
    let prepared = match request(&opts).and_then(|r| run::prepare(r, &settings, origin)) {
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
            let exit = finish(&mut out, RunOutcome::new(Err(failure)), started);
            drop(out);
            return destination.commit(exit);
        }
    };
    let exit = runtime.block_on(run_and_deliver(
        &opts,
        &settings,
        &prepared,
        out,
        destination,
        started,
    ));
    // The run, its delivery and its publication are over; what the runtime still runs (a
    // name lookup the driver left on a blocking thread after a cancel) is abandoned, not
    // waited for, so a signal's grace bounds the exit (DESIGN.md §4.3).
    runtime.shutdown_background();
    exit
}

/// What the run core is asked to do (`run::Request`), from the command line.
fn request(opts: &Opts) -> Result<Request, Failure> {
    if let Some(command) = &opts.command {
        return Ok(Request {
            input: command.input(output::Advice::Cli)?,
            vars: None,
            terminator: "GO".into(),
            check: false,
            continue_on_error: false,
            report: if opts.verbose && command.report() == Report::Off {
                Report::Diagnostic
            } else {
                command.report()
            },
            database: None,
        });
    }
    let input = match (&opts.query, opts.inputs.is_empty()) {
        (Some(_), false) => return Err(Failure::usage("give SQL with -Q or with -i, not both")),
        (Some(query), true) => Input::Query(query.clone()),
        (None, false) => Input::Files(opts.inputs.clone()),
        (None, true) => Input::Stdin,
    };
    let report = if opts.show_connection {
        Report::Note
    } else if opts.verbose {
        Report::Diagnostic
    } else {
        Report::Off
    };
    Ok(Request {
        input,
        vars: (!opts.no_substitution).then(|| opts.vars.clone()),
        terminator: opts.terminator.clone().unwrap_or_else(|| "GO".into()),
        check: opts.check,
        continue_on_error: opts.continue_on_error,
        report,
        database: None,
    })
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
    let gate = out.gate();
    let (outcome, late) = match noted {
        Ok(()) => run::execute(settings, prepared, &mut *out, &gate, &mut signals, started).await,
        Err(failure) => (RunOutcome::new(Err(failure)), None),
    };
    let exit = finish_async(&mut out, outcome, late, started, signals.as_mut().ok()).await;
    drop(out);
    // `signals` lives to the end of the function, so the publication is covered too.
    destination.commit(exit)
}

fn stdout_output(format: Format, limits: Limits) -> Out {
    output::create(format, Box::new(io::stdout()), limits)
}

/// Text output on stdout, for commands that have no --format (`sqlh profile ...`), so the
/// panic hook's text status line (`TEXT_ON_STDOUT`, true unless a run says otherwise) is right
/// for them.
pub(crate) fn stdout_sink(limits: Limits) -> Out {
    stdout_output(Format::Text, limits)
}

/// Reports a failure that happened before any output was set up, before even the options
/// were understood: the default limits apply.
fn fail(failure: Failure, format: Format, started: Instant) -> Exit {
    fail_within(failure, format, Limits::default(), started)
}

/// Reports a failure that happened before any output was set up, within `limits`.
fn fail_within(failure: Failure, format: Format, limits: Limits, started: Instant) -> Exit {
    finish(
        &mut stdout_output(format, limits),
        RunOutcome::new(Err(failure)),
        started,
    )
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

/// couldn't be delivered. For commands without a runtime (`sqlh profile ...`).
pub(crate) fn finish_ok(out: &mut Out, detail: &str, started: Instant) -> Exit {
    let closing = Closing::ok(detail, started);
    deliver(out, &closing.summary(), closing.exit)
}

/// Ends the output with the run's status and returns the exit code. The wait for the reader
/// blocks the thread and nothing can cut it short but its limit, so this is for what runs
/// without a runtime: early usage errors and `sqlh profile ...`. A run uses `finish_async`.
pub(crate) fn finish(out: &mut Out, outcome: RunOutcome, started: Instant) -> Exit {
    let closing = Closing::of(outcome, started, output::Advice::Cli);
    deliver(out, &closing.summary(), closing.exit)
}

/// `finish` for a run on the runtime (R10): the wait for the reader can be cut short by a
/// console event. `late` is a signal that came too late to change the run's outcome.
async fn finish_async(
    out: &mut Out,
    outcome: RunOutcome,
    late: Option<Signal>,
    started: Instant,
    signals: Option<&mut Signals>,
) -> Exit {
    let closing = Closing::of(outcome, started, output::Advice::Cli);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Stats;
    use std::sync::Mutex;

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

    // PLAN.md 2.15: the status line says how many batches had a count stopped, in a run that
    // succeeded and in one that failed (with a SQL error or any other failure), and says
    // nothing otherwise. The counts are the run's own (`run::Accounting`, whose agreement
    // with the output's tally `run.rs` tests).
    #[test]
    fn the_status_line_says_where_counting_stopped() {
        let stats = || Stats {
            batches_run: 3,
            ..Stats::default()
        };
        let outcome = |result, errors, stopped_batches| RunOutcome {
            result,
            errors,
            stopped_batches,
        };
        let clean = Closing::of(
            outcome(Ok(stats()), 0, 0),
            Instant::now(),
            output::Advice::Cli,
        );
        assert!(
            clean.status.starts_with("ok: 3 batches, ") && !clean.status.contains("stopped"),
            "{}",
            clean.status
        );
        let timeout = || Failure::new(Exit::Timeout, "query timeout");
        let clean = Closing::of(
            outcome(Err(timeout()), 0, 0),
            Instant::now(),
            output::Advice::Cli,
        );
        assert!(
            clean.status.starts_with("failed (exit 4): timeout, ")
                && !clean.status.contains("stopped"),
            "{}",
            clean.status
        );
        // A failure that is not a SQL error keeps its exit code and first words, and adds the
        // same words, before the time.
        let failed = Closing::of(
            outcome(Err(timeout()), 0, 2),
            Instant::now(),
            output::Advice::Cli,
        );
        assert_eq!(failed.exit, Exit::Timeout);
        assert!(
            failed
                .status
                .starts_with("failed (exit 4): timeout, counting stopped in 2, "),
            "{}",
            failed.status
        );
        assert_eq!(failed.error.as_deref(), Some("query timeout"));
        let stopped = Closing::of(
            outcome(Ok(stats()), 0, 2),
            Instant::now(),
            output::Advice::Cli,
        );
        assert!(
            stopped
                .status
                .starts_with("ok: 3 batches, counting stopped in 2, "),
            "{}",
            stopped.status
        );
        assert_eq!(stopped.exit, Exit::Ok);
        // A failure keeps its own exit code and first words, and adds the same words.
        let failed = Closing::of(
            outcome(Ok(stats()), 1, 2),
            Instant::now(),
            output::Advice::Cli,
        );
        assert_eq!(failed.exit, Exit::SqlError);
        assert!(
            failed.status.starts_with("failed (exit 1): 1 error")
                && failed.status.contains(", counting stopped in 2, "),
            "{}",
            failed.status
        );
    }

    // In sqlh mcp the status line names no command-line flag: a usage error doesn't point at
    // --help, and the batches a SQL error left unrun don't offer --continue-on-error.
    #[test]
    fn the_status_line_in_mcp_names_no_flag() {
        let skipped = || RunOutcome {
            result: Ok(Stats {
                batches_run: 1,
                batches_skipped: 2,
                first_failed: Some(1),
            }),
            errors: 1,
            stopped_batches: 0,
        };
        let usage = || RunOutcome::new(Err(Failure::usage("bad")));
        for (advice, skip_tail, usage_label) in [
            (
                output::Advice::Cli,
                " (--continue-on-error runs them)",
                "usage error (see sqlh --help)",
            ),
            (output::Advice::Mcp, "", "usage error"),
        ] {
            let status = Closing::of(skipped(), Instant::now(), advice).status;
            assert!(
                status.starts_with(&format!(
                    "failed (exit 1): 1 error, first in batch 1; 2 later batches not run\
                     {skip_tail}, "
                )),
                "{status}"
            );
            let status = Closing::of(usage(), Instant::now(), advice).status;
            assert!(
                status.starts_with(&format!("failed (exit 3): {usage_label}, ")),
                "{status}"
            );
        }
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
            finish(
                &mut out(0, false),
                RunOutcome::new(Ok(Stats::default())),
                started
            ),
            Exit::Undelivered
        );
        assert_eq!(
            finish(
                &mut out(10, false),
                RunOutcome::new(Ok(Stats::default())),
                started
            ),
            Exit::Undelivered
        );
        assert_eq!(
            finish(
                &mut out(usize::MAX, true),
                RunOutcome::new(Ok(Stats::default())),
                started
            ),
            Exit::Undelivered
        );
        let sql_error = Err(Failure::new(Exit::Connect, "cannot connect"));
        assert_eq!(
            finish(&mut out(0, false), RunOutcome::new(sql_error), started),
            Exit::Undelivered
        );
        assert_eq!(
            finish(
                &mut out(usize::MAX, false),
                RunOutcome::new(Ok(Stats::default())),
                started
            ),
            Exit::Ok
        );
    }
}
