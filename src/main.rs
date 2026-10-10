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

use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cli::{Command, Opts};
use config::Settings;
use db::{Cancel, DbError, Session};
use error::{Exit, Failure};
use output::{BatchInfo, Format, Limits, Output, Summary};
use script::{Batch, Source};

const HELP: &str = include_str!("help.txt");

/// Session options matching SSMS, so plans and filtered/computed-column indexes behave as
/// people expect (DESIGN.md §4.3).
const SESSION_SETUP: &str = "SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; \
    SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; SET QUOTED_IDENTIFIER ON; \
    SET NUMERIC_ROUNDABORT OFF;";

type Out = Box<dyn Output>;

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
    console::use_utf8();
    let exit = match cli::parse() {
        Ok(Command::Run(opts)) => run_command(*opts, started),
        Ok(Command::Profile(command)) => profile::run(command, started),
        Ok(Command::Help) => print_plain(HELP),
        Ok(Command::Version) => print_plain(&format!("sqlh {}\n", env!("CARGO_PKG_VERSION"))),
        Err(message) => fail(Failure::usage(message), Format::Text, started),
    };
    exit.into()
}

/// A panic is a bug, but it must still end with the documented exit code 10 rather than an
/// abort (0xC0000409). The rest is best effort: a stderr line, and a status line on stdout
/// in text output. Output still buffered is lost, a JSON document stays unfinished, and
/// `-o` output is never published. Expected failures must not rely on this: they are
/// `Result`s, and nothing here may panic again.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = info.payload_as_str().unwrap_or("unknown panic");
        let location = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        console::restore_echo();
        diag(format_args!("sqlh: internal error: {message}{location}"));
        if TEXT_ON_STDOUT.load(Ordering::Relaxed) {
            let mut line = String::new();
            output::push_escaped(&mut line, message, 0);
            let mut stdout = io::stdout();
            let _ = write!(
                stdout,
                "\n-- error: internal error: {line}\n-- failed (exit 10): internal error\n"
            );
            let _ = stdout.flush();
        }
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
    if opts.write_policy.is_some() || opts.new_password {
        return fail(
            Failure::usage("--write-policy and --new-password only apply to sqlh profile add"),
            format,
            started,
        );
    }
    let settings = match config::resolve(&opts) {
        Ok(settings) => settings,
        Err(failure) => return fail(failure, format, started),
    };
    // Everything that can fail before the run happens before `-o` is opened, so bad input
    // never truncates or replaces an existing output file.
    let prepared = match prepare(&opts, &settings) {
        Ok(prepared) => prepared,
        Err(failure) => return fail(failure, format, started),
    };
    let (destination, writer) = match Destination::open(opts.output.as_deref(), &prepared.sources) {
        Ok(opened) => opened,
        Err(failure) => return fail(failure, format, started),
    };
    let mut out = output::create(format, writer, settings.limits);
    let result = if opts.ignored.is_empty() {
        Ok(())
    } else {
        out.note(&format!(
            "ignored sqlcmd flags {} (sqlh output is always compact)",
            opts.ignored.join(" ")
        ))
        .map_err(output_failure)
    };
    let result = result.and_then(|()| execute(&opts, &settings, &prepared, &mut out));
    let exit = finish(&mut out, result, started);
    drop(out);
    destination.commit(exit)
}

fn stdout_output(format: Format, limits: Limits) -> Out {
    output::create(format, Box::new(io::stdout().lock()), limits)
}

/// Text output on stdout, for commands that have no --format (`sqlh profile ...`).
fn stdout_sink(limits: Limits) -> Out {
    stdout_output(Format::Text, limits)
}

/// Reports a failure that happened before any output was set up.
fn fail(failure: Failure, format: Format, started: Instant) -> Exit {
    finish(
        &mut stdout_output(format, Limits::default()),
        Err(failure),
        started,
    )
}

/// Ends the output with `ok: <detail>, N ms` and returns success, or exit 7 if the output
/// couldn't be delivered.
fn finish_ok(out: &mut Out, detail: &str, started: Instant) -> Exit {
    let elapsed_ms = started.elapsed().as_millis();
    let status = format!("ok: {detail}, {elapsed_ms} ms");
    let summary = Summary {
        exit: 0,
        status: &status,
        error: None,
        elapsed_ms,
    };
    deliver(out, &summary, Exit::Ok)
}

/// Ends the output with the run's status and returns the exit code.
fn finish(out: &mut Out, result: Result<Stats, Failure>, started: Instant) -> Exit {
    let ms = started.elapsed().as_millis();
    let mut error = None;
    let (exit, status) = match result {
        Ok(stats) if out.tally().errors() == 0 => {
            let s = if stats.batches_run == 1 { "" } else { "es" };
            return finish_ok(out, &format!("{} batch{s}", stats.batches_run), started);
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
            (Exit::SqlError, status)
        }
        Err(failure) => {
            let code = failure.exit as u8;
            if !failure.message.is_empty() {
                error = Some(failure.message);
            }
            (
                failure.exit,
                format!("failed (exit {code}): {}, {ms} ms", failure.exit.label()),
            )
        }
    };
    let summary = Summary {
        exit: exit as u8,
        status: &status,
        error: error.as_deref(),
        elapsed_ms: ms,
    };
    deliver(out, &summary, exit)
}

