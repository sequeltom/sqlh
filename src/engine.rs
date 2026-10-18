//! The connection-reuse engine (PLAN.md Phase 5.2; docs/mcp-mode.md §5; owner decisions 14,
//! 15, 17 and 19). It keeps a small pool of open connections per profile and runs each call
//! through the execution core (`run`), exactly as a CLI run does, on a connection that is
//! reset first, so a call behaves like one CLI run without the connect. It has no MCP in it:
//! `sqlh mcp` is its first front end, and a daemon behind the CLI could use it unchanged.
//!
//! - **Pool:** at most `max` connections per profile, opened only when a call needs one. A call
//!   beyond them waits for one, at most its timeout. A connection idle longer than `idle_after`
//!   is closed (`sweep`, and when a call would take it).
//! - **Reset:** a reused connection's first request, the session setup (`run::setup`), carries
//!   the reset (`Session::reset_before_next`), whose acknowledgement the driver verifies; the
//!   call's `USE` follows (`run::use_database`), as after a connect.
//! - **Probe:** that setup gets `REUSE_PROBE` (5 s), not the call's timeout. One it cuts while
//!   the server still answers (the attention acknowledged within the driver's 5 s) is reset and
//!   run again on the same connection with the call's timeout.
//! - **Retry:** a reused connection that turns out dead at the setup or the `USE` (a network
//!   drop, a session a DBA killed, a reset the server didn't acknowledge, a connection gone
//!   silent) is replaced and the call run again, once, silently: nothing of the call has run,
//!   and every call is a read in a rolled-back wrapper. A failure after that point is the
//!   call's own.
//! - **After the call:** a connection the driver kept is put back, with its wrapper rolled back
//!   if a cancel left it open; one the driver marked dead, or one past its settle bound, is
//!   dropped (decision 17, amended). A cancelled call gets `SETTLE_GRACE` to settle; past it,
//!   the call is dropped where it stands, and its connection with it.

use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::config::Settings;
use crate::db::{Backpressure, Cancel, DbError, Session};
use crate::error::{Exit, Failure};
use crate::output::{Advice, Output};
use crate::run::{self, Accounting, Opened, Prepared, RunOutcome, Stats};

/// How long a cancelled call may take to settle before it is dropped, closing its connection:
/// the driver's own wait for an attention's acknowledgement.
const SETTLE_GRACE: Duration = Duration::from_secs(5);

/// The longest a rollback of a connection being put back may take, the driver's own
/// timeout and attention wait included: past it the connection is dropped, so a server gone
/// silent can't hold up the call's reply (the engine tests' finding).
const ROLLBACK_WAIT: Duration = Duration::from_secs(2);

/// The default number of connections a profile's pool may hold (decision 19).
pub(crate) const DEFAULT_POOL: usize = 2;

/// The most connections a profile's pool may hold (`--pool`).
pub(crate) const MAX_POOL: usize = 8;

/// The default time a connection may stay idle before it is closed (decision 19).
pub(crate) const DEFAULT_IDLE: Duration = Duration::from_secs(15 * 60);

/// The engine: a pool for each profile it serves.
pub(crate) struct Engine {
    pools: Vec<Pool>,
    /// How long a connection may stay idle; `None` keeps connections open.
    idle_after: Option<Duration>,
}

/// One profile's connections.
struct Pool {
    name: String,
    settings: Settings,
    /// Connections not in use, the most recently used last.
    idle: RefCell<Vec<Idle>>,
    /// One permit per connection the pool may hold: a call holds one while it runs.
    permits: Semaphore,
    /// How many connections the pool may hold.
    max: usize,
}

struct Idle {
    session: Session,
    since: Instant,
}

/// One call: the profile it runs on (an index into `Engine::profiles`), what to run, and where.
pub(crate) struct Call<'a> {
    pub(crate) profile: usize,
    pub(crate) prepared: &'a Prepared,
    /// The call's `-t`: its settings' query timeout, or less.
    pub(crate) timeout: u32,
    pub(crate) out: &'a mut dyn Output,
    pub(crate) cancel: &'a Cancel,
    /// The call's clock, for its batch lines and closing lines.
    pub(crate) started: Instant,
}

/// What a call's output never has to wait for: it is written to memory.
struct Unbounded;

impl Backpressure for Unbounded {
    fn is_full(&self) -> bool {
        false
    }
    fn room(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_>> {
        Box::pin(async { Ok(()) })
    }
}

impl Engine {
    /// An engine for `profiles` (name and settings, in order), each with at most `max`
    /// connections (1 to `MAX_POOL`), closing those idle longer than `idle_after`.
    pub(crate) fn new(
        profiles: Vec<(String, Settings)>,
        max: usize,
        idle_after: Option<Duration>,
    ) -> Engine {
        let max = max.clamp(1, MAX_POOL);
        let pools = profiles
            .into_iter()
            .map(|(name, settings)| Pool {
                name,
                settings,
                idle: RefCell::new(Vec::new()),
                permits: Semaphore::new(max),
                max,
            })
            .collect();
        Engine { pools, idle_after }
    }

    /// The profiles served, in order: a call names one by its index.
    pub(crate) fn profiles(&self) -> impl Iterator<Item = &str> {
        self.pools.iter().map(|pool| pool.name.as_str())
    }

    /// A profile's settings.
    pub(crate) fn settings(&self, profile: usize) -> &Settings {
        &self.pools[profile].settings
    }

    /// How many connections a profile's pool holds idle now.
    #[cfg(test)]
    #[allow(dead_code)] // for the engine's tests
    pub(crate) fn idle_count(&self, profile: usize) -> usize {
        self.pools[profile].idle.borrow().len()
    }

    /// Runs one call into its output. The output is left for the caller to finish with the
    /// outcome's closing lines.
    pub(crate) async fn run(&self, call: Call<'_>) -> RunOutcome {
        let Call {
            profile,
            prepared,
            timeout,
            out,
            cancel,
            started,
        } = call;
        let pool = &self.pools[profile];
        let mut counts = Accounting::default();
        if let Some(early) = run::preflight(&pool.settings, prepared, out) {
            return counts.outcome(early);
        }
        let result = {
            // On the heap: the run's future is large, the more so in a debug build.
            let mut work = Box::pin(self.run_pooled(
                pool,
                prepared,
                timeout,
                out,
                cancel,
                &mut counts,
                started,
            ));
            tokio::select! {
                result = &mut work => result,
                () = cancel.cancelled() => match tokio::time::timeout(SETTLE_GRACE, &mut work).await {
                    Ok(result) => result,
                    // Dropping the work closes its connection, which stops the server too.
                    Err(_) => Err(Failure::new(
                        Exit::Cancelled,
                        format!(
                            "cancelled: the call didn't finish within {} s of the cancel, so \
                             its connection was closed",
                            SETTLE_GRACE.as_secs()
                        ),
                    )),
                },
            }
        };
        counts.outcome(result)
    }

    /// `run` once a connection is needed: waits for one, opens it (retrying once on a reused
    /// connection that turns out dead), runs the call and puts the connection back.
    #[allow(clippy::too_many_arguments)]
    async fn run_pooled(
        &self,
        pool: &Pool,
        prepared: &Prepared,
        timeout: u32,
        out: &mut dyn Output,
        cancel: &Cancel,
        counts: &mut Accounting,
        started: Instant,
    ) -> Result<Stats, Failure> {
        let _permit = wait_for_connection(pool, timeout, cancel, started).await?;
        let mut retried = false;
        // The connect time a call reports runs from here to the open that worked: a reused
        // connection that opened at once reports 0, and one that took a probe, a second
        // setup or a replacement reports what that took, not hiding it in the total.
        let first_started = Instant::now();
        loop {
            let connect_started = Instant::now();
            // A retry connects afresh: the connection it replaces died while idle, and so,
            // most likely, did the profile's other idle ones, which go too.
            let reusable = if retried {
                pool.idle.borrow_mut().clear();
                None
            } else {
                self.take_idle(pool)
            };
            let (mut session, reused) = match reusable {
                Some(mut session) => {
                    session.reset_before_next();
                    (session, true)
                }
                None => (
                    Session::connect(&pool.settings.connect, cancel)
                        .await
                        .map_err(|e| run::db_failure(e, pool.settings.limits.advice))?,
                    false,
                ),
            };
            // A reused connection's setup (which carries the reset) gets the probe's few
            // seconds, not the call's whole timeout, so one that went silent while idle shows
            // up quickly. If the probe cuts a setup the server still answers (it acknowledged
            // the attention), the connection is alive, only slow: it is kept and set up again,
            // reset first to undo what the cut setup did, with the call's own timeout.
            let mut probe = reused && (timeout == 0 || timeout > REUSE_PROBE);
            let mut probed = false;
            let mut open_started = Instant::now();
            // The probe bounds the setup alone: the call's `USE` comes after the connection
            // has shown it is alive, and gets the call's timeout, once.
            let set_up = loop {
                let limit = if probe { REUSE_PROBE } else { timeout };
                match run::setup(&mut session, &pool.settings, prepared, limit, cancel).await {
                    Err(DbError::Timeout(_))
                        if probe && !session.connection_dead() && !cancel.is_cancelled() =>
                    {
                        probe = false;
                        probed = true;
                        session.reset_before_next();
                        open_started = Instant::now();
                    }
                    set_up => break set_up,
                }
            };
            let opened = match set_up {
                Ok(setup_ms) => run::use_database(&mut session, prepared, timeout, cancel)
                    .await
                    .map(|()| setup_ms),
                Err(error) => Err(error),
            };
            let setup_ms = match opened {
                Ok(setup_ms) => setup_ms,
                Err(error)
                    if reused
                        && !retried
                        && !cancel.is_cancelled()
                        && dead_before_work(&error, &session) =>
                {
                    // Nothing of the call ran: replace the connection and run it again.
                    retried = true;
                    drop(session);
                    continue;
                }
                Err(error) => {
                    self.put_back(pool, session).await;
                    return Err(run::db_failure(error, pool.settings.limits.advice));
                }
            };
            let connect_ms = if reused && !probed && !retried {
                0
            } else if reused || retried {
                open_started.duration_since(first_started).as_millis()
            } else {
                open_started.duration_since(connect_started).as_millis()
            };
            let result = match out.connected(&session.connection(), connect_ms) {
                Ok(()) => {
                    let opened = Opened {
                        connect_ms,
                        steps: Vec::new(),
                        setup_ms,
                    };
                    run::run_opened(
                        &mut session,
                        &opened,
                        &pool.settings,
                        timeout,
                        prepared,
                        out,
                        &Unbounded,
                        cancel,
                        counts,
                        started,
                    )
                    .await
                }
                Err(error) => Err(run::output_failure(error)),
            };
            self.put_back(pool, session).await;
            return result;
        }
    }

