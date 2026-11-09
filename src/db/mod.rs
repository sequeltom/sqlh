//! The only module that touches mssql-tds. Everything it exposes is our own type, so the
//! driver can be upgraded or swapped here without touching the rest of the tool.

pub mod phases;
mod spn;
mod stream;
mod types;
mod value;

use std::borrow::Cow;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::secret::Secret;

use mssql_tds::connection::client_context::{
    ClientContext, TdsAuthenticationMethod, VectorVersion,
};
use mssql_tds::connection::tds_client::{
    BatchErrorMode, CursorColumn, CursorPoll, ExecuteOptions, ResultSet, StatementResult, TdsClient,
};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{CancelHandle, EncryptionOptions, EncryptionSetting, TdsResult};
use mssql_tds::datatypes::row_writer::DiscardRowWriter;
use mssql_tds::error::{Error as TdsError, SqlErrorInfo, SqlInfoMessage};
use mssql_tds::message::login_options::ApplicationIntent;
use mssql_tds::security::SecurityContextProvider;

pub use value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encrypt {
    Optional,
    Mandatory,
    Strict,
}

pub enum Auth {
    Integrated,
    Sql { user: String, password: Secret },
}

impl Auth {
    /// The method's name, as the output states it: `windows` or `sql`. Never the login.
    pub fn method(&self) -> &'static str {
        match self {
            Auth::Integrated => "windows",
            Auth::Sql { .. } => "sql",
        }
    }
}

/// The login's application names (`APP_NAME()`, `program_name`): what a DBA's `sp_who2` or
/// `sp_WhoIsActive` shows of what a run may do (decision 30(c)). Client-asserted, so a courtesy
/// label and not evidence.
pub const APP_READ_ONLY: &str = "sqlh";
pub const APP_MCP: &str = "sqlh mcp";
pub const APP_WRITES: &str = "sqlh (writes)";

pub struct ConnectOptions {
    pub server: String,
    pub database: Option<String>,
    pub auth: Auth,
    pub encrypt: Encrypt,
    pub trust_server_certificate: bool,
    pub login_timeout: u32,
    pub read_only_intent: bool,
    pub multi_subnet_failover: bool,
    /// What a failed connect's hint names: the command line's flags, or the profile's keys.
    pub hints: Hints,
    /// Fixed at login, so a connection keeps it for life: one of the `APP_` names.
    pub application_name: &'static str,
}

/// What a failed connect's hint tells its reader to change (`connect_error`): the command
/// line's flags (`-C`, `-l`), or, in `sqlh mcp`, which connects only through profiles, the
/// profile's keys (`trust_server_certificate`, `login_timeout`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hints {
    #[default]
    Flags,
    Profile,
}

impl Hints {
    fn pick(self, flags: &'static str, profile: &'static str) -> &'static str {
        match self {
            Hints::Flags => flags,
            Hints::Profile => profile,
        }
    }
}

pub struct Column {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
}

/// An INFO or ERROR token from the server.
pub struct ServerMessage {
    pub number: u32,
    pub severity: i32,
    pub state: u8,
    /// Line within the batch, or within `procedure` when that is set.
    pub line: Option<i32>,
    pub procedure: Option<String>,
    pub text: String,
}

/// Receives a batch's results, messages and errors in the order `Session::run` delivers them
/// (DESIGN.md §5.1 and §5.2; the sinks refuse a call out of this order, plan-history.md 2.12):
///
/// - **A result set** is `columns`, then its rows (`row`, or `skipped_row` while
///   `wants_values` is false), then `end_rows`. Between `columns` and `end_rows` the sink
///   gets nothing else: no `message`, no `error`, no `rows_affected`. A sink answers such a
///   call with an `OutOfOrder` error and changes nothing (`Tally::outside_rows`), and the run
///   ends as an internal error (exit 10), so that a driver upgrade or a new caller can't put a
///   message into the middle of a result and break the output without anyone noticing. When a
///   run fails mid-result `end_rows` never comes; `Output::finish` closes what is open.
/// - **A message** (an INFO token: `PRINT`, `RAISERROR` at severity 10 or below, a warning)
///   lands at a statement boundary, not where the server sent it. The driver keeps the
///   messages it reads while it moves through the response, and `Session::run` hands them
///   over in the order they arrived, at each boundary before the statement's own outcome
///   (its result set, its row count, its errors), and once more after the last statement.
///   So a message the server sends while a result set's rows are streaming comes after that
///   result set's `end_rows`, never between its rows. When the rows end in an error, the
///   order is `end_rows_incomplete`, the messages held, then the errors.
/// - **A row count** (`rows_affected`) and **an error** are outside any result set too.
///
/// Messages keep their order among themselves. Their order against results, row counts and
/// errors is the one above, which is not always the wire order. A statement's errors are
/// reported once the driver has read the statement to its end, so a message the server sent
/// after an error, but before that end, comes before the error.
pub trait BatchSink {
    fn columns(&mut self, columns: &[Column]) -> io::Result<()>;
    /// False once the row limit is reached; remaining rows are then only counted.
    fn wants_values(&self) -> bool;
    /// The most characters of a value the sink can show (for binary, hex digits: two for each
    /// byte); 0 for all of them. The read cuts a large value (`varchar(max)` and the other types
    /// the driver streams) to this size as it arrives and hands it over as `Value::TextCut` or
    /// `Value::BinCut`, so it never holds more than what is shown (DESIGN.md §11). With no
    /// `--max-cell`, the output budget is the limit: a value with more characters than the
    /// response has bytes can't be written whole.
    fn cell_limit(&self) -> usize;
    fn row(&mut self, values: &[Value]) -> io::Result<()>;
    fn skipped_row(&mut self);
    /// The count of hidden rows was stopped (`Counting`), by `by`: the rows counted so far are
    /// a lower bound, and the rest of the batch was cancelled. Called once, in place of the rows
    /// that would have followed: just before `end_rows`, or, when the budget ran out as the
    /// result's last row arrived and the cancel met the next step instead of a read, after
    /// `end_rows`, with no result open (the count was complete then, but the batch's later
    /// statements are gone all the same). Either way it is the last call the batch's results
    /// get; held messages may still follow.
    fn counting_stopped(&mut self, by: Stop);
    fn end_rows(&mut self) -> io::Result<()>;
    /// The rows of the current result set are over because a SQL error ended the stream: what
    /// arrived is not the query's result. Closes the result like `end_rows`, but marked
    /// incomplete, so the count is not exact (DESIGN.md §5.1 to §5.3); the messages held and the
    /// errors follow, as after `end_rows`. A later result is a new one and is exact again.
    fn end_rows_incomplete(&mut self) -> io::Result<()>;
    fn rows_affected(&mut self, count: u64) -> io::Result<()>;
    fn message(&mut self, message: &ServerMessage) -> io::Result<()>;
    fn error(&mut self, error: &ServerMessage) -> io::Result<()>;
}

/// What the login says about the connection, for the output (JSON's `connection`, DESIGN.md
/// §5.2). It is read from the session, so it costs no round trip, and it holds nothing from a
/// credential: not the login name, never a connection string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The server's own name for itself (`@@SERVERNAME`), which need not be the name that was
    /// dialled. `None` when the server didn't report one.
    pub server: Option<String>,
    /// The server version, `major.minor.build`, from the login acknowledgement.
    pub version: Option<String>,
    /// The database in effect. The driver follows a `USE` (it applies the server's
    /// environment-change token), so this is the current database whenever it is read.
    pub database: String,
    /// `windows` or `sql`.
    pub auth: &'static str,
    /// Whether the traffic after the login is encrypted.
    pub encrypted: bool,
}

#[derive(Debug)]
pub enum DbError {
    /// Couldn't connect or log in. The message includes a hint where one applies.
    Connect(String),
    /// The connection broke mid-run.
    Lost(String),
    Timeout(String),
    /// A cancel ended the work. The text says what sqlh observed, no more: the connection
    /// stayed usable, or it was closed instead. Never that the server confirmed the cancel
    /// (see `Session::failure`).
    Cancelled(String),
    /// A count of hidden rows that sqlh itself stopped, and the server never acknowledged the
    /// stop, so the connection was closed (`COUNT_STOP_UNCONFIRMED`). The run says how to read
    /// every row instead, in the words of its reader (`run::db_failure`).
    CountStopUnconfirmed,
    /// A statement the tool itself sent failed.
    Sql(String),
    /// Writing output failed (e.g. the reader closed the pipe).
    Output(io::Error),
    Protocol(String),
}

impl From<io::Error> for DbError {
    fn from(error: io::Error) -> Self {
        DbError::Output(error)
    }
}

/// Cancels the connect or the running batch. Cancelling a handle cancels its children; the
/// children share one flag, so cancel only the root.
pub struct Cancel {
    handle: CancelHandle,
    flag: Arc<CancelFlag>,
}

