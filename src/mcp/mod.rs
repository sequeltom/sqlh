//! `sqlh mcp` (plan-history.md Phase 5.3; docs/mcp-mode.md): an MCP server on stdio, the reuse
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
//! - **Backpressure** (R35): stdin keeps being read while `MAX_UNWRITTEN` responses wait for
//!   stdout. A notification (a cancel) is acted on at once; a request is held back
//!   (`Deferred`) until stdout catches up, and only once those held back reach their bound does
//!   the server stop reading stdin. While it is behind, a stdout that takes no byte for the
//!   stall limit (the query timeout, as the CLI's delivery has it, or `STALL_WHEN_NO_TIMEOUT`
//!   for a query timeout of 0: never none) means a client that stopped reading: the server ends
//!   as if stdout had broken.
//! - **Subscriptions:** `subscriptions/listen` is acknowledged with none of what it asks
//!   for (the tool list never changes) and stays open until the client cancels it or the
//!   server ends, which answers it and then tears it down with `notifications/cancelled`.
//! - **The end** (`end`): when stdin closes, the requests held back run as stdout drains, for
//!   as long as it keeps taking bytes (the stall rule ends a client that stops), and then the
//!   calls in flight get `END_GRACE` to finish and answer; when stdout breaks or stalls, or a
//!   console event arrives, they get none. What is still running is cancelled and given time
//!   to settle, what is still held back is dropped (counted as never run), open subscriptions
//!   are answered, the idle connections are closed, the writer gets its flush wait, and the
//!   stderr thread up to `delivery::SIDE_WAIT` for the last `--verbose` lines (`flush_log`);
//!   then the process exits 0, without waiting for anything the runtime still runs. Each later
//!   wait is at most `END_GRACE`, and console events are taken throughout but in that last one
//!   (R36): one cancels what is left at once and makes the end due within its grace
//!   (`Signal::grace`), which a later one can only bring nearer, since Windows ends the process
//!   about 5 s after a close, logoff or shutdown. The closing writes still get up to
//!   `FLUSH_WAIT` past it, and the `--verbose` lines `SIDE_WAIT` after them.

mod protocol;
mod tools;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::{Notify, mpsc};

use protocol::{Era, Incoming, Kind, ProtocolError, member};
use tools::{Launch, Profile};

use crate::cli::{McpOpts, Opts};
use crate::db::Cancel;
use crate::engine::{self, Engine};
use crate::error::{Exit, Failure};
use crate::output::delivery::SIDE_WAIT;
use crate::signals::{Signal, Signals};
use crate::{app, config, console};

/// The longest message sqlh reads: 4 MB. A longer line is answered with an error and skipped.
const MAX_MESSAGE: usize = 4 << 20;

/// How many responses may wait for stdout before the server is behind: a request that comes
/// then is held back (`Deferred`) until stdout catches up, so a client that stops reading
/// can't make the server's memory grow.
const MAX_UNWRITTEN: usize = 64;

/// How many requests may be held back while stdout is behind, and how many bytes of them:
/// once either is reached the server stops reading stdin (one more message, up to
/// `MAX_MESSAGE`, can take the bytes past the bound).
const MAX_DEFERRED: usize = 64;
const MAX_DEFERRED_BYTES: usize = 16 << 20;

/// How many `subscriptions/listen` streams may be open at once. sqlh honors none of their
/// notifications, so a few are plenty; each holds its id until it ends.
const MAX_SUBSCRIPTIONS: usize = 8;

/// How many calls may run at once; one more is answered at once with a tool error, so stdin
/// keeps being read (and cancels keep arriving) whatever a client sends.
const MAX_IN_FLIGHT: usize = 64;

/// How often idle connections are checked against `--idle`.
const SWEEP_EVERY: Duration = Duration::from_secs(30);

/// The stall limit that stands for a query timeout of 0 (`-t 0`, or a profile's
/// `query_timeout = 0`), for the stall rule alone (`Stall`, `stall_limit`): the default query
/// timeout's 60 s. A run at `-t 0` may take as long as it takes, but the transport must end
/// under a finite bound whatever the query timeout (R35's acceptance): with stdout unread and
/// the requests held back at their bound, stdin isn't read, so a cancel or the end of the input
/// can reach the server only through this rule's end. A query's own timeout is unchanged.
/// Debug builds take `SQLH_DEBUG_MCP_STALL_MS` in its place, so a test needn't wait 60 s.
const STALL_WHEN_NO_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest each wait of the end may take, but the replay of the requests held back after
/// stdin closes, which goes on as long as stdout keeps taking bytes, with no limit of its own
/// (the stall rule ends it only for a client that stops reading): for the calls in flight to finish after stdin closes, once nothing is held back, for
/// what is then cancelled to settle (on top of the engine's own settle grace inside each
/// call), for the idle connections to close, and for the writer's flush. A console event makes
/// the whole end due sooner (`End`).
const END_GRACE: Duration = Duration::from_secs(6);

/// How long the end's flush wait goes on with no byte reaching stdout before it stops; the
/// whole wait is bounded by `END_GRACE`. After a console event it gets up to this long past
/// the end's deadline, for the closing writes (the answers and teardowns of open
/// subscriptions).
const FLUSH_WAIT: Duration = Duration::from_secs(1);

/// How often the serving loop looks at stdout again while it is behind or requests are held
/// back, and the end at what it waits for.
const POLL: Duration = Duration::from_millis(10);

/// How much of a line the stdout thread writes at a time, so that the flush wait sees a long
/// line the client is still reading make progress (`FLUSH_WAIT`). A write to a pipe returns
/// only once the reader has taken nearly all of it, so a reader is seen to make progress if it
/// takes this much within `FLUSH_WAIT`: 4 KiB a second, as the CLI's writer has it
/// (`output::delivery`'s `WRITE_UNIT`, R11). Above stdout's 1 KiB line buffer, so each piece
/// goes straight to the pipe.
const WRITE_CHUNK: usize = 4 * 1024;

/// What the stdin thread tells the server.
enum Line {
    Text(Vec<u8>),
    TooLong,
    /// stdin closed, or failed.
    End,
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
    let exit = local.block_on(&runtime, serve(engine, launch, opts.verbose));
    // `serve` has bounded the end; what the runtime still runs (a name lookup the driver left
    // on a blocking thread, a call still settling after a console event) is abandoned, not
    // waited for, as dropping the runtime would.
    drop(local);
    runtime.shutdown_background();
    exit
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
        for_read_tools(&mut settings);
        // The profile's name as the file has it (`--profile` matches it in any case).
        let profile = config.find(name);
        // `resolve` drops a profile's read_uncommitted when the profile allows writes (a write
        // must not rest on a dirty read); here nothing can write, so the profile's choice holds.
        settings.read_uncommitted = profile.and_then(|p| p.read_uncommitted).unwrap_or(false);
        let name = profile.map_or_else(|| name.clone(), |p| p.name.clone());
        described.push(Profile {
            name: name.clone(),
            description: profile.and_then(|p| p.description.clone()),
        });
        profiles.push((name, settings));
    }
    let launch = Launch {
        profiles: described,
        max_rows: opts.max_rows,
        max_cell: opts.max_cell,
    };
    Ok((profiles, launch))
}

