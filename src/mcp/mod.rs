//! `sqlh mcp` (PLAN.md Phase 5.3; docs/mcp-mode.md): an MCP server on stdio, the reuse
//! engine's first front end (owner decision 14). It serves the profiles listed at its launch
//! (decision 18), with read-only tools (decision 16), each call stateless on a pooled,
//! reset connection (decisions 15, 17, 19).
//!
//! - **Threads:** stdin is read on a thread of its own, and stdout and stderr are written by
//!   theirs, so a client that stops reading can't hold up the runtime, its timers or its
//!   cancels (the lesson of R12). stdout carries only protocol messages.
//! - **Concurrency:** calls run concurrently on the current-thread runtime (`LocalSet`), each
//!   with its own cancel; `notifications/cancelled` cancels one, and nothing more is sent for
//!   it.
//! - **The end:** when stdin closes, stdout breaks or a console event arrives, the calls in
//!   flight are cancelled and given the engine's settle grace, the idle connections are
//!   closed, and the process exits 0.

mod protocol;
mod tools;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use protocol::{Era, Incoming, ProtocolError};
use tools::{Launch, Profile};

use crate::cli::{McpOpts, Opts};
use crate::db::Cancel;
use crate::engine::{self, Engine};
use crate::error::{Exit, Failure};
use crate::signals::Signals;
use crate::{app, config, console};

/// The longest message sqlh reads: 4 MB. A longer line is answered with an error and skipped.
const MAX_MESSAGE: usize = 4 << 20;

/// How many responses may wait for stdout before the server stops reading stdin.
const MAX_UNWRITTEN: usize = 64;

/// How often idle connections are checked against `--idle`.
const SWEEP_EVERY: Duration = Duration::from_secs(30);

/// How long calls in flight get when the server ends: to finish after stdin closes, and then,
/// once cancelled, to settle (on top of the engine's own settle grace inside each call).
const END_GRACE: Duration = Duration::from_secs(6);

/// How long the last responses get to reach stdout before the process exits.
const FLUSH_WAIT: Duration = Duration::from_secs(1);

/// What the stdin thread (and the stdout thread, when it breaks) tell the server.
enum Line {
    Text(Vec<u8>),
    TooLong,
    /// stdin closed, or failed.
    End,
    /// stdout failed: the client is gone.
    StdoutBroken,
}

/// Runs the server until its input ends. Launch errors go to stderr, exit 3.
pub(crate) fn run(opts: McpOpts) -> Exit {
    if console::stdin_is_console() {
        app::mcp_log(
            "sqlh mcp: usage error: sqlh mcp speaks MCP on stdin and stdout, so a harness \
             starts it (claude mcp add sqlh -- sqlh mcp --profile NAME); it doesn't read from \
             a console",
        );
        return Exit::Usage;
    }
    let (profiles, launch) = match resolve(&opts) {
        Ok(resolved) => resolved,
        Err(failure) => {
            app::mcp_log(&format!(
                "sqlh mcp: {}: {}",
                failure.exit.label(),
                failure.message
            ));
            return failure.exit;
        }
    };
    let pool = opts.pool.unwrap_or(engine::DEFAULT_POOL);
    let idle = match opts.idle_minutes {
        Some(0) => None,
        Some(minutes) => Some(Duration::from_secs(minutes.saturating_mul(60))),
        None => Some(engine::DEFAULT_IDLE),
    };
    let engine = Engine::new(profiles, pool, idle);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            app::mcp_log(&format!("sqlh mcp: cannot start the async runtime: {e}"));
            return Exit::Internal;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, serve(engine, launch, opts.verbose))
}

/// Each listed profile's settings, resolved as a run's would be with the launch flags, and
/// never allowing writes (decision 16); with the profiles' descriptions for the instructions.
/// Nothing connects here.
fn resolve(opts: &McpOpts) -> Result<(Vec<(String, config::Settings)>, Launch), Failure> {
    let config = config::load().map_err(Failure::usage)?;
    let mut profiles = Vec::new();
    let mut described = Vec::new();
    for name in &opts.profiles {
        let run_opts = Opts {
            profile: Some(name.clone()),
            max_rows: opts.max_rows,
            max_cell: opts.max_cell,
            max_output: opts.max_output,
            query_timeout: opts.query_timeout,
            ..Opts::default()
        };
        let mut settings = config::resolve(&run_opts)?;
        settings.allow_write = false;
        settings.writes_off = Some(crate::guard::WritesOff::Mcp);
        // The profile's name as the file has it (`--profile` matches it in any case).
        let profile = config.find(name);
        let name = profile.map_or_else(|| name.clone(), |p| p.name.clone());
        described.push(Profile {
            name: name.clone(),
            description: profile.and_then(|p| p.description.clone()),
        });
        profiles.push((name, settings));
    }
    let launch = Launch {
        profiles: described,
        rows_given: opts.max_rows.is_some(),
        cell_given: opts.max_cell.is_some(),
    };
    Ok((profiles, launch))
}