/// Lets sqlh's own waits (output backpressure) see a cancel; the driver's handle covers
/// requests to the server.
#[derive(Default)]
struct CancelFlag {
    set: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Cancel {
    pub fn new() -> Self {
        Cancel {
            handle: CancelHandle::new(),
            flag: Arc::default(),
        }
    }

    pub fn child(&self) -> Self {
        Cancel {
            handle: self.handle.child_handle(),
            flag: Arc::clone(&self.flag),
        }
    }

    pub fn cancel(self) {
        self.flag.set.store(true, Ordering::SeqCst);
        self.flag.notify.notify_waiters();
        self.handle.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.set.load(Ordering::SeqCst)
    }

    /// Resolves once the root is cancelled.
    pub async fn cancelled(&self) {
        loop {
            // Registered before checking, so `notify_waiters` in between isn't lost.
            let notified = self.flag.notify.notified();
            if self.flag.set.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// Output backpressure (plan-history.md 2.3): while too much output waits for a slow reader, `run`
/// waits here instead of reading more rows. The wait doesn't block the runtime, so timers
/// and Ctrl+C keep working.
pub trait Backpressure {
    fn is_full(&self) -> bool;
    /// Resolves when there is room again; fails when the reader has stalled for too long.
    fn room(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_>>;
}

/// Waits while the output is backed up, unless the run is cancelled first, or the batch's
/// deadline (`-t`) passes: a reader that is slow, and not stalled, can't hold a batch past it.
/// The connection is closed then, as for a cancel, which stops the server's work too.
async fn wait_for_output(
    gate: &dyn Backpressure,
    cancel: &Cancel,
    deadline: Option<Instant>,
) -> Result<(), DbError> {
    if !gate.is_full() {
        return Ok(());
    }
    let expiry = async {
        match deadline {
            Some(at) => tokio::time::sleep_until(at.into()).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(DbError::Cancelled(
            "cancelled while waiting for the reader; the connection was closed".into(),
        )),
        room = gate.room() => room.map_err(DbError::Output),
        () = expiry => Err(DbError::Timeout("query timeout".into())),
    }
}

/// How long a read-only run goes on counting the rows it does not show (DESIGN.md §4.4).
pub const COUNT_BUDGET: Duration = Duration::from_millis(250);

/// The count's budget in a batch where a stop could cancel a later statement's output
/// (decision 45, `guard::several_outputs`): long enough that a modest result over a slow link
/// (about 600 wide rows took the whole 250 ms on one client's) ends inside it, so the batch runs
/// whole, and short enough that a huge one still answers within seconds.
pub const LATER_STATEMENTS_BUDGET: Duration = Duration::from_secs(5);

/// What stopped a count of hidden rows (`BatchSink::counting_stopped`). The output tells the
/// two apart in its advice: the budget is lifted by `--exact-count`, `-t` by raising it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The count's own time budget ran out (`Counting::budget`).
    Budget,
    /// `-t` ran out while only hidden rows were being read (`Counting::timeout_stops`).
    Timeout,
}

/// How a batch counts the rows it does not show: past `--max-rows`, in a result `--max-results`
/// hides, and after the `--max-output` budget ran out.
///
/// Exact counting reads them all, which costs the size of the result (1 to 4 s for millions of
/// narrow rows, minutes for a few gigabytes) to learn a number. A bounded count stops after
/// `budget`, counted from the first hidden row of a result, and sends the server an attention,
/// the cancel Ctrl+C uses: the total is then a lower bound, and the rest of the batch does not
/// run (`BatchSink::counting_stopped`). A stopped count can skip a `COMMIT`, so the caller picks
/// exact counting wherever that would lose something (docs/notes/drain-experiment.md).
///
/// The clock starts with the first hidden row. Until one arrives there is nothing being counted,
/// and a server that is slow to produce it is waited for, up to `-t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counting {
    /// `None`: count to the end of every result.
    pub budget: Option<Duration>,
    /// When `-t` expires while only hidden rows are being counted, the rows shown are complete,
    /// so the count stops there instead of failing the run (exit 4). Not where a stop would lose
    /// something, which `main` decides: with writes allowed the stop skips what follows (a
    /// skipped `COMMIT` has to be a failure), and with `XACT_ABORT` in the input the attention
    /// rolls the read-only wrapper transaction back, which would end the run anyway.
    pub timeout_stops: bool,
}

impl Counting {
    /// Count to the end, and let `-t` fail the run as it always did.
    pub const EXACT: Counting = Counting {
        budget: None,
        timeout_stops: false,
    };

    /// Stop after `COUNT_BUDGET`.
    pub const BOUNDED: Counting = Counting {
        budget: Some(COUNT_BUDGET),
        timeout_stops: true,
    };

    /// This counting for a batch whose later statements a stop would cancel (decision 45): a
    /// bounded count gets `LATER_STATEMENTS_BUDGET`; an exact one stays exact.
    pub fn with_later_statements(self) -> Counting {
        Counting {
            budget: self
                .budget
                .map(|budget| budget.max(LATER_STATEMENTS_BUDGET)),
            ..self
        }
    }
}

/// How much work, in ticks, a loop does between looks at the clock (`Pace`). A hidden row is
/// 1 tick, a row that is shown 4 and a chunk of a large value 16, so a look comes every few
/// hundred narrow rows, a few dozen rows that are formatted, or a few big chunks, and a look
/// costs well under a percent of that work.
const PACE_TICKS: u32 = 256;
const SHOWN_ROW_TICKS: u32 = 4;
const CHUNK_TICKS: u32 = 16;

/// The longest the batch's loops run before they hand the runtime back (`tokio::task::
/// yield_now`). The runtime has one thread. A timer (the count budget, the deadline), the
/// console events and the writer thread's completions run only when this task gives it up,
/// and a result that is already in the read buffer never makes it wait.
const YIELD_EVERY: Duration = Duration::from_millis(10);

/// How long past a batch's deadline the driver gets to settle the cancel (an attention, or
/// the withdrawal of a request it was still sending) before sqlh drops the work and retires
/// the session: the batch always ends by `-t` plus this (R15 follow-up B, DESIGN.md §4.4). It
/// matches the driver's own wait for an attention's acknowledgement.
const SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Runs `work` to its end, or until `SETTLE_GRACE` past `deadline`, whichever comes first:
/// `None` when the grace ran out, and `work` is then dropped where it stands. No deadline (`-t
/// 0`), no bound. The batch's own loops yield every `YIELD_EVERY`, so the timer here runs even
/// while they work on rows that are already there.
async fn settle_within<T>(deadline: Option<Instant>, work: impl Future<Output = T>) -> Option<T> {
    let expiry = async {
        match deadline.and_then(|at| at.checked_add(SETTLE_GRACE)) {
            Some(at) => tokio::time::sleep_until(at.into()).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        result = work => Some(result),
        () = expiry => None,
    }
}

/// `DbError::Timeout` when the grace ran out (`Session::run`).
const UNSETTLED: &str =
    "query timeout; the cancel was not settled in time, so the connection was closed";

/// What a retired session answers (`Session::retired`).
const RETIRED: &str = "the connection was closed after a batch that did not settle";

/// Who cancelled the batch's statement, other than the user: the first of them to fire.
const CAUSE_NONE: u8 = 0;
const CAUSE_BUDGET: u8 = 1;
const CAUSE_DEADLINE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    /// The count's own time budget (`Counting::budget`).
    Budget,
    /// `-t`: the batch's deadline.
    Deadline,
}

/// A handle that cancels once, and the cause it is recorded under.
type Root = Arc<Mutex<Option<CancelHandle>>>;

/// Cancels the batch's statement through `root`, once, recording `cause` if no other cause
/// came first. A user's cancel does not go through here, so it leaves the cause alone.
fn fire(root: &Mutex<Option<CancelHandle>>, cause: &AtomicU8, which: u8) {
    if let Ok(mut cell) = root.lock()
        && let Some(handle) = cell.take()
    {
        let _ = cause.compare_exchange(CAUSE_NONE, which, Ordering::SeqCst, Ordering::SeqCst);
        handle.cancel();
    }
}

/// sqlh's own `-t` for one batch (R15): a monotonic deadline from the moment the batch is
/// sent, enforced here and not by the driver, whose `timeout_secs` counts only the request's send
/// and the time its transport reads wait (0.4.0), and so never expires for data that is already
/// in the buffer, or for work sqlh does between reads (DESIGN.md §4.4).
///
/// It is enforced three ways, because each covers what the others cannot: a timer task fires
/// the cancel while the batch waits for the server or for the reader; `tick` and `slice` look
/// at the clock as rows and chunks go by, which catches a loop that never waits; and both only
/// run when the loop hands the runtime back, which `slice` does every `YIELD_EVERY`.
struct Pace {
    deadline: Option<Instant>,
    root: Root,
    cause: Arc<AtomicU8>,
    ticks: u32,
    yielded: Instant,
}

impl Pace {
    /// `weight` ticks of work were done: look at the clock when enough have been.
    async fn tick(&mut self, weight: u32) {
        self.ticks += weight;
        if self.ticks >= PACE_TICKS {
            self.slice().await;
        }
    }

    /// Looks at the clock now: fires the deadline if it has passed, and gives the runtime a
    /// turn if the loop has held it for `YIELD_EVERY`.
    async fn slice(&mut self) {
        self.ticks = 0;
        let now = Instant::now();
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            fire(&self.root, &self.cause, CAUSE_DEADLINE);
        }
        if now.duration_since(self.yielded) >= YIELD_EVERY {
            tokio::task::yield_now().await;
            self.yielded = Instant::now();
        }
    }
}

/// One batch's stops: the deadline (`-t`), and the budget of a count of hidden rows. Both
/// cancel the batch's statement without cancelling the run: the driver gets `handle`, which
/// is a child of the budget's root, which is a child of the deadline's root, which is a child
/// of the run's handle. Firing the budget's root cancels the statement; firing the deadline's
/// cancels it and, through it, the budget's; and the run's own cancel (Ctrl+C) still reaches
/// the statement through all of them, without leaving a cause behind. The driver then does
/// what it does for any cancel of a read in progress: it sends an attention, reads on to the
/// server's acknowledgement (DONE with the ATTN flag, at most 5 s) and settles its state.
struct Drain {
    budget: Option<Duration>,
    /// What the driver gets for the batch.
    handle: CancelHandle,
    /// Holds the budget's root until it fires or the result ends. Empty without a budget.
    root: Root,
    /// Which stop fired first (`CAUSE_*`); 0 while none has.
    cause: Arc<AtomicU8>,
    /// When the first hidden row of the current result was counted.
    began: Option<Instant>,
    timer: Option<tokio::task::JoinHandle<()>>,
    pace: Pace,
    deadline_timer: Option<tokio::task::JoinHandle<()>>,
    /// The last result read was read to its end with its shown rows complete and only hidden
    /// rows left (`Session::finished_late`).
    tail_hidden: bool,
}

impl Drain {
    /// `deadline` is `None` for `-t 0`.
    fn new(budget: Option<Duration>, deadline: Option<Instant>, run: &CancelHandle) -> Drain {
        let deadline_root = run.child_handle();
        let budget_root = budget.map(|_| deadline_root.child_handle());
        let handle = budget_root
            .as_ref()
            .unwrap_or(&deadline_root)
            .child_handle();
        let cause: Arc<AtomicU8> = Arc::default();
        let deadline_root: Root = Arc::new(Mutex::new(Some(deadline_root)));
        let deadline_timer = deadline.map(|at| {
            let (root, cause) = (Arc::clone(&deadline_root), Arc::clone(&cause));
            tokio::spawn(async move {
                tokio::time::sleep_until(at.into()).await;
                fire(&root, &cause, CAUSE_DEADLINE);
            })
        });
        Drain {
            budget,
            handle,
            root: Arc::new(Mutex::new(budget_root)),
            cause: Arc::clone(&cause),
            began: None,
            timer: None,
            pace: Pace {
                deadline,
                root: deadline_root,
                cause,
                ticks: 0,
                yielded: Instant::now(),
            },
            deadline_timer,
            tail_hidden: false,
        }
    }

    /// The handle for the driver's `execute`.
    fn handle(&self) -> &CancelHandle {
        &self.handle
    }

    /// The batch's deadline; `None` without one.
    fn deadline(&self) -> Option<Instant> {
        self.pace.deadline
    }

    /// Which of sqlh's own stops cancelled the statement first, if one did.
    fn cause(&self) -> Option<Cause> {
        match self.cause.load(Ordering::SeqCst) {
            CAUSE_BUDGET => Some(Cause::Budget),
            CAUSE_DEADLINE => Some(Cause::Deadline),
            _ => None,
        }
    }

    /// Whether the budget ended the work, as opposed to the user or `-t`.
    fn fired(&self) -> bool {
        self.cause() == Some(Cause::Budget)
    }

    /// Whether `-t` ended the work, as opposed to the user or the budget.
    fn deadline_fired(&self) -> bool {
        self.cause() == Some(Cause::Deadline)
    }

    /// Whether the batch is past its deadline, whether or not the deadline has fired yet (a
    /// loop that never waited may not have looked at the clock since), unless the user
    /// cancelled: then that cancel decides the outcome.
    fn overdue(&self, cancel: &Cancel) -> bool {
        !cancel.is_cancelled()
            && (self.deadline_fired()
                || self
                    .pace
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline))
    }

    /// A result begins, or ends however it ends. The budget's timer stops, so that it can't
    /// cancel the statements that follow. (The deadline's runs to the end of the batch.)
    fn reset(&mut self) {
        self.began = None;
        if let Some(timer) = self.timer.take() {
            timer.abort();
        }
    }

    /// One more row counted without being shown. The first starts the clock and a timer, which
    /// cancels even while the server is looking for the next row (a check between rows cannot);
    /// the check here ends the count on time when rows keep coming.
    fn counted_row(&mut self) {
        let Some(budget) = self.budget else {
            return;
        };
        match self.began {
            None => {
                self.began = Some(Instant::now());
                let (root, cause) = (Arc::clone(&self.root), Arc::clone(&self.cause));
                self.timer = Some(tokio::spawn(async move {
                    tokio::time::sleep(budget).await;
                    fire(&root, &cause, CAUSE_BUDGET);
                }));
            }
            Some(began) if began.elapsed() >= budget => {
                fire(&self.root, &self.cause, CAUSE_BUDGET);
            }
            Some(_) => {}
        }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        self.reset();
        if let Some(timer) = self.deadline_timer.take() {
            timer.abort();
        }
    }
}

/// Debug builds: the work `SQLH_DEBUG_ROW_WORK_US` asks for, per row and per chunk.
#[cfg(debug_assertions)]
fn spin(span: Option<Duration>) {
    if let Some(span) = span {
        let until = Instant::now() + span;
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }
}

pub struct Session {
    client: TdsClient,
    /// Debug builds only (`SQLH_DEBUG_STOP_HOLD_MS`): how long to wait after a count stops,
    /// before telling it from a user's cancel, so that a test can cancel in that gap.
    #[cfg(debug_assertions)]
    stop_hold: Option<Duration>,
    /// Debug builds only (`SQLH_DEBUG_ROW_WORK_US`): work, in microseconds, that the row and
    /// chunk loops do for every row and chunk after reading it, standing in for formatting or
    /// a slow consumer: sqlh's own work, which the driver's clock never sees (R15). A test
    /// can then make a ready stream last longer than `-t` and see the deadline end it.
    #[cfg(debug_assertions)]
    row_work: Option<Duration>,
    /// How the login was made (`Auth::method`), for `connection`.
    auth: &'static str,
    /// Buffers for reading large values, made on first use.
    scratch: stream::Scratch,
    /// Set when a batch's work was abandoned at `SETTLE_GRACE` past its deadline: the driver was
    /// dropped in the middle of an operation, so the connection is in no known state and nothing
    /// more may be sent on it. The caller drops the session, which closes the socket.
    retired: bool,
}

/// What the batch loop gets from one step through a result set's rows.
enum Row {
    /// A row to show.
    Values(Vec<Value>),
    /// A row read only to count it: past `--max-rows`, in a result set `--max-results` hides, or
    /// after the output budget ran out.
    Hidden,
    /// The result set has ended.
    End,
}

impl Session {
    pub async fn connect(options: &ConnectOptions, cancel: &Cancel) -> Result<Session, DbError> {
        Self::connect_with(options, cancel, spn::HopSpn::system()).await
    }

    /// `connect`, with Windows auth's SPN provider given: the tests' seam.
    async fn connect_with(
        options: &ConnectOptions,
        cancel: &Cancel,
        spn: Arc<dyn SecurityContextProvider>,
    ) -> Result<Session, DbError> {
        let mut context = ClientContext::default();
        context.application_name = options.application_name.into();
        context.database = options.database.clone().unwrap_or_default();
        match &options.auth {
            Auth::Integrated => {
                context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
                // The driver calls this for each login hop, after its TCP connect, prelogin and
                // TLS, with the host and port it dials (SQL Browser's port, each routed host), so
                // the SPN is that hop's (decision 72; DESIGN.md §4.2, `-E`). `server_spn` stays
                // unset: its one value would go to every hop (R52).
                context.security_context_provider = Some(spn);
            }
            Auth::Sql { user, password } => {
                context.tds_authentication_method = TdsAuthenticationMethod::Password;
                context.user_name = user.clone();
                // The driver keeps its own `String` for the connection's life (it may log in
                // again); sqlh can't wipe that copy (DESIGN.md §8).
                context.password = password.to_string();
            }
        }
        context.encryption_options = EncryptionOptions {
            mode: match options.encrypt {
                Encrypt::Optional => EncryptionSetting::PreferOff,
                Encrypt::Mandatory => EncryptionSetting::On,
                Encrypt::Strict => EncryptionSetting::Strict,
            },
            trust_server_certificate: options.trust_server_certificate,
            host_name_in_cert: None,
            server_certificate: None,
        };
        // One attempt, no retries: the driver would retry a refused connection after 10 s,
        // until the deadline (14 s with -l 15). An agent can retry itself, and sooner. A count
        // of 0 also keeps idle connection resiliency off: the driver asks the server for
        // session recovery only when it is above 0, so it never logs in again behind sqlh's
        // back, and the engine's one retry on a new connection is the only reconnect (owner
        // decision 41; docs/notes/driver-0.3.0.md §2).
        context.connect_retry_count = 0;
        // Each TCP connect attempt gets -l seconds and the whole login one more, so an
        // unreachable address fails at TCP (exit 2) and a server that takes the connection
        // but doesn't log in hits the deadline (exit 4). Equal limits raced.
        // -l 0 means no limit, as in sqlcmd and like -t 0. The driver would take a TCP limit
        // of 0 as "give up at once", so that gets a day (Windows gives up on a TCP connect
        // after about 21 s anyway); a login deadline of 0 is the driver's "none".
        let (tcp, login) = match options.login_timeout {
            0 => (86_400, 0),
            limit => (limit, limit.saturating_add(1)),
        };
        context.connect_timeout = tcp;
        context.login_timeout = Some(login);
        context.application_intent = if options.read_only_intent {
            ApplicationIntent::ReadOnly
        } else {
            ApplicationIntent::ReadWrite
        };
        // Always explicit (owner decision 40): mssql-tds's `None` (0.3.0, 0.4.0) takes its own
        // default, which is on, or MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT, so a connect without
        // -M would race its addresses in parallel and its timeout would become exit 4. sqlh
        // keeps sqlcmd's rule: one address after another unless -M.
        context.multi_subnet_failover = Some(options.multi_subnet_failover);
        // Version 2, the driver's default since 0.4.0, set here so that a later default can't
        // change the output unseen (owner decision 71): SQL Server 2025 then sends float16
        // vectors natively, which `value` writes at their own precision, and float32 ones as
        // under version 1.
        context.vector_version = VectorVersion::V2;

        let source = data_source(&options.server);
        let provider = TdsConnectionProvider::new();
        let connecting = provider.create_client(context, &source, Some(&cancel.handle));
        // The driver drops a pending attempt itself when its handle is cancelled (0.2.0 to 0.4.0
        // wrap the whole connect in `run_until_cancelled`; with this race removed, a cancel during a
        // TCP attempt and during a login each ended the run within 100 ms). Racing sqlh's own
        // flag keeps the cancel independent of that. Dropping an attempt closes the socket, and
        // a login that never finished leaves nothing to clean up. Only a blocking call outlasts a
        // cancel: opening a pipe, or Windows auth's SPN wait (at most 500 ms, on a thread of the
        // driver's blocking pool, `db::spn`), which is left to end by itself.
        let mut client = tokio::select! {
            client = connecting => client.map_err(|e| connect_error(e, options))?,
            () = cancel.cancelled() => {
                return Err(DbError::Cancelled("cancelled while connecting".into()));
            }
        };
        // Decision 41's guard: session recovery stays off, so a broken connection is reported,
        // never silently replaced. A driver that negotiated it anyway would hide a login from
        // the engine's accounting, so the connect fails instead.
        if client.is_session_recovery_enabled() {
            return Err(DbError::Protocol(
                "the driver negotiated session recovery, which sqlh keeps off".into(),
            ));
        }
        // Login chatter: "Changed database context to ...", "Changed language setting to ...".
        client.take_info_messages();
        Ok(Session {
            client,
            #[cfg(debug_assertions)]
            stop_hold: std::env::var("SQLH_DEBUG_STOP_HOLD_MS")
                .ok()
                .and_then(|ms| ms.trim().parse().ok())
                .map(Duration::from_millis),
            #[cfg(debug_assertions)]
            row_work: std::env::var("SQLH_DEBUG_ROW_WORK_US")
                .ok()
                .and_then(|us| us.trim().parse().ok())
                .map(Duration::from_micros),
            auth: options.auth.method(),
            scratch: stream::Scratch::default(),
            retired: false,
        })
    }

    /// What the login says about the connection. Nothing is sent to the server; the driver
    /// already holds all of it. The database is the current one, since the driver applies the
    /// server's environment-change token for a `USE` (plan-history.md 2.11); read it right after
    /// the connect and it is the database the first batch starts in.
    pub fn connection(&self) -> Connection {
        Connection {
            server: self.client.server_reported_name().map(str::to_owned),
            version: self
                .client
                .server_version()
                .map(|v| format!("{}.{}.{}", v.major, v.minor, v.build)),
            database: self.client.database().to_owned(),
            auth: self.auth,
            encrypted: self.client.is_encrypted(),
        }
    }

    /// One line of connection facts for --verbose.
    pub fn describe(&self) -> String {
        let connection = self.connection();
        format!(
            "server {} version {}, database {}, encrypted {}",
            connection.server.as_deref().unwrap_or("?"),
            connection.version.as_deref().unwrap_or("?"),
            connection.database,
            connection.encrypted
        )
    }

    pub fn in_transaction(&self) -> bool {
        self.client.has_active_transaction()
    }

    /// Runs one batch, streaming results, messages and errors to `sink` in the order that
    /// `BatchSink` describes. In particular, the messages the driver has buffered go out at the
    /// top of each step, before that step's result set, row count or errors, and none is
    /// handed over while a result set's rows are being read. SQL errors go to the sink and
    /// don't fail the call; only transport, timeout and output problems do.
    ///
    /// `timeout_secs` is `-t`, kept here as a deadline from the call to the end of the batch
    /// (`Pace`, DESIGN.md §4.4); 0 is none. It ends the batch with `DbError::Timeout`, except
    /// where only hidden rows are being counted and `Counting::timeout_stops` allows a stop.
    ///
    /// With a bounded `counting`, a result whose hidden rows outrun the budget (or `-t`, when
    /// `Counting::timeout_stops`) ends the batch instead: the rest of it is cancelled, so the
    /// statements after that result do not run, and the sink is told (`counting_stopped`).
    ///
    /// However the driver settles the cancel, the call returns by `SETTLE_GRACE` past the
    /// deadline: after that the batch's work is dropped where it stands, the session is retired
    /// (nothing more may be sent on it) and the result is `DbError::Timeout`.
    pub async fn run(
        &mut self,
        sql: String,
        timeout_secs: u32,
        counting: Counting,
        cancel: &Cancel,
        sink: &mut dyn BatchSink,
        gate: &dyn Backpressure,
    ) -> Result<(), DbError> {
        if self.retired {
            return Err(DbError::Lost(RETIRED.into()));
        }
        // `-t` is sqlh's own deadline from here (R15); the driver's `timeout_secs` stays 0 (none),
        // because it adds up only the time spent waiting for the server and so can neither
        // bound a batch nor agree with this one.
        let deadline = (timeout_secs > 0)
            .then(|| Instant::now().checked_add(Duration::from_secs(u64::from(timeout_secs))))
            .flatten();
        // The driver settles a cancel in its own time: an attention gets 5 s, but withdrawing a
        // request that was cancelled between two of its packets waits up to 120 s for the
        // server (mssql-tds 0.2.0, `withdraw_sent_message`), and does it with the cancel
        // removed. So the bound on the whole batch is sqlh's: the deadline plus the grace,
        // whatever the driver is doing then (R15 follow-up B).
        let work = self.run_batch(sql, deadline, counting, cancel, sink, gate);
        match settle_within(deadline, work).await {
            Some(result) => result,
            None => {
                self.retired = true;
                Err(DbError::Timeout(UNSETTLED.into()))
            }
        }
    }

    /// `run`, without the bound on settling.
    async fn run_batch(
        &mut self,
        sql: String,
        deadline: Option<Instant>,
        counting: Counting,
        cancel: &Cancel,
        sink: &mut dyn BatchSink,
        gate: &dyn Backpressure,
    ) -> Result<(), DbError> {
        let mut drain = Drain::new(counting.budget, deadline, &cancel.handle);
        let options = ExecuteOptions::new()
            .timeout_secs(0)
            .cancel(drain.handle())
            .on_error(BatchErrorMode::Continue);
        let mut step = self.client.execute(sql, options).await;
        loop {
            self.flush_messages(sink)?;
            wait_for_output(gate, cancel, drain.deadline()).await?;
            // A batch of many small statements and results never waits for a read.
            drain.pace.slice().await;
            // Whether the step before this one was a result read to its end while only hidden
            // rows were left (see `finished_late`).
            let hidden_tail = std::mem::take(&mut drain.tail_hidden);
            match step {
                Ok(StatementResult::Rows) => {
                    if self
                        .read_rows(sink, gate, cancel, &mut drain, counting)
                        .await?
                    {
                        // The count stopped and the driver has settled the cancel; there is
                        // nothing left to walk.
                        break;
                    }
                }
                // The budget fired as a result's last row came in, so that the cancel met the
                // next step instead of a read of hidden rows. The count was complete, but the
                // rest of the batch is gone all the same, and the output says so.
                Err(TdsError::OperationCancelledError(_))
                    if self.stopped_by_budget(&drain, cancel) =>
                {
                    sink.counting_stopped(Stop::Budget);
                    break;
                }
                Ok(StatementResult::NoRows {
                    rows_affected: Some(count),
                }) => sink.rows_affected(count)?,
                Ok(StatementResult::NoRows {
                    rows_affected: None,
                }) => {}
                // The batch is over as far as the driver knows. It is a success only if it was
                // over in time: a deadline that fired (or passed) while the last row's work or
                // the final DONE was handled is still a timeout, although there is nothing left
                // to cancel (R15 follow-up A).
                Ok(StatementResult::End) => {
                    if self.finished_late(&drain, cancel, counting, hidden_tail) {
                        return Err(DbError::Timeout("query timeout".into()));
                    }
                    break;
                }
                Err(TdsError::SqlServerError { diagnostics }) => {
                    for error in &diagnostics.errors {
                        sink.error(&from_error(error))?;
                    }
                    if !self.client.has_open_batch() {
                        if self.finished_late(&drain, cancel, counting, false) {
                            return Err(DbError::Timeout("query timeout".into()));
                        }
                        break;
                    }
                }
                // A cancel here can still be sqlh's own stop of a count (see `counting_failure`).
                Err(error) => return Err(self.counting_failure(error, &drain, cancel)),
            }
            step = self.client.advance().await;
        }
        self.flush_messages(sink)?;
        Ok(())
    }

    /// A driver error mid-run. For a cancel, sqlh knows one thing: whether the driver marked
    /// the connection dead, which it does when its attention goes unacknowledged.
    ///
    /// A connection not marked dead is a liveness observation, not a receipt for the
    /// attention. In mssql-tds 0.2.0 the flag is still false after an acknowledged attention
    /// (DONE_ATTN), but also when the cancel came before the request was sent and when a
    /// partly sent request was withdrawn with EOM|IGNORE; in those two the server confirmed
    /// nothing. The driver exposes no receipt: its settlement marker is private, and
    /// `send_attention_with_timeout` would send a new attention rather than report the old one.
    /// So that case says only what was observed, and "the server confirmed" is never claimed.
    fn failure(&self, error: TdsError) -> DbError {
        classify(error, self.client.is_connection_dead(), false)
    }

    /// `failure` for an error met in a batch that has a count of hidden rows (`Drain`). When the
    /// budget's own cancel went unacknowledged (it fired, the user did not cancel, and the driver
    /// marked the connection dead), the message says that it was sqlh's count that ended the
    /// run, not a cancel from outside (DESIGN.md §6, exit 6). A cancel that is sqlh's own `-t`
    /// (and not the user's) is a timeout, exit 4, whether or not the server acknowledged it.
    fn counting_failure(&self, error: TdsError, drain: &Drain, cancel: &Cancel) -> DbError {
        if matches!(error, TdsError::OperationCancelledError(_))
            && drain.deadline_fired()
            && !cancel.is_cancelled()
        {
            return DbError::Timeout("query timeout".into());
        }
        let own_stop = drain.fired() && !cancel.is_cancelled();
        classify(error, self.client.is_connection_dead(), own_stop)
    }

    /// Streams one result set's rows to `sink`, holding one row at a time (DESIGN.md §11).
    ///
    /// A result set with a column the driver can stream (`stream::plan`: `varchar(max)` and the
    /// other PLP types) is read through the driver's column cursor, so that a large value is
    /// cut as it arrives, and a row nobody will see is skipped without being decoded. Any other
    /// result set is read a row at a time, which is faster for narrow rows, and a hidden row is
    /// decoded into nothing: no value is built, and no string is copied out of the read buffer.
    ///
    /// True when the count was stopped (see `run`).
    async fn read_rows(
        &mut self,
        sink: &mut dyn BatchSink,
        gate: &dyn Backpressure,
        cancel: &Cancel,
        drain: &mut Drain,
        counting: Counting,
    ) -> Result<bool, DbError> {
        drain.reset();
        let stopped = self.count_rows(sink, gate, cancel, drain, counting).await;
        drain.reset();
        stopped
    }

    /// Whether a batch the driver has finished took longer than `-t`: its deadline fired, or
    /// passed without a look at the clock catching it (the last few rows' work, the final
    /// DONE, read without waiting). Not after the user cancelled, whose cancel wins.
    ///
    /// The owner's hidden-row exception still holds: when the last thing read was a result whose
    /// shown rows were complete and whose hidden rows were all counted (`hidden_tail`), in a run
    /// whose `-t` may stop a count (`Counting::timeout_stops`), the batch is complete and exact,
    /// and nothing it would have run was skipped, so it is not a timeout.
    fn finished_late(
        &self,
        drain: &Drain,
        cancel: &Cancel,
        counting: Counting,
        hidden_tail: bool,
    ) -> bool {
        drain.overdue(cancel) && !(hidden_tail && counting.timeout_stops)
    }

    /// Whether `drain`'s budget, not the user, cancelled the work, and the driver settled the
    /// cancel and kept the connection (it marks it dead when the server doesn't acknowledge).
    fn stopped_by_budget(&self, drain: &Drain, cancel: &Cancel) -> bool {
        drain.fired() && !cancel.is_cancelled() && !self.client.is_connection_dead()
    }

    async fn count_rows(
        &mut self,
        sink: &mut dyn BatchSink,
        gate: &dyn Backpressure,
        cancel: &Cancel,
        drain: &mut Drain,
        counting: Counting,
    ) -> Result<bool, DbError> {
        let metadata = self.client.get_metadata();
        let columns: Vec<Column> = metadata.iter().map(types::column).collect();
        let plan = stream::plan(metadata);
        sink.columns(&columns)?;
        loop {
            // Asked for every row: the row limit and the output budget can run out at any row.
            let shown = sink.wants_values();
            let step = match &plan {
                Some(plan) => {
                    let cap = sink.cell_limit();
                    self.streamed_row(plan, shown.then_some(cap), &mut drain.pace)
                        .await
                }
                None => self.plain_row(shown).await,
            };
            #[cfg(debug_assertions)]
            if step.is_err()
                && drain.fired()
                && let Some(hold) = self.stop_hold
            {
                // The marker tells a test when the hold begins; a failed write is ignored.
                let _ = io::Write::write_fmt(
                    &mut io::stderr(),
                    format_args!(
                        "sqlh: debug: holding {} ms after the count was cancelled\n",
                        hold.as_millis()
                    ),
                );
                tokio::time::sleep(hold).await;
            }
            match step {
                Ok(Row::Values(values)) => {
                    sink.row(&values)?;
                    #[cfg(debug_assertions)]
                    spin(self.row_work);
                    wait_for_output(gate, cancel, drain.deadline()).await?;
                    drain.pace.tick(SHOWN_ROW_TICKS).await;
                    // Time spent on a row to show (its reading, its formatting, the wait for the
                    // reader) is never time spent counting hidden rows, so a deadline it crossed
                    // is a timeout here and now, before the next row's visibility is asked. Even
                    // when this row filled the quota and the next would be hidden, the hidden-row
                    // exception must not take the overrun over (R15 A, second round).
                    if drain.overdue(cancel) {
                        return Err(DbError::Timeout("query timeout".into()));
                    }
                }
                Ok(Row::Hidden) => {
                    sink.skipped_row();
                    #[cfg(debug_assertions)]
                    spin(self.row_work);
                    drain.counted_row();
                    drain.pace.tick(1).await;
                }
                // The result is over, and so is the work its last rows took. If that crossed
                // the deadline, the result is not reported complete: a result whose rows to show
                // were still coming, or a run where `-t` may not stop a count, ends in a timeout
                // (R15 follow-up A). With the shown rows complete and only hidden rows counted,
                // in a run where `-t` may stop a count, the result is complete and exact.
                Ok(Row::End) => {
                    if drain.overdue(cancel) && (shown || !counting.timeout_stops) {
                        return Err(DbError::Timeout("query timeout".into()));
                    }
                    drain.tail_hidden = !shown;
                    break;
                }
                Err(TdsError::SqlServerError { diagnostics }) => {
                    sink.end_rows_incomplete()?;
                    self.flush_messages(sink)?;
                    for error in &diagnostics.errors {
                        sink.error(&from_error(error))?;
                    }
                    return Ok(false);
                }
                // The budget ran out, or `-t` did while only hidden rows were being read (the
                // rows shown are complete): the driver has settled the cancel and kept the
                // connection, and what was counted is a lower bound.
                Err(TdsError::OperationCancelledError(_))
                    if self.stopped_by_budget(drain, cancel) =>
                {
                    sink.counting_stopped(Stop::Budget);
                    sink.end_rows()?;
                    return Ok(true);
                }
                Err(TdsError::OperationCancelledError(_))
                    if drain.deadline_fired()
                        && !shown
                        && counting.timeout_stops
                        && !cancel.is_cancelled()
                        && !self.client.is_connection_dead() =>
                {
                    sink.counting_stopped(Stop::Timeout);
                    sink.end_rows()?;
                    return Ok(true);
                }
                Err(error) => return Err(self.counting_failure(error, drain, cancel)),
            }
        }
        sink.end_rows()?;
        Ok(false)
    }

    /// The next row of a result set with nothing to stream. Every value is at most 8,016 bytes
    /// (`sql_variant`), apart from `text`, `ntext` and `image`, which the driver reads whole
    /// (and refuses over 100 MB).
    async fn plain_row(&mut self, shown: bool) -> TdsResult<Row> {
        if shown {
            return Ok(match self.client.next_row().await? {
                Some(row) => Row::Values(row.into_iter().map(value::convert).collect()),
                None => Row::End,
            });
        }
        let more = self.client.next_row_into(&mut DiscardRowWriter).await?;
        Ok(if more { Row::Hidden } else { Row::End })
    }

    /// The next row of a result set with a column to stream, through the driver's cursor.
    /// `cap` is `Some(limit)` for a row to show (see `BatchSink::cell_limit`), and `None` for
    /// one to count only: the cursor stays on that row, and the next call skips its columns
    /// without decoding them, a large value chunk by chunk.
    async fn streamed_row(
        &mut self,
        plan: &[stream::Cell],
        cap: Option<usize>,
        pace: &mut Pace,
    ) -> TdsResult<Row> {
        let positioned = match self.client.try_next_row_cursor()? {
            CursorPoll::Ready(positioned) => positioned,
            CursorPoll::Pending => self.client.next_row_cursor().await?,
        };
        if !positioned {
            return Ok(Row::End);
        }
        let Some(cap) = cap else {
            return Ok(Row::Hidden);
        };
        let mut values = Vec::with_capacity(plan.len());
        for (index, cell) in plan.iter().enumerate() {
            values.push(match cell {
                stream::Cell::Whole => {
                    let column = match self.client.try_read_row_column(index)? {
                        CursorPoll::Ready(column) => column,
                        CursorPoll::Pending => self.client.read_row_column(index).await?,
                    };
                    match column {
                        CursorColumn::Value { value, .. } => value::convert(value),
                        _ => return Err(unreadable(index)),
                    }
                }
                cell => self.large_value(index, *cell, cap, pace).await?,
            });
        }
        Ok(Row::Values(values))
    }

    /// Reads column `index`, a PLP column, keeping the first `cap` characters (see `stream`).
    ///
    /// A small value that is already in the read buffer comes in one piece, which also leaves
    /// the row where the next column can follow it without a stream. Anything else is opened
    /// and read chunk by chunk.
    async fn large_value(
        &mut self,
        index: usize,
        cell: stream::Cell,
        cap: usize,
        pace: &mut Pace,
    ) -> TdsResult<Value> {
        let (chunk, room) = self.scratch.buffers();
        match self.client.try_read_row_plp_complete(index, chunk)? {
            CursorPoll::Ready(None) => return Ok(Value::Null),
            CursorPoll::Ready(Some(piece)) => {
                let mut cutter =
                    stream::Cutter::new(cell, cap, room).map_err(TdsError::ProtocolError)?;
                cutter.push(filled(chunk, piece.read)?, true);
                return Ok(cutter.finish());
            }
            CursorPoll::Pending => {}
        }
        let opened = match self.client.try_read_row_column(index)? {
            CursorPoll::Ready(column) => column,
            CursorPoll::Pending => self.client.read_row_column(index).await?,
        };
        match opened {
            CursorColumn::PlpStreaming { .. } => {}
            // NULL.
            CursorColumn::Value { value, .. } => return Ok(value::convert(value)),
            _ => return Err(unreadable(index)),
        }
        let mut cutter = stream::Cutter::new(cell, cap, room).map_err(TdsError::ProtocolError)?;
        loop {
            let piece = match self.client.try_read_active_plp_chunk(chunk)? {
                CursorPoll::Ready(piece) => piece,
                CursorPoll::Pending => self.client.read_active_plp_chunk(chunk).await?,
            };
            cutter.push(filled(chunk, piece.read)?, piece.reached_end);
            if piece.reached_end {
                return Ok(cutter.finish());
            }
            #[cfg(debug_assertions)]
            spin(self.row_work);
            // A value of gigabytes arrives in chunks that may all be ready: the deadline and
            // the runtime's other tasks get their turn between them.
            pace.tick(CHUNK_TICKS).await;
            if piece.read == 0 {
                return Err(TdsError::ProtocolError(
                    "a large value's stream made no progress".into(),
                ));
            }
        }
    }

    /// Runs a statement the tool needs (session setup, NOEXEC, the wrapper transaction),
    /// discarding output. It can be cancelled like a batch.
    pub async fn exec_internal(
        &mut self,
        sql: &str,
        timeout_secs: u32,
        cancel: &Cancel,
    ) -> Result<(), DbError> {
        if self.retired {
            return Err(DbError::Lost(RETIRED.into()));
        }
        let options = ExecuteOptions::new()
            .timeout_secs(timeout_secs)
            .cancel(&cancel.handle);
        let result = match self.client.execute(sql.to_owned(), options).await {
            Ok(_) => self.client.close_query().await,
            Err(error) => Err(error),
        };
        self.client.take_info_messages();
        result.map_err(|error| self.failure(error))
    }

    pub async fn close(mut self) {
        let _ = self.client.close_connection().await;
    }

    /// Asks the server to reset the session before it processes the next request, as a pool
    /// does (`prepare_reset_connection`, the RESETCONNECTION bit): login defaults restored,
    /// temp tables dropped, an open transaction rolled back, the login's database back
    /// (docs/notes/mcp-reuse-spike.md). The driver verifies the server's acknowledgement on
    /// that request, and fails it and marks the connection dead when it is missing, so a
    /// session that wasn't reset is never used (owner decision 17, amended).
    pub fn reset_before_next(&mut self) {
        self.client.prepare_reset_connection(false);
    }

    /// Whether the driver marked the connection dead: it does when an attention goes
    /// unacknowledged, when a reset isn't acknowledged, and when the connection breaks.
    pub fn connection_dead(&self) -> bool {
        self.client.is_connection_dead()
    }

    /// Whether a batch's work was abandoned past its settle bound (`run`): nothing more may be
    /// sent on the connection.
    pub fn is_retired(&self) -> bool {
        self.retired
    }

    fn flush_messages(&mut self, sink: &mut dyn BatchSink) -> io::Result<()> {
        for message in self.client.take_info_messages() {
            sink.message(&from_info(&message))?;
        }
        Ok(())
    }
}

/// The cursor couldn't give a column of the row it is on (a bug in the driver or a protocol
/// anomaly, never expected).
fn unreadable(index: usize) -> TdsError {
    TdsError::ProtocolError(format!("column {} of a row could not be read", index + 1))
}

/// The part of `chunk` a read filled, as the driver counted it.
fn filled(chunk: &[u8], read: usize) -> TdsResult<&[u8]> {
    chunk.get(..read).ok_or_else(|| {
        TdsError::ProtocolError(format!("a read of {read} bytes overran its buffer"))
    })
}

fn from_error(e: &SqlErrorInfo) -> ServerMessage {
    ServerMessage {
        number: e.number,
        severity: e.class,
        state: e.state,
        line: e.line_number,
        procedure: e.proc_name.clone().filter(|p| !p.is_empty()),
        text: e.message.clone(),
    }
}

fn from_info(m: &SqlInfoMessage) -> ServerMessage {
    ServerMessage {
        number: m.number,
        severity: m.class,
        state: m.state,
        line: m.line_number,
        procedure: m.proc_name.clone().filter(|p| !p.is_empty()),
        text: m.message.clone(),
    }
}

fn sql_errors(errors: &[SqlErrorInfo]) -> String {
    errors
        .iter()
        .map(|e| format!("{} (error {}, severity {})", e.message, e.number, e.class))
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a count that sqlh itself stopped says when the server never acknowledged the cancel
/// (the driver waits 5 s, then marks the connection dead and the run ends with exit 6). It
/// names the count, because the user cancelled nothing: the rows shown are complete, since the
/// clock starts only after them. The run adds the way to read on without cancelling
/// (`DbError::CountStopUnconfirmed`).
pub const COUNT_STOP_UNCONFIRMED: &str = "stopped counting the rows that are not shown, but \
    the server didn't confirm the cancel, so the connection was closed; the rows shown are \
    complete";

/// A driver error mid-run, as `DbError` (see `Session::failure`). `dead` is whether the driver
/// marked the connection dead. `own_stop` is whether the error ends a count that sqlh itself
/// stopped, and not a cancel from outside.
fn classify(error: TdsError, dead: bool, own_stop: bool) -> DbError {
    match error {
        TdsError::OperationCancelledError(_) if !dead => {
            DbError::Cancelled("the statement was cancelled; the connection stayed usable".into())
        }
        TdsError::OperationCancelledError(_) if own_stop => DbError::CountStopUnconfirmed,
        TdsError::OperationCancelledError(_) => DbError::Cancelled(
            "the server didn't confirm the cancel, so the connection was closed".into(),
        ),
        error => runtime_error(error),
    }
}

fn runtime_error(error: TdsError) -> DbError {
    match error {
        TdsError::TimeoutError(_) => DbError::Timeout("query timeout".into()),
        TdsError::OperationCancelledError(_) => DbError::Cancelled("cancelled".into()),
        TdsError::SqlServerError { diagnostics } => DbError::Sql(sql_errors(&diagnostics.errors)),
        TdsError::Io(e) => DbError::Lost(e.to_string()),
        e @ (TdsError::ConnectionClosed(_)
        | TdsError::ConnectionError(_)
        | TdsError::SessionRecoveryFailed { .. }
        | TdsError::SessionNotRecoverable(_)) => DbError::Lost(e.to_string()),
        e => DbError::Protocol(e.to_string()),
    }
}

/// What the driver gets for `-S`. Without a protocol, the driver tries TCP and then Named
/// Pipes. Its first pipe open is a synchronous `CreateFileW` on the runtime thread (the wait
/// after a busy pipe, `WaitNamedPipeW`, runs on `spawn_blocking`, but the open does not), so
/// for an unreachable remote host the whole runtime is blocked for about 21 s while SMB gives
/// up, and neither -l nor Ctrl+C can end it. sqlh connects over TCP (DESIGN.md §4.2), so a
/// remote name gets `tcp:`, and `check_server` has already refused an explicit remote pipe.
///
/// Local names keep the driver's order (shared memory, TCP, a local pipe), which never reaches
/// SMB. A protocol, a pipe path or LocalDB passes through as written. An IPv6 address gets
/// `tcp:` like a name, with or without a port or an instance: the driver (0.4.0) splits a
/// protocol off at the first colon, and an address's first group is never one. Other text with a
/// colon, such as `db01:1433`, is no name a host answers to, and the driver fails on it at once.
fn data_source(server: &str) -> Cow<'_, str> {
    let server = server.trim();
    if split_protocol(server).0.is_some() {
        return Cow::Borrowed(server);
    }
    let host = server.split(['\\', '/', ',']).next().unwrap_or_default();
    let local = [".", "(local)", "(localdb)", "localhost", "127.0.0.1", "::1"]
        .iter()
        .any(|name| host.eq_ignore_ascii_case(name));
    if local || host.is_empty() {
        Cow::Borrowed(server)
    } else if !host.contains(':') || looks_like_ipv6(host) {
        Cow::Owned(format!("tcp:{server}"))
    } else {
        Cow::Borrowed(server)
    }
}

/// The protocols a data source can name, as the driver's parser accepts them.
const PROTOCOLS: [&str; 4] = ["tcp", "np", "lpc", "admin"];

/// Whether a host is an IPv6 address rather than text with a stray colon: `::`, or more than
/// one colon.
fn looks_like_ipv6(host: &str) -> bool {
    host.contains("::") || host.matches(':').count() > 1
}

/// Splits a protocol prefix from the rest, the way the driver does (0.4.0,
/// `datasource_parser.rs`, `parse_protocol`): at the first colon, for `tcp`, `np`, `lpc` or
/// `admin` in any case, whatever follows, so `np:::1` is a pipe to `::1` and `np:2001:db8::5\SALES`
/// a pipe to that address (driver-0.4.0.md §1.2). Anything else, such as `http:db01` or an IPv6
/// address's first group, stays part of the name. The protocol comes back in lowercase.
fn split_protocol(server: &str) -> (Option<&'static str>, &str) {
    if let Some(colon) = server.find(':') {
        let name = server[..colon].trim();
        if let Some(protocol) = PROTOCOLS.iter().find(|p| name.eq_ignore_ascii_case(p)) {
            return (Some(protocol), server[colon + 1..].trim_start());
        }
    }
    (None, server)
}

/// Refuses a Named Pipes request to another machine, with a message for a usage error
/// (DESIGN.md §4.2). The driver opens a pipe with a synchronous `CreateFileW` on its runtime
/// thread, so for an unreachable host a run is stuck for about 21 s while SMB gives up, past
/// `-l` and Ctrl+C. TCP can be timed out and cancelled; the open of a pipe cannot. So a remote
/// pipe is refused before anything connects, whether the server came from `-S`, `SQLCMDSERVER`
/// or a profile, and whether it is `np:host`, `np:host\instance`, or a UNC pipe path
/// (`\\host\pipe\sql\query`) with any prefix: the driver reads a UNC path as a pipe even after
/// `tcp:`. The host may be an IPv6 address: 0.4.0 splits `np:` off one (`split_protocol`).
///
/// Local pipes stay: `np:` with `.`, `(local)`, `localhost`, `127.0.0.1`, `::1` or this
/// computer's name, a UNC path to one of those (`\\.\pipe\...`), `lpc:` (shared memory, which
/// the driver only allows locally), `admin:` (a TCP connection) and LocalDB. Text the driver
/// would reject before opening anything, such as `np:` with no host, is left to it.
///
/// The message starts with the server text, so a caller can say where it came from:
/// `-S np:db01: Named Pipes ...`.
pub fn check_server(server: &str) -> Result<(), String> {
    check(server, this_computer().as_deref())
}

/// This computer's name as the driver reads it to decide that a pipe is local, and opens
/// `\\.\pipe\...` for it (owner decision 73): the DNS host name, which the environment can't
/// change. `COMPUTERNAME`, the NetBIOS name (15 characters at most, and set by any parent
/// process), only when the driver has none, which takes `GetComputerNameExW` failing: the
/// driver then opens `\\<that name>\pipe\...` over SMB, which reaches this machine unless the
/// environment was edited to name another.
fn this_computer() -> Option<String> {
    mssql_tds::connection::this_computer_name().or_else(|| std::env::var("COMPUTERNAME").ok())
}

fn check(server: &str, computer: Option<&str>) -> Result<(), String> {
    let server = server.trim();
    // The driver looks for LocalDB before it looks for a protocol.
    let localdb = server
        .get(..9)
        .is_some_and(|s| s.eq_ignore_ascii_case("(localdb)"))
        && matches!(server.as_bytes().get(9), Some(b'\\' | b'/'));
    if localdb {
        return Ok(());
    }
    let local = |name: &str| {
        [".", "(local)", "localhost", "127.0.0.1", "::1"]
            .iter()
            .any(|own| name.eq_ignore_ascii_case(own))
            || computer.is_some_and(|own| name.eq_ignore_ascii_case(own))
    };
    let (protocol, rest) = split_protocol(server);
    let over_tcp = if let Some(path) = rest.strip_prefix(r"\\") {
        let name = path.split('\\').next().unwrap_or_default();
        (!name.is_empty() && !local(name)).then(|| tcp_form(path))
    } else if protocol == Some("np") {
        let name = rest.split(['\\', ',']).next().unwrap_or_default().trim();
        (!name.is_empty() && !local(name)).then(|| rest.to_owned())
    } else {
        None
    };
    match over_tcp {
        Some(tcp) => Err(format!(
            "{server}: Named Pipes to a remote server aren't supported, because their open \
             can't be timed out or cancelled; connect over TCP: -S {tcp}"
        )),
        None => Ok(()),
    }
}

/// The TCP spelling of a pipe path given without its leading `\\`: the server, and the instance
/// when the pipe is `\pipe\MSSQL$instance\sql\query`. Any other pipe name gives the server alone.
fn tcp_form(unc: &str) -> String {
    const PIPE: &str = "pipe\\mssql$";
    const QUERY: &str = "\\sql\\query";
    let (server, pipe) = unc.split_once('\\').unwrap_or((unc, ""));
    let lower = pipe.to_ascii_lowercase();
    match lower
        .strip_prefix(PIPE)
        .and_then(|name| name.strip_suffix(QUERY))
    {
        Some(instance) if !instance.is_empty() => {
            // The slice of the original, to keep the instance's case. ASCII lowercasing
            // doesn't change lengths, so the offsets are the same.
            format!(
                "{server}\\{}",
                &pipe[PIPE.len()..PIPE.len() + instance.len()]
            )
        }
        _ => server.to_owned(),
    }
}

fn connect_error(error: TdsError, options: &ConnectOptions) -> DbError {
    let hints = options.hints;
    let auth = match &options.auth {
        Auth::Integrated => "Windows auth".to_owned(),
        Auth::Sql { user, .. } => format!("SQL login '{user}'"),
    };
    let encrypt = match options.encrypt {
        Encrypt::Optional => "optional",
        Encrypt::Mandatory => "mandatory",
        Encrypt::Strict => "strict",
    };
    let target = format!("{} ({auth}, encrypt {encrypt})", options.server);
    let (detail, hint) = match &error {
        // Under -M, mssql-tds's race of every address (0.3.0, 0.4.0) ends at the TCP limit, -l, with
        // its own "Connection timeout: ..." (parallel_connect.rs). That is no address
        // answering, as an unreachable one is without -M (exit 2), not a login that didn't
        // finish (found in review, 2026-10-31).
        TdsError::TimeoutError(_)
            if options.multi_subnet_failover
                && error.to_string().contains("Connection timeout:") =>
        {
            (
                format!(
                    "no address accepted a TCP connection within {} s (-M tries them all at once)",
                    options.login_timeout
                ),
                hints.pick(
                    "check the server name and port, or raise -l",
                    "check the profile's server and port, or raise its login_timeout",
                ),
            )
        }
        TdsError::TimeoutError(_) => {
            return DbError::Timeout(format!(
                "login timeout: {target} didn't complete the login within {} s; check that it \
                 is a reachable SQL Server and not overloaded, or raise {}",
                options.login_timeout.saturating_add(1),
                hints.pick("-l", "the profile's login_timeout")
            ));
        }
        TdsError::OperationCancelledError(_) => {
            return DbError::Cancelled("cancelled while connecting".into());
        }
        TdsError::SqlServerError { diagnostics } => {
            let detail = sql_errors(&diagnostics.errors);
            let hint = match diagnostics.errors.first().map(|e| e.number) {
                Some(18456) => hints.pick(
                    "check the login and password (SQLCMDPASSWORD), and that the login may use this database",
                    "check the profile's login and its saved password (sqlh profiles add), and that \
                     the login may use this database",
                ),
                Some(4060) => hints.pick(
                    "the database may not exist or the login may lack access; check -d",
                    "the database may not exist or the login may lack access; check the \
                     profile's database",
                ),
                Some(18452) => hints.pick(
                    "the server rejected Windows auth for this login; use -U with a SQL login",
                    "the server rejected Windows auth for this login; use a profile with a SQL \
                     login (auth = \"sql\")",
                ),
                _ => "",
            };
            (detail, hint)
        }
        TdsError::TlsError(_)
        | TdsError::TlsHandshakeError { .. }
        | TdsError::NoServerCertificate => (
            error.to_string(),
            hints.pick(
                "if the server uses a self-signed certificate, pass -C to trust it; old servers without TLS 1.2 may need -No",
                "if the server uses a self-signed certificate, set trust_server_certificate = true \
                 in the profile; old servers without TLS 1.2 may need encrypt = \"optional\"",
            ),
        ),
        TdsError::CertificateExpired | TdsError::CertificateMismatch => (
            error.to_string(),
            hints.pick(
                "pass -C to trust the server certificate anyway",
                "set trust_server_certificate = true in the profile to trust the server \
                 certificate anyway",
            ),
        ),
        TdsError::Security(_) => (
            error.to_string(),
            hints.pick(
                "Windows auth failed; check Kerberos/SPN setup, or use -U with a SQL login",
                "Windows auth failed; check Kerberos/SPN setup, or use a profile with a SQL login",
            ),
        ),
        TdsError::Io(_) | TdsError::ConnectionError(_) | TdsError::ConnectionClosed(_) => (
            error.to_string(),
            "check the server name; for a named instance with SQL Browser unreachable, try host,port",
        ),
        _ => (error.to_string(), ""),
    };
    // The direct Schannel path's errors carry SSPI status names in their text, which say more
    // than the variant: 0.4.0 reports them as `TlsHandshakeError`, 0.3.0 as generic errors.
    // A failure to set up Schannel's credentials on this machine, before anything reached the
    // server, isn't the server's certificate, so neither -C nor -No is the advice: on Windows
    // Server 2016 the driver's `SCH_CREDENTIALS` fails there (upstream-issues.md U12, measured
    // under mandatory with -C on AppVeyor's Server 2016 job), and every mode sends at least the
    // login over TLS to a server that can encrypt. It wins over the strict hint too, which would blame the certificate.
    if detail.contains("AcquireCredentialsHandleW(UNISP_NAME)") {
        return DbError::Connect(format!(
            "cannot connect to {target}: {detail}; this machine couldn't set up TLS, before \
             anything reached the server: the driver's TLS needs Windows 10 1809 or Windows \
             Server 2019 or later (on a later Windows, check that this machine's Schannel \
             settings allow TLS 1.2)"
        ));
    }
    let tls = tls_hint(&detail, hints);
    let certificate = tls.is_some()
        || matches!(
            error,
            TdsError::TlsError(_)
                | TdsError::TlsHandshakeError { .. }
                | TdsError::NoServerCertificate
                | TdsError::CertificateExpired
                | TdsError::CertificateMismatch
        );
    // Strict encryption checks the certificate even when it is trusted (mssql-tds
    // `resolve_tls_validation`), so under -Ns a hint to trust it would only fail again (R46).
    let hint = if certificate && matches!(options.encrypt, Encrypt::Strict) {
        hints.pick(
            "strict encryption checks the server's certificate even with -C, and needs SQL Server \
             2022 or later: connect by the name on the certificate, from a machine that trusts \
             its issuer, or use -Nm, where -C applies",
            "strict encryption checks the server's certificate even with \
             trust_server_certificate = true, and needs SQL Server 2022 or later: connect by the \
             name on the certificate, from a machine that trusts its issuer, or set encrypt = \
             \"mandatory\" in the profile, where trust applies",
        )
    } else {
        tls.unwrap_or(hint)
    };
    let mut message = format!("cannot connect to {target}: {detail}");
    if !hint.is_empty() {
        message.push_str("; ");
        message.push_str(hint);
    }
    DbError::Connect(message)
}

fn tls_hint(detail: &str, hints: Hints) -> Option<&'static str> {
    let has = |needles: &[&str]| needles.iter().any(|n| detail.contains(n));
    if has(&[
        "SEC_E_UNTRUSTED_ROOT",
        "CERT_E_UNTRUSTEDROOT",
        "CERT_E_CHAINING",
    ]) {
        Some(hints.pick(
            "this machine doesn't trust the server's certificate (typical for a self-signed one): \
             pass -C, or save it in the profile with: sqlh profiles add NAME -C",
            "this machine doesn't trust the server's certificate (typical for a self-signed one): \
             set trust_server_certificate = true in the profile",
        ))
    } else if has(&["SEC_E_WRONG_PRINCIPAL", "CERT_E_CN_NO_MATCH"]) {
        Some(hints.pick(
            "the server's certificate names a different host: connect by the name on the \
             certificate, or pass -C",
            "the server's certificate names a different host: connect by the name on the \
             certificate, or set trust_server_certificate = true in the profile",
        ))
    } else if has(&["SEC_E_CERT_EXPIRED", "CERT_E_EXPIRED"]) {
        Some(hints.pick(
            "the server's certificate has expired: pass -C to trust it anyway",
            "the server's certificate has expired: set trust_server_certificate = true in the \
             profile to trust it anyway",
        ))
    } else if has(&["Schannel", "SEC_E_", "TLS"]) {
        Some(hints.pick(
            "the TLS handshake failed: a self-signed certificate needs -C; an old server without \
             TLS 1.2 may need -No",
            "the TLS handshake failed: a self-signed certificate needs trust_server_certificate = \
             true in the profile; an old server without TLS 1.2 may need encrypt = \"optional\"",
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Backpressure, COUNT_STOP_UNCONFIRMED, Cancel, Cause, Counting, DbError, Drain, Hints,
        PACE_TICKS, TdsError, check, classify, connect_error, data_source, fake_server, filled,
        split_protocol, tcp_form, tls_hint, wait_for_output,
    };
    use mssql_tds::core::CancelHandle;
    use std::future::Future;
    use std::time::{Duration, Instant};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    // plan-history.md 2.15: the budget starts with the first hidden row, a timer ends it while the
    // server is still looking for the next row, and a result that ends first leaves nothing behind.
    #[test]
    fn the_count_budget_fires_from_the_first_hidden_row() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let mut drain = Drain::new(Some(Duration::from_millis(20)), None, &run);
            tokio::time::sleep(Duration::from_millis(40)).await;
            assert!(!drain.fired(), "the clock starts at the first hidden row");
            drain.counted_row();
            assert!(!drain.fired());
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(drain.fired(), "the timer fires with no further row");
            drain.reset();
        });
    }