/// What a profile's resolved settings become under `sqlh mcp`'s read tools, whatever its
/// `write_policy`. The login's name goes on every connection of the pool, reused or replaced
/// (decision 30(c)); a tool that could write would need connections of its own.
fn for_read_tools(settings: &mut config::Settings) {
    settings.allow_write = false;
    settings.writes_off = Some(crate::guard::WritesOff::Mcp);
    settings.limits.advice = crate::output::Advice::Mcp;
    settings.connect.hints = crate::db::Hints::Profile;
    settings.connect.application_name = crate::db::APP_MCP;
}

/// The server: what it shares with its calls.
struct Server {
    engine: Engine,
    launch: Launch,
    tools: Value,
    instructions: String,
    out: std_mpsc::Sender<String>,
    /// Lines handed to the stdout thread and not yet written: at `MAX_UNWRITTEN` the server is
    /// behind, and requests are held back.
    unwritten: Arc<AtomicUsize>,
    /// Bytes the stdout thread has handed to stdout: the measure of a client's reading, for
    /// the stall rule and the flush wait.
    progress: Arc<AtomicU64>,
    log: Option<Log>,
    /// The calls in flight, by their id's key (`protocol::key`), with the cancel that ends
    /// each: taken out when the call is cancelled, so the id stays taken until the call has
    /// settled.
    calls: RefCell<HashMap<String, Option<Cancel>>>,
    /// The open `subscriptions/listen` requests, by key and id: answered when the server ends,
    /// unless the client cancelled them first.
    subscriptions: RefCell<Vec<(String, Value)>>,
    /// The requests held back while stdout is behind.
    deferred: RefCell<Deferred>,
    /// stdout failed, or stalled: the client is gone, and nothing more is sent.
    broken: Arc<AtomicBool>,
    /// The legacy version `initialize` agreed, if a legacy client opened.
    agreed: RefCell<Option<&'static str>>,
}

impl Server {
    fn new(
        engine: Engine,
        launch: Launch,
        out: std_mpsc::Sender<String>,
        log: Option<Log>,
    ) -> Server {
        Server {
            tools: tools::definitions(&launch),
            instructions: tools::instructions(&launch),
            engine,
            launch,
            out,
            unwritten: Arc::default(),
            progress: Arc::default(),
            log,
            calls: RefCell::default(),
            subscriptions: RefCell::default(),
            deferred: RefCell::default(),
            broken: Arc::default(),
            agreed: RefCell::new(None),
        }
    }

    fn send(&self, line: String) {
        // Once stdout has failed, the client is gone and the server is ending: nothing is
        // queued for it.
        if self.broken.load(Ordering::Relaxed) {
            return;
        }
        self.unwritten.fetch_add(1, Ordering::Relaxed);
        if self.out.send(line).is_err() {
            self.unwritten.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Whether as many responses wait for stdout as may: requests are held back meanwhile.
    fn behind(&self) -> bool {
        self.unwritten.load(Ordering::Relaxed) >= MAX_UNWRITTEN
    }

    /// The launch's limits, for an answer given before a call has a profile (the
    /// `--max-output` is the launch's whatever the profile).
    fn limits(&self) -> crate::output::Limits {
        self.engine.settings(0).limits
    }

    fn log(&self, line: String) {
        // An empty line is the stderr thread's flush (`Log`), so none is logged.
        if let Some(log) = &self.log
            && !line.is_empty()
        {
            // Dropped when stderr is behind: logging must never hold the server up.
            let _ = log.lines.try_send(line);
        }
    }
}

/// What a request held back becomes when it is let through.
enum Held {
    /// A request, as it came: read again and dispatched then.
    Request(Vec<u8>),
    /// An answer settled when the line came (a malformed message, one too long, a request
    /// reusing an open id), sent as it is.
    Answer(String),
}

impl Held {
    fn len(&self) -> usize {
        match self {
            Held::Request(bytes) => bytes.len(),
            Held::Answer(line) => line.len(),
        }
    }
}

/// The requests held back while stdout is behind (R35), in arrival order, each request with
/// its id's key: a cancel for one drops it, and its id counts as open meanwhile. At most
/// `MAX_DEFERRED` of them, and `MAX_DEFERRED_BYTES`.
#[derive(Default)]
struct Deferred {
    /// Oldest first; short enough that taking from the front costs nothing to speak of.
    queue: Vec<(Option<String>, Held)>,
    bytes: usize,
}

impl Deferred {
    /// Whether it is full: the server then stops reading stdin.
    fn full(&self) -> bool {
        self.queue.len() >= MAX_DEFERRED || self.bytes >= MAX_DEFERRED_BYTES
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn holds(&self, key: &str) -> bool {
        self.queue
            .iter()
            .any(|(held, _)| held.as_deref() == Some(key))
    }

    fn push(&mut self, key: Option<String>, held: Held) {
        self.bytes += held.len();
        self.queue.push((key, held));
    }

    /// The oldest, taken out.
    fn pop(&mut self) -> Option<Held> {
        self.take(0)
    }

    fn take(&mut self, at: usize) -> Option<Held> {
        if at >= self.queue.len() {
            return None;
        }
        let (_, held) = self.queue.remove(at);
        self.bytes -= held.len();
        Some(held)
    }

    /// Drops the request held back under `key`, if one is (only one can be: a request reusing
    /// the id of one held back is refused, `in_use`): it never runs, and nothing is sent for
    /// it.
    fn cancel(&mut self, key: &str) {
        if let Some(at) = self
            .queue
            .iter()
            .position(|(held, _)| held.as_deref() == Some(key))
        {
            self.take(at);
        }
    }

    /// Drops them all: the server is ending. How many there were.
    fn clear(&mut self) -> usize {
        self.bytes = 0;
        let count = self.queue.len();
        self.queue.clear();
        count
    }
}

/// Where the console events come from (`Signals`): an end can be played with a test's own.
trait Events {
    /// The next event; pending for good when there are none to watch.
    fn recv(&mut self) -> Pin<Box<dyn Future<Output = Signal> + '_>>;
}

impl Events for Option<Signals> {
    fn recv(&mut self) -> Pin<Box<dyn Future<Output = Signal> + '_>> {
        Box::pin(async move {
            match self {
                Some(signals) => signals.recv().await,
                None => std::future::pending().await,
            }
        })
    }
}

/// Why the serving stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    StdinClosed,
    /// A write or flush to stdout failed: the client is gone.
    StdoutClosed,
    /// stdout took no byte for the stall limit while the server was behind (R35).
    StdoutStalled,
    Event(Signal),
}

impl Why {
    fn name(self) -> &'static str {
        match self {
            Why::StdinClosed => "stdin closed",
            Why::StdoutClosed => "stdout closed",
            Why::StdoutStalled => "stdout stalled",
            Why::Event(signal) => signal.name(),
        }
    }
}

