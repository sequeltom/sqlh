//! The only module that touches mssql-tds. Everything it exposes is our own type, so the
//! driver can be upgraded or swapped here without touching the rest of the tool.

pub mod phases;
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

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{
    BatchErrorMode, CursorColumn, CursorPoll, ExecuteOptions, ResultSet, StatementResult, TdsClient,
};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{CancelHandle, EncryptionOptions, EncryptionSetting, TdsResult};
use mssql_tds::datatypes::row_writer::DiscardRowWriter;
use mssql_tds::error::{Error as TdsError, SqlErrorInfo, SqlInfoMessage};
use mssql_tds::message::login_options::ApplicationIntent;

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

pub struct ConnectOptions {
    pub server: String,
    pub database: Option<String>,
    pub auth: Auth,
    pub encrypt: Encrypt,
    pub trust_server_certificate: bool,
    pub login_timeout: u32,
    pub read_only_intent: bool,
    pub multi_subnet_failover: bool,
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
/// (DESIGN.md §5.1 and §5.2; the sinks refuse a call out of this order, PLAN.md 2.12):
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

/// Output backpressure (PLAN.md 2.3): while too much output waits for a slow reader, `run`
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
/// sent, enforced here and not by the driver, whose `timeout_secs` adds up only the time its
/// transport reads wait and so never expires for data that is already in the buffer, or for
/// work sqlh does between reads (DESIGN.md §4.4).
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
        let mut context = ClientContext::default();
        context.application_name = "sqlh".into();
        context.database = options.database.clone().unwrap_or_default();
        match &options.auth {
            Auth::Integrated => context.tds_authentication_method = TdsAuthenticationMethod::SSPI,
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
        // until the deadline (14 s with -l 15). An agent can retry itself, and sooner.
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
        context.multi_subnet_failover = options.multi_subnet_failover;

        let source = data_source(&options.server);
        let provider = TdsConnectionProvider::new();
        let connecting = provider.create_client(context, &source, Some(&cancel.handle));
        // The driver drops a pending attempt itself when its handle is cancelled (0.2.0 wraps
        // the whole connect in `run_until_cancelled`; with this race removed, a cancel during a
        // TCP attempt and during a login each ended the run within 100 ms). Racing sqlh's own
        // flag keeps the cancel independent of that. Dropping an attempt closes the socket, and
        // a login that never finished leaves nothing to clean up. Only a blocking call, such as
        // opening a pipe, outlasts a cancel.
        let mut client = tokio::select! {
            client = connecting => client.map_err(|e| connect_error(e, options))?,
            () = cancel.cancelled() => {
                return Err(DbError::Cancelled("cancelled while connecting".into()));
            }
        };
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
    /// server's environment-change token for a `USE` (PLAN.md 2.11); read it right after the
    /// connect and it is the database the first batch starts in.
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
/// clock starts only after them, and `--exact-count` is the way to read on without cancelling.
const COUNT_STOP_UNCONFIRMED: &str = "stopped counting the rows that are not shown, but the \
    server didn't confirm the cancel, so the connection was closed; the rows shown are \
    complete (--exact-count reads every row instead)";

/// A driver error mid-run, as `DbError` (see `Session::failure`). `dead` is whether the driver
/// marked the connection dead. `own_stop` is whether the error ends a count that sqlh itself
/// stopped, and not a cancel from outside.
fn classify(error: TdsError, dead: bool, own_stop: bool) -> DbError {
    match error {
        TdsError::OperationCancelledError(_) if !dead => {
            DbError::Cancelled("the statement was cancelled; the connection stayed usable".into())
        }
        TdsError::OperationCancelledError(_) if own_stop => {
            DbError::Cancelled(COUNT_STOP_UNCONFIRMED.into())
        }
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
/// SMB. A protocol, a pipe path or LocalDB passes through as written. An IPv6 address can't
/// take `tcp:`, because the driver reads its colons as part of the name; without a port it
/// would get the pipe fallback too (an unreachable address held a run for 23 s), so it gets the
/// default port, 1433, and a port makes the driver use TCP only. Other text with a colon, such
/// as `db01:1433`, is no name a host answers to, and the driver fails on it at once.
fn data_source(server: &str) -> Cow<'_, str> {
    let server = server.trim();
    let host = server.split(['\\', '/', ',']).next().unwrap_or_default();
    let local = [".", "(local)", "(localdb)", "localhost", "127.0.0.1", "::1"]
        .iter()
        .any(|name| host.eq_ignore_ascii_case(name));
    if local || host.is_empty() {
        Cow::Borrowed(server)
    } else if !server.contains(':') {
        Cow::Owned(format!("tcp:{server}"))
    } else if looks_like_ipv6(server) && !server.contains([',', '\\']) {
        Cow::Owned(format!("{server},1433"))
    } else {
        Cow::Borrowed(server)
    }
}

/// The protocols a data source can name, as the driver's parser accepts them.
const PROTOCOLS: [&str; 4] = ["tcp", "np", "lpc", "admin"];

/// The driver's test for an IPv6 address: `::`, or more than one colon.
fn looks_like_ipv6(server: &str) -> bool {
    server.contains("::") || server.matches(':').count() > 1
}

/// Splits a protocol prefix from the rest, the way the driver does
/// (`datasource_parser.rs`, `parse_protocol`): at the first colon, unless the text looks like
/// an IPv6 address, and only for `tcp`, `np`, `lpc` or `admin`, in any case. Anything else,
/// such as `http:db01`, stays part of the name. The protocol comes back in lowercase.
fn split_protocol(server: &str) -> (Option<&'static str>, &str) {
    if let Some(colon) = server.find(':')
        && !looks_like_ipv6(server)
    {
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
/// `tcp:`.
///
/// Local pipes stay: `np:` with `.`, `(local)`, `localhost`, `127.0.0.1`, `::1` or this
/// computer's name (`COMPUTERNAME`), a UNC path to one of those (`\\.\pipe\...`), `lpc:` (shared
/// memory, which the driver only allows locally), `admin:` (a TCP connection) and LocalDB. Text
/// the driver would reject before opening anything, such as `np:` with no host, is left to it.
///
/// The message starts with the server text, so a caller can say where it came from:
/// `-S np:db01: Named Pipes ...`.
pub fn check_server(server: &str) -> Result<(), String> {
    check(server, std::env::var("COMPUTERNAME").ok().as_deref())
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
        TdsError::TimeoutError(_) => {
            return DbError::Timeout(format!(
                "login timeout: {target} didn't complete the login within {} s; check that it \
                 is a reachable SQL Server and not overloaded, or raise -l",
                options.login_timeout.saturating_add(1)
            ));
        }
        TdsError::OperationCancelledError(_) => {
            return DbError::Cancelled("cancelled while connecting".into());
        }
        TdsError::SqlServerError { diagnostics } => {
            let detail = sql_errors(&diagnostics.errors);
            let hint = match diagnostics.errors.first().map(|e| e.number) {
                Some(18456) => {
                    "check the login and password (SQLCMDPASSWORD), and that the login may use this database"
                }
                Some(4060) => "the database may not exist or the login may lack access; check -d",
                Some(18452) => {
                    "the server rejected Windows auth for this login; use -U with a SQL login"
                }
                _ => "",
            };
            (detail, hint)
        }
        TdsError::TlsError(_)
        | TdsError::TlsHandshakeError { .. }
        | TdsError::NoServerCertificate => (
            error.to_string(),
            "if the server uses a self-signed certificate, pass -C to trust it; old servers without TLS 1.2 may need -No",
        ),
        TdsError::CertificateExpired | TdsError::CertificateMismatch => (
            error.to_string(),
            "pass -C to trust the server certificate anyway",
        ),
        TdsError::Security(_) => (
            error.to_string(),
            "Windows auth failed; check Kerberos/SPN setup, or use -U with a SQL login",
        ),
        TdsError::Io(_) | TdsError::ConnectionError(_) | TdsError::ConnectionClosed(_) => (
            error.to_string(),
            "check the server name; for a named instance with SQL Browser unreachable, try host,port",
        ),
        _ => (error.to_string(), ""),
    };
    // The direct Schannel path reports TLS failures as generic errors carrying SSPI status
    // names, so match the text too.
    let hint = tls_hint(&detail).unwrap_or(hint);
    let mut message = format!("cannot connect to {target}: {detail}");
    if !hint.is_empty() {
        message.push_str("; ");
        message.push_str(hint);
    }
    DbError::Connect(message)
}

fn tls_hint(detail: &str) -> Option<&'static str> {
    let has = |needles: &[&str]| needles.iter().any(|n| detail.contains(n));
    if has(&[
        "SEC_E_UNTRUSTED_ROOT",
        "CERT_E_UNTRUSTEDROOT",
        "CERT_E_CHAINING",
    ]) {
        Some(
            "this machine doesn't trust the server's certificate (typical for a self-signed one): \
             pass -C, or save it in the profile with: sqlh profile add NAME -C",
        )
    } else if has(&["SEC_E_WRONG_PRINCIPAL", "CERT_E_CN_NO_MATCH"]) {
        Some(
            "the server's certificate names a different host: connect by the name on the \
             certificate, or pass -C",
        )
    } else if has(&["SEC_E_CERT_EXPIRED", "CERT_E_EXPIRED"]) {
        Some("the server's certificate has expired: pass -C to trust it anyway")
    } else if has(&["Schannel", "SEC_E_", "TLS"]) {
        Some(
            "the TLS handshake failed: a self-signed certificate needs -C; an old server without \
             TLS 1.2 may need -No",
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Backpressure, COUNT_STOP_UNCONFIRMED, Cancel, Cause, Counting, DbError, Drain, PACE_TICKS,
        TdsError, check, classify, data_source, filled, split_protocol, tcp_form, tls_hint,
        wait_for_output,
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

    // PLAN.md 2.15: the budget starts with the first hidden row, a timer ends it while the server
    // is still looking for the next row, and a result that ends first leaves nothing behind.
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
            // The budget fires first; the deadline after it does not take its place.
            let soon = Instant::now() + Duration::from_millis(20);
            let mut drain = Drain::new(Some(Duration::from_millis(5)), Some(soon), &run);
            drain.counted_row();
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert_eq!(drain.cause(), Some(Cause::Budget));
            assert!(drain.fired() && !drain.deadline_fired());
        });
        runtime().block_on(async {
            // The deadline fires first, and a count stopped by it later keeps that cause.
            let soon = Instant::now() + Duration::from_millis(5);
            let mut drain = Drain::new(Some(Duration::from_millis(30)), Some(soon), &run);
            drain.counted_row();
            tokio::time::sleep(Duration::from_millis(60)).await;
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

    // PLAN.md 2.15: what a cancel is called depends only on what sqlh observed. A count that
    // sqlh itself stopped and the server never acknowledged (the driver marked the connection
    // dead) names the count and the way round it; the user's cancel, and any other error,
    // keep their words.
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
        let own = words(classify(cancelled(), true, true));
        assert_eq!(own, COUNT_STOP_UNCONFIRMED);
        assert!(
            own.starts_with("stopped counting the rows that are not shown, but the server didn't")
                && own.contains("the connection was closed")
                && own.contains("the rows shown are complete")
                && own.contains("--exact-count"),
            "{own}"
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
            // IPv6 takes no `tcp:` prefix, but without a port it gets one: otherwise the
            // driver would go on to open a pipe to the address.
            ("2001:db8::5", "2001:db8::5,1433"),
            (" fe80::1%12 ", "fe80::1%12,1433"),
            ("2001:db8::5,1433", "2001:db8::5,1433"),
            (r"2001:db8::5\SQLEXPRESS", r"2001:db8::5\SQLEXPRESS"),
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
            // An IPv6 address has colons of its own, so nothing is split off, even after what
            // looks like a protocol.
            ("::1", None, "::1"),
            ("2001:db8::1,1433", None, "2001:db8::1,1433"),
            ("np:::1", None, "np:::1"),
            ("tcp:2001:db8:0:0:0:0:0:5", None, "tcp:2001:db8:0:0:0:0:0:5"),
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
        let untrusted = "Unexpected Implementation Error: Schannel TLS handshake failed: \
                         InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)";
        assert!(tls_hint(untrusted).unwrap().contains("pass -C"));
        assert!(
            tls_hint("SEC_E_WRONG_PRINCIPAL")
                .unwrap()
                .contains("different host")
        );
        assert!(tls_hint("SEC_E_ILLEGAL_MESSAGE").unwrap().contains("-No"));
        assert_eq!(tls_hint("Connection refused"), None);
    }
}

/// `Session::run` against a fake server on a loopback socket: a TDS 7.4 endpoint that answers
/// the prelogin (no encryption) and the login, then plays a script, so the real execution path
/// (driver included) meets a deadline at exactly the points a live server can't be made to
/// hit on demand (R15 follow-ups A and B). Adapted from the reviewer's probe.
#[cfg(test)]
pub(crate) mod fake_server {
    use super::{
        Auth, Backpressure, BatchSink, Cancel, Column, ConnectOptions, Counting, DbError, Encrypt,
        RETIRED, ServerMessage, Session, Stop, Value,
    };
    use std::future::Future;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::pin::Pin;
    use std::time::{Duration, Instant};

    /// The type of the next whole message from the client, after reading all its packets
    /// (`None` once the client is gone). 0x12 prelogin, 0x10 login, 1 SQL batch, 6 attention.
    fn receive(s: &mut TcpStream) -> Option<u8> {
        loop {
            let mut header = [0u8; 8];
            s.read_exact(&mut header).ok()?;
            let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
            let mut payload = vec![0; length.checked_sub(8)?];
            s.read_exact(&mut payload).ok()?;
            if header[1] & 1 != 0 {
                return Some(header[0]);
            }
        }
    }

    fn send(s: &mut TcpStream, payload: &[u8]) {
        send_packet(s, payload, true);
    }

    /// One packet of a server message; `last` marks the end of the message (EOM).
    fn send_packet(s: &mut TcpStream, payload: &[u8], last: bool) {
        let length = u16::try_from(payload.len() + 8).unwrap();
        let [high, low] = length.to_be_bytes();
        s.write_all(&[4, u8::from(last), high, low, 0, 0, 1, 0])
            .unwrap();
        s.write_all(payload).unwrap();
    }

    /// DONE, final, with a row count.
    fn done(rows: u64) -> Vec<u8> {
        let mut token = vec![0xfd, 0x10, 0, 0xc1, 0];
        token.extend(rows.to_le_bytes());
        token
    }

    /// One `int` column named x, `rows` rows of it, and the final DONE.
    pub(crate) fn int_rows(rows: u64) -> Vec<u8> {
        let mut reply = vec![0x81, 1, 0, 0, 0, 0, 0, 0, 0, 0x38, 1, b'x', 0];
        for _ in 0..rows {
            reply.extend([0xd1, 1, 0, 0, 0]);
        }
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

    /// Starts the fake server; returns its address for `-S`.
    pub(crate) fn serve(script: Script) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            if receive(&mut s) != Some(0x12) {
                return;
            }
            send(
                &mut s,
                &[0, 0, 11, 0, 6, 1, 0, 17, 0, 1, 0xff, 17, 0, 0, 0, 0, 0, 2],
            );
            if receive(&mut s) != Some(0x10) {
                return;
            }
            send(&mut s, &login_reply());
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
                        let mut reply = vec![0x81, 1, 0, 0, 0, 0, 0, 0, 0, 0x38, 1, b'x', 0];
                        for _ in 0..rows {
                            reply.extend([0xd1, 1, 0, 0, 0]);
                        }
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
            let options = ConnectOptions {
                server,
                database: None,
                // Synthetic strings for the fake server, unrelated to any real account.
                auth: Auth::Sql {
                    user: "fake".into(),
                    password: crate::secret::Secret::new("fake".into()),
                },
                encrypt: Encrypt::Optional,
                trust_server_certificate: false,
                login_timeout: 5,
                read_only_intent: false,
                multi_subnet_failover: false,
            };
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
            Err(DbError::Cancelled(message)) => assert_eq!(message, super::COUNT_STOP_UNCONFIRMED),
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
}

/// Spike 5.1 (PLAN.md Phase 5): what the connection reset clears, and what each kind of cancel
/// leaves behind (docs/notes/mcp-reuse-spike.md). Live: every state after a reset must equal a fresh
/// login's, and no kind of cancel may lose the connection. `--nocapture` prints what it saw:
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
        AS deadlock_priority, @@SPID AS spid;";

    const DIRTY: &str = "CREATE TABLE #spike (i int); SET LOCK_TIMEOUT 1234; SET XACT_ABORT ON; \
        SET DEADLOCK_PRIORITY HIGH; BEGIN TRANSACTION; USE master;";

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
            println!("-- 1. what the reset clears");
            let fresh = state(&mut s, "fresh login").await;
            run(
                &mut s,
                "dirty batch",
                DIRTY,
                30,
                Counting::EXACT,
                &Cancel::new(),
            )
            .await;
            assert_ne!(
                state(&mut s, "after the dirty batch, no reset").await,
                fresh,
                "after the dirty batch, no reset: the state is still there"
            );
            s.reset_before_next();
            assert_eq!(
                state(&mut s, "after the dirty batch, reset").await,
                fresh,
                "after the dirty batch, reset"
            );
            assert_eq!(
                state(&mut s, "the call after that").await,
                fresh,
                "the call after that"
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
