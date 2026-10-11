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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    Sql { user: String, password: String },
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
///   order is `end_rows`, the messages held, then the errors.
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
    /// The count of hidden rows was stopped (`Counting`): the rows counted so far are a lower
    /// bound, and the rest of the batch was cancelled. Called once, in place of the rows that
    /// would have followed, just before `end_rows`.
    fn counting_stopped(&mut self);
    fn end_rows(&mut self) -> io::Result<()>;
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

/// Waits while the output is backed up, unless the run is cancelled first.
async fn wait_for_output(gate: &dyn Backpressure, cancel: &Cancel) -> Result<(), DbError> {
    if !gate.is_full() {
        return Ok(());
    }
    tokio::select! {
        room = gate.room() => room.map_err(DbError::Output),
        () = cancel.cancelled() => Err(DbError::Cancelled(
            "cancelled while waiting for the reader; the connection was closed".into(),
        )),
    }
}

/// How long a read-only run goes on counting the rows it does not show (DESIGN.md §4.4).
pub const COUNT_BUDGET: Duration = Duration::from_millis(250);

/// How a batch counts the rows it does not show: past `--max-rows`, in a result `--max-results`
/// hides, and after the `--max-output` budget ran out.
///
/// Exact counting reads them all, which costs the size of the result (1 to 4 s for millions of
/// narrow rows, minutes for a few gigabytes) to learn a number. A bounded count stops after
/// `budget`, counted from the first hidden row of a result, and sends the server an attention,
/// the cancel Ctrl+C uses: the total is then a lower bound, and the rest of the batch does not
/// run (`BatchSink::counting_stopped`). A stopped count can skip a `COMMIT`, so the caller picks
/// exact counting wherever that would lose something (docs/notes/drain-experiment.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counting {
    /// `None`: count to the end of every result.
    pub budget: Option<Duration>,
    /// When `-t` expires while only hidden rows are being counted, the rows shown are complete,
    /// so the count stops there instead of failing the run (exit 4). Not with writes allowed:
    /// the stop skips what follows, and a skipped `COMMIT` has to be a failure.
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

/// One batch's counting: the budget, the cancel handles that end the batch when it runs out,
/// and the clock. The driver gets `handle`, a child of `root`, which is a child of the run's
/// handle. Cancelling `root` therefore cancels the batch's statement without cancelling the
/// run, and the run's own cancel (Ctrl+C) still reaches the statement. The driver then does what
/// it does for any cancel of a read in progress: it sends an attention, reads on to the server's
/// acknowledgement (DONE with the ATTN flag) and settles its state.
struct Drain {
    budget: Option<Duration>,
    /// What the driver gets for the batch; `None` without a budget, and then the run's own
    /// handle is used as it always was.
    handle: Option<CancelHandle>,
    /// Holds `root` until the budget fires or the result ends. Taken by whichever comes first.
    root: Arc<Mutex<Option<CancelHandle>>>,
    /// Set just before `root` is cancelled: it tells the budget's cancel from a user's.
    fired: Arc<AtomicBool>,
    /// When the first hidden row of the current result was counted.
    began: Option<Instant>,
    timer: Option<tokio::task::JoinHandle<()>>,
}

impl Drain {
    fn new(budget: Option<Duration>, run: &CancelHandle) -> Drain {
        let (handle, root) = match budget {
            Some(_) => {
                let root = run.child_handle();
                (Some(root.child_handle()), Some(root))
            }
            None => (None, None),
        };
        Drain {
            budget,
            handle,
            root: Arc::new(Mutex::new(root)),
            fired: Arc::default(),
            began: None,
            timer: None,
        }
    }

    /// The handle for the driver's `execute`.
    fn handle<'a>(&'a self, run: &'a CancelHandle) -> &'a CancelHandle {
        self.handle.as_ref().unwrap_or(run)
    }

    /// Whether the budget ended the work, as opposed to the user.
    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// A result begins, or ends however it ends. The timer stops, so that it can't cancel the
    /// statements that follow.
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
                let (root, fired) = (Arc::clone(&self.root), Arc::clone(&self.fired));
                self.timer = Some(tokio::spawn(async move {
                    tokio::time::sleep(budget).await;
                    fire(&root, &fired);
                }));
            }
            Some(began) if began.elapsed() >= budget => fire(&self.root, &self.fired),
            Some(_) => {}
        }
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Cancels the batch's statement, once.
fn fire(root: &Mutex<Option<CancelHandle>>, fired: &AtomicBool) {
    if let Ok(mut cell) = root.lock()
        && let Some(handle) = cell.take()
    {
        fired.store(true, Ordering::SeqCst);
        handle.cancel();
    }
}