    #[test]
    fn a_result_that_ends_in_time_cancels_nothing() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let mut drain = Drain::new(Some(Duration::from_millis(20)), None, &run);
            drain.counted_row();
            drain.reset();
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(!drain.fired(), "the timer ended with its result");
            // The next result starts a new clock.
            drain.counted_row();
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(drain.fired());
        });
    }

    #[test]
    fn rows_that_keep_coming_stop_on_time_too() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let mut drain = Drain::new(Some(Duration::from_millis(10)), None, &run);
            drain.counted_row();
            // No await, so the timer task never runs: the check in the row does the work.
            std::thread::sleep(Duration::from_millis(30));
            assert!(!drain.fired());
            drain.counted_row();
            assert!(drain.fired());
        });
    }

    #[test]
    fn exact_counting_has_no_budget_and_no_handle_of_its_own() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let mut drain = Drain::new(Counting::EXACT.budget, None, &run);
            drain.counted_row();
            assert!(drain.cause().is_none() && drain.timer.is_none() && drain.deadline().is_none());
        });
        let (bounded, exact) = (Counting::BOUNDED, Counting::EXACT);
        assert!(bounded.timeout_stops && bounded.budget.is_some());
        assert!(!exact.timeout_stops);
    }

    // Decision 45: a batch whose later statements a stop would cancel counts for longer; an
    // exact count stays exact, and what `-t` may stop is unchanged.
    #[test]
    fn a_batch_with_later_statements_counts_for_longer() {
        let longer = Counting::BOUNDED.with_later_statements();
        assert_eq!(longer.budget, Some(super::LATER_STATEMENTS_BUDGET));
        assert!(longer.timeout_stops);
        assert_eq!(Counting::EXACT.with_later_statements(), Counting::EXACT);
        let exact_stoppable = Counting {
            budget: None,
            timeout_stops: true,
        };
        assert_eq!(exact_stoppable.with_later_statements(), exact_stoppable);
    }

    /// Work of sqlh's own that waits for nothing: what a loop over rows already in the read
    /// buffer does between the driver's reads.
    fn busy(span: Duration) {
        let until = Instant::now() + span;
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }

    // R15, the deterministic regression: every "row" is ready at once and costs work of our
    // own, so nothing in the loop ever waits for the server, and the driver's own clock (which
    // adds up waiting only) would never run out. The batch's deadline still fires, from the
    // loop, and the loop stops soon after, not after all its rows. 200,000 rows of 100 µs is
    // 20 s of work against a 100 ms deadline.
    #[test]
    fn the_deadline_fires_in_a_loop_that_never_waits() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let deadline = Instant::now() + Duration::from_millis(100);
            let mut drain = Drain::new(None, Some(deadline), &run);
            let started = Instant::now();
            let mut rows = 0u32;
            while rows < 200_000 && drain.cause().is_none() {
                busy(Duration::from_micros(100));
                rows += 1;
                drain.pace.tick(1).await;
            }
            let late = started.elapsed().saturating_sub(Duration::from_millis(100));
            assert!(drain.deadline_fired() && !drain.fired());
            assert!(rows < 200_000, "the loop ran to the end");
            // One look comes every PACE_TICKS rows (25.6 ms of work here), or every slice.
            assert!(late < Duration::from_millis(250), "{late:?} late");
        });
    }

    // The clock check alone, with no chance for the timer to run (no await between the work
    // and the check): the deadline has passed, so the next look fires it.
    #[test]
    fn the_clock_check_fires_the_deadline_without_the_timer() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let deadline = Instant::now() + Duration::from_millis(10);
            let mut drain = Drain::new(None, Some(deadline), &run);
            busy(Duration::from_millis(30));
            assert!(drain.cause().is_none(), "the timer can't have run yet");
            // Less than a slice's worth of work does not look.
            drain.pace.tick(PACE_TICKS - 1).await;
            assert!(drain.cause().is_none());
            drain.pace.tick(1).await;
            assert!(drain.deadline_fired());
        });
    }

    // The loop hands the runtime back, so that a timer and the console listeners (other tasks
    // on the one thread) run while rows keep arriving. Here a task that would set a flag after
    // 5 ms can only do so if the loop yields.
    #[test]
    fn a_busy_loop_lets_other_tasks_run() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = std::sync::Arc::clone(&flag);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            let mut drain = Drain::new(None, None, &run);
            let started = Instant::now();
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                busy(Duration::from_micros(50));
                drain.pace.tick(1).await;
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "the loop starved the runtime"
                );
            }
            // Not at every row: the first yield waits for a slice of YIELD_EVERY.
            assert!(started.elapsed() < Duration::from_millis(200));
        });
    }

    // `-t 0` is no deadline at all, and the first stop to fire is the one on record.
    #[test]
    fn no_timeout_means_no_deadline_and_the_first_stop_wins() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let mut drain = Drain::new(None, None, &run);
            for _ in 0..10 {
                busy(Duration::from_millis(2));
                drain.pace.tick(PACE_TICKS).await;
            }
            assert!(drain.deadline().is_none() && drain.cause().is_none());
        });
        runtime().block_on(async {
            // The budget fires first; the deadline after it does not take its place. The two are
            // far apart: Windows timers fire on a tick of about 15.6 ms, and a loaded runner can
            // let two that are closer come due in the same tick (CI, 2026-10-27).
            let soon = Instant::now() + Duration::from_millis(200);
            let mut drain = Drain::new(Some(Duration::from_millis(5)), Some(soon), &run);
            drain.counted_row();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(drain.cause(), Some(Cause::Budget));
            assert!(drain.fired() && !drain.deadline_fired());
        });
        runtime().block_on(async {
            // The deadline fires first, and a count stopped by it later keeps that cause.
            let soon = Instant::now() + Duration::from_millis(5);
            let mut drain = Drain::new(Some(Duration::from_millis(200)), Some(soon), &run);
            drain.counted_row();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(drain.cause(), Some(Cause::Deadline));
            assert!(drain.deadline_fired() && !drain.fired());
        });
    }

    // What the pacing costs a loop of hidden rows, which does nothing else: nanoseconds per
    // row, to compare with the 100 to 500 ns that reading a narrow row takes (the numbers are
    // in docs/notes/drain-experiment.md). Run it with
    // `cargo test --release pacing_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn pacing_costs_next_to_nothing() {
        let run = CancelHandle::new();
        runtime().block_on(async {
            let deadline = Instant::now() + Duration::from_secs(3600);
            let mut drain = Drain::new(None, Some(deadline), &run);
            let rows = 200_000_000u64;
            let started = Instant::now();
            for _ in 0..rows {
                drain.pace.tick(1).await;
            }
            let each = started.elapsed().as_nanos() as f64 / rows as f64;
            eprintln!("pacing: {each:.2} ns per hidden row ({rows} rows)");
            assert!(each < 20.0, "{each} ns per row");
        });
    }

    /// A reader that never takes what is queued.
    struct Stuck;

    impl Backpressure for Stuck {
        fn is_full(&self) -> bool {
            true
        }
        fn room(&self) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<()>> + '_>> {
            Box::pin(std::future::pending())
        }
    }

    // R15: the wait for a reader is inside the deadline, and the user's cancel still comes
    // first.
    #[test]
    fn a_wait_for_the_reader_ends_at_the_deadline_and_a_cancel_wins() {
        runtime().block_on(async {
            let cancel = Cancel::new();
            let started = Instant::now();
            let deadline = Some(started + Duration::from_millis(40));
            let outcome = wait_for_output(&Stuck, &cancel, deadline).await;
            assert!(matches!(outcome, Err(DbError::Timeout(_))), "{outcome:?}");
            assert!(started.elapsed() >= Duration::from_millis(40));
            assert!(started.elapsed() < Duration::from_millis(500));

            // Both at once: the deadline has passed and the run is cancelled.
            let root = Cancel::new();
            let child = root.child();
            root.cancel();
            let past = Some(Instant::now());
            let outcome = wait_for_output(&Stuck, &child, past).await;
            assert!(matches!(outcome, Err(DbError::Cancelled(_))), "{outcome:?}");
        });
    }

    // plan-history.md 2.15: what a cancel is called depends only on what sqlh observed. A count
    // that sqlh itself stopped and the server never acknowledged (the driver marked the connection
    // dead) is its own error, which the run words (`run::db_failure`); the user's cancel, and
    // any other error, keep their words.
    #[test]
    fn a_count_stop_the_server_did_not_confirm_names_the_count() {
        let cancelled = || TdsError::OperationCancelledError("x".into());
        let words = |error: DbError| match error {
            DbError::Cancelled(words) => words,
            other => panic!("not a cancel: {other:?}"),
        };
        let usable = "the statement was cancelled; the connection stayed usable";
        let closed = "the server didn't confirm the cancel, so the connection was closed";
        // A usable connection says so, whoever cancelled.
        assert_eq!(words(classify(cancelled(), false, false)), usable);
        assert_eq!(words(classify(cancelled(), false, true)), usable);
        // A dead one: the user's cancel, and sqlh's own stop of the count.
        assert_eq!(words(classify(cancelled(), true, false)), closed);
        assert!(matches!(
            classify(cancelled(), true, true),
            DbError::CountStopUnconfirmed
        ));
        assert!(
            COUNT_STOP_UNCONFIRMED
                .starts_with("stopped counting the rows that are not shown, but the server didn't")
                && COUNT_STOP_UNCONFIRMED.contains("the connection was closed")
                && COUNT_STOP_UNCONFIRMED.ends_with("the rows shown are complete"),
            "{COUNT_STOP_UNCONFIRMED}"
        );
        // Not a cancel: unchanged.
        assert!(matches!(
            classify(TdsError::ConnectionClosed("x".into()), true, true),
            DbError::Lost(_)
        ));
    }

    // A count from the driver that doesn't fit the buffer it was given is an error, not a panic.
    #[test]
    fn a_read_count_past_the_buffer_is_an_error() {
        let chunk = [1u8, 2, 3, 4];
        assert_eq!(filled(&chunk, 0).unwrap(), &[] as &[u8]);
        assert_eq!(filled(&chunk, 3).unwrap(), &[1, 2, 3]);
        assert_eq!(filled(&chunk, 4).unwrap(), &chunk);
        let error = filled(&chunk, 5).unwrap_err().to_string();
        assert!(
            error.contains("a read of 5 bytes overran its buffer"),
            "{error}"
        );
    }

    #[test]
    fn remote_names_connect_over_tcp_only() {
        for (server, expected) in [
            ("db01", "tcp:db01"),
            (" db01.corp.example ", "tcp:db01.corp.example"),
            (r"db01\SALES", r"tcp:db01\SALES"),
            ("db01,14330", "tcp:db01,14330"),
            ("10.1.2.3", "tcp:10.1.2.3"),
            // A protocol is the user's choice (`check_server` has refused remote pipes).
            ("tcp:db01", "tcp:db01"),
            ("TCP:db01,1433", "TCP:db01,1433"),
            ("np:db01", "np:db01"),
            ("admin:db01", "admin:db01"),
            // Local names keep the driver's order (shared memory, TCP, a local pipe).
            (".", "."),
            (r".\SQLEXPRESS", r".\SQLEXPRESS"),
            ("(local)", "(local)"),
            ("LocalHost", "LocalHost"),
            ("localhost,1433", "localhost,1433"),
            ("127.0.0.1", "127.0.0.1"),
            ("::1", "::1"),
            (r"(localdb)\MSSQLLocalDB", r"(localdb)\MSSQLLocalDB"),
            // IPv6 takes `tcp:` like a name, so an instance stays on TCP too, and a port is
            // never added (driver-0.4.0.md §1.2).
            ("2001:db8::5", "tcp:2001:db8::5"),
            (" fe80::1%12 ", "tcp:fe80::1%12"),
            ("2001:db8::5,1433", "tcp:2001:db8::5,1433"),
            (r"2001:db8::5\SQLEXPRESS", r"tcp:2001:db8::5\SQLEXPRESS"),
            ("tcp:2001:db8::5", "tcp:2001:db8::5"),
            // Other protocols before an address pass as written: no port, no second prefix.
            ("admin:fe80::1", "admin:fe80::1"),
            ("lpc:::1", "lpc:::1"),
            ("np:::1", "np:::1"),
            // A pipe path names its protocol; other text with a colon is left to the driver.
            (r"\\db01\pipe\sql\query", r"\\db01\pipe\sql\query"),
            ("db01:1433", "db01:1433"),
        ] {
            assert_eq!(data_source(server), expected, "{server}");
        }
    }

    // The driver's own parse_protocol examples, plus the spellings sqlh has to agree with.
    #[test]
    fn protocol_prefixes_are_split_like_the_driver_does() {
        for (server, protocol, rest) in [
            ("tcp:myserver,1433", Some("tcp"), "myserver,1433"),
            ("NP:db01", Some("np"), "db01"),
            ("Lpc: .", Some("lpc"), "."),
            ("admin:db01", Some("admin"), "db01"),
            ("np :db01", Some("np"), "db01"),
            (
                r"np:\\db01\pipe\sql\query",
                Some("np"),
                r"\\db01\pipe\sql\query",
            ),
            // No protocol, or not one of the four.
            ("db01", None, "db01"),
            (r"myserver\instance", None, r"myserver\instance"),
            ("http:myserver", None, "http:myserver"),
            (":db01", None, ":db01"),
            (r"\\db01\pipe\sql\query", None, r"\\db01\pipe\sql\query"),
            // An address's first group is no protocol, but a protocol before an address is
            // split off whatever follows (0.4.0's rule; 0.3.0 split nothing from these).
            ("::1", None, "::1"),
            ("2001:db8::1,1433", None, "2001:db8::1,1433"),
            ("np:::1", Some("np"), "::1"),
            (
                "tcp:2001:db8:0:0:0:0:0:5",
                Some("tcp"),
                "2001:db8:0:0:0:0:0:5",
            ),
            (r"np:2001:db8::5\SALES", Some("np"), r"2001:db8::5\SALES"),
            ("ADMIN:fe80::1", Some("admin"), "fe80::1"),
        ] {
            assert_eq!(split_protocol(server), (protocol, rest), "{server}");
        }
    }

    // DESIGN.md §4.2: a Named Pipes request to another machine is refused, with the TCP form
    // of the same server in the message.
    #[test]
    fn remote_named_pipes_are_refused() {
        for (server, tcp) in [
            ("np:db01", "db01"),
            (" NP:db01 ", "db01"),
            ("np: db01.corp.example", "db01.corp.example"),
            ("np:192.0.2.1", "192.0.2.1"),
            (r"np:db01\SALES", r"db01\SALES"),
            ("np:db01,1433", "db01,1433"),
            // This computer's name only counts when it is the computer's own.
            ("np:buildbox2", "buildbox2"),
            // A UNC pipe path, with or without a prefix. The driver reads it as a pipe even
            // after `tcp:`.
            (r"\\db01\pipe\sql\query", "db01"),
            (r"np:\\db01\pipe\sql\query", "db01"),
            (r"tcp:\\db01\pipe\sql\query", "db01"),
            (r"lpc:\\db01\pipe\sql\query", "db01"),
            (r"\\192.0.2.1\pipe\sql\query", "192.0.2.1"),
            (r"\\db01\pipe\MSSQL$SALES\sql\query", r"db01\SALES"),
            (r"\\DB01\PIPE\mssql$Sales\SQL\QUERY", r"DB01\Sales"),
            (r"\\db01\pipe\custom\app", "db01"),
            (r"\\db01", "db01"),
            // An IPv6 address after `np:`, or in a UNC path after any prefix: 0.4.0 opens a pipe
            // to it, where 0.3.0 read the whole text as a name (driver-0.4.0.md §1.2).
            ("np:2001:db8::5", "2001:db8::5"),
            (r"np:2001:db8::5\SALES", r"2001:db8::5\SALES"),
            ("np:fe80::1%12,1433", "fe80::1%12,1433"),
            (r"np:\\2001:db8::5\pipe\sql\query", "2001:db8::5"),
            (r"tcp:\\fe80::1\pipe\sql\query", "fe80::1"),
            (
                r"\\2001:db8::5\pipe\MSSQL$SALES\sql\query",
                r"2001:db8::5\SALES",
            ),
        ] {
            let message = check(server, Some("BUILDBOX")).unwrap_err();
            assert!(
                message.starts_with(&format!(
                    "{}: Named Pipes to a remote server",
                    server.trim()
                )) && message.ends_with(&format!("connect over TCP: -S {tcp}")),
                "{server}: {message}"
            );
        }
        // Without a computer name nothing counts as the computer's own.
        assert!(check("np:buildbox", None).is_err());
        assert_eq!(
            check("np:db01", Some("BUILDBOX")).unwrap_err(),
            "np:db01: Named Pipes to a remote server aren't supported, because their open \
             can't be timed out or cancelled; connect over TCP: -S db01"
        );
    }

    // Decision 73: this computer's name is the driver's, the one its pipe open takes as local,
    // not the environment's `COMPUTERNAME`; where the two differ (a DNS host name longer than 15
    // characters, or an edited environment), `np:` with the environment's is refused.
    #[test]
    fn this_computers_name_is_the_one_the_driver_takes_as_local() {
        let name = mssql_tds::connection::this_computer_name().expect("a computer name");
        assert_eq!(super::this_computer().as_deref(), Some(name.as_str()));
        assert_eq!(super::check_server(&format!("np:{name}")), Ok(()));
        assert_eq!(
            super::check_server(&format!("np:{}", name.to_lowercase())),
            Ok(())
        );
        assert_eq!(
            super::check_server(&format!(r"np:{name}\SQLEXPRESS")),
            Ok(())
        );
        let environment = std::env::var("COMPUTERNAME").unwrap_or_default();
        if !environment.is_empty() && !environment.eq_ignore_ascii_case(&name) {
            assert!(super::check_server(&format!("np:{environment}")).is_err());
        }
    }

    #[test]
    fn local_pipes_and_other_servers_are_accepted() {
        for server in [
            // Names with no pipe in them.
            "db01",
            r"db01\SALES",
            "db01,1433",
            "tcp:db01",
            "TCP:db01,1433",
            "admin:db01",
            "2001:db8::5",
            "2001:db8::5,1433",
            r"2001:db8::5\SALES",
            "tcp:2001:db8::5",
            "admin:fe80::1",
            "db01:1433",
            // Local pipes, by every name for this computer.
            "np:.",
            r"np:.\SQLEXPRESS",
            "np:(local)",
            "NP:LocalHost",
            "np:127.0.0.1",
            "np:::1",
            "np:buildbox",
            r"np:BUILDBOX\SQLEXPRESS",
            r"\\.\pipe\sql\query",
            r"\\.\pipe\MSSQL$SQLEXPRESS\sql\query",
            r"\\localhost\pipe\sql\query",
            r"\\127.0.0.1\pipe\sql\query",
            r"\\::1\pipe\sql\query",
            r"\\BUILDBOX\pipe\sql\query",
            r"np:\\.\pipe\sql\query",
            r"tcp:\\.\pipe\sql\query",
            // Shared memory is local-only (the driver refuses a remote name itself), and
            // LocalDB resolves to a local pipe.
            "lpc:.",
            "lpc:db01",
            r"(localdb)\MSSQLLocalDB",
            "(LocalDB)/MSSQLLocalDB",
            // Nothing remote in it: the driver fails on these before it opens anything.
            "np:",
            r"np:\SALES",
            "np:,1433",
            r"\\",
            r"\\\pipe\sql\query",
            "",
        ] {
            assert_eq!(check(server, Some("BUILDBOX")), Ok(()), "{server:?}");
        }
    }

    #[test]
    fn a_pipe_path_has_a_tcp_spelling() {
        for (unc, expected) in [
            (r"db01\pipe\sql\query", "db01"),
            (r"db01\pipe\MSSQL$SALES\sql\query", r"db01\SALES"),
            (r"db01\pipe\mssql$sales\sql\query", r"db01\sales"),
            // Not the standard pipe of an instance: the instance isn't known.
            (r"db01\pipe\MSSQL$\sql\query", "db01"),
            (r"db01\pipe\MSSQL$SALES\other", "db01"),
            ("db01", "db01"),
        ] {
            assert_eq!(tcp_form(unc), expected, "{unc}");
        }
    }

    #[test]
    fn tls_hints_from_schannel_status() {
        // mssql-tds 0.4.0's text (`TlsHandshakeError`), and 0.3.0's (driver-0.4.0.md §1.3).
        let untrusted = "TLS handshake failed for 'db01': InitializeSecurityContextW failed: \
                         SEC_E_UNTRUSTED_ROOT (status=0x80090325)";
        let untrusted_030 = "Unexpected Implementation Error: Schannel TLS handshake failed: \
                             InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT \
                             (status=0x80090325)";
        let flags = |detail| tls_hint(detail, Hints::Flags);
        assert!(flags(untrusted).unwrap().contains("pass -C"));
        assert!(flags(untrusted_030).unwrap().contains("pass -C"));
        assert!(
            flags("SEC_E_WRONG_PRINCIPAL")
                .unwrap()
                .contains("different host")
        );
        assert!(flags("SEC_E_ILLEGAL_MESSAGE").unwrap().contains("-No"));
        assert_eq!(flags("Connection refused"), None);
        // sqlh mcp connects only through profiles: its hints name the profile's keys, no flag.
        for detail in [
            untrusted,
            "SEC_E_WRONG_PRINCIPAL",
            "SEC_E_CERT_EXPIRED",
            "SEC_E_ILLEGAL_MESSAGE",
        ] {
            let hint = tls_hint(detail, Hints::Profile).unwrap();
            assert!(
                hint.contains("trust_server_certificate = true") && !hint.contains(" -"),
                "{hint}"
            );
        }
        assert!(
            tls_hint("SEC_E_ILLEGAL_MESSAGE", Hints::Profile)
                .unwrap()
                .contains("encrypt = \"optional\"")
        );
    }

    // The hints of a failed connect, in sqlh mcp, name no command-line flag (`Hints`).
    #[test]
    fn connect_hints_in_mcp_name_the_profile() {
        let mut options = fake_server::options("db01".into());
        options.hints = Hints::Profile;
        let words = |error: TdsError| match connect_error(error, &options) {
            DbError::Connect(words) | DbError::Timeout(words) => words,
            other => panic!("{other:?}"),
        };
        let timeout = words(TdsError::TimeoutError(
            mssql_tds::error::TimeoutErrorType::String("x".into()),
        ));
        assert!(
            timeout.ends_with("or raise the profile's login_timeout"),
            "{timeout}"
        );
        for hint in [
            words(TdsError::CertificateExpired),
            words(TdsError::Security(
                mssql_tds::security::SecurityError::LoadLibraryFailed("x".into()),
            )),
            words(TdsError::NoServerCertificate),
        ] {
            assert!(!hint.contains(" -C") && !hint.contains(" -U"), "{hint}");
        }
    }

    // R46: strict encryption checks the certificate even with -C, so its certificate failures
    // never suggest trusting it; the same failures under mandatory still do.
    #[test]
    fn a_certificate_failure_under_strict_encryption_offers_no_trust() {
        let mut options = fake_server::options("db01".into());
        let words = |options: &super::ConnectOptions, error: TdsError| match connect_error(
            error, options,
        ) {
            DbError::Connect(words) => words,
            other => panic!("{other:?}"),
        };
        // The direct Schannel path's form in mssql-tds 0.4.0; its status name is matched by its
        // text (`tls_hint`).
        let untrusted = || TdsError::TlsHandshakeError {
            source: Box::new(std::io::Error::other(
                "InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)",
            )),
            expected_host: "db01".into(),
        };
        // 0.3.0's form, a generic error that only the text marks as a certificate failure.
        let untrusted_030 = || {
            TdsError::ImplementationError(
                "Schannel TLS handshake failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)".into(),
            )
        };
        options.encrypt = super::Encrypt::Strict;
        options.trust_server_certificate = true;
        for hints in [Hints::Flags, Hints::Profile] {
            options.hints = hints;
            for error in [
                untrusted(),
                untrusted_030(),
                TdsError::CertificateMismatch,
                TdsError::NoServerCertificate,
            ] {
                let hint = words(&options, error);
                assert!(
                    hint.contains("strict encryption checks the server's certificate even with")
                        && !hint.contains("pass -C")
                        && !hint.contains("= true in the profile to")
                        && !hint.contains("needs trust_server_certificate"),
                    "{hint}"
                );
            }
        }
        assert!(words(&options, TdsError::CertificateExpired).ends_with("where trust applies"));
        options.hints = Hints::Flags;
        assert!(words(&options, untrusted()).ends_with("or use -Nm, where -C applies"));
        options.encrypt = super::Encrypt::Mandatory;
        assert!(words(&options, untrusted()).contains("pass -C"));
        assert!(words(&options, untrusted_030()).contains("pass -C"));
        assert!(
            words(&options, untrusted()).contains(
                "): TLS handshake failed for 'db01': InitializeSecurityContextW failed: \
                 SEC_E_UNTRUSTED_ROOT (status=0x80090325); this machine doesn't trust"
            ),
            "{}",
            words(&options, untrusted())
        );
        // A failure that isn't the certificate's keeps its own hint under strict.
        options.encrypt = super::Encrypt::Strict;
        assert!(words(&options, TdsError::ConnectionClosed("x".into())).ends_with("try host,port"));
    }

    // AppVeyor's Windows Server 2016 job (U12): Schannel's credentials fail on this machine
    // before anything reaches the server, so in every mode and form the hint names the Windows
    // the driver's TLS needs, never -C, -No or the certificate.
    #[test]
    fn a_tls_setup_failure_on_this_machine_names_the_windows_it_needs() {
        let mut options = fake_server::options("db01".into());
        let setup = || TdsError::TlsHandshakeError {
            source: Box::new(std::io::Error::other(
                "AcquireCredentialsHandleW(UNISP_NAME) failed: unknown SECURITY_STATUS \
                 (status=0x8009030D)",
            )),
            expected_host: "localhost".into(),
        };
        for encrypt in [
            super::Encrypt::Optional,
            super::Encrypt::Mandatory,
            super::Encrypt::Strict,
        ] {
            for hints in [Hints::Flags, Hints::Profile] {
                options.encrypt = encrypt;
                options.hints = hints;
                let DbError::Connect(words) = connect_error(setup(), &options) else {
                    panic!("not a connect failure")
                };
                assert!(
                    words.contains(
                        "(status=0x8009030D); this machine couldn't set up TLS, before anything \
                         reached the server: the driver's TLS needs Windows 10 1809 or Windows \
                         Server 2019 or later (on a later Windows, check that this machine's \
                         Schannel settings allow TLS 1.2)"
                    ) && words.ends_with(')')
                        && !words.contains(" -")
                        && !words.contains("trust_server_certificate")
                        && !words.contains("certificate needs"),
                    "{words}"
                );
            }
        }
    }

    // Found in review (2026-10-31): under -M, mssql-tds 0.3.0's race of every address ends at
    // -l with "Connection timeout: ..."; that is no address answering, a connect failure (exit
    // 2) worded for it, not a login that missed its -l + 1 deadline (exit 4). The same error
    // without -M, and the login deadline's own error under -M, keep the login wording.
    #[test]
    fn a_failover_race_that_times_out_is_a_connect_failure() {
        let race = || {
            TdsError::TimeoutError(mssql_tds::error::TimeoutErrorType::String(
                "Connection timeout: Connection attempt timed out".into(),
            ))
        };
        let mut options = fake_server::options("192.0.2.1".into());
        options.login_timeout = 2;
        options.multi_subnet_failover = true;
        match connect_error(race(), &options) {
            DbError::Connect(words) => assert_eq!(
                words,
                "cannot connect to 192.0.2.1 (SQL login 'fake', encrypt optional): no address \
                 accepted a TCP connection within 2 s (-M tries them all at once); check the \
                 server name and port, or raise -l"
            ),
            other => panic!("{other:?}"),
        }
        let login_deadline = TdsError::TimeoutError(mssql_tds::error::TimeoutErrorType::String(
            "login timed out".into(),
        ));
        assert!(matches!(
            connect_error(login_deadline, &options),
            DbError::Timeout(words) if words.contains("didn't complete the login within 3 s")
        ));
        options.multi_subnet_failover = false;
        assert!(matches!(
            connect_error(race(), &options),
            DbError::Timeout(_)
        ));
    }
}