/// The server: what it shares with its calls.
struct Server {
    engine: Engine,
    launch: Launch,
    tools: Value,
    instructions: String,
    out: std_mpsc::Sender<String>,
    /// Lines handed to the stdout thread and not yet written: past `MAX_UNWRITTEN`, stdin
    /// waits, so a client that stops reading can't make the server's memory grow.
    unwritten: Arc<AtomicUsize>,
    log: Option<std_mpsc::SyncSender<String>>,
    /// The calls in flight, by request id (as JSON text), with the cancel that ends each.
    calls: RefCell<HashMap<String, Cancel>>,
    /// The legacy version `initialize` agreed, if a legacy client opened.
    agreed: RefCell<Option<&'static str>>,
}

impl Server {
    fn send(&self, line: String) {
        // A closed channel means the stdout thread ended: the client is gone, and the
        // server is ending already.
        self.unwritten.fetch_add(1, Ordering::Relaxed);
        if self.out.send(line).is_err() {
            self.unwritten.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn log(&self, line: String) {
        if let Some(log) = &self.log {
            // Dropped when stderr is behind: logging must never hold the server up.
            let _ = log.try_send(line);
        }
    }
}

async fn serve(engine: Engine, launch: Launch, verbose: bool) -> Exit {
    let (lines_tx, mut lines) = mpsc::channel::<Line>(64);
    let reader_tx = lines_tx.clone();
    if thread::Builder::new()
        .name("sqlh-mcp-stdin".into())
        .spawn(move || read_stdin(&reader_tx))
        .is_err()
    {
        app::mcp_log("sqlh mcp: cannot start the stdin reader");
        return Exit::Internal;
    }
    let (out, out_rx) = std_mpsc::channel::<String>();
    let unwritten = Arc::new(AtomicUsize::new(0));
    let written = Arc::clone(&unwritten);
    let writer = match thread::Builder::new()
        .name("sqlh-mcp-stdout".into())
        .spawn(move || write_stdout(&out_rx, &lines_tx, &written))
    {
        Ok(writer) => writer,
        Err(_) => {
            app::mcp_log("sqlh mcp: cannot start the stdout writer");
            return Exit::Internal;
        }
    };
    let log = verbose.then(start_log).flatten();
    let server = Rc::new(Server {
        tools: tools::definitions(&launch),
        instructions: tools::instructions(&launch),
        engine,
        launch,
        out,
        unwritten,
        log,
        calls: RefCell::new(HashMap::new()),
        agreed: RefCell::new(None),
    });
    let names: Vec<&str> = server.engine.profiles().collect();
    server.log(format!("sqlh mcp: serving {}", names.join(", ")));
    let mut signals = Signals::new().ok();
    let mut sweep = tokio::time::interval(SWEEP_EVERY);
    let running = Rc::new(std::cell::Cell::new(0usize));
    let ended_by = loop {
        let behind = server.unwritten.load(Ordering::Relaxed) >= MAX_UNWRITTEN;
        tokio::select! {
            line = lines.recv(), if !behind => match line {
                Some(Line::Text(bytes)) => dispatch(&server, &running, &bytes),
                Some(Line::TooLong) => server.send(protocol::error_line(
                    None,
                    &ProtocolError::new(
                        protocol::INVALID_REQUEST,
                        format!("Invalid request: a message is at most {MAX_MESSAGE} bytes"),
                    ),
                )),
                Some(Line::End) | None => break "stdin closed",
                Some(Line::StdoutBroken) => break "stdout closed",
            },
            // While stdout is behind, look again soon; stdin waits meanwhile.
            () = tokio::time::sleep(Duration::from_millis(10)), if behind => {}
            _ = sweep.tick() => server.engine.sweep(),
            signal = async {
                match signals.as_mut() {
                    Some(signals) => signals.recv().await,
                    None => std::future::pending().await,
                }
            } => break signal.name(),
        }
    };
    server.log(format!("sqlh mcp: ending ({ended_by})"));
    // At the end of the input, the calls in flight may still finish and answer (a client that
    // pipes its requests and closes stdin gets its results); a console event or a broken
    // stdout ends them at once.
    if ended_by == "stdin closed" {
        wait_for_calls(&running, END_GRACE).await;
    }
    // What is still running is cancelled; nothing more is sent for it.
    let cancels: Vec<Cancel> = server.calls.borrow_mut().drain().map(|(_, c)| c).collect();
    for cancel in cancels {
        cancel.cancel();
    }
    wait_for_calls(&running, END_GRACE).await;
    server.engine.close_all().await;
    // The writer ends once every sender is gone and the last lines are out.
    drop(server);
    let flushed_by = Instant::now() + FLUSH_WAIT;
    while !writer.is_finished() && Instant::now() < flushed_by {
        thread::sleep(Duration::from_millis(10));
    }
    Exit::Ok
}

/// Waits until no call is running, at most `limit`.
async fn wait_for_calls(running: &std::cell::Cell<usize>, limit: Duration) {
    let deadline = Instant::now() + limit;
    while running.get() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One message from the client.
fn dispatch(server: &Rc<Server>, running: &Rc<std::cell::Cell<usize>>, bytes: &[u8]) {
    let Ok(line) = std::str::from_utf8(bytes) else {
        server.send(protocol::error_line(
            None,
            &ProtocolError::new(protocol::PARSE_ERROR, "Parse error: not UTF-8"),
        ));
        return;
    };
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    match protocol::parse(line) {
        Err((id, error)) => server.send(protocol::error_line(id.as_ref(), &error)),
        Ok(Incoming::Response) => {}
        Ok(Incoming::Notification { method, params }) => notification(server, &method, &params),
        Ok(Incoming::Request { id, method, params }) => {
            request(server, running, id, &method, &params)
        }
    }
}

fn notification(server: &Server, method: &str, params: &Map<String, Value>) {
    if method == "notifications/cancelled"
        && let Some(id) = params.get("requestId")
    {
        let cancel = server.calls.borrow_mut().remove(&id.to_string());
        if let Some(cancel) = cancel {
            server.log(format!("sqlh mcp: request {id} cancelled"));
            cancel.cancel();
        }
    }
    // notifications/initialized, and anything else, need nothing.
}

fn request(
    server: &Rc<Server>,
    running: &Rc<std::cell::Cell<usize>>,
    id: Value,
    method: &str,
    params: &Map<String, Value>,
) {
    if method == "initialize" {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = protocol::negotiate(requested);
        *server.agreed.borrow_mut() = Some(version);
        let result = protocol::object(json!({
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": protocol::server_info(),
            "instructions": server.instructions,
        }));
        server.send(protocol::result_line(&id, Era::Legacy(version), result));
        return;
    }
    let agreed = *server.agreed.borrow();
    let era = match protocol::era_of(params, agreed) {
        Ok(era) => era,
        // A legacy client may ping before `initialize` (the only request it may send then).
        Err(error)
            if method == "ping"
                && error.code == protocol::INVALID_PARAMS
                && !params.contains_key("_meta") =>
        {
            server.send(protocol::result_line(
                &id,
                Era::Legacy(protocol::LEGACY[0]),
                Map::new(),
            ));
            return;
        }
        Err(error) => {
            server.send(protocol::error_line(Some(&id), &error));
            return;
        }
    };
    let cacheable = |mut result: Map<String, Value>| {
        if era == Era::Modern {
            result.insert("ttlMs".into(), json!(protocol::TTL_MS));
            result.insert("cacheScope".into(), json!("private"));
        }
        result
    };
    match method {
        "server/discover" => {
            let result = cacheable(protocol::object(json!({
                "supportedVersions": protocol::supported_versions(),
                "capabilities": {"tools": {}},
                "instructions": server.instructions,
            })));
            server.send(protocol::result_line(&id, era, result));
        }
        "ping" => server.send(protocol::result_line(&id, era, Map::new())),
        "tools/list" => {
            if params.contains_key("cursor") {
                server.send(protocol::error_line(
                    Some(&id),
                    &ProtocolError::invalid_params("Invalid params: no such cursor"),
                ));
                return;
            }
            let result = cacheable(protocol::object(json!({"tools": server.tools})));
            server.send(protocol::result_line(&id, era, result));
        }
        "tools/call" => call(server, running, id, era, params),
        _ => server.send(protocol::error_line(
            Some(&id),
            &ProtocolError::new(
                protocol::METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            ),
        )),
    }
}

/// Starts a tool call on the runtime; its response goes out when it ends, unless it was
/// cancelled.
fn call(
    server: &Rc<Server>,
    running: &Rc<std::cell::Cell<usize>>,
    id: Value,
    era: Era,
    params: &Map<String, Value>,
) {
    let planned = match tools::plan(params, &server.launch) {
        Ok(planned) => planned,
        Err(error) => {
            server.send(protocol::error_line(Some(&id), &error));
            return;
        }
    };
    let key = id.to_string();
    if server.calls.borrow().contains_key(&key) {
        server.send(protocol::error_line(
            Some(&id),
            &ProtocolError::new(
                protocol::INVALID_REQUEST,
                "Invalid request: a call with this id is still running",
            ),
        ));
        return;
    }
    let root = Cancel::new();
    let cancel = root.child();
    server.calls.borrow_mut().insert(key.clone(), root);
    running.set(running.get() + 1);
    let server = Rc::clone(server);
    let running = Rc::clone(running);
    tokio::task::spawn_local(async move {
        let tool = planned.tool;
        let profile = planned.profile;
        let started = Instant::now();
        let (text, exit) = tools::execute(&server.engine, &server.launch, planned, &cancel).await;
        let cancelled = cancel.is_cancelled();
        server.calls.borrow_mut().remove(&key);
        if !cancelled {
            let result = protocol::object(json!({
                "content": [{"type": "text", "text": text}],
                "isError": exit != Exit::Ok,
            }));
            server.send(protocol::result_line(&id, era, result));
        }
        let profile = server.engine.profiles().nth(profile).unwrap_or("?");
        server.log(format!(
            "sqlh mcp: {tool} on {profile}: exit {}{} in {} ms",
            exit as u8,
            if cancelled { " (cancelled)" } else { "" },
            started.elapsed().as_millis()
        ));
        running.set(running.get() - 1);
    });
}

/// The stdin thread: one message per line, at most `MAX_MESSAGE` bytes each.
fn read_stdin(lines: &mpsc::Sender<Line>) {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        let line = match read_line(&mut reader, MAX_MESSAGE) {
            Ok(Some(Some(line))) => Line::Text(line),
            Ok(Some(None)) => Line::TooLong,
            Ok(None) | Err(_) => {
                let _ = lines.blocking_send(Line::End);
                return;
            }
        };
        if lines.blocking_send(line).is_err() {
            return;
        }
    }
}

/// Reads up to the next newline: `None` at the end of input, `Some(None)` for a line longer
/// than `max` (read and dropped to its end), else the line without its line break.
fn read_line(reader: &mut impl BufRead, max: usize) -> io::Result<Option<Option<Vec<u8>>>> {
    let mut line = Vec::new();
    let mut too_long = false;
    let mut any = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(any.then_some((!too_long).then_some(line)));
        }
        any = true;
        let (taken, end) = match buffer.iter().position(|b| *b == b'\n') {
            Some(at) => (at, true),
            None => (buffer.len(), false),
        };
        if !too_long {
            if line.len() + taken > max {
                too_long = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(&buffer[..taken]);
            }
        }
        reader.consume(taken + usize::from(end));
        if end {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some((!too_long).then_some(line)));
        }
    }
}