/// R35's rule for a client that stops reading without closing stdout: while the server is
/// behind, a stdout that takes no byte for the limit (`stall_limit`, always finite) means the
/// client is gone.
struct Stall {
    limit: Duration,
    /// What stdout had taken when the server was last seen behind with no progress, and since
    /// when.
    seen: Option<(u64, Instant)>,
}

impl Stall {
    fn new(limit: Duration) -> Stall {
        Stall { limit, seen: None }
    }

    fn stalled(&mut self, server: &Server) -> bool {
        if !server.behind() {
            self.seen = None;
            return false;
        }
        let taken = server.progress.load(Ordering::Relaxed);
        match self.seen {
            Some((at, since)) if at == taken => since.elapsed() >= self.limit,
            _ => {
                self.seen = Some((taken, Instant::now()));
                false
            }
        }
    }
}

/// The stall limit, from the listed profiles' stall seconds (`Limits::stall_secs`, which
/// follows the query timeout, as the CLI's delivery has it: the launch's `-t` when it set one,
/// else each profile's own): the longest of them, a 0 (no query timeout) counting as
/// `no_timeout` (`STALL_WHEN_NO_TIMEOUT`), so the limit is never none (R35).
fn stall_limit(stall_secs: impl IntoIterator<Item = u32>, no_timeout: Duration) -> Duration {
    stall_secs
        .into_iter()
        .map(|secs| match secs {
            0 => no_timeout,
            secs => Duration::from_secs(secs.into()),
        })
        .max()
        .unwrap_or(no_timeout)
}

/// What stands for a query timeout of 0 in the stall limit: `STALL_WHEN_NO_TIMEOUT`, or in a
/// debug build `SQLH_DEBUG_MCP_STALL_MS` when it is set (a release build takes no variable).
fn stall_when_no_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("SQLH_DEBUG_MCP_STALL_MS")
        .ok()
        .and_then(|ms| ms.trim().parse().ok())
    {
        return Duration::from_millis(ms);
    }
    STALL_WHEN_NO_TIMEOUT
}

async fn serve(engine: Engine, launch: Launch, verbose: bool) -> Exit {
    let (lines_tx, mut lines) = mpsc::channel::<Line>(64);
    if thread::Builder::new()
        .name("sqlh-mcp-stdin".into())
        .spawn(move || read_stdin(&lines_tx))
        .is_err()
    {
        app::mcp_log("sqlh mcp: cannot start the stdin reader");
        return Exit::Internal;
    }
    let (out, out_rx) = std_mpsc::channel::<String>();
    let log = verbose.then(start_log).flatten();
    let server = Rc::new(Server::new(engine, launch, out, log));
    // stdout's failure reaches the server outside the stdin channel, which may be full and
    // unread while stdout is behind (the review's orphan-process finding).
    let gone = Arc::new(Notify::new());
    let (written, progress, broken, writer_gone) = (
        Arc::clone(&server.unwritten),
        Arc::clone(&server.progress),
        Arc::clone(&server.broken),
        Arc::clone(&gone),
    );
    if thread::Builder::new()
        .name("sqlh-mcp-stdout".into())
        .spawn(move || write_stdout(&out_rx, &written, &progress, &broken, &writer_gone))
        .is_err()
    {
        app::mcp_log("sqlh mcp: cannot start the stdout writer");
        return Exit::Internal;
    }
    let names: Vec<&str> = server.engine.profiles().collect();
    server.log(format!("sqlh mcp: serving {}", names.join(", ")));
    let mut signals = match Signals::new() {
        Ok(signals) => Some(signals),
        Err(e) => {
            // Windows' own handling stays: a console event ends the process at once.
            server.log(format!(
                "sqlh mcp: console events can't be watched ({e}); Ctrl+C ends the process at once"
            ));
            None
        }
    };
    let running = Rc::new(Cell::new(0usize));
    let stall_secs = (0..server.engine.profiles().count())
        .map(|profile| server.engine.settings(profile).limits.stall_secs);
    let mut stall = Stall::new(stall_limit(stall_secs, stall_when_no_timeout()));
    let why = serving(
        &server,
        &running,
        &mut lines,
        &gone,
        &mut signals,
        &mut stall,
    )
    .await;
    end(&server, &running, why, &mut signals, stall).await;
    Exit::Ok
}

/// The serving loop, until stdin closes, stdout breaks or stalls, or a console event comes.
async fn serving(
    server: &Rc<Server>,
    running: &Rc<Cell<usize>>,
    lines: &mut mpsc::Receiver<Line>,
    gone: &Notify,
    events: &mut dyn Events,
    stall: &mut Stall,
) -> Why {
    let mut sweep = tokio::time::interval(SWEEP_EVERY);
    loop {
        if server.broken.load(Ordering::Relaxed) {
            return Why::StdoutClosed;
        }
        replay(server, running);
        if stall.stalled(server) {
            return Why::StdoutStalled;
        }
        // While stdout is behind, or requests are held back, the loop looks again every POLL.
        // Read once: requests are held back only while stdout is behind when `replay` last
        // looked, but the stdout thread can catch up a moment later without waking the loop,
        // and polling on `behind` alone would then leave them held back until the next sweep
        // (up to SWEEP_EVERY).
        let poll = server.behind() || !server.deferred.borrow().is_empty();
        // Only a full queue of requests held back stops stdin: until then a cancel, or the
        // end of the input, is taken whatever stdout does.
        let full = server.deferred.borrow().full();
        tokio::select! {
            line = lines.recv(), if !full => match line {
                Some(Line::Text(bytes)) => arrive(server, running, read(&bytes), Some(bytes)),
                Some(Line::TooLong) => {
                    let error = ProtocolError::new(
                        protocol::INVALID_REQUEST,
                        format!("Invalid request: a message is at most {MAX_MESSAGE} bytes"),
                    );
                    arrive(server, running, Message::Refused(None, error), None);
                }
                Some(Line::End) | None => return Why::StdinClosed,
            },
            () = gone.notified() => return Why::StdoutClosed,
            // Look again soon: for stdout's progress, to let the requests held back through,
            // and for the stall rule.
            () = tokio::time::sleep(POLL), if poll => {}
            _ = sweep.tick() => server.engine.sweep(),
            signal = events.recv() => return Why::Event(signal),
        }
    }
}