    /// An idle connection of `pool` to reuse, the most recently used first; ones idle too long
    /// are dropped on the way.
    fn take_idle(&self, pool: &Pool) -> Option<Session> {
        let mut idle = pool.idle.borrow_mut();
        while let Some(entry) = idle.pop() {
            if self.expired(&entry, Instant::now()) {
                continue;
            }
            return Some(entry.session);
        }
        None
    }

    fn expired(&self, entry: &Idle, now: Instant) -> bool {
        self.idle_after
            .is_some_and(|after| now.saturating_duration_since(entry.since) >= after)
    }

    /// Puts a connection back in the pool after a call, if the driver kept it: a wrapper a
    /// cancel left open is rolled back first, so an idle connection holds no transaction.
    /// Anything that fails here drops the connection instead.
    ///
    /// A `check` call that was cancelled or timed out left `SET NOEXEC ON` in force (only its
    /// normal end turns it off), and under NOEXEC the rollback would only be compiled: the
    /// wrapper would stay open and the connection be dropped. So NOEXEC goes off first, in a
    /// request of its own, so that the rollback is a batch NOEXEC no longer covers; both within
    /// `ROLLBACK_WAIT`. Without NOEXEC in force, the extra `SET` changes nothing.
    async fn put_back(&self, pool: &Pool, mut session: Session) {
        if session.is_retired() || session.connection_dead() {
            return;
        }
        if session.in_transaction() {
            let rollback = async {
                let cancel = Cancel::new();
                let wait = ROLLBACK_WAIT.as_secs() as u32;
                session
                    .exec_internal("SET NOEXEC OFF;", wait, &cancel)
                    .await?;
                session
                    .exec_internal("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;", wait, &cancel)
                    .await
            };
            // Past the wait the request is abandoned midway, so the connection, in no known
            // state, is dropped (the return below).
            let rolled_back = matches!(
                tokio::time::timeout(ROLLBACK_WAIT, rollback).await,
                Ok(Ok(()))
            );
            if !rolled_back || session.connection_dead() || session.in_transaction() {
                return;
            }
        }
        pool.idle.borrow_mut().push(Idle {
            session,
            since: Instant::now(),
        });
    }

    /// Closes the connections that have been idle too long (decision 19). Meant to be called
    /// now and then; `take_idle` also skips them.
    pub(crate) fn sweep(&self) {
        let now = Instant::now();
        for pool in &self.pools {
            pool.idle
                .borrow_mut()
                .retain(|entry| !self.expired(entry, now));
        }
    }

    /// Closes every idle connection, politely: the end of the server's life.
    pub(crate) async fn close_all(&self) {
        for pool in &self.pools {
            let idle: Vec<Idle> = pool.idle.borrow_mut().drain(..).collect();
            for entry in idle {
                entry.session.close().await;
            }
        }
    }
}

/// Waits for one of `pool`'s connections to be free: at most the call's timeout, and a cancel
/// ends the wait.
async fn wait_for_connection<'p>(
    pool: &'p Pool,
    timeout: u32,
    cancel: &Cancel,
    started: Instant,
) -> Result<tokio::sync::SemaphorePermit<'p>, Failure> {
    let limit = (timeout > 0).then(|| Duration::from_secs(u64::from(timeout)));
    let deadline = async {
        match limit {
            Some(limit) => tokio::time::sleep_until((started + limit).into()).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        permit = pool.permits.acquire() => permit.map_err(|_| {
            Failure::new(Exit::Internal, "internal error: the connection pool was closed")
        }),
        () = cancel.cancelled() => Err(Failure::new(
            Exit::Cancelled,
            "cancelled while waiting for a free connection",
        )),
        () = deadline => Err(Failure::new(
            Exit::Timeout,
            format!(
                "query timeout after {timeout} s waiting for a free connection ({}); {}",
                match pool.max {
                    1 => "the profile's only connection was busy".to_owned(),
                    max => format!("all {max} of the profile's connections were busy"),
                },
                more_connections(&pool.settings, timeout)
            ),
        )),
    }
}

/// The advice when every connection was busy for the call's `timeout`: more time, or more
/// connections. In `sqlh mcp` the pool's size is the server's launch flag, which the client
/// can't pass, and so is the timeout once the call has the server's limit.
fn more_connections(settings: &Settings, timeout: u32) -> String {
    match (
        settings.limits.advice,
        run::raise_timeout(settings, timeout),
    ) {
        (Advice::Cli, raise) => format!(
            "{}, or --pool for more connections",
            raise.unwrap_or_else(|| "raise -t".into())
        ),
        (Advice::Mcp, Some(raise)) => format!("{raise}, or start sqlh mcp with a larger --pool"),
        (Advice::Mcp, None) => "start sqlh mcp with a larger --pool or -t".into(),
    }
}

/// How long a reused connection's setup (`run::setup`, which carries the reset) may take
/// before the call stops waiting on it, in seconds. It answers in milliseconds on a live
/// connection; one that went silent while idle (a firewall's idle cut, a half-open socket)
/// would otherwise hold the call for its whole timeout and then the driver's 5 s wait for the
/// attention. A setup the probe cuts runs again on the same connection (`run_pooled`) when
/// the server acknowledges the attention within the driver's 5 s, so about 10 s after the
/// setup was sent. A live server stalled longer than that can't be told from a half-open
/// connection: the driver marks the connection dead, and it is replaced like one, the
/// profile's other idle connections with it (`dead_before_work`).
const REUSE_PROBE: u32 = 5;

/// Whether a reused connection's failed setup or `USE` (`run::setup`, `run::use_database`:
/// sqlh's own requests, so nothing of the call ran) means the connection was gone, so the call
/// runs again on a new one, once (decision 19): the driver marked it dead (it broke while
/// idle, `Lost`; the server didn't acknowledge the reset, `Protocol`; it went silent, a
/// `Timeout` whose attention went unanswered within the driver's 5 s) or it was retired. A
/// SQL error or a cancel is the call's own, and so is a timeout on a connection the driver
/// kept.
fn dead_before_work(error: &DbError, session: &Session) -> bool {
    (session.connection_dead() || session.is_retired())
        && !matches!(error, DbError::Cancelled(_) | DbError::Sql(_))
}

