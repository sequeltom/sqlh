//! The connection-reuse engine (PLAN.md Phase 5.2; docs/mcp-mode.md §5; owner decisions 14,
//! 15, 17 and 19). It keeps a small pool of open connections per profile and runs each call
//! through the execution core (`run`), exactly as a CLI run does, on a connection that is
//! reset first, so a call behaves like one CLI run without the connect. It has no MCP in it:
//! `sqlh mcp` is its first front end, and a daemon behind the CLI could use it unchanged.
//!
//! - **Pool:** at most `max` connections per profile, opened only when a call needs one. A call
//!   beyond them waits for one, at most its timeout. A connection idle longer than `idle_after`
//!   is closed (`sweep`, and when a call would take it).
//! - **Reset:** a reused connection's first request carries the reset
//!   (`Session::reset_before_next`), whose acknowledgement the driver verifies; then the session
//!   setup runs, as after a connect (`run::open`).
//! - **Retry:** a reused connection that turns out dead at that first request (a network drop,
//!   a session a DBA killed, a reset the server didn't acknowledge) is replaced and the call run
//!   again, once, silently: nothing of the call has run, and every call is a read in a
//!   rolled-back wrapper. A failure after that point is the call's own.
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
use crate::output::Output;
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
                        .map_err(run::db_failure)?,
                    false,
                ),
            };
            let connect_ms = if reused {
                0
            } else {
                connect_started.elapsed().as_millis()
            };
            let setup_ms =
                match run::open(&mut session, &pool.settings, prepared, timeout, cancel).await {
                    Ok(setup_ms) => setup_ms,
                    Err(error) if reused && !retried && dead_before_work(&error, &session) => {
                        // Nothing of the call ran: replace the connection and run it again.
                        retried = true;
                        drop(session);
                        continue;
                    }
                    Err(error) => {
                        self.put_back(pool, session).await;
                        return Err(run::db_failure(error));
                    }
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
    async fn put_back(&self, pool: &Pool, mut session: Session) {
        if session.is_retired() || session.connection_dead() {
            return;
        }
        if session.in_transaction() {
            let cancel = Cancel::new();
            let rollback = session.exec_internal(
                "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;",
                ROLLBACK_WAIT.as_secs() as u32,
                &cancel,
            );
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
                "query timeout after {timeout} s waiting for a free connection (all {} of the \
                 profile's connections were busy); raise -t, or --pool for more connections",
                pool.max
            ),
        )),
    }
}

/// Whether an error at a reused connection's first request means the connection was dead
/// before any of the call could run: the driver marked it dead (it broke while idle, which
/// shows as `Lost`, or the server didn't acknowledge the reset, which shows as `Protocol`), or
/// it was retired. A SQL error, a timeout or a cancel is the call's own, whatever the state.
fn dead_before_work(error: &DbError, session: &Session) -> bool {
    (session.connection_dead() || session.is_retired())
        && !matches!(
            error,
            DbError::Timeout(_) | DbError::Cancelled(_) | DbError::Sql(_)
        )
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

    /// One call as the MCP front end makes it: its exit code, and its text with the closing
    /// lines.
    struct Called {
        exit: Exit,
        text: String,
        elapsed: Duration,
    }

    /// Runs `sql` as one call on the engine's profile, with `-t timeout`, into text.
    async fn call(engine: &Engine, sql: &str, timeout: u32, cancel: &Cancel) -> Called {
        let started = Instant::now();
        let settings = engine.settings(0);
        let request = Request {
            input: Input::Query(sql.into()),
            vars: None,
            terminator: "GO".into(),
            check: false,
            continue_on_error: false,
            report: Report::Off,
            database: None,
        };
        let prepared = match run::prepare(request, settings, QueryOrigin::User) {
            Ok(prepared) => prepared,
            Err(failure) => panic!("prepare: {}", failure.message),
        };
        let mut out = TextSink::with_meta(Vec::new(), Box::new(io::sink()), settings.limits);
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
        let exit = run::close(&mut out, outcome, started);
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
        let root = Cancel::new();
        let cancel = root.child();
        let canceller = async move {
            tokio::time::sleep(after).await;
            root.cancel();
        };
        let (called, ()) = tokio::join!(call(engine, "SELECT 1", 30, &cancel), canceller);
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
    // is acknowledged, but the server goes silent at the rollback `put_back` sends, which gives
    // up after `ROLLBACK_WAIT` (2 s), so the call ends with its own result, well inside the
    // settle grace. Either way the connection is discarded, never put back, and the next call
    // reconnects. The one slow test (the first case's bounds are 5 s), so the two run at once.
    #[test]
    fn a_cancel_that_does_not_settle_discards_the_connection() {
        let unacknowledged = serve_pool(vec![Conn::new().at(2, Turn::Hang { ack: false })]);
        let rollback_unanswered = serve_pool(vec![
            Conn::new()
                .at(2, Turn::Hang { ack: true })
                .at(3, Turn::Silent),
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
        assert_eq!(rollback_unanswered.sql(0)[2], ROLLBACK);
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
        // The cancelled call's setup, its SELECT and the rollback `put_back` sent; then the
        // next call, reset first.
        assert_eq!(fake.resets(0), [false, false, false, true, false, false]);
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
        assert_eq!(sql[1..], ["SELECT 1", ROLLBACK]);
        assert!(!fake.in_transaction(0));
        {
            let idle = engine.pools[0].idle.borrow();
            assert_eq!(idle.len(), 1);
            assert!(!idle[0].session.in_transaction());
        }

        // The rollback (request 3) meets a lost connection: dropped, not pooled.
        let fake = serve_pool(vec![
            Conn::new()
                .at(2, Turn::Hang { ack: true })
                .at(3, Turn::Drop),
        ]);
        let engine = serving(&fake, DEFAULT_POOL, None);
        let (cancelled, idle, next) = block_on(async {
            let cancelled = cancelled(&engine, Duration::from_millis(100)).await;
            let idle = engine.idle_count(0);
            (cancelled, idle, plain(&engine).await)
        });
        assert_eq!(cancelled.exit, Exit::Cancelled, "{}", cancelled.text);
        assert_eq!(fake.sql(0)[2], ROLLBACK);
        assert_eq!(idle, 0, "the connection whose rollback failed was dropped");
        assert_ok(&next);
        assert_eq!(fake.accepted(), 2);
    }
}