/// `Session::run` against a fake server on a loopback socket: a TDS 7.4 endpoint that answers
/// the prelogin (no encryption) and the login, then plays a script, so the real execution path
/// (driver included) meets a deadline at exactly the points a live server can't be made to
/// hit on demand (R15 follow-ups A and B). Adapted from the reviewer's probe. `serve` plays one
/// connection and one batch; `serve_pool` plays a pooled session, connection after connection
/// and request after request, for the engine's tests (`engine::tests`).
#[cfg(test)]
pub(crate) mod fake_server {
    use super::{
        Auth, Backpressure, BatchSink, Cancel, Column, ConnectOptions, Counting, DbError, Encrypt,
        RETIRED, ServerMessage, Session, Stop, Value,
    };
    use std::future::Future;
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// The status byte of a packet's header (MS-TDS 2.2.3.1.2): the message's last packet, and
    /// the reset a client asks for before the server processes the message, which only the
    /// first packet of a SQL batch (or an RPC) carries.
    const EOM: u8 = 0x01;
    const RESET: u8 = 0x08;
    const RESET_SKIP_TRAN: u8 = 0x10;

    /// The most of a message's payload `Message` keeps: more than any SQL a test reads back, and
    /// not all 8 MB of `long_sql`.
    const KEEP: usize = 64 << 10;