/// The stdout thread: each message on its own line. When stdout fails, the client is gone,
/// and the server is told.
fn write_stdout(
    lines: &std_mpsc::Receiver<String>,
    server: &mpsc::Sender<Line>,
    unwritten: &AtomicUsize,
) {
    let stdout = io::stdout();
    for line in lines {
        let mut out = stdout.lock();
        let written = out
            .write_all(line.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
        unwritten.fetch_sub(1, Ordering::Relaxed);
        if written.is_err() {
            let _ = server.blocking_send(Line::StdoutBroken);
            return;
        }
    }
}

/// The stderr thread for `--verbose`: lines are dropped when stderr is behind.
fn start_log() -> Option<std_mpsc::SyncSender<String>> {
    let (log, lines) = std_mpsc::sync_channel::<String>(256);
    thread::Builder::new()
        .name("sqlh-mcp-stderr".into())
        .spawn(move || {
            let stderr = io::stderr();
            for line in lines {
                let mut err = stderr.lock();
                let _ = writeln!(err, "{line}").and_then(|()| err.flush());
            }
        })
        .ok()?;
    Some(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], max: usize) -> Vec<Option<Vec<u8>>> {
        let mut reader = io::BufReader::with_capacity(4, input);
        let mut out = Vec::new();
        while let Some(line) = read_line(&mut reader, max).unwrap() {
            out.push(line);
        }
        out
    }

    // One message per line, read whatever the buffer size: a CR before the LF is dropped, a
    // last line without a newline still counts, and a line over the limit is skipped whole.
    #[test]
    fn stdin_is_read_a_line_at_a_time_within_a_limit() {
        assert_eq!(
            lines(b"{\"a\":1}\r\n{\"b\":2}\nlast", 100),
            [
                Some(b"{\"a\":1}".to_vec()),
                Some(b"{\"b\":2}".to_vec()),
                Some(b"last".to_vec())
            ]
        );
        assert_eq!(lines(b"0123456789\nok\n", 5), [None, Some(b"ok".to_vec())]);
        assert_eq!(lines(b"", 5), Vec::<Option<Vec<u8>>>::new());
        assert_eq!(lines(b"\n", 5), [Some(Vec::new())]);
    }
}