/// The end: one deadline over all of its steps, which a console event sets and a later one
/// can only bring nearer (R36).
struct End<'e> {
    events: &'e mut dyn Events,
    deadline: Option<Instant>,
    /// Requests held back that the end dropped, never run.
    never_ran: usize,
}

impl End<'_> {
    /// A console event, at any step: what is left is cancelled at once, and the end is due
    /// within the event's grace, or sooner if an earlier event made it so.
    fn event(&mut self, server: &Server, signal: Signal) {
        self.cut(server);
        let due = Instant::now() + signal.grace();
        self.deadline = Some(self.deadline.map_or(due, |deadline| deadline.min(due)));
    }

    /// Cancels the calls still running (nothing more is sent for them) and drops the requests
    /// held back.
    fn cut(&mut self, server: &Server) {
        let cancels: Vec<Cancel> = server
            .calls
            .borrow_mut()
            .values_mut()
            .filter_map(Option::take)
            .collect();
        for cancel in cancels {
            cancel.cancel();
        }
        self.never_ran += server.deferred.borrow_mut().clear();
    }

    /// Waits until `done` (looked at every `POLL`), for at most `limit` (none: until `done`)
    /// and, once a console event has set one, the end's deadline, which can't end the wait
    /// within `floor` of its start (the closing writes' due); an event meanwhile is taken at
    /// once.
    async fn wait(
        &mut self,
        server: &Server,
        limit: Option<Duration>,
        floor: Duration,
        done: &mut dyn FnMut() -> bool,
    ) {
        let start = Instant::now();
        let (until, floor) = (limit.map(|limit| start + limit), start + floor);
        while !done() {
            let due = match (until, self.deadline) {
                (Some(until), Some(deadline)) => Some(until.min(deadline.max(floor))),
                (until, None) => until,
                (None, Some(deadline)) => Some(deadline.max(floor)),
            };
            let now = Instant::now();
            if due.is_some_and(|due| now >= due) {
                return;
            }
            let tick = due.map_or(now + POLL, |due| due.min(now + POLL)).into();
            if let Ok(signal) = tokio::time::timeout_at(tick, self.events.recv()).await {
                server.log(format!("sqlh mcp: {} during the end", signal.name()));
                self.event(server, signal);
            }
        }
    }
}

/// The end (docs/mcp-mode.md §2, The end), from where the serving stopped. Every step is
/// bounded by `END_GRACE`, but the replay of the requests held back after stdin closed, which
/// has no limit of its own while stdout takes bytes (the stall rule ends a client that stops
/// reading); the answers still queued then get the flush wait's `END_GRACE` at most, as
/// before, so a reader too slow to take them in that time loses the rest, which `--verbose`
/// counts. A console event at any of them cuts what is left and bounds the rest
/// by its grace, the closing writes getting up to `FLUSH_WAIT` past it, so after a close,
/// logoff or shutdown the whole end takes about 2 s at most, inside Windows' 5 s, and the last
/// `--verbose` lines up to `delivery::SIDE_WAIT` more (`flush_log`, which takes no events).
async fn end(
    server: &Rc<Server>,
    running: &Rc<Cell<usize>>,
    why: Why,
    events: &mut dyn Events,
    mut stall: Stall,
) {
    server.log(format!("sqlh mcp: ending ({})", why.name()));
    let mut end = End {
        events,
        deadline: None,
        never_ran: 0,
    };
    match why {
        Why::Event(signal) => end.event(server, signal),
        // The client is gone, or taken for gone: nothing more is queued for it.
        Why::StdoutClosed | Why::StdoutStalled => server.broken.store(true, Ordering::Relaxed),
        // At the end of the input, the requests held back run as stdout drains, and the calls
        // in flight may still finish and answer, so a client that pipes its requests, closes
        // stdin and reads slowly gets its results, the last of them within the flush wait's
        // END_GRACE; a stdout that breaks or stalls ends that.
        Why::StdinClosed => {
            let mut gone = || {
                if server.broken.load(Ordering::Relaxed) {
                    return true;
                }
                replay(server, running);
                if stall.stalled(server) {
                    server.log("sqlh mcp: stdout stalled during the end".into());
                    server.broken.store(true, Ordering::Relaxed);
                }
                server.broken.load(Ordering::Relaxed)
            };
            // First the requests held back, with no limit of its own: they run as fast as the
            // client reads, and the stall rule (always finite) ends a client that has stopped.
            // A console event drops them (`End::event`). END_GRACE would charge their
            // dispatch, not only their work, to the calls' grace: a steady slow reader lost
            // the answers of what was still held back when it ran out.
            end.wait(server, None, Duration::ZERO, &mut || {
                gone() || server.deferred.borrow().is_empty()
            })
            .await;
            // Then, nothing being held back, the calls in flight get END_GRACE to finish.
            end.wait(server, Some(END_GRACE), Duration::ZERO, &mut || {
                gone() || running.get() == 0
            })
            .await;
        }
    }
    // What is still running is cancelled, and nothing more is sent for it; what is still held
    // back never runs, and is counted so (`End::never_ran`), not as a client's cancel.
    end.cut(server);
    end.wait(server, Some(END_GRACE), Duration::ZERO, &mut || {
        running.get() == 0
    })
    .await;
    // An open subscription ends gracefully: its request is answered (the subscriptions page's
    // Graceful Closure), so the client can tell this end from a dropped transport.
    // The stream is then torn down with `notifications/cancelled`, which the cancellation page
    // requires of a server ending a subscription (and allows for nothing else); the answer
    // comes first, as the subscriptions page has it sent before the stream closes.
    let open: Vec<(String, Value)> = server.subscriptions.borrow_mut().drain(..).collect();
    for (_, id) in open {
        end_subscription(server, &id, "the server is ending");
    }
    // The idle connections are closed politely, while console events are taken as in every
    // other wait; past it they are dropped where they stand.
    let mut never = || false;
    tokio::select! {
        () = server.engine.close_all() => {}
        () = end.wait(server, Some(END_GRACE), Duration::ZERO, &mut never) => {}
    }
    // The last lines get as long as bytes keep reaching stdout (FLUSH_WAIT with none ends the
    // wait), up to END_GRACE: a long line the client is still reading counts as progress, a
    // client that has stopped reading doesn't. Done once every line queued is written.
    let mut taken = server.progress.load(Ordering::Relaxed);
    let mut stalled_at = Instant::now() + FLUSH_WAIT;
    end.wait(server, Some(END_GRACE), FLUSH_WAIT, &mut || {
        let now = server.progress.load(Ordering::Relaxed);
        if now > taken {
            taken = now;
            stalled_at = Instant::now() + FLUSH_WAIT;
        }
        server.broken.load(Ordering::Relaxed)
            || server.unwritten.load(Ordering::Relaxed) == 0
            || Instant::now() >= stalled_at
    })
    .await;
    let unwritten = server.unwritten.load(Ordering::Relaxed);
    if unwritten + end.never_ran > 0 && server.log.is_some() {
        let never_ran = match end.never_ran {
            0 => String::new(),
            n => format!(" ({n} for requests that never ran)"),
        };
        server.log(format!(
            "sqlh mcp: {} responses were never written{never_ran}",
            unwritten + end.never_ran
        ));
    }
    flush_log(server.log.as_ref()).await;
}