    /// One whole message from the client.
    struct Message {
        /// 0x12 prelogin, 0x10 login, 1 SQL batch, 6 attention.
        kind: u8,
        /// The first packet's status byte.
        status: u8,
        /// The payload of all its packets, up to `KEEP` bytes.
        payload: Vec<u8>,
    }

    impl Message {
        /// Whether the client asked for the session to be reset first, its transaction rolled
        /// back (the RESETCONNECTION bit, which `Session::reset_before_next` sets).
        fn reset(&self) -> bool {
            self.status & RESET != 0
        }

        /// Whether the client asked for a reset that keeps the transaction
        /// (RESETCONNECTIONSKIPTRAN), which sqlh never does.
        fn reset_keeping_transaction(&self) -> bool {
            self.status & RESET_SKIP_TRAN != 0
        }

        /// A SQL batch's text. Its payload is ALL_HEADERS, a u32 LE total length (which counts
        /// itself) and the headers (the driver sends one, the transaction descriptor: `u32 18 |
        /// u16 2 | u64 descriptor | u32 1`), then the SQL as UTF-16LE.
        fn sql(&self) -> String {
            let headers = match self.payload.get(..4) {
                Some(&[a, b, c, d]) => u32::from_le_bytes([a, b, c, d]) as usize,
                _ => 0,
            };
            let units: Vec<u16> = self
                .payload
                .get(headers..)
                .unwrap_or_default()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect();
            String::from_utf16_lossy(&units)
        }
    }

    /// The next whole message from the client, read packet by packet to the one marked EOM
    /// (`None` once the client is gone). A packet's header is `type | status | length u16 BE,
    /// the header's 8 bytes included | spid u16 | packet id | window`.
    fn receive_message(s: &mut TcpStream) -> Option<Message> {
        let mut message: Option<Message> = None;
        loop {
            let mut header = [0u8; 8];
            s.read_exact(&mut header).ok()?;
            let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
            let mut part = vec![0; length.checked_sub(8)?];
            s.read_exact(&mut part).ok()?;
            let whole = message.get_or_insert_with(|| Message {
                kind: header[0],
                status: header[1],
                payload: Vec::new(),
            });
            let room = KEEP.saturating_sub(whole.payload.len());
            whole.payload.extend(part.iter().take(room));
            if header[1] & EOM != 0 {
                return message;
            }
        }
    }

    /// The type of the next whole message from the client (see `receive_message`).
    fn receive(s: &mut TcpStream) -> Option<u8> {
        receive_message(s).map(|message| message.kind)
    }

    fn send(s: &mut TcpStream, payload: &[u8]) {
        send_packet(s, payload, true);
    }

    /// One packet of a server message; `last` marks the end of the message (EOM).
    fn send_packet(s: &mut TcpStream, payload: &[u8], last: bool) {
        write_packet(s, payload, last).unwrap();
    }

    /// `send_packet`, failing when the client has gone (the pooled fake's connections outlive
    /// the calls that use them). The header is type 4 (a reply), the status (EOM or not), the
    /// length, spid 0, packet id 1, window 0.
    fn write_packet(s: &mut TcpStream, payload: &[u8], last: bool) -> io::Result<()> {
        let length = u16::try_from(payload.len() + 8).map_err(io::Error::other)?;
        let [high, low] = length.to_be_bytes();
        s.write_all(&[4, u8::from(last), high, low, 0, 0, 1, 0])?;
        s.write_all(payload)
    }

    /// DONE: `FD | status u16 LE | curcmd u16 LE | row count u64 LE`. Status bits: 0x01 MORE,
    /// 0x02 ERROR, 0x04 INXACT, 0x10 COUNT (the row count is valid), 0x20 ATTN.
    fn done_token(status: u16, curcmd: u16, rows: u64) -> Vec<u8> {
        let mut token = vec![0xfd];
        token.extend(status.to_le_bytes());
        token.extend(curcmd.to_le_bytes());
        token.extend(rows.to_le_bytes());
        token
    }

    /// DONE, final, with a row count, after a SELECT (curcmd 0xC1).
    fn done(rows: u64) -> Vec<u8> {
        done_token(0x10, 0xc1, rows)
    }

    /// DONE, final, with no row count: what a `SET`, `USE` or `BEGIN TRANSACTION` ends with.
    fn done_empty() -> Vec<u8> {
        done_token(0, 0, 0)
    }

    /// DONE with ATTN and MORE clear: the server's acknowledgement of an attention, the only
    /// form the driver takes as one.
    fn done_attn() -> Vec<u8> {
        done_token(0x20, 0, 0)
    }

    /// ENVCHANGE ResetConnection (subtype 18), the acknowledgement of a reset: `E3 | length
    /// u16 LE, 3 | 18 | new value, an empty B_VARBYTE (0) | old value, empty (0)`.
    fn env_reset() -> Vec<u8> {
        vec![0xe3, 3, 0, 18, 0, 0]
    }

    /// ENVCHANGE BeginTransaction (subtype 8): `E3 | 11 0 | 8 | new value, 8 bytes: the
    /// descriptor u64 LE | old value, empty (0)`. The driver keeps the descriptor, which is
    /// what `Session::in_transaction` reads.
    fn env_begin(descriptor: u64) -> Vec<u8> {
        let mut token = vec![0xe3, 11, 0, 8, 8];
        token.extend(descriptor.to_le_bytes());
        token.push(0);
        token
    }

    /// ENVCHANGE RollbackTransaction (subtype 10): `E3 | 11 0 | 10 | new value, empty (0) |
    /// old value, 8 bytes: the descriptor u64 LE`. The driver clears its descriptor.
    fn env_rollback(descriptor: u64) -> Vec<u8> {
        let mut token = vec![0xe3, 11, 0, 10, 0, 8];
        token.extend(descriptor.to_le_bytes());
        token
    }

    /// COLMETADATA for one `int` column named x (`81 | column count u16 LE | user type u32 |
    /// flags u16 | type 0x38, INT4 | name B_VARCHAR, UTF-16`), then `rows` ROWs (`D1 | the int
    /// 1, LE`), and no DONE: a result still being produced.
    fn int_rows_open(rows: u64) -> Vec<u8> {
        let mut reply = vec![0x81, 1, 0, 0, 0, 0, 0, 0, 0, 0x38, 1, b'x', 0];
        for _ in 0..rows {
            reply.extend([0xd1, 1, 0, 0, 0]);
        }
        reply
    }

    /// One `int` column named x, `rows` rows of it, and the final DONE.
    pub(crate) fn int_rows(rows: u64) -> Vec<u8> {
        let mut reply = int_rows_open(rows);
        reply.extend(done(rows));
        reply
    }

    /// The login's answer: LOGINACK, the packet size, database and collation changes, DONE.
    fn login_reply() -> Vec<u8> {
        let name: Vec<u8> = "fake".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut ack = vec![1, 0x74, 0, 0, 4, 4];
        ack.extend(name);
        ack.extend([17, 0, 0, 1]);
        let mut reply = vec![0xad];
        reply.extend(u16::try_from(ack.len()).unwrap().to_le_bytes());
        reply.extend(ack);
        reply.extend([0xe3, 8, 0, 7, 5, 9, 4, 0xd0, 0, 0x34, 0]);
        let master: Vec<u8> = "master".encode_utf16().flat_map(u16::to_le_bytes).collect();
        reply.extend([0xe3, 15, 0, 1, 6]);
        reply.extend(master);
        reply.push(0);
        let size: Vec<u8> = "4096".encode_utf16().flat_map(u16::to_le_bytes).collect();
        reply.extend([0xe3, 19, 0, 4, 4]);
        reply.extend(&size);
        reply.push(4);
        reply.extend(&size);
        reply.extend(done(0));
        reply
    }

    /// What the fake server does with the batch `Session::run` sends.
    pub(crate) enum Script {
        /// Answer it at once with these tokens.
        Reply(Vec<u8>),
        /// Read it, and everything after it (the attention), and never answer.
        Silent,
        /// Answer it with an `int` column and `rows` rows but no DONE, as a server does while
        /// it is still producing a result; then read on (the attention) and never answer again.
        RowsThenSilent { rows: u64 },
        /// Read `bytes` of it, then stop reading altogether and never answer, so the client's
        /// writes block part way through a request of several packets.
        StopReading { bytes: usize },
    }

    /// The prelogin's answer: the VERSION option (offset 11, 6 bytes) and ENCRYPTION (offset
    /// 17, 1 byte), the terminator, then the version 17.0.0.0 and ENCRYPT_NOT_SUP (2), so that
    /// with `Encrypt::Optional` the connection has no TLS.
    const PRELOGIN_REPLY: [u8; 18] = [0, 0, 11, 0, 6, 1, 0, 17, 0, 1, 0xff, 17, 0, 0, 0, 0, 0, 2];

    /// Answers the prelogin and the login; false when the client went away or sent something
    /// else first.
    fn handshake(s: &mut TcpStream) -> bool {
        receive(s) == Some(0x12)
            && write_packet(s, &PRELOGIN_REPLY, true).is_ok()
            && receive(s) == Some(0x10)
            && write_packet(s, &login_reply(), true).is_ok()
    }

