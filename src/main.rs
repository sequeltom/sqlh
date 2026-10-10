mod cli;
mod config;
mod console;
mod credman;
mod db;
mod error;
mod guard;
mod output;
mod profile;
mod script;

use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::process::ExitCode;
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

#[derive(Default)]
struct Stats {
    batches_run: usize,
    batches_skipped: usize,
    first_failed: Option<usize>,
}

fn main() -> ExitCode {
    let started = Instant::now();
    console::use_utf8();
    let exit = match cli::parse() {
        Ok(Command::Run(opts)) => run_command(*opts, started),
        Ok(Command::Profile(command)) => profile::run(command, started),
        Ok(Command::Help) => {
            print!("{HELP}");
            Exit::Ok
        }
        Ok(Command::Version) => {
            println!("sqlh {}", env!("CARGO_PKG_VERSION"));
            Exit::Ok
        }
        Err(message) => fail(Failure::usage(message), Format::Text, started),
    };
    exit.into()
}

fn run_command(opts: Opts, started: Instant) -> Exit {
    let format = opts.format.unwrap_or(Format::Text);
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
    let mut out = match &opts.output {
        Some(path) => match File::create(path) {
            Ok(file) => output::create(format, Box::new(file), settings.limits),
            Err(e) => {
                return fail(
                    Failure::usage(format!("-o {}: {e}", path.display())),
                    format,
                    started,
                );
            }
        },
        None => stdout_output(format, settings.limits),
    };
    if !opts.ignored.is_empty() {
        let _ = out.note(&format!(
            "ignored sqlcmd flags {} (sqlh output is always compact)",
            opts.ignored.join(" ")
        ));
    }
    let result = run(&opts, &settings, &mut out);
    finish(&mut out, result, started)
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

/// Ends the output with `ok: <detail>, N ms` and returns success.
fn finish_ok(out: &mut Out, detail: &str, started: Instant) -> Exit {
    let elapsed_ms = started.elapsed().as_millis();
    let status = format!("ok: {detail}, {elapsed_ms} ms");
    let _ = out.finish(&Summary {
        exit: 0,
        status: &status,
        error: None,
        elapsed_ms,
    });
    let _ = out.flush();
    Exit::Ok
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
    let _ = out.finish(&Summary {
        exit: exit as u8,
        status: &status,
        error: error.as_deref(),
        elapsed_ms: ms,
    });
    let _ = out.flush();
    exit
}

fn run(opts: &Opts, settings: &Settings, out: &mut Out) -> Result<Stats, Failure> {
    let sources = load_sources(opts)?;
    let vars = (!opts.no_substitution).then_some(opts.vars.as_slice());
    let terminator = opts.terminator.as_deref().unwrap_or("GO");
    let batches = script::prepare(&sources, terminator, vars).map_err(Failure::usage)?;
    if batches.is_empty() {
        let _ = out.note("no SQL to run");
        return Ok(Stats::default());
    }
    if !settings.allow_write {
        for batch in &batches {
            if let Err(blocked) = guard::check(&batch.sql) {
                let location = location(&sources, batch.source, batch.line + blocked.line - 1);
                let _ = out.blocked(&location, &blocked.reason);
                return Err(Failure::new(Exit::Blocked, ""));
            }
        }
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
    runtime.block_on(run_cancellable(opts, settings, &sources, &batches, out))
}

fn load_sources(opts: &Opts) -> Result<Vec<Source>, Failure> {
    if opts.query.is_some() && !opts.inputs.is_empty() {
        return Err(Failure::usage("give SQL with -Q or with -i, not both"));
    }
    if let Some(query) = &opts.query {
        return Ok(vec![Source {
            name: "-Q".into(),
            text: query.clone(),
        }]);
    }
    let mut sources = Vec::new();
    for input in &opts.inputs {
        let (name, bytes) = if input == "-" {
            ("stdin".to_owned(), read_stdin()?)
        } else {
            let bytes =
                std::fs::read(input).map_err(|e| Failure::usage(format!("-i {input}: {e}")))?;
            (input.clone(), bytes)
        };
        let text = script::decode(bytes, &name).map_err(Failure::usage)?;
        sources.push(Source { name, text });
    }
    if sources.is_empty() {
        if io::stdin().is_terminal() {
            return Err(Failure::usage(
                "no SQL given: pass -Q \"...\", -i file.sql, or pipe SQL on stdin",
            ));
        }
        let text = script::decode(read_stdin()?, "stdin").map_err(Failure::usage)?;
        sources.push(Source {
            name: "stdin".into(),
            text,
        });
    }
    Ok(sources)
}

fn read_stdin() -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::usage(format!("reading stdin: {e}")))?;
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
    let connect_started = Instant::now();
    let mut session = Session::connect(&settings.connect, cancel)
        .await
        .map_err(db_failure)?;
    if opts.verbose || opts.show_connection {
        let profile = settings.profile.as_deref().unwrap_or("(none)");
        let line = format!(
            "profile {profile}; connected in {} ms; {}",
            connect_started.elapsed().as_millis(),
            session.describe()
        );
        if opts.show_connection {
            out.note(&line).map_err(output_failure)?;
        } else {
            eprintln!("sqlh: {line}");
        }
    }
    let read_only = !settings.allow_write;
    let mut setup = String::from(SESSION_SETUP);
    if read_only {
        // Layer 2 of the read-only guard: everything runs in a transaction that is rolled back.
        setup.push_str(" BEGIN TRANSACTION;");
    }
    session
        .exec_internal(&setup, timeout)
        .await
        .map_err(db_failure)?;
    if opts.check {
        session
            .exec_internal("SET NOEXEC ON;", timeout)
            .await
            .map_err(db_failure)?;
    }

    let mut stats = Stats::default();
    let total: usize = batches.iter().map(|b| b.repeat as usize).sum();
    let mut number = 0;
    'batches: for batch in batches {
        for run in 1..=batch.repeat {
            number += 1;
            if read_only && !session.in_transaction() {
                return Err(Failure::new(
                    Exit::SqlError,
                    "read-only guard: an error rolled back the wrapper transaction; stopping so nothing runs outside it",
                ));
            }
            out.begin_batch(&BatchInfo {
                number,
                total,
                source: &sources[batch.source].name,
                multi_source: sources.len() > 1,
                line: batch.line,
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
            "--check compiled the SQL without running it. Missing tables are only reported when \
             a query runs (SQL Server defers their name resolution)",
        )
        .map_err(output_failure)?;
        let _ = session.exec_internal("SET NOEXEC OFF;", timeout).await;
    }
    if read_only && session.in_transaction() {
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

fn output_failure(error: io::Error) -> Failure {
    Failure::new(Exit::Internal, format!("writing output failed: {error}"))
}