pub struct Session {
    client: TdsClient,
    /// Debug builds only (`SQLH_DEBUG_STOP_HOLD_MS`): how long to wait after a count stops,
    /// before telling it from a user's cancel, so that a test can cancel in that gap.
    #[cfg(debug_assertions)]
    stop_hold: Option<Duration>,
    /// How the login was made (`Auth::method`), for `connection`.
    auth: &'static str,
    /// Buffers for reading large values, made on first use.
    scratch: stream::Scratch,
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
                context.password = password.clone();
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
            auth: options.auth.method(),
            scratch: stream::Scratch::default(),
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
    /// With a bounded `counting`, a result whose hidden rows outrun the budget (or `-t`, when
    /// `Counting::timeout_stops`) ends the batch instead: the rest of it is cancelled, so the
    /// statements after that result do not run, and the sink is told (`counting_stopped`).
    pub async fn run(
        &mut self,
        sql: String,
        timeout_secs: u32,
        counting: Counting,
        cancel: &Cancel,
        sink: &mut dyn BatchSink,
        gate: &dyn Backpressure,
    ) -> Result<(), DbError> {
        let mut drain = Drain::new(counting.budget, &cancel.handle);
        let options = ExecuteOptions::new()
            .timeout_secs(timeout_secs)
            .cancel(drain.handle(&cancel.handle))
            .on_error(BatchErrorMode::Continue);
        let mut step = self.client.execute(sql, options).await;
        loop {
            self.flush_messages(sink)?;
            wait_for_output(gate, cancel).await?;
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
                    sink.counting_stopped();
                    break;
                }
                Ok(StatementResult::NoRows {
                    rows_affected: Some(count),
                }) => sink.rows_affected(count)?,
                Ok(StatementResult::NoRows {
                    rows_affected: None,
                }) => {}
                Ok(StatementResult::End) => break,
                Err(TdsError::SqlServerError { diagnostics }) => {
                    for error in &diagnostics.errors {
                        sink.error(&from_error(error))?;
                    }
                    if !self.client.has_open_batch() {
                        break;
                    }
                }
                Err(error) => return Err(self.failure(error)),
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
        match error {
            TdsError::OperationCancelledError(_) if !self.client.is_connection_dead() => {
                DbError::Cancelled(
                    "the statement was cancelled; the connection stayed usable".into(),
                )
            }
            TdsError::OperationCancelledError(_) => DbError::Cancelled(
                "the server didn't confirm the cancel, so the connection was closed".into(),
            ),
            error => runtime_error(error),
        }
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
                    self.streamed_row(plan, shown.then_some(cap)).await
                }
                None => self.plain_row(shown).await,
            };
            #[cfg(debug_assertions)]
            if step.is_err()
                && drain.fired()
                && let Some(hold) = self.stop_hold
            {
                tokio::time::sleep(hold).await;
            }
            match step {
                Ok(Row::Values(values)) => {
                    sink.row(&values)?;
                    wait_for_output(gate, cancel).await?;
                }
                Ok(Row::Hidden) => {
                    sink.skipped_row();
                    drain.counted_row();
                }
                Ok(Row::End) => break,
                Err(TdsError::SqlServerError { diagnostics }) => {
                    sink.end_rows()?;
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
                    sink.counting_stopped();
                    sink.end_rows()?;
                    return Ok(true);
                }
                Err(TdsError::TimeoutError(_))
                    if !shown
                        && counting.timeout_stops
                        && !cancel.is_cancelled()
                        && !self.client.is_connection_dead() =>
                {
                    sink.counting_stopped();
                    sink.end_rows()?;
                    return Ok(true);
                }
                Err(error) => return Err(self.failure(error)),
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
    async fn streamed_row(&mut self, plan: &[stream::Cell], cap: Option<usize>) -> TdsResult<Row> {
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
                cell => self.large_value(index, *cell, cap).await?,
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
    use super::{Counting, Drain, check, data_source, filled, split_protocol, tcp_form, tls_hint};
    use mssql_tds::core::CancelHandle;
    use std::time::Duration;

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
            let mut drain = Drain::new(Some(Duration::from_millis(20)), &run);
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
            let mut drain = Drain::new(Some(Duration::from_millis(20)), &run);
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
            let mut drain = Drain::new(Some(Duration::from_millis(10)), &run);
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
            let mut drain = Drain::new(Counting::EXACT.budget, &run);
            drain.counted_row();
            assert!(drain.handle.is_none() && !drain.fired() && drain.timer.is_none());
        });
        let (bounded, exact) = (Counting::BOUNDED, Counting::EXACT);
        assert!(bounded.timeout_stops && bounded.budget.is_some());
        assert!(!exact.timeout_stops);
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