/// What a line from the client is, read once.
enum Message {
    /// Nothing to do: an empty line, or a response.
    Nothing,
    Notification(String, Map<String, Value>),
    Request(Value, String, Map<String, Value>),
    /// An error to answer with, with the request's id when it could be read.
    Refused(Option<Value>, ProtocolError),
}

fn read(bytes: &[u8]) -> Message {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return Message::Refused(
            None,
            ProtocolError::new(protocol::PARSE_ERROR, "Parse error: not UTF-8"),
        );
    };
    let line = line.trim();
    if line.is_empty() {
        return Message::Nothing;
    }
    match protocol::parse(line) {
        Err((id, error)) => Message::Refused(id, error),
        Ok(Incoming::Response) => Message::Nothing,
        Ok(Incoming::Notification { method, params }) => Message::Notification(method, params),
        Ok(Incoming::Request { id, method, params }) => Message::Request(id, method, params),
    }
}

/// One message from the client (`bytes`, the line it came in). A notification is acted on at
/// once, even while stdout is behind, so a cancel is never held up behind answers the client
/// hasn't read (R35). Anything to answer goes behind the requests held back already, and is
/// held back while stdout is behind, its id checked as it comes, as it would have been at
/// once.
fn arrive(
    server: &Rc<Server>,
    running: &Rc<Cell<usize>>,
    message: Message,
    bytes: Option<Vec<u8>>,
) {
    let hold = server.behind() || !server.deferred.borrow().is_empty();
    let (id, error) = match message {
        Message::Nothing => return,
        Message::Notification(method, params) => return notification(server, &method, &params),
        Message::Request(id, method, params) => match bytes {
            Some(bytes) if hold => {
                let key = protocol::key(&id);
                match in_use(server, &key) {
                    Some(error) => (Some(id), error),
                    None => {
                        return server
                            .deferred
                            .borrow_mut()
                            .push(Some(key), Held::Request(bytes));
                    }
                }
            }
            _ => return request(server, running, id, &method, &params),
        },
        // A malformed request reusing an open id gets the same refusal as a well-formed one.
        Message::Refused(id, error) => {
            let open = id
                .as_ref()
                .and_then(|id| in_use(server, &protocol::key(id)));
            (id, open.unwrap_or(error))
        }
    };
    let line = protocol::error_line(id.as_ref(), &error);
    if hold {
        server.deferred.borrow_mut().push(None, Held::Answer(line));
    } else {
        server.send(line);
    }
}

/// Lets the requests held back through, in the order they came, while stdout keeps up.
fn replay(server: &Rc<Server>, running: &Rc<Cell<usize>>) {
    while !server.behind() {
        let held = server.deferred.borrow_mut().pop();
        match held {
            None => return,
            Some(Held::Answer(line)) => server.send(line),
            // Read again as it was read when it came: a request.
            Some(Held::Request(bytes)) => {
                if let Message::Request(id, method, params) = read(&bytes) {
                    request(server, running, id, &method, &params);
                }
            }
        }
    }
}

fn notification(server: &Server, method: &str, params: &Map<String, Value>) {
    if method == "notifications/cancelled"
        && let Some(id) = params.get("requestId")
    {
        let key = protocol::key(id);
        // A subscription the client cancels just ends, and a request still held back never
        // runs: nothing more is sent for either.
        server
            .subscriptions
            .borrow_mut()
            .retain(|(open, _)| *open != key);
        server.deferred.borrow_mut().cancel(&key);
        // Taken out, not removed: the id stays taken until the call has settled, so a call
        // reusing it is refused rather than left uncancellable.
        let cancel = server
            .calls
            .borrow_mut()
            .get_mut(&key)
            .and_then(Option::take);
        if let Some(cancel) = cancel {
            server.log(format!("sqlh mcp: request {key} cancelled"));
            cancel.cancel();
        }
    }
    // notifications/initialized, and anything else, need nothing.
}

fn request(
    server: &Rc<Server>,
    running: &Rc<Cell<usize>>,
    id: Value,
    method: &str,
    params: &Map<String, Value>,
) {
    // An id still open (a call in flight or settling, a subscription, a request held back)
    // gets only this refusal, whatever the method, so no request is ever answered twice under
    // one id.
    if let Some(error) = in_use(server, &protocol::key(&id)) {
        server.send(protocol::error_line(Some(&id), &error));
        return;
    }
    if method == "initialize" {
        // The legacy `InitializeRequestParams`: all three members are required (R39).
        let checked = protocol::check_meta(params).and_then(|_| {
            member(params, "", "protocolVersion", true, Kind::String)?;
            member(params, "", "capabilities", true, Kind::Object)?;
            member(params, "", "clientInfo", true, Kind::Implementation)
        });
        if let Err(error) = checked {
            server.send(protocol::error_line(Some(&id), &error));
            return;
        }
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
                // A cursor is a string, and the one page has none to go on from.
                let error = member(params, "", "cursor", false, Kind::String)
                    .err()
                    .unwrap_or_else(|| {
                        ProtocolError::invalid_params("Invalid params: no such cursor")
                    });
                server.send(protocol::error_line(Some(&id), &error));
                return;
            }
            let result = cacheable(protocol::object(json!({"tools": server.tools})));
            server.send(protocol::result_line(&id, era, result));
        }
        "tools/call" => call(server, running, id, era, params),
        "subscriptions/listen" if era == Era::Modern => listen(server, id, params),
        _ => server.send(protocol::error_line(
            Some(&id),
            &ProtocolError::new(
                protocol::METHOD_NOT_FOUND,
                format!("Method not found: {}", tools::echo(method)),
            ),
        )),
    }
}