    /// Starts the fake server; returns its address for `-S`.
    pub(crate) fn serve(script: Script) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            if !handshake(&mut s) {
                return;
            }
            match script {
                Script::Reply(tokens) => {
                    if receive(&mut s) == Some(1) {
                        send(&mut s, &tokens);
                    }
                    while receive(&mut s).is_some() {}
                }
                Script::Silent => while receive(&mut s).is_some() {},
                Script::RowsThenSilent { rows } => {
                    if receive(&mut s) == Some(1) {
                        let reply = int_rows_open(rows);
                        // In packets of at most 4,096 bytes, as negotiated, none of them the
                        // message's last: the result is still being produced.
                        for chunk in reply.chunks(4088) {
                            send_packet(&mut s, chunk, false);
                        }
                    }
                    while receive(&mut s).is_some() {}
                }
                Script::StopReading { bytes } => {
                    let mut taken = vec![0; bytes];
                    let _ = s.read_exact(&mut taken);
                    // Hold the socket open, unread, for longer than any test waits.
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
        });
        format!("127.0.0.1,{port}")
    }

    // A pooled session (plan-history.md 5.2, the engine's tests): `serve_pool` accepts any number
    // of connections and answers request after request on each, the way SQL Server answers the
    // batches sqlh sends (`answer`), with the reset, the wrapper transaction and attentions
    // handled, and with the faults a pool has to survive played where a test asks (`Conn`).

    /// What one connection does with one of its SQL batches.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Turn {
        /// Answer it as the server would (`answer`).
        Answer,
        /// Answer it after this long: a slow query, which holds its connection meanwhile.
        Delay(Duration),
        /// Send a result's first rows and no end, as a query that is still running does, then
        /// wait for the attention: acknowledge it with DONE ATTN (`ack`), which settles the
        /// cancel and keeps the connection in step, or never answer anything again (the driver
        /// waits 5 s for the acknowledgement, then marks the connection dead).
        Hang { ack: bool },
        /// Never answer it, nor anything after it, the attention included: a server that has
        /// stopped responding.
        Silent,
        /// Send a result's first two rows and no end, wait this long, then end it and send a
        /// second result of one row: a slow result with more output after it in the batch, as
        /// `SET STATISTICS XML` or `DBCC SHOW_STATISTICS` give (R48). An attention during the
        /// wait is acknowledged (DONE ATTN) and the rest is never sent, as a cancelled batch's
        /// isn't.
        Pause(Duration),
        /// Send a result's first rows, then close the connection: a server lost mid-reply.
        Drop,
    }

    /// The script of one connection of `serve_pool`: `Turn::Answer` for every SQL batch but
    /// the ones it names, every reset acknowledged, and open until the client closes it.
    #[derive(Clone, Debug, Default)]
    pub(crate) struct Conn {
        turns: Vec<(usize, Turn)>,
        close_after: Option<usize>,
        no_reset_ack: bool,
        refuse_login: bool,
    }

    impl Conn {
        pub(crate) fn new() -> Conn {
            Conn::default()
        }

        /// Plays `turn` for the connection's `request`th SQL batch, counted from 1. A call on a
        /// new connection sends three: the session setup, its SQL, the wrapper's rollback; a
        /// call on a reused one the same, the setup carrying the reset.
        pub(crate) fn at(mut self, request: usize, turn: Turn) -> Conn {
            self.turns.push((request, turn));
            self
        }

        /// Closes the connection once `requests` SQL batches are answered, without reading
        /// another: a connection dropped while it sat idle (the network, a DBA's `KILL`), which
        /// the client only finds at its next request.
        pub(crate) fn close_after(mut self, requests: usize) -> Conn {
            self.close_after = Some(requests);
            self
        }

        /// Answers a request that asks for a reset without acknowledging it (no ENVCHANGE 18),
        /// as a server that didn't reset the session would.
        pub(crate) fn without_reset_ack(mut self) -> Conn {
            self.no_reset_ack = true;
            self
        }

        /// Refuses the login as SQL Server refuses a wrong password (`login_refused`: error
        /// 18456), then closes the connection: a login that fails, such as a pool's replacement
        /// connection can meet.
        pub(crate) fn refusing_login(mut self) -> Conn {
            self.refuse_login = true;
            self
        }

        fn turn(&self, request: usize) -> Turn {
            self.turns
                .iter()
                .find(|(number, _)| *number == request)
                .map_or(Turn::Answer, |&(_, turn)| turn)
        }
    }

    /// A SQL batch the pooled fake received.
    #[derive(Clone, Debug)]
    pub(crate) struct Seen {
        /// The connection, numbered from 0 in the order they were accepted.
        pub(crate) conn: usize,
        /// Whether its first packet carried the RESETCONNECTION bit (0x08).
        pub(crate) reset: bool,
        pub(crate) sql: String,
    }

    /// How a connection of the pooled fake ended.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum End {
        /// The client closed it (or went away mid-message).
        Client,
        /// The script closed it (`Conn::close_after`, `Turn::Drop`).
        Server,
    }

    /// What the pooled fake has seen, shared by its threads and the test.
    #[derive(Default)]
    struct Log {
        accepted: usize,
        requests: Vec<Seen>,
        /// For each connection, the descriptor of its open transaction; 0 when none is.
        transactions: Vec<u64>,
        /// For each connection, whether `SET NOEXEC ON` is in force (`answer`).
        noexec: Vec<bool>,
        ended: Vec<(usize, End)>,
    }

    /// The pooled fake: its address, and what it has seen so far.
    pub(crate) struct Fake {
        /// The address for `-S` (`ConnectOptions::server`).
        pub(crate) server: String,
        log: Arc<Mutex<Log>>,
    }

    impl Fake {
        fn log(&self) -> std::sync::MutexGuard<'_, Log> {
            self.log.lock().unwrap()
        }

        /// How many connections the server accepted.
        pub(crate) fn accepted(&self) -> usize {
            self.log().accepted
        }

        /// The SQL batches connection `conn` received, in order.
        pub(crate) fn requests(&self, conn: usize) -> Vec<Seen> {
            let log = self.log();
            log.requests
                .iter()
                .filter(|seen| seen.conn == conn)
                .cloned()
                .collect()
        }

        /// Which of connection `conn`'s SQL batches carried the reset bit, in order.
        pub(crate) fn resets(&self, conn: usize) -> Vec<bool> {
            self.requests(conn).iter().map(|seen| seen.reset).collect()
        }

        /// Connection `conn`'s SQL batches, in order.
        pub(crate) fn sql(&self, conn: usize) -> Vec<String> {
            self.requests(conn)
                .into_iter()
                .map(|seen| seen.sql)
                .collect()
        }

        /// Whether connection `conn` has a transaction open, as the server sees it.
        pub(crate) fn in_transaction(&self, conn: usize) -> bool {
            self.log().transactions.get(conn).is_some_and(|&d| d != 0)
        }

        /// How connection `conn` ended, if it has.
        pub(crate) fn ended(&self, conn: usize) -> Option<End> {
            let log = self.log();
            log.ended
                .iter()
                .find(|(number, _)| *number == conn)
                .map(|&(_, end)| end)
        }

        /// Waits until `ready` holds of the fake, at most `within`; whether it came to hold.
        /// The fake's threads run on their own, so a test that needs one of them to have got
        /// somewhere (a connection closed) waits for it here, on the runtime's clock.
        pub(crate) async fn wait_for(
            &self,
            within: Duration,
            ready: impl Fn(&Fake) -> bool,
        ) -> bool {
            let until = Instant::now() + within;
            while !ready(self) {
                if Instant::now() >= until {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            true
        }
    }

    /// Starts a fake server that accepts any number of connections, each on its own thread, and
    /// plays `conns[n]` on the `n`th (from 0), `Conn::new()` past the end of the list. The
    /// listener's thread waits in `accept` for as long as the test process lives.
    pub(crate) fn serve_pool(conns: Vec<Conn>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Log::default()));
        let shared = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let conn = {
                    let mut log = shared.lock().unwrap();
                    log.accepted += 1;
                    log.transactions.push(0);
                    log.noexec.push(false);
                    log.accepted - 1
                };
                let script = conns.get(conn).cloned().unwrap_or_default();
                let log = Arc::clone(&shared);
                std::thread::spawn(move || {
                    let end = play(stream, conn, &script, &log);
                    log.lock().unwrap().ended.push((conn, end));
                });
            }
        });
        Fake {
            server: format!("127.0.0.1,{port}"),
            log,
        }
    }

    /// The answer to a login SQL Server refuses: ERROR (`AA | length u16 LE | number i32 LE |
    /// state | class | message US_VARCHAR | server B_VARCHAR | procedure B_VARCHAR | line i32
    /// LE`, the lengths in characters), error 18456 of class 14, and a final DONE with the error
    /// bit (0x02).
    fn login_refused() -> Vec<u8> {
        let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_le_bytes).collect() };
        let message = "Login failed for user 'fake'.";
        let mut body = Vec::new();
        body.extend(18456i32.to_le_bytes());
        body.extend([1, 14]);
        body.extend(
            u16::try_from(message.encode_utf16().count())
                .unwrap()
                .to_le_bytes(),
        );
        body.extend(utf16(message));
        body.push(4);
        body.extend(utf16("fake"));
        body.push(0);
        body.extend(1i32.to_le_bytes());
        let mut reply = vec![0xaa];
        reply.extend(u16::try_from(body.len()).unwrap().to_le_bytes());
        reply.extend(body);
        reply.extend(done_token(0x02, 0, 0));
        reply
    }

    /// One connection of `serve_pool`, from the handshake to its end.
    fn play(mut s: TcpStream, conn: usize, script: &Conn, log: &Mutex<Log>) -> End {
        if script.refuse_login {
            if receive(&mut s) == Some(0x12)
                && write_packet(&mut s, &PRELOGIN_REPLY, true).is_ok()
                && receive(&mut s) == Some(0x10)
            {
                let _ = write_packet(&mut s, &login_refused(), true);
            }
            let _ = s.shutdown(Shutdown::Both);
            return End::Server;
        }
        if !handshake(&mut s) {
            return End::Client;
        }
        let mut answered = 0;
        loop {
            if script.close_after == Some(answered) {
                let _ = s.shutdown(Shutdown::Both);
                return End::Server;
            }
            let Some(message) = receive_message(&mut s) else {
                return End::Client;
            };
            match message.kind {
                1 => {}
                // An attention that found no reply in progress (it came after the final DONE):
                // the server acknowledges it all the same.
                6 => {
                    if write_packet(&mut s, &done_attn(), true).is_err() {
                        return End::Client;
                    }
                    continue;
                }
                _ => continue,
            }
            answered += 1;
            let sql = message.sql();
            log.lock().unwrap().requests.push(Seen {
                conn,
                reset: message.reset(),
                sql: sql.clone(),
            });
            // The server resets the session before it runs the request, so the acknowledgement
            // leads the reply; the reset restores the login's SET options (NOEXEC among them)
            // and rolls back an open transaction, which needs no token.
            let mut reply = Vec::new();
            if message.reset() || message.reset_keeping_transaction() {
                let mut log = log.lock().unwrap();
                log.noexec[conn] = false;
                if message.reset() {
                    log.transactions[conn] = 0;
                }
                if !script.no_reset_ack {
                    reply.extend(env_reset());
                }
            }
            let sent = match script.turn(answered) {
                Turn::Answer => {
                    reply.extend(answer(&sql, conn, log));
                    write_packet(&mut s, &reply, true)
                }
                Turn::Delay(delay) => {
                    std::thread::sleep(delay);
                    reply.extend(answer(&sql, conn, log));
                    write_packet(&mut s, &reply, true)
                }
                Turn::Hang { ack } => {
                    reply.extend(int_rows_open(3));
                    if write_packet(&mut s, &reply, false).is_err() {
                        return End::Client;
                    }
                    loop {
                        match receive_message(&mut s) {
                            None => return End::Client,
                            Some(next) if next.kind == 6 => break,
                            Some(_) => {}
                        }
                    }
                    if !ack {
                        while receive(&mut s).is_some() {}
                        return End::Client;
                    }
                    write_packet(&mut s, &done_attn(), true)
                }
                Turn::Pause(wait) => {
                    reply.extend(int_rows_open(2));
                    if write_packet(&mut s, &reply, false).is_err() {
                        return End::Client;
                    }
                    // A message during the wait is the attention (nothing else may come while
                    // a reply is open); none by its end, and the rest of the batch follows.
                    let _ = s.set_read_timeout(Some(wait));
                    let mut peeked = [0u8; 1];
                    let early = s.peek(&mut peeked);
                    let _ = s.set_read_timeout(None);
                    match early {
                        Ok(0) => return End::Client,
                        Ok(_) => {
                            match receive_message(&mut s) {
                                Some(next) if next.kind == 6 => {}
                                _ => return End::Client,
                            }
                            write_packet(&mut s, &done_attn(), true)
                        }
                        Err(_) => {
                            let mut rest = done_token(0x11, 0xc1, 2);
                            rest.extend(int_rows(1));
                            write_packet(&mut s, &rest, true)
                        }
                    }
                }
                Turn::Silent => {
                    while receive(&mut s).is_some() {}
                    return End::Client;
                }
                Turn::Drop => {
                    reply.extend(int_rows_open(3));
                    let _ = write_packet(&mut s, &reply, false);
                    let _ = s.shutdown(Shutdown::Both);
                    return End::Server;
                }
            };
            if sent.is_err() {
                return End::Client;
            }
        }
    }

    /// The reply to one of the batches sqlh sends, as SQL Server gives it: the session setup
    /// ends in the wrapper's `BEGIN TRANSACTION`, which opens a transaction (ENVCHANGE 8, then
    /// DONE); the rollback ends the open one (ENVCHANGE 10, then DONE); a `SELECT` returns one
    /// row (`int_rows`); anything else (a `SET`, a `USE`) only a DONE. Under `SET NOEXEC ON`
    /// a batch is compiled, not run: only a DONE, so a rollback leaves the transaction open,
    /// until a `SET NOEXEC OFF` (or a reset). Connection `conn`'s transaction is tracked in
    /// the log, where the test can see it.
    fn answer(sql: &str, conn: usize, log: &Mutex<Log>) -> Vec<u8> {
        let mut log = log.lock().unwrap();
        let open = log.transactions[conn];
        let mut reply = Vec::new();
        if sql.contains("SET NOEXEC") {
            log.noexec[conn] = sql.contains("SET NOEXEC ON");
            reply.extend(done_empty());
        } else if log.noexec[conn] {
            reply.extend(done_empty());
        } else if sql.contains("BEGIN TRANSACTION") {
            // A nested BEGIN only counts up @@TRANCOUNT, which the client isn't told.
            if open == 0 {
                // Unique to the connection, and never 0.
                let descriptor = ((conn as u64 + 1) << 32) | (log.requests.len() as u64 + 1);
                log.transactions[conn] = descriptor;
                reply.extend(env_begin(descriptor));
            }
            reply.extend(done_empty());
        } else if sql.contains("ROLLBACK TRANSACTION") {
            if open != 0 {
                log.transactions[conn] = 0;
                reply.extend(env_rollback(open));
            }
            reply.extend(done_empty());
        } else if sql.trim_start().starts_with("SELECT") {
            reply.extend(int_rows(1));
        } else {
            reply.extend(done_empty());
        }
        reply
    }

    /// Shows rows (up to `quota`, like `--max-rows`), or hides them all, and spends `work`
    /// on each hidden row and on each shown row from number `work_from` on.
    struct Sink {
        show: bool,
        quota: Option<u64>,
        shown: u64,
        work: Duration,
        work_from: u64,
        stopped: Option<Stop>,
        complete: bool,
    }

    impl Sink {
        fn new(show: bool, work: Duration) -> Sink {
            Sink {
                show,
                quota: None,
                shown: 0,
                work,
                work_from: 1,
                stopped: None,
                complete: false,
            }
        }

        /// Shows `quota` rows, then hides the rest, as `--max-rows` does: `wants_values` turns
        /// false on the row that fills it. The work goes on the quota's last row only.
        fn quota(quota: u64, work: Duration) -> Sink {
            Sink {
                quota: Some(quota),
                work_from: quota,
                ..Sink::new(true, work)
            }
        }

        fn spend(&self) {
            let until = Instant::now() + self.work;
            while Instant::now() < until {
                std::hint::spin_loop();
            }
        }
    }

    impl BatchSink for Sink {
        fn columns(&mut self, _: &[Column]) -> io::Result<()> {
            Ok(())
        }
        fn wants_values(&self) -> bool {
            self.show && self.quota.is_none_or(|quota| self.shown < quota)
        }
        fn cell_limit(&self) -> usize {
            100
        }
        fn row(&mut self, _: &[Value]) -> io::Result<()> {
            self.shown += 1;
            if self.shown >= self.work_from {
                self.spend();
            }
            Ok(())
        }
        fn skipped_row(&mut self) {
            self.spend();
        }
        fn counting_stopped(&mut self, by: Stop) {
            self.stopped = Some(by);
        }
        fn end_rows(&mut self) -> io::Result<()> {
            self.complete = true;
            Ok(())
        }
        fn end_rows_incomplete(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn rows_affected(&mut self, _: u64) -> io::Result<()> {
            Ok(())
        }
        fn message(&mut self, _: &ServerMessage) -> io::Result<()> {
            Ok(())
        }
        fn error(&mut self, _: &ServerMessage) -> io::Result<()> {
            Ok(())
        }
    }

    struct Open;

    impl Backpressure for Open {
        fn is_full(&self) -> bool {
            false
        }
        fn room(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    pub(crate) struct Ran<S> {
        pub(crate) outcome: Result<(), DbError>,
        pub(crate) elapsed: Duration,
        pub(crate) sink: S,
        /// What a later statement on the same session got, when the session was retired.
        after: Option<Result<(), DbError>>,
    }

    /// How to connect to a fake server at `server`: SQL authentication with synthetic strings,
    /// unrelated to any real account, no TLS (see `PRELOGIN_REPLY`), a 5 s login timeout.
    pub(crate) fn options(server: String) -> ConnectOptions {
        ConnectOptions {
            server,
            database: None,
            auth: Auth::Sql {
                user: "fake".into(),
                password: crate::secret::Secret::new("fake".into()),
            },
            encrypt: Encrypt::Optional,
            trust_server_certificate: false,
            login_timeout: 5,
            read_only_intent: false,
            multi_subnet_failover: false,
            hints: super::Hints::Flags,
            application_name: super::APP_READ_ONLY,
        }
    }

    /// Connects to `server`, runs `sql` with `-t 1`, and reports.
    pub(crate) fn run_once<S: BatchSink>(
        server: String,
        sql: String,
        counting: Counting,
        sink: S,
    ) -> Ran<S> {
        run_with(server, sql, 1, counting, sink)
    }

    /// `run_once` with `-t timeout` (0: no deadline).
    fn run_with<S: BatchSink>(
        server: String,
        sql: String,
        timeout: u32,
        counting: Counting,
        mut sink: S,
    ) -> Ran<S> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let cancel = Cancel::new();
            let options = options(server);
            let mut session = Session::connect(&options, &cancel)
                .await
                .expect("connected to the fake server");
            let started = Instant::now();
            let outcome = session
                .run(sql, timeout, counting, &cancel, &mut sink, &Open)
                .await;
            let elapsed = started.elapsed();
            let after = if session.retired {
                Some(session.exec_internal("SELECT 1", 1, &cancel).await)
            } else {
                None
            };
            Ran {
                outcome,
                elapsed,
                sink,
                after,
            }
        })
    }

    const SHOWN: Counting = Counting::EXACT;
    /// `--exact-count` in a read-only run: exact, but `-t` may stop a count of hidden rows.
    const HIDDEN_MAY_STOP: Counting = Counting {
        budget: None,
        timeout_stops: true,
    };
    const LATE: Duration = Duration::from_millis(1200);

    fn timed_out(outcome: &Result<(), DbError>) -> bool {
        matches!(outcome, Err(DbError::Timeout(_)))
    }

    // R15 follow-up A: a short result whose last row's work (sqlh's, not the driver's) crosses
    // `-t`, with the final DONE already in the buffer, so no read waits and no look at the clock
    // falls inside the result (fewer than 64 shown rows). It used to end `Ok(())`.
    #[test]
    fn a_deadline_crossed_by_the_last_rows_work_is_a_timeout() {
        let reply = || serve(Script::Reply(int_rows(1)));
        let ran = run_once(reply(), "SELECT 1".into(), SHOWN, Sink::new(true, LATE));
        assert!(timed_out(&ran.outcome), "{:?}", ran.outcome);
        assert!(
            !ran.sink.complete,
            "a result past -t is not closed as complete"
        );

        // Writes allowed (`-t` stops no count), with the row hidden: a timeout too.
        let ran = run_once(reply(), "SELECT 1".into(), SHOWN, Sink::new(false, LATE));
        assert!(timed_out(&ran.outcome), "{:?}", ran.outcome);

        // A normal short result is a success, and complete.
        let ran = run_once(
            serve(Script::Reply(int_rows(3))),
            "SELECT 1".into(),
            SHOWN,
            Sink::new(true, Duration::ZERO),
        );
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert!(ran.sink.complete);
    }

    // R15 A, second round: the row that fills `--max-rows` is a row to show, so a deadline its
    // work crosses is a timeout, although the next row would be hidden and the run may stop a
    // count of hidden rows. No hidden row is read here: the result has exactly the quota. It
    // used to be taken for a late hidden tail and end `Ok(())`, complete and exact.
    #[test]
    fn a_deadline_crossed_by_the_row_that_fills_the_quota_is_a_timeout() {
        for counting in [HIDDEN_MAY_STOP, Counting::BOUNDED] {
            let ran = run_once(
                serve(Script::Reply(int_rows(1))),
                "SELECT 1".into(),
                counting,
                Sink::quota(1, LATE),
            );
            assert!(timed_out(&ran.outcome), "{counting:?}: {:?}", ran.outcome);
            assert!(!ran.sink.complete, "{counting:?}");
        }
        // The default quota, filled by the 100th row, which alone carries the work.
        let ran = run_once(
            serve(Script::Reply(int_rows(100))),
            "SELECT 1".into(),
            Counting::BOUNDED,
            Sink::quota(100, LATE),
        );
        assert!(timed_out(&ran.outcome), "{:?}", ran.outcome);
        assert!(!ran.sink.complete);
        // The control: the same quota with no work is a success, complete.
        let ran = run_once(
            serve(Script::Reply(int_rows(1))),
            "SELECT 1".into(),
            HIDDEN_MAY_STOP,
            Sink::quota(1, Duration::ZERO),
        );
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert!(ran.sink.complete);
    }

    // A genuine hidden tail after a filled quota: the shown row was done in time, and the
    // deadline passed while a hidden row was counted. In a read-only run that is complete and
    // exact (the owner's exception), with writes allowed a timeout.
    #[test]
    fn a_late_hidden_tail_after_the_quota_follows_the_owners_exception() {
        let hidden_work = || Sink {
            work_from: u64::MAX,
            ..Sink::quota(1, LATE)
        };
        let ran = run_once(
            serve(Script::Reply(int_rows(2))),
            "SELECT 1".into(),
            HIDDEN_MAY_STOP,
            hidden_work(),
        );
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert!(ran.sink.complete && ran.sink.stopped.is_none());
        let ran = run_once(
            serve(Script::Reply(int_rows(2))),
            "SELECT 1".into(),
            SHOWN,
            hidden_work(),
        );
        assert!(timed_out(&ran.outcome), "{:?}", ran.outcome);
    }

    // The review's open item (Phase 2 completion review): sqlh's own stop of a count, which the
    // server never acknowledges. The rows arrive, the 250 ms budget sends the attention, and
    // the server, mid-result, goes silent. The driver gives up after its 5 s and closes the
    // connection, and the run says it was sqlh's stop that went unconfirmed, not a cancel from
    // outside (DESIGN.md §6, exit 6). Before this, only the message had a test.
    #[test]
    fn a_count_stop_the_server_never_confirms_closes_the_connection_and_says_so() {
        let ran = run_with(
            serve(Script::RowsThenSilent { rows: 2_000 }),
            "SELECT x".into(),
            0,
            Counting::BOUNDED,
            Sink::new(false, Duration::ZERO),
        );
        match &ran.outcome {
            Err(DbError::CountStopUnconfirmed) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            ran.elapsed >= Duration::from_secs(5) && ran.elapsed < Duration::from_secs(8),
            "{:?}",
            ran.elapsed
        );
        assert!(!ran.sink.complete);
    }

    // The owner's hidden-row exception is kept: in a read-only run, a result whose shown rows
    // are complete and whose hidden rows were all counted, however late, is complete and exact.
    #[test]
    fn a_late_end_of_hidden_rows_in_a_read_only_run_is_still_complete() {
        let ran = run_once(
            serve(Script::Reply(int_rows(1))),
            "SELECT 1".into(),
            HIDDEN_MAY_STOP,
            Sink::new(false, LATE),
        );
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        assert!(ran.sink.complete && ran.sink.stopped.is_none());
    }

    // R15 follow-up B: the server takes the batch and then never answers, not even the
    // attention. The batch still ends within -t plus `SETTLE_GRACE`, as a timeout.
    #[test]
    fn a_cancel_the_server_never_acknowledges_ends_within_the_grace() {
        let ran = run_once(
            serve(Script::Silent),
            "SELECT 1".into(),
            SHOWN,
            Sink::new(true, Duration::ZERO),
        );
        assert!(timed_out(&ran.outcome), "{:?}", ran.outcome);
        assert!(
            ran.elapsed < Duration::from_millis(7_500),
            "{:?}",
            ran.elapsed
        );
    }

    // R15 follow-up B: a request of many packets that the server stops reading part way
    // through, and never answers, so the deadline meets the driver in the middle of sending
    // it (where it abandons the write, or withdraws the request and can wait up to 120 s).
    // The batch ends within -t plus `SETTLE_GRACE`, and a retired session takes nothing more.
    #[test]
    fn a_request_cut_off_while_it_is_sent_ends_within_the_grace() {
        let ran = run_once(
            serve(Script::StopReading { bytes: 64 << 10 }),
            long_sql(),
            SHOWN,
            Sink::new(true, Duration::ZERO),
        );
        assert!(timed_out(&ran.outcome), "{:?}", ran.outcome);
        assert!(
            ran.elapsed < Duration::from_millis(7_500),
            "{:?}",
            ran.elapsed
        );
        if let Some(after) = &ran.after {
            assert!(
                matches!(after, Err(DbError::Lost(message)) if message == RETIRED),
                "{after:?}"
            );
        }
    }

    /// A batch of about 8 MB: some 2,000 packets of 4 KB, far more than the socket buffers hold.
    fn long_sql() -> String {
        format!("SELECT 1 --{}", "x".repeat(8 << 20))
    }

    // R15 follow-up B, the reviewer's case: the driver settles a cancel in its own time, and
    // withdrawing a request cancelled between two of its packets waits up to 120 s for the
    // server's DONE, with the cancel removed. `settle_within` is what bounds that: here the
    // work is a future that never ends, standing in for the withdrawal, and the bound ends it
    // at the deadline plus `SETTLE_GRACE`; without a deadline (`-t 0`) nothing is bounded.
    //
    // That driver state was not reproduced through `Session::run` here: it needs the cancel to
    // land after one packet's write completes and before the next starts, in the same poll. In
    // a sweep of buffer sizes and runtime holds, sqlh's deadline always met a write in progress,
    // which the driver abandons at once; the likely reason is that the runtime resumed the
    // writing task before the timer task that cancels. That is an observation on this machine,
    // not a guarantee, and the bound does not depend on it.
    #[test]
    fn a_settlement_that_never_ends_is_cut_at_the_grace() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = Instant::now();
            let deadline = Instant::now() + Duration::from_secs(1);
            let never = std::future::pending::<()>();
            assert_eq!(super::settle_within(Some(deadline), never).await, None);
            let waited = started.elapsed();
            assert!(
                waited >= super::SETTLE_GRACE
                    && waited <= super::SETTLE_GRACE + Duration::from_secs(2),
                "{waited:?}"
            );
            // Work that ends is never cut, and without a deadline there is no bound at all.
            assert_eq!(
                super::settle_within(Some(deadline), async { 7 }).await,
                Some(7)
            );
            let slow = tokio::time::sleep(Duration::from_millis(50));
            assert_eq!(
                super::settle_within(None, async {
                    slow.await;
                    8
                })
                .await,
                Some(8)
            );
        });
    }

    /// The FeatureExt ids a LOGIN7 payload asks for (MS-TDS 2.2.6.4). The OffsetLength block's
    /// ibExtension (bytes 56..58) points at a DWORD holding the FeatureExt block's offset; the
    /// block is `id u8 | data length u32 LE | data`, repeated, ended by 0xFF.
    fn login_features(login: &[u8]) -> Vec<u8> {
        login_feature_data(login)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// `login_features` with each feature's data.
    fn login_feature_data(login: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let at = |i: usize, n: usize| login.get(i..i + n);
        let Some(&[a, b]) = at(56, 2) else {
            return Vec::new();
        };
        let Some(&[c, d, e, f]) = at(usize::from(u16::from_le_bytes([a, b])), 4) else {
            return Vec::new();
        };
        let mut i = u32::from_le_bytes([c, d, e, f]) as usize;
        let mut features = Vec::new();
        while let Some(&[id]) = at(i, 1) {
            if id == 0xff {
                break;
            }
            let Some(&[g, h, j, k]) = at(i + 1, 4) else {
                break;
            };
            let length = u32::from_le_bytes([g, h, j, k]) as usize;
            features.push((id, at(i + 5, length).unwrap_or_default().to_vec()));
            i += 5 + length;
        }
        features
    }

    /// LOGINACK alone (the first token of `login_reply`), for login replies built by hand.
    fn login_ack() -> Vec<u8> {
        let reply = login_reply();
        let length = usize::from(u16::from_le_bytes([reply[1], reply[2]]));
        reply[..3 + length].to_vec()
    }

    /// ENVCHANGE Routing (subtype 20, MS-TDS 2.2.7.9): `E3 | length u16 LE | 20 | routing data
    /// length u16 LE | protocol 0 (TCP) | port u16 LE | server US_VARCHAR | old value, u16 0`.
    fn env_routing(port: u16, server: &str) -> Vec<u8> {
        env_route(20, port, server, None)
    }

    /// A routing ENVCHANGE of `subtype`: 20, or 21, Enhanced Routing, whose routing data also
    /// names a `database` (a US_VARCHAR after the server; mssql-tds 0.3.0's
    /// `parse_routing_data`).
    fn env_route(subtype: u8, port: u16, server: &str, database: Option<&str>) -> Vec<u8> {
        let us_varchar = |text: &str| {
            let mut field = u16::try_from(text.encode_utf16().count())
                .unwrap()
                .to_le_bytes()
                .to_vec();
            field.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            field
        };
        let mut data = vec![0];
        data.extend(port.to_le_bytes());
        data.extend(us_varchar(server));
        if let Some(database) = database {
            data.extend(us_varchar(database));
        }
        let data_length = u16::try_from(data.len()).unwrap();
        let mut value = vec![subtype];
        value.extend(data_length.to_le_bytes());
        value.extend(data);
        value.extend([0, 0]);
        let mut token = vec![0xe3];
        token.extend(u16::try_from(value.len()).unwrap().to_le_bytes());
        token.extend(value);
        token
    }

    /// A one-connection fake that answers the prelogin, keeps the LOGIN7's payload, answers it
    /// with `reply`, then reads until the client goes. The payload is `None` until a LOGIN7
    /// has arrived.
    fn serve_login(reply: Vec<u8>) -> (u16, Arc<Mutex<Option<Vec<u8>>>>) {
        serve_login_on("127.0.0.1", reply)
    }

    /// `serve_login` on another loopback address, such as a route's target.
    fn serve_login_on(ip: &str, reply: Vec<u8>) -> (u16, Arc<Mutex<Option<Vec<u8>>>>) {
        let listener = TcpListener::bind((ip, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(None));
        let kept = seen.clone();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            if receive(&mut s) != Some(0x12) || write_packet(&mut s, &PRELOGIN_REPLY, true).is_err()
            {
                return;
            }
            let Some(login) = receive_message(&mut s).filter(|m| m.kind == 0x10) else {
                return;
            };
            *kept.lock().unwrap() = Some(login.payload);
            if write_packet(&mut s, &reply, true).is_ok() {
                while receive(&mut s).is_some() {}
            }
        });
        (port, seen)
    }

    fn connect(server: String) -> Result<Session, DbError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Session::connect(&options(server), &Cancel::new()))
    }

    // Owner decisions 40 and 41 (docs/notes/driver-0.3.0.md): the login never asks for session
    // recovery (FeatureExt 0x01), so the driver can't log in again behind sqlh's back, and
    // multi-subnet failover is set by sqlh itself (off without -M), never the driver's default.
    // The login does ask for DNS caching (0x0B) and Enhanced Routing (0x0F), which mssql-tds
    // always advertises (0.3.0, 0.4.0; decisions 42 and 43).
    #[test]
    fn the_login_asks_for_no_session_recovery_and_failover_is_sqlhs_own() {
        let (port, seen) = serve_login(login_reply());
        let session = connect(format!("127.0.0.1,{port}")).expect("connected");
        assert!(!session.client.is_session_recovery_enabled());
        let policy = session
            .client
            .multi_subnet_failover_policy()
            .expect("a resolved policy");
        assert!(!policy.enabled);
        assert!(matches!(
            policy.source,
            mssql_tds::connection::multi_subnet_failover::MultiSubnetFailoverSource::Connection
        ));
        let login = seen.lock().unwrap().clone().expect("a LOGIN7");
        let features = login_features(&login);
        assert!(!features.contains(&0x01), "{features:02x?}");
        assert!(
            features.contains(&0x0b) && features.contains(&0x0f),
            "{features:02x?}"
        );
    }

    // Decision 71: the login asks for vector feature version 2 (FeatureExt 0x0E, one data byte),
    // set by sqlh, so a later driver default can't change how vectors arrive unseen; and it
    // never asks for data classification (0x09), which would change every login and put a new
    // token in results (driver-0.4.0.md §3.2).
    #[test]
    fn the_login_asks_for_vector_version_2_and_no_data_classification() {
        let (port, seen) = serve_login(login_reply());
        connect(format!("127.0.0.1,{port}")).expect("connected");
        let login = seen.lock().unwrap().clone().expect("a LOGIN7");
        let features = login_feature_data(&login);
        let vector: Vec<_> = features.iter().filter(|(id, _)| *id == 0x0e).collect();
        assert_eq!(vector, [&(0x0e, vec![2])], "{features:02x?}");
        assert!(
            features.iter().all(|(id, _)| *id != 0x09),
            "{features:02x?}"
        );
    }

    // Decision 30(c): the name the caller chose is the one the login sends (ibAppName, bytes
    // 48..52 of LOGIN7's OffsetLength block), spelled as the contract spells it.
    #[test]
    fn the_login_sends_the_application_name_it_was_given() {
        for (name, sent) in [
            (crate::db::APP_READ_ONLY, "sqlh"),
            (crate::db::APP_MCP, "sqlh mcp"),
            (crate::db::APP_WRITES, "sqlh (writes)"),
        ] {
            let (port, seen) = serve_login(login_reply());
            let mut options = options(format!("127.0.0.1,{port}"));
            options.application_name = name;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Session::connect(&options, &Cancel::new()))
                .expect("connected");
            let login = seen.lock().unwrap().clone().expect("a LOGIN7");
            assert_eq!(login_text(&login, 48).as_deref(), Some(sent));
        }
    }

    /// Keeps a result's columns and values, which `Sink` drops.
    #[derive(Default)]
    struct Kept {
        columns: Vec<(String, String, bool)>,
        rows: Vec<Vec<Value>>,
    }

    impl BatchSink for Kept {
        fn columns(&mut self, columns: &[Column]) -> io::Result<()> {
            self.columns = columns
                .iter()
                .map(|c| (c.name.clone(), c.type_name.clone(), c.nullable))
                .collect();
            Ok(())
        }
        fn wants_values(&self) -> bool {
            true
        }
        fn cell_limit(&self) -> usize {
            100
        }
        fn row(&mut self, values: &[Value]) -> io::Result<()> {
            self.rows.push(values.to_vec());
            Ok(())
        }
        fn skipped_row(&mut self) {}
        fn counting_stopped(&mut self, _: Stop) {}
        fn end_rows(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn end_rows_incomplete(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn rows_affected(&mut self, _: u64) -> io::Result<()> {
            Ok(())
        }
        fn message(&mut self, _: &ServerMessage) -> io::Result<()> {
            Ok(())
        }
        fn error(&mut self, _: &ServerMessage) -> io::Result<()> {
            Ok(())
        }
    }

    // Decision 71, through the driver's decoder: a float32 and a float16 vector column as SQL
    // Server sends them natively under vector feature version 2 (TYPE_INFO 0xF5 with a u16
    // length and the base type as its scale, 0x00 float32 and 0x01 float16; each value a u16
    // length, the 8-byte header `A9 01 dimensions base 00 00 00`, then the elements), and a NULL
    // row. Both columns are `vector`, and each element is printed at its own precision.
    #[test]
    fn native_float32_and_float16_vectors_reach_the_sink_as_sqlh_prints_them() {
        let float32: Vec<u8> = [0.1f32, 2.0, -3.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let float16: Vec<u8> = [0.1f32, 2.0, -3.5, 65504.0]
            .iter()
            .flat_map(|&v| half::f16::from_f32(v).to_le_bytes())
            .collect();
        let column = |name: u8, base: u8, elements: &[u8]| {
            let length = u16::try_from(8 + elements.len()).unwrap();
            let mut c = vec![0, 0, 0, 0, 1, 0, 0xf5];
            c.extend(length.to_le_bytes());
            c.extend([base, 1, name, 0]);
            c
        };
        let value = |base: u8, dimensions: u16, elements: &[u8]| {
            let mut v = u16::try_from(8 + elements.len())
                .unwrap()
                .to_le_bytes()
                .to_vec();
            v.extend([0xa9, 1]);
            v.extend(dimensions.to_le_bytes());
            v.extend([base, 0, 0, 0]);
            v.extend(elements);
            v
        };
        let mut reply = vec![0x81, 2, 0];
        reply.extend(column(b'v', 0, &float32));
        reply.extend(column(b'w', 1, &float16));
        reply.push(0xd1);
        reply.extend(value(0, 3, &float32));
        reply.extend(value(1, 4, &float16));
        reply.extend([0xd1, 0xff, 0xff, 0xff, 0xff]);
        reply.extend(done(2));
        let ran = run_once(
            serve(Script::Reply(reply)),
            "SELECT v, w".into(),
            Counting::EXACT,
            Kept::default(),
        );
        assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
        let vector = |name: &str| (name.to_owned(), "vector".to_owned(), true);
        assert_eq!(ran.sink.columns, [vector("v"), vector("w")]);
        assert_eq!(
            ran.sink.rows,
            [
                vec![
                    Value::Text("[0.1,2,-3.5]".into()),
                    Value::Text("[0.1,2,-3.5,65504]".into())
                ],
                vec![Value::Null, Value::Null],
            ]
        );
    }

    // driver-0.4.0.md §3.3: mssql-tds 0.4.0 rewrote the UTF-16 encoder every batch's text goes
    // through, and keeps a supplementary character's low surrogate pending when a packet fills
    // between the two halves. A wrong unit would change the query silently, so the text that
    // arrives is compared with the text sent: ASCII runs longer than the encoder's 256-unit
    // stage, characters of the Basic Multilingual Plane and 3,000 surrogate pairs. The fake's
    // 4,096-byte packets hold 2,033 units of SQL after the 22-byte ALL_HEADERS, then 2,044 each,
    // so they cut between the halves of a pair for an odd `k` and between pairs for an even
    // one. A dropped or misplaced half would read back as U+FFFD.
    #[test]
    fn a_batch_reaches_the_server_unit_for_unit_across_packets() {
        let fake = serve_pool(Vec::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut sent = Vec::new();
        runtime.block_on(async {
            let cancel = Cancel::new();
            let mut session = Session::connect(&options(fake.server.clone()), &cancel)
                .await
                .expect("connected to the fake server");
            for k in 0..4 {
                let mut sql = format!("-- {}", "a".repeat(k));
                sql.push_str(&"\u{1F600}".repeat(3000));
                sql.push_str(&"x".repeat(600));
                sql.push_str(&"é日本\u{10FFFF}".repeat(500));
                sql.push_str(&format!("\nSELECT N'{}';", "\u{1F600}b".repeat(700)));
                session.exec_internal(&sql, 5, &cancel).await.unwrap();
                sent.push(sql);
            }
        });
        let arrived = fake.sql(0);
        assert_eq!(arrived.len(), sent.len(), "batches");
        for (k, (arrived, sent)) in arrived.iter().zip(&sent).enumerate() {
            assert!(
                arrived == sent,
                "k = {k}: {} units sent, {} arrived, first difference at unit {:?}",
                sent.encode_utf16().count(),
                arrived.encode_utf16().count(),
                sent.encode_utf16()
                    .zip(arrived.encode_utf16())
                    .position(|(a, b)| a != b)
            );
        }
    }

    /// A connect to a gateway whose login reply is `reply`, with a second fake listening as the
    /// route's target: the connect's outcome, and the LOGIN7 that reached the target, if any.
    fn route_through(
        reply: impl FnOnce(u16) -> Vec<u8>,
    ) -> (Result<Session, DbError>, Option<Vec<u8>>) {
        let (target, routed) = serve_login(login_reply());
        let (gateway, _) = serve_login(reply(target));
        let outcome = connect(format!("127.0.0.1,{gateway}"));
        // A refused route sends nothing more; give a route that was followed time to land.
        std::thread::sleep(Duration::from_millis(200));
        let reached = routed.lock().unwrap().clone();
        (outcome, reached)
    }

    /// FEATUREEXTACK (0xAE, MS-TDS 2.2.7.11) acknowledging one feature: `id | data length u32
    /// LE | data`, then the 0xFF terminator.
    fn feature_ext_ack(id: u8, data: &[u8]) -> Vec<u8> {
        let mut token = vec![0xae, id];
        token.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        token.extend(data);
        token.push(0xff);
        token
    }

    /// The database a LOGIN7 payload asks for: ibDatabase and cchDatabase (bytes 68..72 of the
    /// OffsetLength block, MS-TDS 2.2.6.4).
    fn login_database(login: &[u8]) -> Option<String> {
        login_text(login, 68)
    }

    /// A LOGIN7 payload's text field: its offset at `at` in the OffsetLength block, its length
    /// after that, in UTF-16 characters.
    fn login_text(login: &[u8], at: usize) -> Option<String> {
        let field = |i: usize| {
            login
                .get(i..i + 2)
                .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        };
        let (offset, chars) = (field(at)?, field(at + 2)?);
        let units: Vec<u16> = login
            .get(offset..offset + 2 * chars)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect();
        String::from_utf16(&units).ok()
    }

    /// `login_reply` with `token` placed after its LOGINACK (`after_ack`) or before it: every
    /// other token a login needs is there, so a driver that ignored the route would connect,
    /// and a test that expects a refusal fails.
    fn login_reply_with(token: Vec<u8>, after_ack: bool) -> Vec<u8> {
        let ack = login_ack();
        let rest = login_reply()[ack.len()..].to_vec();
        if after_ack {
            [ack, token, rest].concat()
        } else {
            [token, ack, rest].concat()
        }
    }

    // Decision 43: a route (ENVCHANGE 20) after LOGINACK takes the login, with the same
    // credentials, to the server it names, as it did in 0.2.0; one before LOGINACK is refused
    // (mssql-tds 0.3.0, [MS-TDS] 2.2.7.9), and so is an Enhanced Routing one (21, which can
    // also name a database) that the login's FEATUREEXTACK didn't acknowledge. A refused route
    // sends no second login. Each refused reply is otherwise a whole login, so a driver that
    // ignored the route would connect: these pin the driver's behaviour, and a later version
    // that changes it fails here (refusals strengthened in review, 2026-10-31).
    #[test]
    fn a_route_after_the_login_acknowledgement_is_followed_and_one_before_it_is_refused() {
        let (followed, reached) =
            route_through(|target| login_reply_with(env_routing(target, "127.0.0.1"), true));
        assert!(followed.is_ok(), "{:?}", followed.err());
        assert!(reached.is_some(), "the login reached the route");

        let (early, reached) =
            route_through(|target| login_reply_with(env_routing(target, "127.0.0.1"), false));
        assert!(
            matches!(&early, Err(DbError::Connect(words))
                if words.contains("routing ENVCHANGE before LOGINACK")),
            "{:?}",
            early.err()
        );
        assert!(reached.is_none(), "no login after a refused route");

        let (unacknowledged, reached) = route_through(|target| {
            login_reply_with(env_route(21, target, "127.0.0.1", Some("sales")), true)
        });
        assert!(
            matches!(&unacknowledged, Err(DbError::Connect(words))
                if words.contains("without an Enhanced Routing feature acknowledgement")),
            "{:?}",
            unacknowledged.err()
        );
        assert!(
            reached.is_none(),
            "no login after an unacknowledged Enhanced Routing route"
        );
    }

    // The external review's judgment 4 (2026-10-31): an acknowledged Enhanced Routing route
    // (ENVCHANGE 21) that names a database is followed, and the routed LOGIN7 asks for that
    // database in place of the login's own; the routed login asks for no session recovery
    // either (decision 41). The driver's FEATUREEXTACK check is the one the unacknowledged case
    // above fails, so this pins both sides of it.
    #[test]
    fn an_acknowledged_enhanced_route_takes_the_login_to_its_database() {
        let (followed, reached) = route_through(|target| {
            let route = env_route(21, target, "127.0.0.1", Some("sales"));
            login_reply_with([feature_ext_ack(0x0f, &[1]), route].concat(), true)
        });
        assert!(followed.is_ok(), "{:?}", followed.err());
        let login = reached.expect("the login reached the route");
        assert_eq!(login_database(&login).as_deref(), Some("sales"));
        let features = login_features(&login);
        assert!(!features.contains(&0x01), "{features:02x?}");
    }

    // Decision 72: Windows auth's SPN provider (`db::spn`), through a real connect. The fake
    // prelogin offers no encryption, so there is no TLS, and a LOGIN7 with a token from the
    // recorder's mock context is answered with a plain LOGINACK, which the driver takes; real
    // SSPI is a live check (driver-0.4.0.md §10).

    use super::spn::tests::{Hop, NOT_LOOPBACK, TOKEN, answering, held, recorder};
    use super::spn::{HopSpn, Slots};
    use mssql_tds::security::SecurityContextProvider;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The hops a recording provider saw.
    type Hops = Arc<Mutex<Vec<Hop>>>;

    fn integrated(server: String) -> ConnectOptions {
        ConnectOptions {
            auth: Auth::Integrated,
            ..options(server)
        }
    }

    /// The production provider's rule and slots, with `create` recording each hop, and a lookup
    /// that counts its calls (no fake is looked up: every one is on a loopback address).
    fn recording() -> (Arc<dyn SecurityContextProvider>, Hops, Arc<AtomicUsize>) {
        static SLOTS: Slots = Slots::new(4);
        let (create, seen) = recorder();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = HopSpn::with(
            Duration::from_secs(10),
            &SLOTS,
            answering("looked-up.example", Arc::clone(&calls)),
            NOT_LOOPBACK,
            create,
        );
        (Arc::new(provider), seen, calls)
    }

    fn connect_as(
        options: &ConnectOptions,
        spn: Arc<dyn SecurityContextProvider>,
    ) -> Result<Session, DbError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Session::connect_with(options, &Cancel::new(), spn))
    }

    /// LOGIN7's fIntSecurity (OptionFlags2, byte 25, 0x80) and its SSPI data (ibSSPI and cbSSPI
    /// at 78..82 of the OffsetLength block, MS-TDS 2.2.6.4).
    fn login_sspi(login: &[u8]) -> Option<&[u8]> {
        let int_security = login.get(25).is_some_and(|b| b & 0x80 != 0);
        let field = |i: usize| {
            login
                .get(i..i + 2)
                .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        };
        let (offset, length) = (field(78)?, field(80)?);
        int_security.then(|| login.get(offset..offset + length))?
    }

    fn hop(server: &str, port: u16) -> Hop {
        Hop {
            server: server.into(),
            port,
            spn: Some(format!("MSSQLSvc/{server}:{port}")),
            loopback: true,
            bindings: None,
            package: "Negotiate",
        }
    }

    // The provider gets the host and port the driver dials, sets that hop's SPN, and its
    // context's token is what the LOGIN7 carries. A port replaces an instance, so no SQL
    // Browser is asked and the SPN names the port.
    #[test]
    fn windows_auth_gives_the_provider_the_dialled_host_and_port() {
        for server in ["127.0.0.1,{port}", r"127.0.0.1\SQLHNOBROWSER,{port}"] {
            let (port, seen_login) = serve_login(login_reply());
            let (provider, seen, calls) = recording();
            let server = server.replace("{port}", &port.to_string());
            let session = connect_as(&integrated(server.clone()), provider);
            assert!(session.is_ok(), "{server}: {:?}", session.err());
            assert_eq!(*seen.lock().unwrap(), [hop("127.0.0.1", port)], "{server}");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let login = seen_login.lock().unwrap().clone().expect("a LOGIN7");
            assert_eq!(login_sspi(&login), Some(TOKEN), "{server}");
        }
    }

    // R52, closed: each routed hop gets its own SPN, its host's and port's, with a read-only
    // intent too (the pre-lookup left those to the driver), and through an acknowledged
    // Enhanced Routing route, whose hop the driver builds from a cloned context.
    #[test]
    fn each_routed_hop_gets_its_own_spn() {
        type Route = fn(u16) -> Vec<u8>;
        let plain: Route = |target| login_reply_with(env_routing(target, "127.0.0.2"), true);
        let enhanced: Route = |target| {
            let route = env_route(21, target, "127.0.0.2", Some("sales"));
            login_reply_with([feature_ext_ack(0x0f, &[1]), route].concat(), true)
        };
        for (route, read_only) in [(plain, false), (plain, true), (enhanced, false)] {
            let (target, routed) = serve_login_on("127.0.0.2", login_reply());
            let (gateway, _) = serve_login(route(target));
            let (provider, seen, _) = recording();
            let options = ConnectOptions {
                read_only_intent: read_only,
                ..integrated(format!("127.0.0.1,{gateway}"))
            };
            let session = connect_as(&options, provider);
            assert!(session.is_ok(), "{:?}", session.err());
            assert_eq!(
                *seen.lock().unwrap(),
                [hop("127.0.0.1", gateway), hop("127.0.0.2", target)],
                "read-only {read_only}"
            );
            let login = routed.lock().unwrap().clone().expect("the routed LOGIN7");
            assert_eq!(login_sspi(&login), Some(TOKEN));
        }
    }

    /// A one-shot SQL Browser on `ip`:1434 (SSRP's fixed port, MS-SQLR) that names `tcp_port`
    /// for `instance`, replying from the address and port asked, as the driver requires. It
    /// returns once its thread is waiting for the request: the driver waits 1 s for the answer,
    /// and a thread not yet started on a busy machine could miss that.
    fn serve_browser(ip: &str, instance: &str, tcp_port: u16) {
        let socket = std::net::UdpSocket::bind((ip, 1434)).unwrap_or_else(|e| {
            panic!(
                "{ip}:1434 can't be bound ({e}): another test process using this address, or a \
                 SQL Server Browser holding the port exclusively"
            )
        });
        let answer = format!(
            "ServerName;FAKE;InstanceName;{instance};IsClustered;No;Version;17.0.0.0;tcp;{tcp_port};;"
        );
        let (ready, waiting) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut asked = [0u8; 512];
            let _ = ready.send(());
            let Ok((_, from)) = socket.recv_from(&mut asked) else {
                return;
            };
            let mut reply = vec![0x05];
            reply.extend(u16::try_from(answer.len()).unwrap().to_le_bytes());
            reply.extend(answer.as_bytes());
            let _ = socket.send_to(&reply, from);
        });
        waiting
            .recv_timeout(Duration::from_secs(10))
            .expect("the fake SQL Browser started");
    }

    // R52, closed: a named instance without a port gets SQL Browser's port in its SPN, which
    // the pre-lookup couldn't know before the connect. SQL Browser's port is fixed, so the
    // address is this process's own (127.a.b.3, from the process id, never 127.0.0.x), and the
    // instance's name is unique, since the driver caches what Browser said for the process.
    #[test]
    fn a_named_instance_gives_the_provider_sql_browsers_port() {
        let pid = std::process::id();
        let ip = format!("127.{}.{}.3", 1 + pid % 250, 1 + (pid / 250) % 250);
        let (port, seen_login) = serve_login_on(&ip, login_reply());
        serve_browser(&ip, "SQLHBROWSER72", port);
        let (provider, seen, _) = recording();
        let session = connect_as(&integrated(format!(r"{ip}\SQLHBROWSER72")), provider);
        assert!(session.is_ok(), "{:?}", session.err());
        assert_eq!(*seen.lock().unwrap(), [hop(&ip, port)]);
        assert!(
            seen_login.lock().unwrap().is_some(),
            "the LOGIN7 reached the instance"
        );
    }

    // The driver's own error from creating its context ends the connect with the Windows auth
    // hint, before any LOGIN7.
    #[test]
    fn the_drivers_security_error_ends_the_connect_before_the_login() {
        static SLOTS: Slots = Slots::new(4);
        let (port, seen_login) = serve_login(login_reply());
        let failing = HopSpn::with(
            Duration::from_secs(10),
            &SLOTS,
            answering("x", Arc::new(AtomicUsize::new(0))),
            NOT_LOOPBACK,
            Arc::new(|_: &_, _: &str, _: u16| {
                Err(mssql_tds::security::SecurityError::TargetUnknown(
                    "MSSQLSvc/x:1".into(),
                ))
            }),
        );
        let outcome = connect_as(&integrated(format!("127.0.0.1,{port}")), Arc::new(failing));
        assert!(
            matches!(&outcome, Err(DbError::Connect(words))
                if words.contains("Windows auth failed; check Kerberos/SPN setup")),
            "{:?}",
            outcome.err()
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(seen_login.lock().unwrap().is_none(), "no LOGIN7");
    }

    /// The provider with every host looked up, by a lookup held until the `Sender` drops.
    fn waiting(
        budget: Duration,
    ) -> (
        Arc<dyn SecurityContextProvider>,
        std::sync::mpsc::Sender<()>,
    ) {
        static SLOTS: Slots = Slots::new(4);
        let (lookup, release, _) = held();
        let (create, _) = recorder();
        let provider = HopSpn::with(budget, &SLOTS, lookup, |_| true, create);
        (Arc::new(provider), release)
    }

    // A cancel during the SPN wait ends the connect at once, as `Session::connect` races it;
    // the wait is left to end by itself on the driver's thread. The runtime ends as sqlh's do,
    // `shutdown_background`, since dropping it would wait for that thread.
    #[test]
    fn a_cancel_during_the_spn_wait_ends_the_connect() {
        let (port, _) = serve_login(login_reply());
        let (provider, release) = waiting(Duration::from_secs(10));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = Instant::now();
        let outcome = runtime.block_on(async {
            let root = Cancel::new();
            let cancel = root.child();
            let options = integrated(format!("127.0.0.1,{port}"));
            let connecting = Session::connect_with(&options, &cancel, provider);
            tokio::pin!(connecting);
            tokio::select! {
                outcome = &mut connecting => return outcome,
                () = tokio::time::sleep(Duration::from_millis(100)) => root.cancel(),
            }
            connecting.await
        });
        runtime.shutdown_background();
        drop(release);
        assert!(
            matches!(&outcome, Err(DbError::Cancelled(words)) if words == "cancelled while connecting"),
            "{:?}",
            outcome.err()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    // The SPN wait holds no runtime thread: the runtime keeps running other tasks through it,
    // and past the budget the login goes on with the fallback SPN.
    #[test]
    fn a_slow_spn_wait_holds_no_runtime_thread() {
        let (port, seen_login) = serve_login(login_reply());
        let (provider, release) = waiting(Duration::from_millis(300));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (outcome, ticks) = runtime.block_on(async {
            let cancel = Cancel::new();
            let options = integrated(format!("127.0.0.1,{port}"));
            let connecting = Session::connect_with(&options, &cancel, provider);
            tokio::pin!(connecting);
            let mut ticks = 0;
            loop {
                tokio::select! {
                    outcome = &mut connecting => break (outcome, ticks),
                    () = tokio::time::sleep(Duration::from_millis(10)) => ticks += 1,
                }
            }
        });
        runtime.shutdown_background();
        drop(release);
        assert!(outcome.is_ok(), "{:?}", outcome.err());
        assert!(
            ticks >= 10,
            "the runtime ticked {ticks} times during a 300 ms wait"
        );
        let login = seen_login.lock().unwrap().clone().expect("a LOGIN7");
        assert_eq!(login_sspi(&login), Some(TOKEN));
    }
}

/// Spike 5.1 (plan-history.md Phase 5): what the connection reset clears, and what each kind of
/// cancel leaves behind (docs/notes/mcp-reuse-spike.md). Live: every state after a reset must equal
/// a fresh login's but for the isolation level, which the reset leaves as the last call set it (so
/// sqlh's session setup sets it, step 1), and no kind of cancel may lose the connection.
/// `--nocapture` prints what it saw:
/// `SQLH_TEST_PROFILE=test cargo test --lib spike_reset -- --ignored --nocapture`.
#[cfg(test)]
mod spike {
    use super::*;

    #[derive(Default)]
    struct Collect {
        rows: Vec<Vec<String>>,
        errors: Vec<String>,
        stopped: bool,
    }

    impl BatchSink for Collect {
        fn columns(&mut self, _: &[Column]) -> io::Result<()> {
            Ok(())
        }
        fn wants_values(&self) -> bool {
            self.rows.len() < 5
        }
        fn cell_limit(&self) -> usize {
            200
        }
        fn row(&mut self, values: &[Value]) -> io::Result<()> {
            self.rows
                .push(values.iter().map(|v| format!("{v:?}")).collect());
            Ok(())
        }
        fn skipped_row(&mut self) {}
        fn counting_stopped(&mut self, _: Stop) {
            self.stopped = true;
        }
        fn end_rows(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn end_rows_incomplete(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn rows_affected(&mut self, _: u64) -> io::Result<()> {
            Ok(())
        }
        fn message(&mut self, _: &ServerMessage) -> io::Result<()> {
            Ok(())
        }
        fn error(&mut self, e: &ServerMessage) -> io::Result<()> {
            self.errors.push(format!("{} {}", e.number, e.text));
            Ok(())
        }
    }

    struct Open;

    impl Backpressure for Open {
        fn is_full(&self) -> bool {
            false
        }
        fn room(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    const STATE: &str = "SELECT OBJECT_ID(N'tempdb..#spike') AS temp, @@TRANCOUNT AS trancount, \
        DB_NAME() AS db, @@LOCK_TIMEOUT AS lock_timeout, @@OPTIONS & 16384 AS xact_abort, \
        (SELECT deadlock_priority FROM sys.dm_exec_sessions WHERE session_id = @@SPID) \
        AS deadlock_priority, \
        (SELECT transaction_isolation_level FROM sys.dm_exec_sessions WHERE session_id = @@SPID) \
        AS isolation, @@SPID AS spid;";

    /// STATE's `isolation` column, and the values of it the spike meets
    /// (`sys.dm_exec_sessions.transaction_isolation_level`: 2 READ COMMITTED, 4 SERIALIZABLE).
    const ISOLATION: usize = 6;
    const READ_COMMITTED: &str = "Int(2)";
    const SERIALIZABLE: &str = "Int(4)";

    const DIRTY: &str = "CREATE TABLE #spike (i int); SET LOCK_TIMEOUT 1234; SET XACT_ABORT ON; \
        SET DEADLOCK_PRIORITY HIGH; SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; \
        BEGIN TRANSACTION; USE master;";

    /// A state without its isolation level.
    fn but_isolation(state: &[String]) -> Vec<String> {
        let mut rest = state.to_vec();
        rest.remove(ISOLATION);
        rest
    }

    async fn run(
        s: &mut Session,
        label: &str,
        sql: &str,
        timeout: u32,
        counting: Counting,
        cancel: &Cancel,
    ) -> Option<Vec<String>> {
        let mut sink = Collect::default();
        let started = Instant::now();
        let outcome = s
            .run(sql.into(), timeout, counting, cancel, &mut sink, &Open)
            .await;
        println!(
            "{label:<40} {:>5} ms  outcome {:<60} rows {:?} errors {:?} stopped {} dead {}",
            started.elapsed().as_millis(),
            format!("{:?}", outcome.err()),
            sink.rows.first(),
            sink.errors,
            sink.stopped,
            s.connection_dead()
        );
        assert!(!s.connection_dead(), "{label}: the connection was kept");
        sink.rows.into_iter().next()
    }

    async fn state(s: &mut Session, label: &str) -> Vec<String> {
        run(s, label, STATE, 30, Counting::EXACT, &Cancel::new())
            .await
            .expect("a state row")
    }

    /// Runs `sql` and cancels it from outside after `after`.
    async fn cancelled(s: &mut Session, label: &str, sql: &str, after: Duration) {
        let root = Cancel::new();
        let call = root.child();
        let canceller = async move {
            tokio::time::sleep(after).await;
            root.cancel();
        };
        tokio::join!(run(s, label, sql, 30, Counting::EXACT, &call), canceller);
    }

    #[test]
    #[ignore = "live spike: needs SQLH_TEST_PROFILE"]
    fn spike_reset() {
        let profile = std::env::var("SQLH_TEST_PROFILE").expect("SQLH_TEST_PROFILE");
        let opts = crate::cli::Opts {
            profile: Some(profile),
            database: Some("sqlh_test".into()),
            ..Default::default()
        };
        let settings = match crate::config::resolve(&opts) {
            Ok(settings) => settings,
            Err(failure) => panic!("settings: {}", failure.message),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut s = Session::connect(&settings.connect, &Cancel::new())
                .await
                .expect("connected");
            println!("-- 1. what the reset clears, and what it doesn't");
            let fresh = state(&mut s, "fresh login").await;
            assert_eq!(fresh[ISOLATION], READ_COMMITTED, "a fresh login's level");
            run(
                &mut s,
                "dirty batch",
                DIRTY,
                30,
                Counting::EXACT,
                &Cancel::new(),
            )
            .await;
            let dirty = state(&mut s, "after the dirty batch, no reset").await;
            assert_ne!(
                but_isolation(&dirty),
                but_isolation(&fresh),
                "after the dirty batch, no reset: the state is still there"
            );
            assert_eq!(dirty[ISOLATION], SERIALIZABLE, "the dirty batch's level");
            // The reset restores everything else, but not the isolation level: on SQL Server
            // (all but Azure SQL Database) sp_reset_connection leaves it as the last call set
            // it, so a reused connection must set its own (`run::session_setup`). This drives
            // the session directly, without sqlh's setup.
            s.reset_before_next();
            let reset = state(&mut s, "after the dirty batch, reset").await;
            assert_eq!(
                but_isolation(&reset),
                but_isolation(&fresh),
                "after the dirty batch, reset: all but the isolation level"
            );
            assert_eq!(
                reset[ISOLATION], SERIALIZABLE,
                "after the dirty batch, reset: the isolation level is the dirty batch's still \
                 (if this fails, the server's reset restores it, and the setup's SET is only a \
                 safeguard)"
            );
            assert_eq!(
                state(&mut s, "the call after that").await,
                reset,
                "the call after that"
            );
            // sqlh's session setup, as a read-only call sends it after the reset: it sets the
            // level, and the reset after it leaves a state equal to a fresh login's again.
            let setup = crate::run::session_setup(settings.lock_timeout, false, true);
            run(
                &mut s,
                "sqlh's session setup",
                &setup,
                30,
                Counting::EXACT,
                &Cancel::new(),
            )
            .await;
            assert_eq!(
                state(&mut s, "after sqlh's session setup").await[ISOLATION],
                READ_COMMITTED,
                "after sqlh's session setup"
            );
            s.reset_before_next();
            assert_eq!(
                state(&mut s, "after it, reset").await,
                fresh,
                "after sqlh's session setup, reset"
            );

            println!("-- 2. what the reset costs");
            for _ in 0..4 {
                assert_eq!(state(&mut s, "no reset").await, fresh, "no reset");
            }
            for _ in 0..4 {
                s.reset_before_next();
                assert_eq!(state(&mut s, "reset").await, fresh, "reset");
            }

            println!("-- 3. a cancel from outside, mid-wait, in an open transaction");
            cancelled(
                &mut s,
                "cancelled wait",
                "BEGIN TRANSACTION; CREATE TABLE #spike (i int); WAITFOR DELAY '00:00:10';",
                Duration::from_millis(400),
            )
            .await;
            assert_ne!(
                state(&mut s, "after it, no reset").await,
                fresh,
                "after it, no reset: the state is still there"
            );
            s.reset_before_next();
            assert_eq!(
                state(&mut s, "after it, reset").await,
                fresh,
                "after it, reset"
            );

            println!("-- 4. the bounded count of hidden rows stops (sqlh's own attention)");
            run(
                &mut s,
                "count stop",
                "BEGIN TRANSACTION; CREATE TABLE #spike (i int); \
                 SELECT a.object_id FROM sys.all_columns AS a CROSS JOIN sys.all_columns AS b;",
                30,
                Counting::BOUNDED,
                &Cancel::new(),
            )
            .await;
            s.reset_before_next();
            assert_eq!(
                state(&mut s, "after it, reset").await,
                fresh,
                "after it, reset"
            );

            println!("-- 5. -t expires mid-wait");
            run(
                &mut s,
                "timeout",
                "BEGIN TRANSACTION; CREATE TABLE #spike (i int); WAITFOR DELAY '00:00:05';",
                1,
                Counting::EXACT,
                &Cancel::new(),
            )
            .await;
            s.reset_before_next();
            assert_eq!(
                state(&mut s, "after it, reset").await,
                fresh,
                "after it, reset"
            );

            println!("-- 6. a cancel before the request is sent");
            let root = Cancel::new();
            let call = root.child();
            root.cancel();
            run(
                &mut s,
                "cancelled before sending",
                STATE,
                30,
                Counting::EXACT,
                &call,
            )
            .await;
            s.reset_before_next();
            assert_eq!(
                state(&mut s, "after it, reset").await,
                fresh,
                "after it, reset"
            );

            println!("-- 7. a cancel while rows are streaming");
            cancelled(
                &mut s,
                "cancelled mid-rows",
                "BEGIN TRANSACTION; CREATE TABLE #spike (i int); \
                 SELECT a.object_id FROM sys.all_columns AS a CROSS JOIN sys.all_columns AS b;",
                Duration::from_millis(300),
            )
            .await;
            s.reset_before_next();
            assert_eq!(
                state(&mut s, "after it, reset").await,
                fresh,
                "after it, reset"
            );
            s.close().await;
        });
    }
}
