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

/// The longest a rollback of a connection being put back may take.
const ROLLBACK_SECS: u32 = 5;

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

    /// The index of the profile called `name`, if the engine serves it.
    pub(crate) fn profile(&self, name: &str) -> Option<usize> {
        self.pools.iter().position(|pool| pool.name == name)
    }

    /// A profile's settings.
    pub(crate) fn settings(&self, profile: usize) -> &Settings {
        &self.pools[profile].settings
    }

    /// How many connections a profile's pool holds idle now.
    #[cfg(test)]
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
                            "cancelled: nothing confirmed the cancel within {} s, so the \
                             connection was closed",
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
            let rolled_back = session
                .exec_internal(
                    "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;",
                    ROLLBACK_SECS,
                    &Cancel::new(),
                )
                .await;
            if rolled_back.is_err() || session.connection_dead() || session.in_transaction() {
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
                pool.permits.available_permits().max(1)
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