/// Whether a request whose id has `key` is still open: a call in flight (until it settles), a
/// subscription, or a request held back. A request reusing it is refused, with the error this
/// returns.
fn in_use(server: &Server, key: &str) -> Option<ProtocolError> {
    let why = if server.calls.borrow().contains_key(key) {
        "Invalid request: a call with this id is still running"
    } else if server
        .subscriptions
        .borrow()
        .iter()
        .any(|(open, _)| open == key)
    {
        "Invalid request: a subscription with this id is still open"
    } else if server.deferred.borrow().holds(key) {
        "Invalid request: a request with this id is still waiting to run"
    } else {
        return None;
    };
    Some(ProtocolError::new(protocol::INVALID_REQUEST, why))
}

/// `subscriptions/listen` (2026-07-28, the subscriptions page): acknowledged at once with the
/// subset the server honors, which is none (the tool list never changes, and sqlh has no
/// prompts or resources), then held open until the client cancels it or the server ends.
fn listen(server: &Server, id: Value, params: &Map<String, Value>) {
    // The `SubscriptionFilter` is required, and its members are typed; an extension's members
    // pass (R39). A malformed one takes no place.
    let checked = match params.get("notifications") {
        Some(Value::Object(filter)) => [
            "toolsListChanged",
            "promptsListChanged",
            "resourcesListChanged",
        ]
        .into_iter()
        .try_for_each(|name| member(filter, "notifications.", name, false, Kind::Bool))
        .and_then(|()| {
            member(
                filter,
                "notifications.",
                "resourceSubscriptions",
                false,
                Kind::Strings,
            )
        }),
        _ => member(params, "", "notifications", true, Kind::Object),
    };
    if let Err(error) = checked {
        server.send(protocol::error_line(Some(&id), &error));
        return;
    }
    if server.subscriptions.borrow().len() >= MAX_SUBSCRIPTIONS {
        server.send(protocol::error_line(
            Some(&id),
            &ProtocolError::new(
                protocol::INVALID_REQUEST,
                format!(
                    "Invalid request: at most {MAX_SUBSCRIPTIONS} subscriptions may be open; \
                     cancel one first"
                ),
            ),
        ));
        return;
    }
    let ack = json!({"_meta": {protocol::META_SUBSCRIPTION: id}, "notifications": {}});
    server.send(protocol::notification_line(
        "notifications/subscriptions/acknowledged",
        protocol::object(ack),
    ));
    server
        .subscriptions
        .borrow_mut()
        .push((protocol::key(&id), id));
}

/// Ends an open subscription from the server's side: its answer, then the cancel that tears
/// the stream down.
fn end_subscription(server: &Server, id: &Value, reason: &str) {
    let meta = json!({"_meta": {protocol::META_SUBSCRIPTION: id}});
    server.send(protocol::result_line(
        id,
        Era::Modern,
        protocol::object(meta),
    ));
    let cancelled = json!({"requestId": id, "reason": reason});
    server.send(protocol::notification_line(
        "notifications/cancelled",
        protocol::object(cancelled),
    ));
}

/// Starts a tool call on the runtime; its response goes out when it ends, unless it was
/// cancelled.
fn call(
    server: &Rc<Server>,
    running: &Rc<Cell<usize>>,
    id: Value,
    era: Era,
    params: &Map<String, Value>,
) {
    let planned = match tools::plan(params, &server.launch) {
        Ok(planned) => planned,
        Err(tools::Refused::Protocol(error)) => {
            server.send(protocol::error_line(Some(&id), &error));
            return;
        }
        Err(tools::Refused::Arguments { message, format }) => {
            let result = protocol::object(json!({
                "content": [{"type": "text", "text": tools::refusal(message, format, server.limits())}],
                "isError": true,
            }));
            server.send(protocol::result_line(&id, era, result));
            return;
        }
    };
    if running.get() >= MAX_IN_FLIGHT {
        let text = tools::refusal(
            format!("{MAX_IN_FLIGHT} calls are already running; wait for one to finish"),
            planned.format,
            server.limits(),
        );
        let result = protocol::object(json!({
            "content": [{"type": "text", "text": text}],
            "isError": true,
        }));
        server.send(protocol::result_line(&id, era, result));
        return;
    }
    let key = protocol::key(&id);
    let root = Cancel::new();
    let cancel = root.child();
    server.calls.borrow_mut().insert(key.clone(), Some(root));
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
    unwritten: &AtomicUsize,
    progress: &AtomicU64,
    broken: &AtomicBool,
    gone: &Notify,
) {
    let stdout = io::stdout();
    for line in lines {
        let mut out = stdout.lock();
        let written = line
            .as_bytes()
            .chunks(WRITE_CHUNK)
            .try_for_each(|chunk| {
                out.write_all(chunk)?;
                progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                Ok(())
            })
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
        unwritten.fetch_sub(1, Ordering::Relaxed);
        if written.is_err() {
            broken.store(true, Ordering::Relaxed);
            // A permit is kept if the server isn't waiting yet.
            gone.notify_one();
            return;
        }
    }
}

/// The `--verbose` log: the stderr thread's lines, and the flush it answers.
struct Log {
    /// Lines for stderr; an empty one, which no log line is (`Server::log`), asks for a flush.
    /// They are `String`s as stdout's are, so the two channels share one copy of the channel
    /// code: a type of its own made the exe 8 KB larger.
    lines: std_mpsc::SyncSender<String>,
    /// Notified when the thread takes a flush, by which time every line handed over before it
    /// is written.
    flushed: Arc<Notify>,
}

/// The stderr thread for `--verbose`: lines are dropped when stderr is behind.
fn start_log() -> Option<Log> {
    start_log_to(io::stderr())
}

/// The stderr thread, writing to `sink`: one write and flush a line (for stderr, under one
/// lock, so no other diagnostic splits it).
fn start_log_to(mut sink: impl Write + Send + 'static) -> Option<Log> {
    let (lines, taken) = std_mpsc::sync_channel::<String>(256);
    let flushed = Arc::new(Notify::new());
    let notify = Arc::clone(&flushed);
    thread::Builder::new()
        .name("sqlh-mcp-stderr".into())
        .spawn(move || {
            for line in taken {
                if line.is_empty() {
                    notify.notify_one();
                } else {
                    let _ = writeln!(sink, "{line}").and_then(|()| sink.flush());
                }
            }
        })
        .ok()?;
    Some(Log { lines, flushed })
}