/// The engine against the fake TDS server's pooled session (`db::fake_server::serve_pool`):
/// the real execution path, driver included, on loopback, with the faults a pool meets played
/// on demand (docs/mcp-mode.md §9).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fake_server::{self, Conn, End, Fake, Turn, serve_pool};
    use crate::output::Limits;
    use crate::output::text::TextSink;
    use crate::run::{Input, QueryOrigin, Report, Request};
    use std::rc::Rc;

    /// Settings for a profile on the fake server, as the CLI's defaults resolve them:
    /// read-only, so every call runs in the wrapper transaction.
    fn settings(fake: &Fake) -> Settings {
        Settings {
            connect: fake_server::options(fake.server.clone()),
            limits: Limits::default(),
            query_timeout: 30,
            allow_write: false,
            writes_off: None,
            lock_timeout: 10,
            read_uncommitted: false,
            exact_count: false,
            profile: Some("fake".into()),
        }
    }

    /// An engine serving one profile on `fake`.
    fn serving(fake: &Fake, max: usize, idle_after: Option<Duration>) -> Engine {
        Engine::new(vec![("fake".into(), settings(fake))], max, idle_after)
    }

    /// `settings` as `sqlh mcp` gives a listed profile (`mcp::resolve`): writes off, and the
    /// advice and a failed connect's hints in the words of a call and of the server's owner
    /// (docs/mcp-mode.md §4). `query_timeout` is the server's own limit (its `-t`; 0 is none).
    fn mcp_settings(fake: &Fake, query_timeout: u32) -> Settings {
        let mut settings = settings(fake);
        settings.writes_off = Some(crate::guard::WritesOff::Mcp);
        settings.limits.advice = Advice::Mcp;
        settings.connect.hints = crate::db::Hints::Profile;
        settings.query_timeout = query_timeout;
        settings
    }

    /// An engine serving one profile on `fake`, as `sqlh mcp` does (`mcp_settings`).
    fn serving_mcp(fake: &Fake, max: usize, query_timeout: u32) -> Engine {
        Engine::new(
            vec![("fake".into(), mcp_settings(fake, query_timeout))],
            max,
            None,
        )
    }

    /// One call as the MCP front end makes it: its exit code, and its text with the closing
    /// lines.
    struct Called {
        exit: Exit,
        text: String,
        elapsed: Duration,
    }

    /// A call's request for `sql`, as the MCP front end makes it for a plain `query`.
    fn request(sql: &str) -> Request {
        Request {
            input: Input::Query(sql.into()),
            vars: None,
            terminator: "GO".into(),
            check: false,
            continue_on_error: false,
            report: Report::Off,
            database: None,
        }
    }

    /// Runs `sql` as one call on the engine's profile, with `-t timeout`, into text.
    async fn call(engine: &Engine, sql: &str, timeout: u32, cancel: &Cancel) -> Called {
        call_with(engine, request(sql), timeout, cancel).await
    }

    /// `call`, for any request (a `check`, a `database`).
    async fn call_with(engine: &Engine, request: Request, timeout: u32, cancel: &Cancel) -> Called {
        call_as(engine, request, QueryOrigin::User, timeout, cancel).await
    }

    /// `call_with`, for SQL of either origin (a schema command's is sqlh's own).
    async fn call_as(
        engine: &Engine,
        request: Request,
        origin: QueryOrigin,
        timeout: u32,
        cancel: &Cancel,
    ) -> Called {
        let started = Instant::now();
        let settings = engine.settings(0);
        let prepared = match run::prepare(request, settings, origin) {
            Ok(prepared) => prepared,
            Err(failure) => panic!("prepare: {}", failure.message),
        };
        // As `mcp::tools::execute` sets it: a call at the server's own limit is offered no
        // larger timeout (it matters only to `Advice::Mcp`).
        let limits = Limits {
            timeout_at_limit: settings.query_timeout > 0 && timeout >= settings.query_timeout,
            ..settings.limits
        };
        let mut out = TextSink::with_meta(Vec::new(), Box::new(io::sink()), limits);
        let outcome = engine
            .run(Call {
                profile: 0,
                prepared: &prepared,
                timeout,
                out: &mut out,
                cancel,
                started,
            })
            .await;
        let exit = run::close(&mut out, outcome, started, settings.limits.advice);
        Called {
            exit,
            text: String::from_utf8(out.into_inner()).expect("text output is UTF-8"),
            elapsed: started.elapsed(),
        }
    }

    /// `call`, with no cancel and `-t 30`.
    async fn plain(engine: &Engine) -> Called {
        call(engine, "SELECT 1", 30, &Cancel::new()).await
    }

    /// `call`, cancelled from outside `after` it starts, as an MCP client's
    /// `notifications/cancelled` would cancel it.
    async fn cancelled(engine: &Engine, after: Duration) -> Called {
        cancelled_with(engine, request("SELECT 1"), after).await
    }

    /// `cancelled`, for any request.
    async fn cancelled_with(engine: &Engine, request: Request, after: Duration) -> Called {
        let root = Cancel::new();
        let cancel = root.child();
        let canceller = async move {
            tokio::time::sleep(after).await;
            root.cancel();
        };
        let (called, ()) = tokio::join!(call_with(engine, request, 30, &cancel), canceller);
        called
    }

    /// `call` with `-t timeout`, as a task of its own (`spawn_local`), started `after` a pause.
    async fn later(engine: Rc<Engine>, after: Duration, timeout: u32) -> Called {
        tokio::time::sleep(after).await;
        call(&engine, "SELECT 1", timeout, &Cancel::new()).await
    }

    fn block_on<F: Future>(work: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(work)
    }

    /// `block_on`, inside a `LocalSet`, so that the work can `spawn_local` concurrent calls
    /// (the engine isn't `Send`), as the MCP front end will.
    fn block_on_local<F: Future>(work: F) -> F::Output {
        block_on(tokio::task::LocalSet::new().run_until(work))
    }

    /// The text with every run of digits as `N`, so that two calls' outputs compare apart
    /// from their timings.
    fn masked(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for ch in text.chars() {
            if !ch.is_ascii_digit() {
                out.push(ch);
            } else if !out.ends_with('N') {
                out.push('N');
            }
        }
        out
    }

    /// The text with the number before each ` ms` as `N`: a call's text apart from its
    /// timings, every other number kept, for exact-output tests.
    fn untimed(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(" ms") {
            let before = rest[..at].trim_end_matches(|c: char| c.is_ascii_digit());
            out.push_str(before);
            if before.len() < at {
                out.push('N');
            }
            out.push_str(" ms");
            rest = &rest[at + " ms".len()..];
        }
        out.push_str(rest);
        out
    }

    fn assert_ok(called: &Called) {
        assert_eq!(called.exit, Exit::Ok, "{}", called.text);
        assert!(
            called
                .text
                .starts_with("-- result 1\nx:int\n1\n-- 1 row\n-- ok: 1 batch, "),
            "{}",
            called.text
        );
    }

    /// Whether `sql` is the session setup that a read-only call sends first, ending in the
    /// wrapper's `BEGIN TRANSACTION`.
    fn is_setup(sql: &str) -> bool {
        sql.starts_with("SET ANSI_NULLS ON;") && sql.ends_with("BEGIN TRANSACTION;")
    }

    /// The wrapper's rollback, from `run_opened` or `put_back`.
    const ROLLBACK: &str = "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;";

    /// What `put_back` sends ahead of its rollback, and `run_opened` after a `check` call's
    /// batches.
    const NOEXEC_OFF: &str = "SET NOEXEC OFF;";

    // A wait for a free connection that times out advises what its reader can change: in
    // sqlh mcp, the call's timeout up to the server's, and the server's own --pool.
    #[test]
    fn the_wait_advice_names_what_its_reader_can_change() {
        let fake = serve_pool(Vec::new());
        let mut settings = settings(&fake);
        assert_eq!(
            more_connections(&settings, 30),
            "raise -t, or --pool for more connections"
        );
        settings.limits.advice = Advice::Mcp;
        assert_eq!(
            more_connections(&settings, 10),
            "raise timeout (the server allows up to 30 s), or start sqlh mcp with a larger --pool"
        );
        // At the server's limit there is no larger timeout to ask for.
        assert_eq!(
            more_connections(&settings, 30),
            "start sqlh mcp with a larger --pool or -t"
        );
        settings.query_timeout = 0;
        assert_eq!(
            more_connections(&settings, 10),
            "raise timeout, or start sqlh mcp with a larger --pool"
        );
    }

    // Decisions 14 and 15: calls on one profile share its connection, and each starts fresh:
    // the second call reuses the first's connection, its first request carries the reset, and
    // the session setup runs again after it (the reset restores the login's defaults, not
    // sqlh's). Apart from its timings, the second call's output is the first's.
    #[test]
    fn calls_on_one_profile_reuse_its_connection_with_a_reset() {
        let fake = serve_pool(Vec::new());
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async { (plain(&engine).await, plain(&engine).await) });
        assert_ok(&first);
        assert_ok(&second);
        assert_eq!(masked(&first.text), masked(&second.text));
        // A reused connection made no connect of its own (DESIGN.md §5.1).
        assert!(second.text.contains(", connect 0 ms\n"), "{}", second.text);
        assert_eq!(fake.accepted(), 1);
        assert_eq!(fake.resets(0), [false, false, false, true, false, false]);
        let sql = fake.sql(0);
        assert!(is_setup(&sql[0]) && is_setup(&sql[3]), "{sql:?}");
        assert_eq!(sql[1..3], ["SELECT 1", ROLLBACK]);
        assert_eq!(sql[4..], ["SELECT 1", ROLLBACK]);
        // Back in the pool, with no transaction open on either side.
        assert_eq!(engine.idle_count(0), 1);
        assert!(!fake.in_transaction(0));
        assert!(!engine.pools[0].idle.borrow()[0].session.in_transaction());
    }

    // Decisions 17 (amended) and 19: the reset is verified on every reuse. A server that
    // answers the reused connection's first request without acknowledging the reset makes the
    // driver fail it and mark the connection dead; nothing of the call ran, so the call is run
    // again, once, silently, on a new connection, and succeeds.
    #[test]
    fn a_reset_the_server_does_not_acknowledge_is_retried_on_a_new_connection() {
        let fake = serve_pool(vec![Conn::new().without_reset_ack()]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async {
            let first = plain(&engine).await;
            let second = plain(&engine).await;
            // The connection that wasn't reset is closed, not kept.
            let closed = fake
                .wait_for(Duration::from_secs(2), |f| f.ended(0).is_some())
                .await;
            assert!(closed, "the unreset connection was closed");
            (first, second)
        });
        assert_ok(&first);
        assert_ok(&second);
        assert_eq!(
            masked(&first.text),
            masked(&second.text),
            "the retry is silent"
        );
        assert_eq!(fake.accepted(), 2);
        assert_eq!(fake.resets(0), [false, false, false, true]);
        assert_eq!(fake.resets(1), [false, false, false]);
        assert_eq!(fake.ended(0), Some(End::Client));
        assert_eq!(engine.idle_count(0), 1);
    }

    // Decision 19, and REUSE_PROBE: a reused connection that went silent while idle (a
    // firewall's idle cut) holds the call for the probe and the driver's wait for the
    // attention, not for the call's whole timeout; nothing of the call ran, so it runs again,
    // once, on a new connection, and succeeds.
    #[test]
    fn a_reused_connection_gone_silent_is_retried_after_the_probe() {
        let fake = serve_pool(vec![Conn::new().at(4, Turn::Silent)]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async {
            let first = plain(&engine).await;
            (first, call(&engine, "SELECT 1", 60, &Cancel::new()).await)
        });
        assert_ok(&first);
        assert_ok(&second);
        // The probe's 5 s and the driver's 5 s for the attention, then a new connection.
        assert!(
            second.elapsed >= Duration::from_secs(9) && second.elapsed < Duration::from_secs(20),
            "{:?}",
            second.elapsed
        );
        assert_eq!(fake.accepted(), 2);
        assert_eq!(
            masked(&first.text),
            masked(&second.text),
            "the retry is silent"
        );
        assert_eq!(engine.idle_count(0), 1);
        // The time the dead connection took is the call's connect time, not hidden in its
        // total.
        assert!(connect_ms(&second.text) >= 9_000, "{}", second.text);
    }

    /// The `connect N ms` of a call's status line.
    fn connect_ms(text: &str) -> u64 {
        let at = text.rfind("connect ").expect("a connect time") + "connect ".len();
        text[at..]
            .split(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a number")
    }

    // A reused connection whose setup is slow but alive: the probe cuts it, the server
    // acknowledges the attention, so the connection is kept, reset again and set up with the
    // call's own timeout. A server that acknowledges the attention within the driver's 5 s
    // keeps its connection: no new login, which under load
    // is the step most likely to fail.
    #[test]
    fn a_setup_the_probe_cuts_short_keeps_its_live_connection() {
        let fake = serve_pool(vec![Conn::new().at(4, Turn::Delay(Duration::from_secs(7)))]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async {
            let first = plain(&engine).await;
            (first, call(&engine, "SELECT 1", 60, &Cancel::new()).await)
        });
        assert_ok(&first);
        assert_ok(&second);
        assert!(
            second.elapsed >= Duration::from_secs(5) && second.elapsed < Duration::from_secs(15),
            "{:?}",
            second.elapsed
        );
        assert_eq!(fake.accepted(), 1, "the same connection");
        // The cut setup, then the setup again, reset first, then the call.
        let resets = fake.resets(0);
        assert_eq!(resets[3..6], [true, true, false], "{resets:?}");
        assert!(connect_ms(&second.text) >= 5_000, "{}", second.text);
        assert_eq!(engine.idle_count(0), 1);
    }

    // A reused connection's USE that runs past the call's own timeout, on a live connection:
    // the call's own timeout, once, at about that timeout, with the USE sent once and no
    // connect time; never a second setup and USE.
    #[test]
    fn a_use_past_the_calls_timeout_fails_once() {
        let fake = serve_pool(vec![Conn::new().at(5, Turn::Hang { ack: true })]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async {
            let first = plain(&engine).await;
            let mut use_db = request("SELECT 1");
            use_db.database = Some("d".into());
            (first, call_with(&engine, use_db, 6, &Cancel::new()).await)
        });
        assert_ok(&first);
        assert_eq!(second.exit, Exit::Timeout, "{}", second.text);
        assert!(
            second
                .text
                .starts_with("-- error: query timeout after 6 s changing to the call's database\n"),
            "{}",
            second.text
        );
        assert!(
            second.elapsed >= Duration::from_secs(6) && second.elapsed < Duration::from_secs(10),
            "{:?}",
            second.elapsed
        );
        let uses = fake.sql(0).iter().filter(|sql| *sql == "USE [d];").count();
        assert_eq!(uses, 1, "{:?}", fake.sql(0));
        assert_eq!(fake.accepted(), 1);
    }

    // The probe bounds only the setup, which carries the reset; a call's `USE` comes after the
    // connection has shown it is alive, so a slow one (a database being opened) gets the call's
    // own timeout on the same connection.
    #[test]
    fn a_slow_use_is_not_probed() {
        let fake = serve_pool(vec![Conn::new().at(5, Turn::Delay(Duration::from_secs(6)))]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async {
            let first = plain(&engine).await;
            let mut use_db = request("SELECT 1");
            use_db.database = Some("d".into());
            (first, call_with(&engine, use_db, 60, &Cancel::new()).await)
        });
        assert_ok(&first);
        assert_ok(&second);
        assert_eq!(fake.accepted(), 1, "no retry");
        assert_eq!(fake.sql(0)[4], "USE [d];");
        assert_eq!(connect_ms(&second.text), 0, "{}", second.text);
    }

    // The probe applies only to a reused connection, and only when it is shorter than the
    // call's timeout; a call at a short timeout of its own keeps it, and a timeout on a live
    // connection at that limit is the call's own, not retried.
    #[test]
    fn the_probe_is_never_longer_than_the_call() {
        let fake = serve_pool(vec![Conn::new().at(4, Turn::Delay(Duration::from_secs(3)))]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async {
            let first = plain(&engine).await;
            (first, call(&engine, "SELECT 1", 1, &Cancel::new()).await)
        });
        assert_ok(&first);
        assert_eq!(second.exit, Exit::Timeout, "{}", second.text);
        assert!(
            second
                .text
                .starts_with("-- error: query timeout after 1 s in the session setup\n"),
            "{}",
            second.text
        );
        assert_eq!(fake.accepted(), 1, "no retry at the call's own limit");
    }

    // Decision 19: a connection the server closed while it sat idle (a network drop, a session
    // a DBA killed) shows up at the next call's first request, before any of the call ran, so
    // the call is retried once, silently, on a new connection. The retry also drops the
    // profile's other idle connections, which most likely died the same way.
    #[test]
    fn a_connection_closed_while_idle_is_replaced_and_the_call_retried() {
        let fake = serve_pool(vec![Conn::new().close_after(3)]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second) = block_on(async {
            let first = plain(&engine).await;
            let closed = fake
                .wait_for(Duration::from_secs(2), |f| f.ended(0) == Some(End::Server))
                .await;
            assert!(closed, "the server closed the idle connection");
            (first, plain(&engine).await)
        });
        assert_ok(&first);
        assert_ok(&second);
        assert_eq!(
            masked(&first.text),
            masked(&second.text),
            "the retry is silent"
        );
        assert_eq!(fake.accepted(), 2);
        assert_eq!(
            fake.requests(0).len(),
            3,
            "the closed connection read nothing more"
        );
        assert_eq!(fake.resets(1), [false, false, false]);
        assert_eq!(engine.idle_count(0), 1);

        // Two idle connections, both closed by the server: one retry, and the other dead one
        // goes with it instead of failing a later call.
        let both = || Conn::new().at(2, Turn::Delay(Duration::from_millis(50)));
        let fake = serve_pool(vec![both().close_after(3), both().close_after(3)]);
        let engine = Rc::new(serving(&fake, DEFAULT_POOL, None));
        let calls = block_on_local(async {
            let a = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let b = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let (a, b) = (a.await.expect("a"), b.await.expect("b"));
            assert_eq!(engine.idle_count(0), 2);
            let closed = fake
                .wait_for(Duration::from_secs(2), |f| {
                    f.ended(0).is_some() && f.ended(1).is_some()
                })
                .await;
            assert!(closed, "the server closed both idle connections");
            let c = plain(&engine).await;
            let d = plain(&engine).await;
            [a, b, c, d]
        });
        for called in &calls {
            assert_ok(called);
        }
        assert_eq!(fake.accepted(), 3);
        assert_eq!(engine.idle_count(0), 1);
        // Neither dead connection got more than its first call; the new one served the
        // other two, the second of them reset.
        assert_eq!(fake.requests(0).len() + fake.requests(1).len(), 6);
        assert_eq!(fake.resets(2), [false, false, false, true, false, false]);

        // Once only: when the new connection fails too (the server drops it right after the
        // login), the call gets that real error, and there is no third attempt.
        let fake = serve_pool(vec![Conn::new().close_after(3), Conn::new().close_after(0)]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, failed, accepted, next) = block_on(async {
            let first = plain(&engine).await;
            let closed = fake
                .wait_for(Duration::from_secs(2), |f| f.ended(0) == Some(End::Server))
                .await;
            assert!(closed, "the server closed the idle connection");
            let failed = plain(&engine).await;
            let accepted = fake.accepted();
            (first, failed, accepted, plain(&engine).await)
        });
        assert_ok(&first);
        assert_eq!(failed.exit, Exit::Connect, "{}", failed.text);
        assert!(
            failed.text.starts_with("-- error: connection lost: "),
            "{}",
            failed.text
        );
        assert_eq!(accepted, 2, "no third attempt");
        assert_ok(&next);
        assert_eq!(fake.accepted(), 3);
    }

    // Decision 19's limit: a call is retried only when it failed before any of its work could
    // have run. One whose connection breaks mid-reply, after the session setup, gets the real
    // error (exit 2, the connection lost), and its connection is discarded; there is no second
    // attempt, and the next call connects afresh.
    #[test]
    fn a_connection_lost_mid_run_is_not_retried() {
        // Requests 1 to 3 are the first call's; 4 is the second's setup, 5 its SELECT.
        let fake = serve_pool(vec![Conn::new().at(5, Turn::Drop)]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (first, second, accepted, third) = block_on(async {
            let first = plain(&engine).await;
            let second = plain(&engine).await;
            let accepted = fake.accepted();
            (first, second, accepted, plain(&engine).await)
        });
        assert_ok(&first);
        assert_eq!(accepted, 1, "no second attempt");
        assert_eq!(second.exit, Exit::Connect, "{}", second.text);
        // The rows that arrived before the loss are shown, then the error.
        assert!(
            second.text.starts_with("-- result 1\nx:int\n1\n1\n1\n"),
            "{}",
            second.text
        );
        assert!(
            second.text.contains("-- error: connection lost: "),
            "{}",
            second.text
        );
        assert!(
            second
                .text
                .contains("-- failed (exit 2): connection failed"),
            "{}",
            second.text
        );
        assert_eq!(fake.requests(0).len(), 5);
        assert!(
            fake.resets(0)[3],
            "the second call's setup carried the reset"
        );
        assert_ok(&third);
        assert_eq!(fake.accepted(), 2);
        assert_eq!(fake.resets(1), [false, false, false]);
        assert_eq!(engine.idle_count(0), 1);
    }

    /// A call cancelled 100 ms in, on a connection that won't settle, then the next call: the
    /// cancelled call, how many connections were idle right after it, and the next call.
    async fn unsettled_then_next(fake: &Fake, engine: &Engine) -> (Called, usize, Called) {
        let stuck = cancelled(engine, Duration::from_millis(100)).await;
        let idle = engine.idle_count(0);
        let closed = fake
            .wait_for(Duration::from_secs(2), |f| f.ended(0).is_some())
            .await;
        assert!(closed, "the connection was closed");
        (stuck, idle, plain(engine).await)
    }

    // Decision 17: a cancel that doesn't settle. In the first case the server goes silent
    // mid-result and never acknowledges the attention; the driver gives up after its own 5 s
    // wait for the acknowledgement and marks the connection dead (unless the engine's grace,
    // which starts within microseconds of it, ends the call first). In the second the attention
    // is acknowledged, but the server goes silent at the rollback `put_back` sends (after its
    // `SET NOEXEC OFF`, answered), which gives up after `ROLLBACK_WAIT` (2 s), so the call ends
    // with its own result, well inside the settle grace. Either way the connection is
    // discarded, never put back, and the next call reconnects. The one slow test (the first
    // case's bounds are 5 s), so the two run at once.
    #[test]
    fn a_cancel_that_does_not_settle_discards_the_connection() {
        let unacknowledged = serve_pool(vec![Conn::new().at(2, Turn::Hang { ack: false })]);
        let rollback_unanswered = serve_pool(vec![
            Conn::new()
                .at(2, Turn::Hang { ack: true })
                .at(4, Turn::Silent),
        ]);
        let (by_driver, by_grace) = (
            serving(&unacknowledged, DEFAULT_POOL, None),
            serving(&rollback_unanswered, DEFAULT_POOL, None),
        );
        let (first, second) = block_on(async {
            tokio::join!(
                unsettled_then_next(&unacknowledged, &by_driver),
                unsettled_then_next(&rollback_unanswered, &by_grace)
            )
        });
        let grace = format!(
            "cancelled: the call didn't finish within {} s of the cancel, so its connection was \
             closed",
            SETTLE_GRACE.as_secs()
        );
        let driver = "the server didn't confirm the cancel, so the connection was closed";
        let kept = "the statement was cancelled; the connection stayed usable";
        for ((stuck, idle, next), fake, messages, (from, until)) in [
            (
                first,
                &unacknowledged,
                vec![driver, grace.as_str()],
                (SETTLE_GRACE, SETTLE_GRACE + Duration::from_secs(2)),
            ),
            (
                second,
                &rollback_unanswered,
                vec![kept],
                (ROLLBACK_WAIT, SETTLE_GRACE),
            ),
        ] {
            assert_eq!(stuck.exit, Exit::Cancelled, "{}", stuck.text);
            assert!(
                messages
                    .iter()
                    .any(|message| stuck.text.contains(&format!("-- error: {message}\n"))),
                "{}",
                stuck.text
            );
            assert!(
                stuck.elapsed >= from && stuck.elapsed < until,
                "{:?}",
                stuck.elapsed
            );
            assert_eq!(idle, 0, "the connection was not put back");
            assert_eq!(fake.ended(0), Some(End::Client));
            assert_ok(&next);
            assert_eq!(fake.accepted(), 2);
            assert_eq!(fake.resets(1), [false, false, false]);
        }
        assert_eq!(rollback_unanswered.sql(0)[2..], [NOEXEC_OFF, ROLLBACK]);
    }

    // Decision 17 (amended): a cancel the server acknowledges (DONE ATTN) leaves the driver's
    // connection in step, so it goes back to the pool, and the next call reuses it, its first
    // request carrying the reset that clears whatever the cancelled call left.
    #[test]
    fn an_acknowledged_cancel_keeps_the_connection_for_the_next_call() {
        let fake = serve_pool(vec![Conn::new().at(2, Turn::Hang { ack: true })]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (cancelled, idle, next) = block_on(async {
            let cancelled = cancelled(&engine, Duration::from_millis(100)).await;
            let idle = engine.idle_count(0);
            (cancelled, idle, plain(&engine).await)
        });
        assert_eq!(cancelled.exit, Exit::Cancelled, "{}", cancelled.text);
        assert!(
            cancelled
                .text
                .contains("-- error: the statement was cancelled; the connection stayed usable"),
            "{}",
            cancelled.text
        );
        assert!(
            cancelled.elapsed < Duration::from_secs(2),
            "{:?}",
            cancelled.elapsed
        );
        assert_eq!(idle, 1, "the connection was put back");
        assert_ok(&next);
        assert_eq!(fake.accepted(), 1);
        // The cancelled call's setup, its SELECT, and the NOEXEC OFF and rollback `put_back`
        // sent; then the next call, reset first.
        assert_eq!(
            fake.resets(0),
            [false, false, false, false, true, false, false]
        );
        assert_eq!(fake.sql(0)[2..4], [NOEXEC_OFF, ROLLBACK]);
        assert_eq!(engine.idle_count(0), 1);
    }

    // Decision 19: at most `max` connections per profile. With one, a second call made while
    // the first runs waits for its connection, then reuses it. With every connection held past
    // a waiting call's timeout, that call fails as a timeout (exit 4) that says what it was
    // waiting for, and connects nothing.
    #[test]
    fn calls_beyond_the_pool_wait_for_a_connection_within_their_timeout() {
        let fake = serve_pool(vec![
            Conn::new().at(2, Turn::Delay(Duration::from_millis(300))),
        ]);
        let engine = Rc::new(serving(&fake, 1, None));
        let (first, waiting) = block_on_local(async {
            let first = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let waiting =
                tokio::task::spawn_local(later(Rc::clone(&engine), Duration::from_millis(50), 30));
            (first.await.expect("first"), waiting.await.expect("waiting"))
        });
        assert_ok(&first);
        assert_ok(&waiting);
        assert!(
            waiting.elapsed >= Duration::from_millis(200),
            "it waited for the first call: {:?}",
            waiting.elapsed
        );
        assert_eq!(fake.accepted(), 1);
        assert_eq!(fake.resets(0), [false, false, false, true, false, false]);

        // Two connections, both held for 1.2 s; a third call with -t 1 gives up waiting.
        let slow = || Conn::new().at(2, Turn::Delay(Duration::from_millis(1200)));
        let fake = serve_pool(vec![slow(), slow()]);
        let engine = Rc::new(serving(&fake, 2, None));
        let (a, b, c, accepted) = block_on_local(async {
            let a = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let b = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let c =
                tokio::task::spawn_local(later(Rc::clone(&engine), Duration::from_millis(50), 1));
            let c = c.await.expect("c");
            // Counted when the waiting call gave up, while both connections were held.
            let accepted = fake.accepted();
            (a.await.expect("a"), b.await.expect("b"), c, accepted)
        });
        assert_ok(&a);
        assert_ok(&b);
        assert_eq!(c.exit, Exit::Timeout, "{}", c.text);
        assert_eq!(
            masked(&c.text),
            "-- error: query timeout after N s waiting for a free connection (all N of the \
             profile's connections were busy); raise -t, or --pool for more connections\n\
             -- failed (exit N): timeout, N ms\n"
        );
        assert!(
            c.text
                .contains("after 1 s waiting for a free connection (all 2 of the profile's"),
            "{}",
            c.text
        );
        assert!(
            c.elapsed >= Duration::from_secs(1) && c.elapsed < Duration::from_millis(1500),
            "{:?}",
            c.elapsed
        );
        assert_eq!(accepted, 2);
        assert_eq!(fake.accepted(), 2);
    }

    // Decision 19: a connection idle longer than `--idle` is closed, by `sweep` and by a call
    // that would have taken it, and the next call reconnects.
    #[test]
    fn idle_connections_are_closed_and_the_next_call_reconnects() {
        let fake = serve_pool(Vec::new());
        let idle = Duration::from_millis(150);
        let engine = serving(&fake, DEFAULT_POOL, Some(idle));
        let calls = block_on(async {
            let first = plain(&engine).await;
            // Not idle long enough yet: kept.
            engine.sweep();
            assert_eq!(engine.idle_count(0), 1);
            tokio::time::sleep(idle + Duration::from_millis(50)).await;
            engine.sweep();
            assert_eq!(engine.idle_count(0), 0);
            let closed = fake
                .wait_for(Duration::from_secs(2), |f| f.ended(0) == Some(End::Client))
                .await;
            assert!(closed, "the swept connection was closed");
            let second = plain(&engine).await;
            // Without a sweep, a call skips (and drops) a connection idle too long.
            tokio::time::sleep(idle + Duration::from_millis(50)).await;
            let third = plain(&engine).await;
            [first, second, third]
        });
        for called in &calls {
            assert_ok(called);
        }
        assert_eq!(fake.accepted(), 3);
        for conn in 0..3 {
            assert_eq!(
                fake.resets(conn),
                [false, false, false],
                "connection {conn}"
            );
        }
        assert_eq!(engine.idle_count(0), 1);
    }

    // Decision 17 and docs/mcp-mode.md §5: a call cancelled mid-batch (here by its own -t,
    // whose attention the server acknowledges) returns without the wrapper's rollback, so the
    // engine rolls the wrapper back before the connection goes back to the pool: an idle
    // connection holds no transaction. A rollback that fails drops the connection instead.
    #[test]
    fn a_wrapper_left_open_is_rolled_back_before_the_connection_is_pooled() {
        let fake = serve_pool(vec![Conn::new().at(2, Turn::Hang { ack: true })]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let timed_out = block_on(call(&engine, "SELECT 1", 1, &Cancel::new()));
        assert_eq!(timed_out.exit, Exit::Timeout, "{}", timed_out.text);
        assert!(
            timed_out.text.contains(
                "-- error: query timeout after 1 s in batch 1; raise -t or narrow the query"
            ),
            "{}",
            timed_out.text
        );
        let sql = fake.sql(0);
        assert!(is_setup(&sql[0]), "{sql:?}");
        assert_eq!(sql[1..], ["SELECT 1", NOEXEC_OFF, ROLLBACK]);
        assert!(!fake.in_transaction(0));
        {
            let idle = engine.pools[0].idle.borrow();
            assert_eq!(idle.len(), 1);
            assert!(!idle[0].session.in_transaction());
        }

        // The rollback (request 4, after the NOEXEC OFF) meets a lost connection: dropped, not
        // pooled.
        let fake = serve_pool(vec![
            Conn::new()
                .at(2, Turn::Hang { ack: true })
                .at(4, Turn::Drop),
        ]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (cancelled, idle, next) = block_on(async {
            let cancelled = cancelled(&engine, Duration::from_millis(100)).await;
            let idle = engine.idle_count(0);
            (cancelled, idle, plain(&engine).await)
        });
        assert_eq!(cancelled.exit, Exit::Cancelled, "{}", cancelled.text);
        assert_eq!(fake.sql(0)[2..], [NOEXEC_OFF, ROLLBACK]);
        assert_eq!(idle, 0, "the connection whose rollback failed was dropped");
        assert_ok(&next);
        assert_eq!(fake.accepted(), 2);
    }

    // A `check` call cancelled or timed out mid-batch leaves `SET NOEXEC ON` in force (only its
    // normal end turns it off), and under NOEXEC a rollback is compiled, not run. So `put_back`
    // turns NOEXEC off first, in a request of its own; the rollback then ends the wrapper, the
    // connection goes back to the pool, and the next call reuses it.
    #[test]
    fn a_check_call_cut_short_turns_noexec_off_before_the_rollback() {
        /// A `check` call cut short, by a cancel or else by its -t 1, how many connections
        /// were idle right after it, and the next call.
        async fn cut_short_then_next(engine: &Engine, by_cancel: bool) -> (Called, usize, Called) {
            let check = Request {
                check: true,
                ..request("SELECT 1")
            };
            let called = if by_cancel {
                cancelled_with(engine, check, Duration::from_millis(100)).await
            } else {
                call_with(engine, check, 1, &Cancel::new()).await
            };
            let idle = engine.idle_count(0);
            (called, idle, plain(engine).await)
        }
        // Requests 1 to 3 are the setup, SET NOEXEC ON and the batch, which hangs until the
        // cancel (or the -t 1), whose attention is acknowledged.
        let conn = || Conn::new().at(3, Turn::Hang { ack: true });
        let (by_cancel, by_timeout) = (serve_pool(vec![conn()]), serve_pool(vec![conn()]));
        let (cancelling, timing_out) = (
            serving(&by_cancel, DEFAULT_POOL, None),
            serving(&by_timeout, DEFAULT_POOL, None),
        );
        let (first, second) = block_on(async {
            tokio::join!(
                cut_short_then_next(&cancelling, true),
                cut_short_then_next(&timing_out, false)
            )
        });
        for ((called, idle, next), fake, exit) in [
            (first, &by_cancel, Exit::Cancelled),
            (second, &by_timeout, Exit::Timeout),
        ] {
            assert_eq!(called.exit, exit, "{}", called.text);
            assert_eq!(
                idle, 1,
                "the connection was put back, its wrapper rolled back"
            );
            let sql = fake.sql(0);
            assert!(is_setup(&sql[0]), "{sql:?}");
            assert_eq!(
                sql[1..5],
                ["SET NOEXEC ON;", "SELECT 1", NOEXEC_OFF, ROLLBACK],
                "{sql:?}"
            );
            assert_ok(&next);
            assert_eq!(fake.accepted(), 1);
            assert_eq!(
                fake.resets(0),
                [false, false, false, false, false, true, false, false]
            );
            assert!(!fake.in_transaction(0));
        }
    }

    // On all but Azure SQL Database, the connection reset doesn't restore the isolation level,
    // so a call that sets one (the guard allows it) would hand it to the next call on its
    // connection. Every call's setup names the level, a reused connection's too, ahead of the
    // wrapper's BEGIN TRANSACTION: READ COMMITTED, or READ UNCOMMITTED when the profile reads
    // that way.
    #[test]
    fn every_call_sets_the_isolation_level_the_reset_leaves_alone() {
        for (read_uncommitted, level) in [(false, "READ COMMITTED"), (true, "READ UNCOMMITTED")] {
            let fake = serve_pool(Vec::new());
            let profile = Settings {
                read_uncommitted,
                ..settings(&fake)
            };
            let engine = Engine::new(vec![("fake".into(), profile)], DEFAULT_POOL, None);
            let serializable = "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;";
            let (first, second) = block_on(async {
                let first = call(&engine, serializable, 30, &Cancel::new()).await;
                (first, plain(&engine).await)
            });
            assert_eq!(first.exit, Exit::Ok, "{}", first.text);
            assert_eq!(second.exit, Exit::Ok, "{}", second.text);
            if !read_uncommitted {
                assert_ok(&second);
            }
            assert_eq!(fake.accepted(), 1);
            assert_eq!(fake.resets(0), [false, false, false, true, false, false]);
            let sql = fake.sql(0);
            assert_eq!(sql[1], serializable);
            let wanted = format!(" SET TRANSACTION ISOLATION LEVEL {level}; BEGIN TRANSACTION;");
            for setup in [&sql[0], &sql[3]] {
                assert!(is_setup(setup) && setup.ends_with(&wanted), "{setup}");
            }
        }
    }

    // An MCP call's `database` is switched to with a USE of its own, after the setup and
    // before the call's SQL, the name bracketed with its `]` doubled: that escaping is what
    // stops a database argument from injecting SQL. The reset restores the login's database,
    // so the next call on the connection, naming none, gets the reset and no USE.
    #[test]
    fn a_calls_database_is_used_bracketed_and_the_next_call_gets_the_reset_alone() {
        let fake = serve_pool(Vec::new());
        let engine = serving(&fake, DEFAULT_POOL, None);
        let named = Request {
            database: Some("a]b".into()),
            ..request("SELECT 1")
        };
        let (first, second) = block_on(async {
            let first = call_with(&engine, named, 30, &Cancel::new()).await;
            (first, plain(&engine).await)
        });
        assert_ok(&first);
        assert_ok(&second);
        assert_eq!(masked(&first.text), masked(&second.text));
        assert_eq!(fake.accepted(), 1);
        let sql = fake.sql(0);
        assert!(is_setup(&sql[0]) && is_setup(&sql[4]), "{sql:?}");
        assert_eq!(sql[1..4], ["USE [a]]b];", "SELECT 1", ROLLBACK], "{sql:?}");
        assert_eq!(sql[5..], ["SELECT 1", ROLLBACK], "{sql:?}");
        assert_eq!(
            fake.resets(0),
            [false, false, false, false, true, false, false]
        );
    }

    // A call cancelled while it waits for a connection (the pool's only one held by a slow
    // call) ends at once, exit 6, saying what it was waiting for. It took no permit, so once
    // the slow call ends, the next call reuses the one connection.
    #[test]
    fn a_call_cancelled_while_waiting_for_a_connection_ends_promptly() {
        let fake = serve_pool(vec![
            Conn::new().at(2, Turn::Delay(Duration::from_millis(800))),
        ]);
        let engine = Rc::new(serving(&fake, 1, None));
        let (slow, waiting, next) = block_on_local(async {
            let slow = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let waiting = tokio::task::spawn_local({
                let engine = Rc::clone(&engine);
                async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    cancelled(&engine, Duration::from_millis(100)).await
                }
            });
            let waiting = waiting.await.expect("waiting");
            let slow = slow.await.expect("slow");
            (slow, waiting, plain(&engine).await)
        });
        assert_ok(&slow);
        assert_eq!(waiting.exit, Exit::Cancelled, "{}", waiting.text);
        assert_eq!(
            masked(&waiting.text),
            "-- error: cancelled while waiting for a free connection\n\
             -- failed (exit N): cancelled, N ms\n"
        );
        assert!(waiting.text.contains("(exit 6)"), "{}", waiting.text);
        assert!(
            waiting.elapsed >= Duration::from_millis(100)
                && waiting.elapsed < Duration::from_millis(500),
            "it ended at the cancel, not when the connection came free: {:?}",
            waiting.elapsed
        );
        assert_ok(&next);
        assert_eq!(fake.accepted(), 1);
        assert_eq!(fake.resets(0), [false, false, false, true, false, false]);
        assert_eq!(engine.idle_count(0), 1);
        assert_eq!(engine.pools[0].permits.available_permits(), 1);
    }

    // The end of the server's life: `close_all` closes every idle connection (the fake sees the
    // client close each one), sends them nothing more, and leaves the pool empty.
    #[test]
    fn close_all_closes_the_idle_connections() {
        let both = || Conn::new().at(2, Turn::Delay(Duration::from_millis(50)));
        let fake = serve_pool(vec![both(), both()]);
        let engine = Rc::new(serving(&fake, DEFAULT_POOL, None));
        let (calls, idle, closed) = block_on_local(async {
            let a = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let b = tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30));
            let calls = [a.await.expect("a"), b.await.expect("b")];
            let idle = engine.idle_count(0);
            engine.close_all().await;
            let closed = fake
                .wait_for(Duration::from_secs(2), |f| {
                    f.ended(0) == Some(End::Client) && f.ended(1) == Some(End::Client)
                })
                .await;
            (calls, idle, closed)
        });
        for called in &calls {
            assert_ok(called);
        }
        assert_eq!(idle, 2, "both connections were idle");
        assert!(closed, "the client closed both connections");
        assert_eq!(engine.idle_count(0), 0);
        for conn in 0..2 {
            assert_eq!(fake.requests(conn).len(), 3, "connection {conn}");
        }
    }

    // Decision 19 as built: the regressions the external review of Phase 5.2 and 5.3 asked to
    // keep (PLAN.md): the probe's two sides, the idle peers a retry drops, a replacement that
    // can't log in, a cancel in the probe's attention wait, and a connection that dies at the
    // call's USE.

    /// A connection of the pooled fake that holds its first call's SELECT 50 ms, and answers the
    /// next call's setup, a reused connection's probe, after `setup`.
    fn answering_the_probe_after(setup: Duration) -> Conn {
        Conn::new()
            .at(2, Turn::Delay(Duration::from_millis(50)))
            .at(4, Turn::Delay(setup))
    }

    /// Two calls at once, so the profile opens two connections, both idle once the calls end:
    /// the next call reuses one, and the other is its idle peer.
    async fn two_idle(engine: &Rc<Engine>) {
        let a = tokio::task::spawn_local(later(Rc::clone(engine), Duration::ZERO, 30));
        let b = tokio::task::spawn_local(later(Rc::clone(engine), Duration::ZERO, 30));
        for called in [a.await.expect("a"), b.await.expect("b")] {
            assert_ok(&called);
        }
        assert_eq!(engine.idle_count(0), 2);
    }

    /// Of `two_idle`'s connections, the one the next call reused (it got more than its first
    /// call's three requests), and its peer.
    fn reused_and_peer(fake: &Fake) -> (usize, usize) {
        if fake.requests(0).len() > 3 {
            (0, 1)
        } else {
            (1, 0)
        }
    }

    // The probe's live side (docs/mcp-mode.md §5, The boundary). The probe cuts a reused setup
    // at 5 s; a server that answers it at 9 s acknowledges the attention 4 s into the driver's
    // 5 s wait for that, so the connection is alive: it is reset and set up again, and the call
    // runs on it. No new login, and the profile's other idle connection is kept.
    #[test]
    fn a_setup_acknowledged_within_the_attention_wait_keeps_its_connection_and_its_peer() {
        let late = || answering_the_probe_after(Duration::from_secs(9));
        let fake = serve_pool(vec![late(), late()]);
        let engine = Rc::new(serving(&fake, DEFAULT_POOL, None));
        let called = block_on_local(async {
            two_idle(&engine).await;
            call(&engine, "SELECT 1", 60, &Cancel::new()).await
        });
        assert_ok(&called);
        assert!(
            called.elapsed >= Duration::from_secs(9) && called.elapsed < Duration::from_secs(12),
            "{:?}",
            called.elapsed
        );
        assert_eq!(fake.accepted(), 2, "no new login");
        let (reused, peer) = reused_and_peer(&fake);
        // The first call's three requests; the cut setup and the setup again, each with the
        // reset; the call's SELECT and its rollback.
        assert_eq!(
            fake.resets(reused),
            [false, false, false, true, true, false, false]
        );
        assert_eq!(fake.requests(peer).len(), 3, "the peer was left alone");
        assert_eq!(engine.idle_count(0), 2, "both connections were kept");
        assert_eq!((fake.ended(reused), fake.ended(peer)), (None, None));
        // The probe and the wait for the attention are the call's connect time.
        assert!(connect_ms(&called.text) >= 9_000, "{}", called.text);
    }

    // The probe's other side (docs/mcp-mode.md §5, The boundary). A server that answers a reused
    // setup only at 11 s leaves the probe's attention unacknowledged past the driver's 5 s wait,
    // so at about 10 s the driver marks the connection dead: it can't be told from a half-open
    // one, and it is replaced like one. The profile's other idle connection goes too, healthy as
    // it is (the documented policy, right for a firewall's idle cut), and the call runs on a new
    // login.
    #[test]
    fn a_setup_silent_past_the_attention_wait_is_replaced_with_its_idle_peer() {
        let late = || answering_the_probe_after(Duration::from_secs(11));
        let fake = serve_pool(vec![late(), late()]);
        let engine = Rc::new(serving(&fake, DEFAULT_POOL, None));
        let (called, both_closed) = block_on_local(async {
            two_idle(&engine).await;
            let called = call(&engine, "SELECT 1", 60, &Cancel::new()).await;
            let both_closed = fake
                .wait_for(Duration::from_secs(5), |f| {
                    f.ended(0) == Some(End::Client) && f.ended(1) == Some(End::Client)
                })
                .await;
            (called, both_closed)
        });
        assert_ok(&called);
        assert!(
            called.elapsed >= Duration::from_secs(9) && called.elapsed < Duration::from_secs(15),
            "{:?}",
            called.elapsed
        );
        assert_eq!(fake.accepted(), 3, "a new login");
        let (reused, peer) = reused_and_peer(&fake);
        assert_eq!(fake.resets(reused), [false, false, false, true]);
        assert_eq!(
            fake.requests(peer).len(),
            3,
            "the healthy peer ran nothing more"
        );
        assert!(
            both_closed,
            "the dead connection and its healthy peer were closed"
        );
        assert_eq!(fake.resets(2), [false, false, false]);
        assert_eq!(engine.idle_count(0), 1, "only the new connection is left");
        assert!(connect_ms(&called.text) >= 9_000, "{}", called.text);
    }

    // Decision 19's one retry, when its new login fails: the call gets the connect error, as a
    // call with nothing to reuse would (exit 2; in sqlh mcp its hint names the profile), and no
    // third attempt. Its permit goes back: with a pool of one, the next call gets the slot at
    // once, logs in on a new connection and succeeds.
    #[test]
    fn a_replacement_that_cannot_log_in_fails_the_call_and_frees_its_slot() {
        let fake = serve_pool(vec![
            Conn::new().close_after(3),
            Conn::new().refusing_login(),
        ]);
        let engine = serving_mcp(&fake, 1, 30);
        let (first, failed, accepted, next) = block_on(async {
            let first = plain(&engine).await;
            let closed = fake
                .wait_for(Duration::from_secs(2), |f| f.ended(0) == Some(End::Server))
                .await;
            assert!(closed, "the server closed the idle connection");
            let failed = plain(&engine).await;
            let accepted = fake.accepted();
            assert_eq!(
                engine.pools[0].permits.available_permits(),
                1,
                "the failed call's permit was released"
            );
            assert_eq!(engine.idle_count(0), 0);
            (first, failed, accepted, plain(&engine).await)
        });
        assert_ok(&first);
        assert_eq!(failed.exit, Exit::Connect, "{}", failed.text);
        assert_eq!(
            untimed(&failed.text),
            format!(
                "-- error: cannot connect to {} (SQL login 'fake', encrypt optional): Login failed \
                 for user 'fake'. (error 18456, severity 14); check the profile's login and its \
                 saved password (sqlh profile add), and that the login may use this database\n\
                 -- failed (exit 2): connection failed, N ms\n",
                fake.server
            )
        );
        assert_eq!(accepted, 2, "no third attempt");
        assert_ok(&next);
        assert_eq!(fake.accepted(), 3);
        assert_eq!(fake.resets(2), [false, false, false]);
        assert_eq!(engine.idle_count(0), 1);
    }

    /// A call on a reused connection cancelled 6 s in, while the probe (cut at 5 s) waits for
    /// its attention's acknowledgement; then the next call. The cancelled call, how many
    /// connections had been accepted and were idle after it, whether the server saw a
    /// transaction still open on the first, and the next call.
    async fn cancelled_in_the_attention_wait(
        fake: &Fake,
        engine: &Engine,
    ) -> (Called, usize, usize, bool, Called) {
        assert_ok(&plain(engine).await);
        let cut = cancelled(engine, Duration::from_secs(6)).await;
        let after = (
            fake.accepted(),
            engine.idle_count(0),
            fake.in_transaction(0),
        );
        (cut, after.0, after.1, after.2, plain(engine).await)
    }

    // A cancel during the probe's wait for its attention (`!cancel.is_cancelled()` in both of
    // `run_pooled`'s conditions): the setup isn't run again and the call isn't retried, and no
    // transaction is left open. A server that acknowledges the attention after the cancel (at
    // 7 s) kept the connection in step, so it goes back to the pool and the next call reuses
    // it; one that never does left it dead, so it is dropped, and the next call logs in anew.
    #[test]
    fn a_cancel_during_the_probes_attention_wait_is_neither_set_up_again_nor_retried() {
        let answering = serve_pool(vec![Conn::new().at(4, Turn::Delay(Duration::from_secs(7)))]);
        let silent = serve_pool(vec![Conn::new().at(4, Turn::Silent)]);
        let (kept, dropped) = (
            serving(&answering, DEFAULT_POOL, None),
            serving(&silent, DEFAULT_POOL, None),
        );
        let (on_answering, on_silent) = block_on(async {
            tokio::join!(
                cancelled_in_the_attention_wait(&answering, &kept),
                cancelled_in_the_attention_wait(&silent, &dropped)
            )
        });

        // The cancelled call ends with the probe's timeout, which sqlh mcp never sends: a
        // cancelled call gets no response (docs/mcp-mode.md §2, §5).
        let probe_cut = "-- error: query timeout after 5 s in the session setup\n\
                         -- failed (exit 4): timeout, N ms\n";
        let (cut, accepted, idle, open, next) = on_answering;
        assert_eq!(untimed(&cut.text), probe_cut);
        assert!(
            cut.elapsed >= Duration::from_millis(6_500) && cut.elapsed < Duration::from_secs(9),
            "{:?}",
            cut.elapsed
        );
        assert_eq!(accepted, 1, "not retried");
        assert_eq!(idle, 1, "the connection the driver kept was pooled");
        assert!(!open, "no transaction was left open");
        assert_ok(&next);
        assert_eq!(answering.accepted(), 1);
        // The first call; the cut setup (its BEGIN TRANSACTION ran), never set up again, and
        // `put_back`'s NOEXEC OFF and rollback; then the next call, reset first.
        let sql = answering.sql(0);
        assert!([0, 3, 6].iter().all(|&at| is_setup(&sql[at])), "{sql:?}");
        assert_eq!(sql[1..3], ["SELECT 1", ROLLBACK], "{sql:?}");
        assert_eq!(sql[4..6], [NOEXEC_OFF, ROLLBACK], "{sql:?}");
        assert_eq!(sql[7..], ["SELECT 1", ROLLBACK], "{sql:?}");
        assert_eq!(
            answering.resets(0),
            [false, false, false, true, false, false, true, false, false]
        );

        // A connection closed holds no transaction (the server rolls it back), whatever the
        // fake last saw on it.
        let (cut, accepted, idle, _, next) = on_silent;
        assert_eq!(untimed(&cut.text), probe_cut);
        assert!(
            cut.elapsed >= Duration::from_secs(9) && cut.elapsed < Duration::from_secs(12),
            "{:?}",
            cut.elapsed
        );
        assert_eq!(accepted, 1, "not retried");
        assert_eq!(idle, 0, "the dead connection was dropped");
        assert_ok(&next);
        assert_eq!(silent.accepted(), 2);
        assert_eq!(silent.ended(0), Some(End::Client), "and closed");
        assert_eq!(silent.requests(0).len(), 4, "the setup wasn't sent again");
    }

    /// A call naming the database `d`, with `-t timeout`, on the connection the call before it
    /// left idle.
    async fn use_on_reused(engine: &Engine, timeout: u32) -> Called {
        assert_ok(&plain(engine).await);
        let use_db = Request {
            database: Some("d".into()),
            ..request("SELECT 1")
        };
        call_with(engine, use_db, timeout, &Cancel::new()).await
    }

    // Decision 19's retry at the call's USE: a reused connection whose setup, with its reset,
    // was acknowledged, and that then dies at the USE, ran nothing of the call, so the call
    // runs again, once, on a new connection, which gets the setup and the USE again. Two ways
    // to die there: the server closes the connection once it has answered the setup, or the
    // USE goes unanswered past the call's timeout and the driver's 5 s wait for the attention,
    // which the retried call reports as connect time.
    #[test]
    fn a_connection_that_dies_at_the_calls_use_is_retried_with_its_use() {
        let closed = serve_pool(vec![Conn::new().close_after(4)]);
        let silent = serve_pool(vec![Conn::new().at(5, Turn::Silent)]);
        let (on_closed, on_silent) = (
            serving(&closed, DEFAULT_POOL, None),
            serving(&silent, DEFAULT_POOL, None),
        );
        let (after_close, after_silence) = block_on(async {
            tokio::join!(use_on_reused(&on_closed, 30), use_on_reused(&on_silent, 6))
        });
        for (called, fake, engine) in [
            (&after_close, &closed, &on_closed),
            (&after_silence, &silent, &on_silent),
        ] {
            assert_ok(called);
            assert_eq!(fake.accepted(), 2, "retried once");
            assert!(
                fake.resets(0)[3],
                "the reused connection's setup carried the reset"
            );
            let sql = fake.sql(1);
            assert!(is_setup(&sql[0]), "{sql:?}");
            assert_eq!(sql[1..], ["USE [d];", "SELECT 1", ROLLBACK], "{sql:?}");
            assert_eq!(fake.resets(1), [false, false, false, false]);
            assert_eq!(engine.idle_count(0), 1);
        }
        assert_eq!(
            closed.requests(0).len(),
            4,
            "the closed connection read no USE"
        );
        assert_eq!(silent.sql(0)[4..], ["USE [d];"]);
        // The call's 6 s and the driver's 5 s for the attention, then a new login: about 11 s.
        assert!(
            after_silence.elapsed >= Duration::from_secs(10)
                && after_silence.elapsed < Duration::from_secs(16),
            "{:?}",
            after_silence.elapsed
        );
        assert!(
            connect_ms(&after_silence.text) >= 10_000,
            "{}",
            after_silence.text
        );
    }

    // What MCP results advise (docs/mcp-mode.md §4's tables), exactly, as the engine's calls
    // end: what a call can change, and the server's own launch flags once a call can't.

    /// One call's exit and text apart from its timings (`untimed`), on a fake server playing
    /// `conns`, its profile served as `sqlh mcp` serves it with the server's `-t limit`, and the
    /// call's own `timeout`. The fake reports no server name, so the status line's tail reads
    /// `?/master`.
    fn mcp_text(
        conns: Vec<Conn>,
        limit: u32,
        request: Request,
        origin: QueryOrigin,
        timeout: u32,
    ) -> (Exit, String) {
        let fake = serve_pool(conns);
        let engine = serving_mcp(&fake, DEFAULT_POOL, limit);
        let called = block_on(call_as(&engine, request, origin, timeout, &Cancel::new()));
        (called.exit, untimed(&called.text))
    }

    // A batch timeout: a larger `timeout`, up to the server's limit (any, when the server has
    // none), or narrower SQL, and once the call has the server's limit, only narrower SQL.
    #[test]
    fn in_mcp_a_batch_timeout_offers_what_the_call_can_raise() {
        for (limit, advice) in [
            (
                30,
                "raise timeout (the server allows up to 30 s) or narrow the query",
            ),
            (0, "raise timeout or narrow the query"),
            (1, "narrow the query (1 s is the server's limit)"),
        ] {
            let hangs = vec![Conn::new().at(2, Turn::Hang { ack: true })];
            let (exit, text) = mcp_text(hangs, limit, request("SELECT 1"), QueryOrigin::User, 1);
            assert_eq!(exit, Exit::Timeout, "{text}");
            assert_eq!(
                text,
                format!(
                    "-- result 1\nx:int\n1\n1\n1\n\
                     -- error: query timeout after 1 s in batch 1; {advice}\n\
                     -- failed (exit 4): timeout, N ms; ?/master, connect N ms\n"
                ),
                "the server's -t {limit}"
            );
        }
    }

    // A schema tool's name lookup that times out (`describe`, `definition` and `deps` look the
    // name up first): a larger `timeout`, or that the call has the server's limit.
    #[test]
    fn in_mcp_a_lookup_timeout_offers_what_the_call_can_raise() {
        for (limit, advice) in [
            (30, "raise timeout (the server allows up to 30 s)"),
            (0, "raise timeout"),
            (1, "1 s is the server's limit"),
        ] {
            let describe = Request {
                input: crate::schema::Command::Describe("dbo.T".into())
                    .input(Advice::Mcp)
                    .expect("a lookup"),
                ..request("")
            };
            let hangs = vec![Conn::new().at(2, Turn::Hang { ack: true })];
            let (exit, text) = mcp_text(hangs, limit, describe, QueryOrigin::Generated, 1);
            assert_eq!(exit, Exit::Timeout, "{text}");
            assert_eq!(
                text,
                format!(
                    "-- error: query timeout after 1 s while looking the name up; {advice}\n\
                     -- failed (exit 4): timeout, N ms; ?/master, connect N ms\n"
                ),
                "the server's -t {limit}"
            );
        }
    }

    // The wait for a free connection, whole: what was busy (the profile's only connection, or
    // all of them), then a larger `timeout` and the server's --pool, or, once the call has the
    // server's limit, the server's --pool and -t.
    #[test]
    fn in_mcp_the_wait_for_a_free_connection_says_what_was_busy_and_what_can_change() {
        for (pool, limit, after) in [
            (
                1,
                30,
                "(the profile's only connection was busy); raise timeout (the server allows up \
                 to 30 s), or start sqlh mcp with a larger --pool",
            ),
            (
                1,
                0,
                "(the profile's only connection was busy); raise timeout, or start sqlh mcp \
                 with a larger --pool",
            ),
            (
                2,
                1,
                "(all 2 of the profile's connections were busy); start sqlh mcp with a larger \
                 --pool or -t",
            ),
        ] {
            let busy = || Conn::new().at(2, Turn::Delay(Duration::from_millis(1500)));
            let fake = serve_pool(vec![busy(), busy()]);
            let engine = Rc::new(serving_mcp(&fake, pool, limit));
            let waiting = block_on_local(async {
                let holders: Vec<_> = (0..pool)
                    .map(|_| {
                        tokio::task::spawn_local(later(Rc::clone(&engine), Duration::ZERO, 30))
                    })
                    .collect();
                let waiting = tokio::task::spawn_local(later(
                    Rc::clone(&engine),
                    Duration::from_millis(50),
                    1,
                ));
                let waiting = waiting.await.expect("waiting");
                for holder in holders {
                    assert_ok(&holder.await.expect("holder"));
                }
                waiting
            });
            assert_eq!(waiting.exit, Exit::Timeout, "{}", waiting.text);
            assert_eq!(
                untimed(&waiting.text),
                format!(
                    "-- error: query timeout after 1 s waiting for a free connection {after}\n\
                     -- failed (exit 4): timeout, N ms\n"
                ),
                "a pool of {pool}, the server's -t {limit}"
            );
        }
    }

    // `check`'s closing note names the tool, not --check.
    #[test]
    fn in_mcp_the_check_note_names_the_tool() {
        let check = Request {
            check: true,
            ..request("SELECT 1")
        };
        let (exit, text) = mcp_text(Vec::new(), 30, check, QueryOrigin::User, 30);
        assert_eq!(exit, Exit::Ok, "{text}");
        assert_eq!(
            text,
            "-- note: check compiled each batch without running it. Missing tables are only \
             reported when a query runs (SQL Server defers their name resolution)\n\
             -- ok: 1 batch, N ms; ?/master, connect N ms\n"
        );
    }

    // The dirty-read note names the profile's setting, not --read-uncommitted.
    #[test]
    fn in_mcp_the_dirty_read_note_names_the_profiles_setting() {
        let fake = serve_pool(Vec::new());
        let profile = Settings {
            read_uncommitted: true,
            ..mcp_settings(&fake, 30)
        };
        let engine = Engine::new(vec![("fake".into(), profile)], DEFAULT_POOL, None);
        let called = block_on(plain(&engine));
        assert_eq!(called.exit, Exit::Ok, "{}", called.text);
        assert_eq!(
            untimed(&called.text),
            "-- note: reads were uncommitted (the profile's read_uncommitted = true): rows may \
             include uncommitted, duplicated or missing data\n\
             -- result 1\nx:int\n1\n-- 1 row\n\
             -- ok: 1 batch, N ms; ?/master, connect N ms\n"
        );
    }

    // A count of hidden rows that sqlh stopped (`max_rows 1`, then two hidden rows and the
    // count's 250 ms) and the server never confirmed (exit 6, after the driver's 5 s): the way
    // to read every row is the profile's exact_count. The result it was cut in gets no footer,
    // as when a lost connection ends a run inside a result
    // (`a_connection_lost_mid_run_is_not_retried`); the error says the rows shown are complete.
    #[test]
    fn in_mcp_an_unconfirmed_count_stop_names_the_profiles_exact_count() {
        let fake = serve_pool(vec![Conn::new().at(2, Turn::Hang { ack: false })]);
        let mut profile = mcp_settings(&fake, 30);
        profile.limits.max_rows = 1;
        let engine = Engine::new(vec![("fake".into(), profile)], DEFAULT_POOL, None);
        let called = block_on(plain(&engine));
        assert_eq!(called.exit, Exit::Cancelled, "{}", called.text);
        assert_eq!(
            untimed(&called.text),
            "-- result 1\nx:int\n1\n\
             -- error: stopped counting the rows that are not shown, but the server didn't \
             confirm the cancel, so the connection was closed; the rows shown are complete (the \
             profile's exact_count = true reads every row instead)\n\
             -- failed (exit 6): cancelled, N ms; ?/master, connect N ms\n"
        );
    }
}