/// Writes the closing lines and flushes. If that fails, the reader didn't get the complete
/// output, so the exit code becomes 7 and stderr says what the run itself ended with.
fn deliver(out: &mut Out, summary: &Summary, exit: Exit) -> Exit {
    let delivered = out.finish(summary).and_then(|()| out.flush());
    if exit == Exit::Undelivered {
        // Writing failed mid-run (`output_failure`), and sqlh stopped at that point.
        diag(format_args!(
            "sqlh: output could not be delivered ({}); sqlh stopped there, so the outcome is \
             unknown: the batch being written may have run, and later batches did not",
            summary.error.unwrap_or("writing output failed")
        ));
        return exit;
    }
    match delivered {
        Ok(()) => exit,
        Err(e) => {
            diag(format_args!(
                "sqlh: output could not be delivered ({e}); the run itself ended with exit {}: {}",
                exit as u8, summary.status
            ));
            Exit::Undelivered
        }
    }
}

/// Writes one line to stderr, best effort. Unlike `eprintln!`, it never panics when stderr
/// is closed, so a diagnostic that can't be delivered never changes the exit code.
fn diag(message: std::fmt::Arguments) {
    let _ = writeln!(io::stderr().lock(), "{message}");
}

/// Output that has no status line (`--help`, `--version`): exit 0, or 7 if stdout is gone.
fn print_plain(text: &str) -> Exit {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => Exit::Ok,
        Err(e) => {
            diag(format_args!("sqlh: output could not be delivered ({e})"));
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
    fn open(target: Option<&Path>, sources: &[Source]) -> Result<(Self, Box<dyn Write>), Failure> {
        let Some(target) = target else {
            return Ok((Destination::Stdout, Box::new(io::stdout().lock())));
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
        let cannot = |e: io::Error| Failure::usage(format!("-o {shown}: {e}"));
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
            diag(format_args!(
                "sqlh: -o {} was left as it was",
                target.display()
            ));
            return exit;
        }
        match partial.publish(&target) {
            Ok(()) => exit,
            Err(e) => {
                diag(format_args!(
                    "sqlh: output could not be delivered: moving it into {}: {e}; the run itself \
                     ended with exit {}",
                    target.display(),
                    exit as u8
                ));
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

fn execute(
    opts: &Opts,
    settings: &Settings,
    prepared: &Prepared,
    out: &mut Out,
) -> Result<Stats, Failure> {
    if let Some((location, blocked)) = &prepared.blocked {
        out.blocked(location, &blocked.reason, &blocked.advice)
            .map_err(output_failure)?;
        return Err(Failure::new(Exit::Blocked, ""));
    }
    if prepared.batches.is_empty() {
        out.note("no SQL to run").map_err(output_failure)?;
        return Ok(Stats::default());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Failure::new(
                Exit::Internal,
                format!("cannot start the async runtime: {e}"),
            )
        })?;
    runtime.block_on(run_cancellable(
        opts,
        settings,
        &prepared.sources,
        &prepared.batches,
        out,
    ))
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
            let file =
                std::fs::File::open(input).map_err(|e| Failure::usage(format!("{label}: {e}")))?;
            let bytes = read_capped(file, &label, &mut left)?;
            (input.clone(), Some(PathBuf::from(input)), bytes)
        };
        let text = script::decode(bytes, &name).map_err(Failure::usage)?;
        sources.push(Source { name, path, text });
    }
    if sources.is_empty() {
        if io::stdin().is_terminal() {
            return Err(Failure::usage(
                "no SQL given: pass -Q \"...\", -i file.sql, or pipe SQL on stdin",
            ));
        }
        let bytes = read_capped(io::stdin().lock(), "stdin", &mut left)?;
        let text = script::decode(bytes, "stdin").map_err(Failure::usage)?;
        sources.push(Source {
            name: "stdin".into(),
            path: None,
            text,
        });
    }
    Ok(sources)
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

/// Runs the session; Ctrl+C sends an attention and gives it two seconds to land.
async fn run_cancellable(
    opts: &Opts,
    settings: &Settings,
    sources: &[Source],
    batches: &[Batch],
    out: &mut Out,
) -> Result<Stats, Failure> {
    let root = Cancel::new();
    let cancel = root.child();
    let work = run_session(opts, settings, sources, batches, out, &cancel);
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => result,
        _ = tokio::signal::ctrl_c() => {
            root.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut work).await;
            Err(Failure::new(Exit::Cancelled, "interrupted; the running batch was cancelled"))
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
    // Layer 2 of the read-only guard: everything runs in a transaction that is rolled back.
    // `--check` always gets it too, as a second line of defense behind NOEXEC.
    let wrap = !settings.allow_write || opts.check;
    let mut setup = String::from(SESSION_SETUP);
    if wrap {
        setup.push_str(" BEGIN TRANSACTION;");
    }
    let setup_started = Instant::now();
    session
        .exec_internal(&setup, timeout)
        .await
        .map_err(db_failure)?;
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
            diag(format_args!("sqlh: {line}"));
        }
    }

    let mut stats = Stats::default();
    let total: usize = batches.iter().map(|b| b.repeat as usize).sum();
    let mut number = 0;
    'batches: for batch in batches {
        for run in 1..=batch.repeat {
            number += 1;
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
                    .exec_internal("SET NOEXEC ON;", timeout)
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
            })
            .map_err(output_failure)?;
            session
                .run(batch.sql.clone(), timeout, cancel, &mut **out)
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
            out.end_batch().map_err(output_failure)?;
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
        let _ = session.exec_internal("SET NOEXEC OFF;", timeout).await;
    }
    if wrap && session.in_transaction() {
        let _ = session
            .exec_internal("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;", timeout)
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
        DbError::Cancelled => Failure::new(Exit::Cancelled, "cancelled"),
        DbError::Sql(message) => Failure::new(Exit::SqlError, message),
        DbError::Output(e) => output_failure(e),
        DbError::Protocol(message) => Failure::new(Exit::Internal, message),
    }
}

/// Writing output failed mid-run: the reader went away, or the `-o` file couldn't be written.
fn output_failure(error: io::Error) -> Failure {
    Failure::new(Exit::Undelivered, format!("writing output failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