/// Waits for the lines logged so far to reach stderr, for `delivery::SIDE_WAIT` at most, the
/// CLI's bound for its own `--verbose` lines: the process exits as soon as `serve` returns,
/// which would cut the stderr thread's last writes (on CI, a client gone at once ended the
/// server before `ending (stdout closed)` was written). A stderr behind by more than the
/// channel holds has its lines dropped, as `Server::log` has it, and isn't waited for. No
/// console event is taken meanwhile: nothing is left to cut, and every grace is longer.
async fn flush_log(log: Option<&Log>) {
    let Some(log) = log else { return };
    // A permit `notify_one` stores before the wait begins is kept, so the answer can't be
    // missed. This is the one flush, so no earlier permit is left to be taken for it.
    if log.lines.try_send(String::new()).is_ok() {
        let _ = tokio::time::timeout(SIDE_WAIT, log.flushed.notified()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    // Decisions 16 and 30(c): a profile that allows writes still reads here, under this name.
    #[test]
    fn the_read_tools_log_in_as_sqlh_mcp_whatever_the_profile_allows() {
        let mut settings = crate::run::unconnected_settings(true);
        for_read_tools(&mut settings);
        assert_eq!(settings.connect.application_name, "sqlh mcp");
        assert!(!settings.allow_write);
    }

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

    /// A stderr for the log thread: each write tells `entered`, then waits for `pace` (a slow
    /// reader) or, with none, until the test lets it go (a stalled one), and keeps its bytes.
    struct Sink {
        written: Arc<std::sync::Mutex<Vec<u8>>>,
        pace: Option<Duration>,
        stalled: std_mpsc::Receiver<()>,
        entered: std_mpsc::Sender<()>,
    }

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.entered.send(());
            match self.pace {
                Some(pace) => thread::sleep(pace),
                None => {
                    let _ = self.stalled.recv();
                }
            }
            self.written.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A log thread writing to a `Sink`: what it has written, the end that lets a stalled sink
    /// go when it is dropped, and the sink's word that a write began.
    struct Sunk {
        log: Log,
        written: Arc<std::sync::Mutex<Vec<u8>>>,
        _release: std_mpsc::Sender<()>,
        entered: std_mpsc::Receiver<()>,
    }

    impl Sunk {
        fn new(pace: Option<Duration>) -> Sunk {
            let written = Arc::default();
            let (release, stalled) = std_mpsc::channel();
            let (tell, entered) = std_mpsc::channel();
            let log = start_log_to(Sink {
                written: Arc::clone(&written),
                pace,
                stalled,
                entered: tell,
            })
            .unwrap();
            Sunk {
                log,
                written,
                _release: release,
                entered,
            }
        }

        fn text(written: &std::sync::Mutex<Vec<u8>>) -> String {
            String::from_utf8(written.lock().unwrap().clone()).unwrap()
        }
    }

    // The last `--verbose` lines reach stderr before the server ends (`flush_log`), a slow one
    // included, and the thread's answer ends the wait, not its bound; a stalled stderr holds it
    // for `SIDE_WAIT`, and one behind by a full channel isn't waited for. Before, the process
    // could exit with them still queued: on CI a client gone at once ended the server before
    // `ending (stdout closed)` was written.
    #[test]
    fn the_last_log_lines_reach_stderr_within_a_bound() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let flush = |log: Option<&Log>| {
            let started = Instant::now();
            runtime.block_on(flush_log(log));
            started.elapsed()
        };

        // 5 ms a write, two writes a line.
        let slow = Sunk::new(Some(Duration::from_millis(5)));
        for n in 0..5 {
            slow.log.lines.try_send(format!("line {n}")).unwrap();
        }
        let took = flush(Some(&slow.log));
        assert_eq!(
            Sunk::text(&slow.written),
            "line 0\nline 1\nline 2\nline 3\nline 4\n"
        );
        assert!(took < SIDE_WAIT / 2, "{took:?}");

        let stalled = Sunk::new(None);
        stalled.log.lines.try_send("stuck".into()).unwrap();
        let took = flush(Some(&stalled.log));
        assert!(took >= SIDE_WAIT && took < SIDE_WAIT * 3, "{took:?}");
        assert_eq!(Sunk::text(&stalled.written), "");

        // The thread holds a line, stuck in its write, and the channel is full behind it: the
        // flush can't be queued, so it isn't waited for.
        let full = Sunk::new(None);
        full.log.lines.try_send("stuck".into()).unwrap();
        full.entered.recv().unwrap();
        let mut queued = 0;
        while full.log.lines.try_send(format!("line {queued}")).is_ok() {
            queued += 1;
        }
        assert_eq!(queued, 256);
        let took = flush(Some(&full.log));
        assert!(took < SIDE_WAIT / 5, "{took:?}");

        // Without --verbose there is nothing to wait for.
        let took = flush(None);
        assert!(took < SIDE_WAIT / 5, "{took:?}");
    }

    // R35: what is held back is counted in requests and bytes, kept in order, and a cancel
    // drops the request it names (its key), and nothing else.
    #[test]
    fn requests_held_back_are_bounded_in_order_and_cancelled_by_key() {
        let mut deferred = Deferred::default();
        deferred.push(Some("1".into()), Held::Request(b"one".to_vec()));
        deferred.push(None, Held::Answer("answer".into()));
        deferred.push(Some("\"1\"".into()), Held::Request(b"quoted".to_vec()));
        assert_eq!(deferred.bytes, 3 + 6 + 6);
        assert!(deferred.holds("1") && deferred.holds("\"1\"") && !deferred.holds("2"));
        deferred.cancel("1");
        assert_eq!((deferred.queue.len(), deferred.bytes), (2, 12));
        assert!(matches!(deferred.pop(), Some(Held::Answer(line)) if line == "answer"));
        assert!(matches!(deferred.pop(), Some(Held::Request(bytes)) if bytes == b"quoted"));
        assert_eq!((deferred.pop().is_none(), deferred.bytes), (true, 0));
        for _ in 0..MAX_DEFERRED - 1 {
            deferred.push(None, Held::Answer(String::new()));
        }
        assert!(!deferred.full());
        deferred.push(None, Held::Answer(String::new()));
        assert!(deferred.full(), "by count");
        assert_eq!(deferred.clear(), MAX_DEFERRED);
        deferred.push(None, Held::Request(vec![0; MAX_DEFERRED_BYTES]));
        assert!(deferred.full(), "by bytes");
    }

    // R35: the stall limit is never none. It is the longest of the profiles' stall seconds,
    // which follow the query timeout, and a 0 among them (no query timeout) counts as
    // STALL_WHEN_NO_TIMEOUT, 60 s. Before the fix a 0 anywhere meant no limit, so a client that
    // stopped reading at -t 0 held the server, its cancels and its end of input, for good.
    #[test]
    fn the_stall_limit_is_always_finite() {
        let secs = Duration::from_secs;
        let none = STALL_WHEN_NO_TIMEOUT;
        assert_eq!(none, secs(60));
        assert_eq!(stall_limit([2], none), secs(2));
        assert_eq!(stall_limit([2, 30], none), secs(30));
        assert_eq!(stall_limit([0], none), none);
        assert_eq!(stall_limit([0, 2], none), none);
        assert_eq!(stall_limit([0, 120], none), secs(120));
        let short = Duration::from_millis(1500);
        assert_eq!(stall_limit([0, 0], short), short);
        assert_eq!(stall_limit([], none), none);
    }

    /// A test's console events: each at its time, once.
    struct Script(VecDeque<(Instant, Signal)>);

    impl Events for Script {
        fn recv(&mut self) -> Pin<Box<dyn Future<Output = Signal> + '_>> {
            Box::pin(async move {
                match self.0.front().copied() {
                    Some((at, signal)) => {
                        tokio::time::sleep_until(at.into()).await;
                        self.0.pop_front();
                        signal
                    }
                    None => std::future::pending().await,
                }
            })
        }
    }

    /// Plays an end after stdin closed, with a call still running whose cancel can't settle
    /// (the fake server never acknowledges its attention, and the driver waits 5 s for it):
    /// `first` comes 300 ms into the end and Ctrl+Break 300 ms after it. What the end took,
    /// whether the call was still running when it returned, whether its connection was closed
    /// once the runtime let it go, and what was written.
    fn end_with(first: Signal) -> (Duration, bool, bool, Vec<String>) {
        use crate::db::fake_server::{Conn, End as Ended, Turn, serve_pool};
        let fake = serve_pool(vec![Conn::new().at(2, Turn::Hang { ack: false })]);
        let (server, writer) = test_server(fake.server.clone(), None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        let running = Rc::new(Cell::new(0));
        let (took, still_running) = local.block_on(&runtime, async {
            let params =
                protocol::object(json!({"name": "query", "arguments": {"sql": "SELECT 1"}}));
            call(&server, &running, json!(1), Era::Modern, &params);
            let hung = fake
                .wait_for(Duration::from_secs(10), |f| f.requests(0).len() >= 2)
                .await;
            assert!(hung, "the call's batch reached the server");
            let started = Instant::now();
            let mut events = Script(VecDeque::from([
                (started + Duration::from_millis(300), first),
                (started + Duration::from_millis(600), Signal::CtrlBreak),
            ]));
            end(
                &server,
                &running,
                Why::StdinClosed,
                &mut events,
                Stall::new(STALL_WHEN_NO_TIMEOUT),
            )
            .await;
            (started.elapsed(), running.get() == 1)
        });
        // As `run` does: the call still settling is dropped where it stands, and with it its
        // connection.
        drop(local);
        drop(runtime);
        let mut closed = false;
        for _ in 0..200 {
            if fake.ended(0) == Some(Ended::Client) {
                closed = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        drop(server);
        (took, still_running, closed, writer.join().unwrap())
    }

    /// A server with one profile, `fake`, connecting to `server`, and `log` for `--verbose`;
    /// and the stdout thread's part: every line written at once, all of them returned once the
    /// server is dropped.
    fn test_server(
        server: String,
        log: Option<Log>,
    ) -> (Rc<Server>, thread::JoinHandle<Vec<String>>) {
        let settings = config::Settings {
            connect: crate::db::fake_server::options(server),
            limits: crate::output::Limits {
                advice: crate::output::Advice::Mcp,
                ..crate::output::Limits::default()
            },
            query_timeout: 30,
            allow_write: false,
            writes_off: Some(crate::guard::WritesOff::Mcp),
            lock_timeout: 10,
            read_uncommitted: false,
            exact_count: false,
            profile: Some("fake".into()),
        };
        let engine = Engine::new(vec![("fake".into(), settings)], 2, None);
        let launch = Launch {
            profiles: vec![Profile {
                name: "fake".into(),
                description: None,
            }],
            max_rows: None,
            max_cell: None,
        };
        let (out, written) = std_mpsc::channel::<String>();
        let server = Rc::new(Server::new(engine, launch, out, log));
        let unwritten = Arc::clone(&server.unwritten);
        let writer = thread::spawn(move || {
            let mut lines = Vec::new();
            for line in written {
                lines.push(line);
                unwritten.fetch_sub(1, Ordering::Relaxed);
            }
            lines
        });
        (server, writer)
    }

    // The end itself waits for its last `--verbose` lines (`flush_log`), however the serving
    // stopped. With stdout closed at once (the CI failure after `ef321ab`), or stdin closed,
    // and nothing in flight, nothing else holds the end, so without the flush it returns before
    // a slow stderr has `ending (...)`.
    #[test]
    fn the_end_waits_for_its_last_log_lines() {
        for (why, reason) in [
            (Why::StdoutClosed, "stdout closed"),
            (Why::StdinClosed, "stdin closed"),
        ] {
            // 100 ms a write, two writes a line: 200 ms for the one line, well inside SIDE_WAIT.
            let stderr = Sunk::new(Some(Duration::from_millis(100)));
            let written = Arc::clone(&stderr.written);
            // No call connects: the profile's port is never tried.
            let (server, writer) = test_server("127.0.0.1,1".into(), Some(stderr.log));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            let running = Rc::new(Cell::new(0));
            local.block_on(
                &runtime,
                end(
                    &server,
                    &running,
                    why,
                    &mut Script(VecDeque::new()),
                    Stall::new(STALL_WHEN_NO_TIMEOUT),
                ),
            );
            assert_eq!(
                Sunk::text(&written),
                format!("sqlh mcp: ending ({reason})\n"),
                "{reason}"
            );
            drop(local);
            drop(server);
            assert!(
                writer.join().unwrap().is_empty(),
                "{reason}: nothing on stdout"
            );
        }
    }

    // R36: a close, logoff or shutdown during the end, here the wait after stdin closed,
    // cancels what is left at once and makes the whole end due within its grace (1 s), however
    // long the waits would otherwise be (END_GRACE, 6 s, for the call to finish); a later
    // Ctrl+Break (2 s) doesn't put it off. A call that can't settle in time is left to the
    // runtime's end, which closes its connection; it gets no answer. With the events from a
    // test's script in place of the console's.
    #[test]
    fn a_close_logoff_or_shutdown_during_the_end_bounds_it_by_its_grace() {
        let ends: Vec<_> = [Signal::Close, Signal::Logoff, Signal::Shutdown]
            .into_iter()
            .map(|signal| thread::spawn(move || (signal, end_with(signal))))
            .collect();
        for end in ends {
            let (signal, (took, still_running, closed, written)) = end.join().unwrap();
            let due = Duration::from_millis(300) + signal.grace();
            assert!(
                took >= due - Duration::from_millis(50) && took < due + Duration::from_millis(500),
                "{signal:?}: {took:?}"
            );
            assert!(
                still_running,
                "{signal:?}: the call couldn't settle in time"
            );
            assert!(closed, "{signal:?}: its connection was closed");
            assert!(written.is_empty(), "{signal:?}: {written:?}");
        }
    }
}
