//! The only module that touches mssql-tds. Everything it exposes is our own type, so the
//! driver can be upgraded or swapped here without touching the rest of the tool.

pub mod phases;
mod types;
mod value;

use std::borrow::Cow;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{
    BatchErrorMode, ExecuteOptions, ResultSet, StatementResult, TdsClient,
};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{CancelHandle, EncryptionOptions, EncryptionSetting};
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

/// Receives a batch's results in wire order.
pub trait BatchSink {
    fn columns(&mut self, columns: &[Column]) -> io::Result<()>;
    /// False once the row limit is reached; remaining rows are then only counted.
    fn wants_values(&self) -> bool;
    fn row(&mut self, values: &[Value]) -> io::Result<()>;
    fn skipped_row(&mut self);
    fn end_rows(&mut self) -> io::Result<()>;
    fn rows_affected(&mut self, count: u64) -> io::Result<()>;
    fn message(&mut self, message: &ServerMessage) -> io::Result<()>;
    fn error(&mut self, error: &ServerMessage) -> io::Result<()>;
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

pub struct Session {
    client: TdsClient,
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
        // The driver checks its cancel only between attempts; dropping the attempt closes
        // the socket, and a login that never finished leaves nothing to clean up.
        let mut client = tokio::select! {
            client = connecting => client.map_err(|e| connect_error(e, options))?,
            () = cancel.cancelled() => {
                return Err(DbError::Cancelled("cancelled while connecting".into()));
            }
        };
        // Login chatter: "Changed database context to ...", "Changed language setting to ...".
        client.take_info_messages();
        Ok(Session { client })
    }

    /// One line of connection facts for --verbose.
    pub fn describe(&self) -> String {
        let version = self
            .client
            .server_version()
            .map(|v| format!("{}.{}.{}", v.major, v.minor, v.build))
            .unwrap_or_else(|| "?".into());
        format!(
            "server {} version {version}, database {}, encrypted {}",
            self.client.server_reported_name().unwrap_or("?"),
            self.client.database(),
            self.client.is_encrypted()
        )
    }

    pub fn in_transaction(&self) -> bool {
        self.client.has_active_transaction()
    }

    /// Runs one batch, streaming results, messages and errors to `sink` in order. SQL errors
    /// go to the sink and don't fail the call; only transport, timeout and output problems do.
    pub async fn run(
        &mut self,
        sql: String,
        timeout_secs: u32,
        cancel: &Cancel,
        sink: &mut dyn BatchSink,
        gate: &dyn Backpressure,
    ) -> Result<(), DbError> {
        let options = ExecuteOptions::new()
            .timeout_secs(timeout_secs)
            .cancel(&cancel.handle)
            .on_error(BatchErrorMode::Continue);
        let mut step = self.client.execute(sql, options).await;
        loop {
            self.flush_messages(sink)?;
            wait_for_output(gate, cancel).await?;
            match step {
                Ok(StatementResult::Rows) => self.read_rows(sink, gate, cancel).await?,
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

    async fn read_rows(
        &mut self,
        sink: &mut dyn BatchSink,
        gate: &dyn Backpressure,
        cancel: &Cancel,
    ) -> Result<(), DbError> {
        let columns: Vec<Column> = self
            .client
            .get_metadata()
            .iter()
            .map(types::column)
            .collect();
        sink.columns(&columns)?;
        loop {
            match self.client.next_row().await {
                Ok(Some(row)) => {
                    if sink.wants_values() {
                        let values: Vec<Value> = row.into_iter().map(value::convert).collect();
                        sink.row(&values)?;
                        wait_for_output(gate, cancel).await?;
                    } else {
                        sink.skipped_row();
                    }
                }
                Ok(None) => break,
                Err(TdsError::SqlServerError { diagnostics }) => {
                    sink.end_rows()?;
                    self.flush_messages(sink)?;
                    for error in &diagnostics.errors {
                        sink.error(&from_error(error))?;
                    }
                    return Ok(());
                }
                Err(error) => return Err(self.failure(error)),
            }
        }
        sink.end_rows()?;
        Ok(())
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
/// Pipes, and it opens the pipe synchronously: for an unreachable remote host that blocks the
/// whole runtime for about 21 s while SMB gives up, and neither -l nor Ctrl+C can end it. sqlh
/// connects over TCP (DESIGN.md §4.2), so a remote name gets `tcp:`. Local names keep the
/// driver's order (shared memory, TCP, a local pipe), which never reaches SMB. A protocol, a
/// colon (IPv6, which takes no prefix), a pipe path or LocalDB passes through as written.
fn data_source(server: &str) -> Cow<'_, str> {
    let server = server.trim();
    let host = server.split(['\\', '/', ',']).next().unwrap_or_default();
    let local = [".", "(local)", "(localdb)", "localhost", "127.0.0.1"]
        .iter()
        .any(|name| host.eq_ignore_ascii_case(name));
    if local || host.is_empty() || server.contains(':') {
        Cow::Borrowed(server)
    } else {
        Cow::Owned(format!("tcp:{server}"))
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
    use super::{data_source, tls_hint};

    #[test]
    fn remote_names_connect_over_tcp_only() {
        for (server, expected) in [
            ("db01", "tcp:db01"),
            (" db01.corp.example ", "tcp:db01.corp.example"),
            (r"db01\SALES", r"tcp:db01\SALES"),
            ("db01,14330", "tcp:db01,14330"),
            ("10.1.2.3", "tcp:10.1.2.3"),
            // A protocol is the user's choice.
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
            (r"(localdb)\MSSQLLocalDB", r"(localdb)\MSSQLLocalDB"),
            // IPv6 takes no prefix, and a pipe path names its protocol.
            ("::1", "::1"),
            ("2001:db8::5,1433", "2001:db8::5,1433"),
            (r"\\db01\pipe\sql\query", r"\\db01\pipe\sql\query"),
        ] {
            assert_eq!(data_source(server), expected, "{server}");
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
