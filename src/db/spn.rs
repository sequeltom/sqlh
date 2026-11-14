//! Windows auth's SPN for each login hop (owner decision 72): the driver's
//! `SecurityContextProvider`, which it calls after a hop's TCP connect, prelogin and TLS, with
//! the host and port it dials: the port given or 1433, SQL Browser's port for a named instance,
//! 1434 for `admin:`, and each routed host's own.
//!
//! The SPN is built by the driver's `make_spn` from the canonical name sqlh's lookup gives within
//! 500 ms, else from the host as dialled, which is what the driver builds when its lookup fails.
//! Either way the config carries the SPN, so the driver makes no lookup of its own, which has no
//! bound (upstream-issues.md U9; about 5 s for an address with no PTR record, enough to fail a
//! login at `-l 3` or less). Only the SPN changes, so the TLS channel bindings and the loopback
//! retry are the driver's.
//!
//! The lookup is `canonical_name` (owner decision 79): a name's forward lookup, not the driver's
//! reverse one, which named Azure's host instead of the domain's and so fell back to NTLM where
//! Microsoft's SqlClient got Kerberos (upstream-issues.md U13).

use std::net::IpAddr;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use mssql_tds::security::{
    IntegratedAuthConfig, SecurityContext, SecurityContextProvider, SecurityError,
    canonicalize_hostname, create_security_context, is_loopback_address, make_spn,
};

/// How long a hop's name lookup gets before the SPN is built from the host as dialled. A name
/// server answers well inside this; what it cuts is a lookup that falls through to NetBIOS, about
/// 5 s for an address with no PTR record (notes/connect-latency.md, Windows authentication). It
/// runs inside the login deadline (`-l` + 1 s), once per hop.
const SPN_LOOKUP_BUDGET: Duration = Duration::from_millis(500);

/// How many lookups may run at once in this process. A lookup past its budget is left running,
/// so in `sqlh mcp`, under DNS trouble and repeated cancels, they would pile up without a cap
/// (R53); a slot is held until the lookup itself ends. std's lock and condition variable, not
/// tokio's semaphore: the provider runs synchronously on a thread of the driver's blocking pool,
/// where a tokio timer would need the runtime thread, and panic once the runtime has shut down.
const LOOKUP_THREADS: usize = 4;

static LOOKUPS: Slots = Slots::new(LOOKUP_THREADS);

pub(super) type Lookup = Arc<dyn Fn(&str) -> String + Send + Sync>;
pub(super) type Create = Arc<
    dyn Fn(&IntegratedAuthConfig, &str, u16) -> Result<Box<dyn SecurityContext>, SecurityError>
        + Send
        + Sync,
>;

pub(super) struct HopSpn {
    budget: Duration,
    slots: &'static Slots,
    /// `canonical_name`, which never fails and gives back its input when nothing resolves.
    lookup: Lookup,
    /// Which hosts go to the lookup: `not_loopback` (tests on a 127.x fake take every host).
    looked_up: fn(&str) -> bool,
    /// The driver's own context, `create_security_context`, run on the driver's thread, not
    /// the lookup's, so it sees the caller's identity as the built-in path does.
    create: Create,
}

impl HopSpn {
    pub(super) fn system() -> Arc<dyn SecurityContextProvider> {
        Arc::new(HopSpn {
            budget: SPN_LOOKUP_BUDGET,
            slots: &LOOKUPS,
            lookup: Arc::new(|host: &str| canonical_name(host, crate::dns::canonical_name)),
            looked_up: not_loopback,
            create: Arc::new(create_security_context),
        })
    }

    #[cfg(test)]
    pub(super) fn with(
        budget: Duration,
        slots: &'static Slots,
        lookup: Lookup,
        looked_up: fn(&str) -> bool,
        create: Create,
    ) -> HopSpn {
        HopSpn {
            budget,
            slots,
            lookup,
            looked_up,
            create,
        }
    }
}

impl SecurityContextProvider for HopSpn {
    // Never an error of its own: one would end every transport of the connect and read as a
    // Kerberos failure. Slowness, a full cap or a failed thread give the fallback SPN; the only
    // error is the driver's own, from creating its context.
    fn create_context(
        &self,
        config: &IntegratedAuthConfig,
        server: &str,
        port: u16,
    ) -> Result<Box<dyn SecurityContext>, SecurityError> {
        let canonical = (self.looked_up)(server)
            .then(|| canonical_within(self.slots, self.budget, server, &self.lookup))
            .flatten();
        let spn = make_spn(canonical.as_deref().unwrap_or(server), None, port);
        // Only the SPN changes. One the driver passes is replaced: sqlh never sets
        // `ClientContext::server_spn`, whose one value would go to every hop (R52).
        let config = IntegratedAuthConfig {
            server_spn: Some(spn),
            ..config.clone()
        };
        (self.create)(&config, server, port)
    }
}

/// Whether `host` goes to the lookup: any name but one the driver's `is_loopback_address` takes
/// for this machine (`localhost`, `127.*`, `::1`, `.`, the DNS host name), which
/// `canonicalize_hostname` gives back as it is. A local `\\.\pipe`, shared memory and LocalDB
/// arrive as `localhost`; a pipe named by host arrives under that name. A dotted name goes to
/// the lookup too, which follows an alias to its host (decision 79), where the driver's rule
/// looks a dotted name up not at all.
fn not_loopback(host: &str) -> bool {
    !is_loopback_address(host)
}

/// The name a hop's SPN is built from (decision 79). An address keeps the driver's rule,
/// `canonicalize_hostname`: its reverse lookup's name, else the address. A name, short or full,
/// takes its forward lookup's canonical name, else stays as given: a short name gets its DNS
/// suffix, and an alias (a CNAME) the real host's name, whose SPN the service registers. The
/// driver's rule for a short name, forward then reverse, takes whatever a PTR record says, which
/// needn't be the domain's name (U13); for a dotted name it looks nothing up, so an alias kept
/// its own name. `forward` is the lookup, a parameter for the tests.
fn canonical_name(host: &str, forward: fn(&str) -> Option<String>) -> String {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<IpAddr>().is_ok() {
        return canonicalize_hostname(host);
    }
    forward(host)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| host.to_owned())
}

/// The lookup's answer if it came within `budget`, else `None`. The wait for a slot comes out of
/// the same budget (R53). A lookup past it is left running on its own thread, holding its slot
/// until it ends: it can't be cancelled, and process exit doesn't wait for it.
fn canonical_within(
    slots: &'static Slots,
    budget: Duration,
    host: &str,
    lookup: &Lookup,
) -> Option<String> {
    let deadline = Instant::now() + budget;
    let slot = slots.take(deadline)?;
    // Room for the answer, so the send never blocks once the wait has gone.
    let (answer, answered) = mpsc::sync_channel(1);
    let (host, lookup) = (host.to_owned(), Arc::clone(lookup));
    std::thread::Builder::new()
        .name("sqlh-spn-lookup".into())
        .spawn(move || {
            let canonical = lookup(&host);
            drop(slot);
            let _ = answer.send(canonical);
        })
        .ok()?;
    answered
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
}

/// A count of running lookups, at most `most`.
pub(super) struct Slots {
    taken: Mutex<usize>,
    freed: Condvar,
    most: usize,
}

impl Slots {
    pub(super) const fn new(most: usize) -> Slots {
        Slots {
            taken: Mutex::new(0),
            freed: Condvar::new(),
            most,
        }
    }

    /// A slot, waiting for one to be freed until `deadline`.
    fn take(&'static self, deadline: Instant) -> Option<Slot> {
        let taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
        let wait = deadline.saturating_duration_since(Instant::now());
        let (mut taken, _) = self
            .freed
            .wait_timeout_while(taken, wait, |taken| *taken >= self.most)
            .unwrap_or_else(PoisonError::into_inner);
        if *taken >= self.most {
            return None;
        }
        *taken += 1;
        Some(Slot(self))
    }

    #[cfg(test)]
    fn taken(&self) -> usize {
        *self.taken.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Slot(&'static Slots);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.taken.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.0.freed.notify_one();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use mssql_tds::security::SecurityPackage;
    use mssql_tds::security::mock::MockSecurityContext;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What the driver's context was created with, for one hop.
    #[derive(Clone, Debug, PartialEq)]
    pub(in crate::db) struct Hop {
        pub(in crate::db) server: String,
        pub(in crate::db) port: u16,
        pub(in crate::db) spn: Option<String>,
        pub(in crate::db) loopback: bool,
        pub(in crate::db) bindings: Option<Vec<u8>>,
        pub(in crate::db) package: &'static str,
    }

    /// The production rule, for tests that call `HopSpn::with`.
    pub(in crate::db) const NOT_LOOPBACK: fn(&str) -> bool = not_loopback;

    /// The token a recorded hop's context sends.
    pub(in crate::db) const TOKEN: &[u8] = b"sqlh";

    /// A `create` that records each hop and returns a context sending `TOKEN`, in place of the
    /// driver's real SSPI context.
    pub(in crate::db) fn recorder() -> (Create, Arc<Mutex<Vec<Hop>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&seen);
        let create: Create = Arc::new(move |config: &IntegratedAuthConfig, server, port| {
            kept.lock().unwrap().push(Hop {
                server: server.to_owned(),
                port,
                spn: config.server_spn.clone(),
                loopback: config.is_loopback,
                bindings: config.channel_bindings.clone(),
                package: config.security_package.display_name(),
            });
            Ok(Box::new(MockSecurityContext::single_round(TOKEN.to_vec()))
                as Box<dyn SecurityContext>)
        });
        (create, seen)
    }

    /// A lookup held until its `Sender` is dropped (or 30 s), counting its calls.
    pub(in crate::db) fn held() -> (Lookup, mpsc::Sender<()>, Arc<AtomicUsize>) {
        let (release, wait) = mpsc::channel::<()>();
        let wait = Arc::new(Mutex::new(wait));
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let lookup: Lookup = Arc::new(move |host: &str| {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = wait.lock().unwrap().recv_timeout(Duration::from_secs(30));
            host.to_owned()
        });
        (lookup, release, calls)
    }

    pub(in crate::db) fn answering(answer: &'static str, calls: Arc<AtomicUsize>) -> Lookup {
        Arc::new(move |_: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            answer.to_owned()
        })
    }

    /// Waits up to 10 s for `done`: for what a lookup thread does, which nothing makes run
    /// within the budget (a busy machine can start it late).
    fn eventually(what: &str, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(10), "never: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn last(seen: &Arc<Mutex<Vec<Hop>>>) -> Hop {
        seen.lock().unwrap().last().cloned().expect("a hop")
    }

    fn call(provider: &HopSpn, server: &str, port: u16) {
        provider
            .create_context(&IntegratedAuthConfig::default(), server, port)
            .unwrap();
    }

    // Past the budget, the SPN is the driver's own fallback for that hop: the host as dialled and
    // the port dialled, whatever `-S` said (a named instance's is SQL Browser's), and the call
    // returns with the budget, the lookup left running.
    #[test]
    fn a_late_lookup_names_the_dialled_host_and_port() {
        static SLOTS: Slots = Slots::new(8);
        let (lookup, release, calls) = held();
        let (create, seen) = recorder();
        let provider = HopSpn::with(
            Duration::from_millis(50),
            &SLOTS,
            lookup,
            NOT_LOOPBACK,
            create,
        );
        for (server, port, spn) in [
            ("192.0.2.1", 1500, "MSSQLSvc/192.0.2.1:1500"),
            ("db01", 1433, "MSSQLSvc/db01:1433"),
            ("2001:db8::5", 1433, "MSSQLSvc/2001:db8::5:1433"),
            ("db01", 50123, "MSSQLSvc/db01:50123"),
        ] {
            let started = Instant::now();
            call(&provider, server, port);
            assert!(started.elapsed() < Duration::from_secs(2), "{server}");
            let hop = last(&seen);
            assert_eq!((hop.server.as_str(), hop.port), (server, port));
            assert_eq!(hop.spn.as_deref(), Some(spn), "{server}");
        }
        eventually("four lookups started", || calls.load(Ordering::SeqCst) == 4);
        drop(release);
    }

    // In time, the SPN names what the lookup found for the dialled host: `make_spn_canonicalized`'s,
    // so the driver has nothing left to look up. A lookup that found nothing gives back its input.
    #[test]
    fn an_answer_in_time_names_the_spn() {
        static SLOTS: Slots = Slots::new(4);
        let found: Lookup = Arc::new(|host: &str| format!("{host}.corp.example"));
        let nothing: Lookup = Arc::new(|host: &str| host.to_owned());
        for (lookup, spn) in [
            (found, "MSSQLSvc/db01.corp.example:1500"),
            (nothing, "MSSQLSvc/db01:1500"),
        ] {
            let (create, seen) = recorder();
            let provider = HopSpn::with(
                Duration::from_secs(10),
                &SLOTS,
                lookup,
                NOT_LOOPBACK,
                create,
            );
            call(&provider, "db01", 1500);
            assert_eq!(last(&seen).spn.as_deref(), Some(spn));
        }
        // The slot is given back before the answer is sent.
        assert_eq!(SLOTS.taken(), 0);
    }

    // A loopback name isn't looked up, as the driver doesn't look it up; the SPN is still set,
    // so the driver makes no lookup of its own.
    #[test]
    fn a_loopback_name_is_not_looked_up() {
        static SLOTS: Slots = Slots::new(4);
        let calls = Arc::new(AtomicUsize::new(0));
        let (create, seen) = recorder();
        let provider = HopSpn::with(
            Duration::from_secs(10),
            &SLOTS,
            answering("elsewhere.example", Arc::clone(&calls)),
            NOT_LOOPBACK,
            create,
        );
        let mut names = vec![
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.5.5.5",
            "::1",
            "[::1]",
            ".",
        ];
        let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
        if is_loopback_address(&computer) {
            names.push(&computer);
        }
        for name in names {
            call(&provider, name, 1433);
            assert_eq!(last(&seen).spn, Some(make_spn(name, None, 1433)), "{name}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    // The config reaches the driver's context as it came, the SPN apart: the TLS channel
    // bindings, the loopback flag that allows the empty-SPN retry, and the package. A fixed SPN
    // is replaced by the hop's own (R52). The fake server has no TLS, so this is the only
    // coverage of the bindings.
    #[test]
    fn the_config_passes_through_with_only_the_spn_set() {
        static SLOTS: Slots = Slots::new(4);
        let (create, seen) = recorder();
        let provider = HopSpn::with(
            Duration::from_secs(10),
            &SLOTS,
            answering("db01.corp.example", Arc::new(AtomicUsize::new(0))),
            NOT_LOOPBACK,
            create,
        );
        let config = IntegratedAuthConfig {
            server_spn: Some("MSSQLSvc/fixed:1".into()),
            security_package: SecurityPackage::Kerberos,
            channel_bindings: Some(vec![1, 2, 3]),
            is_loopback: true,
        };
        provider.create_context(&config, "db01", 1433).unwrap();
        assert_eq!(
            last(&seen),
            Hop {
                server: "db01".into(),
                port: 1433,
                spn: Some("MSSQLSvc/db01.corp.example:1433".into()),
                loopback: true,
                bindings: Some(vec![1, 2, 3]),
                package: "Kerberos",
            }
        );
    }

    // Decision 79: a name, short or dotted, takes its forward lookup's canonical name, so a short
    // name gets its domain and an alias its host; with no answer, or an empty one, it stays as
    // given. An address never goes to the forward lookup: it keeps the driver's rule (here the
    // loopback ones, which `canonicalize_hostname` gives back without a network lookup).
    #[test]
    fn a_name_takes_its_forward_lookups_canonical_name_and_an_address_doesnt() {
        fn found(host: &str) -> Option<String> {
            Some(match host {
                "db01" => "db01.corp.example".into(),
                "sqlalias.corp.example" | "sqlalias" => "db01.corp.example".into(),
                other => other.to_owned(),
            })
        }
        fn nothing(_: &str) -> Option<String> {
            None
        }
        fn empty(_: &str) -> Option<String> {
            Some(String::new())
        }
        fn never(host: &str) -> Option<String> {
            panic!("{host} went to the forward lookup")
        }
        for (host, forward, name) in [
            (
                "db01",
                found as fn(&str) -> Option<String>,
                "db01.corp.example",
            ),
            ("sqlalias", found, "db01.corp.example"),
            ("sqlalias.corp.example", found, "db01.corp.example"),
            ("db01.corp.example", found, "db01.corp.example"),
            ("db01", nothing, "db01"),
            ("db01.corp.example", nothing, "db01.corp.example"),
            ("db01", empty, "db01"),
            ("127.0.0.1", never, "127.0.0.1"),
            ("::1", never, "::1"),
            ("[::1]", never, "[::1]"),
        ] {
            assert_eq!(canonical_name(host, forward), name, "{host}");
        }
    }

    // The driver's own error comes back unchanged.
    #[test]
    fn the_drivers_error_passes_through() {
        static SLOTS: Slots = Slots::new(4);
        let create: Create = Arc::new(|_: &IntegratedAuthConfig, _: &str, _: u16| {
            Err(SecurityError::TargetUnknown("MSSQLSvc/x:1".into()))
        });
        let provider = HopSpn::with(
            Duration::from_secs(10),
            &SLOTS,
            answering("x", Arc::new(AtomicUsize::new(0))),
            NOT_LOOPBACK,
            create,
        );
        let error = provider
            .create_context(&IntegratedAuthConfig::default(), "db01", 1433)
            .err()
            .expect("the driver's error");
        assert!(
            matches!(error, SecurityError::TargetUnknown(_)),
            "{error:?}"
        );
    }

    // R53: the threads left behind by lookups past their budget are capped, each holds its slot
    // until it ends, and a wait for a slot ends with the budget.
    #[test]
    fn lookups_past_their_budget_are_capped_and_hold_their_slot_until_they_end() {
        static SLOTS: Slots = Slots::new(2);
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        let mut releases = Vec::new();
        for _ in 0..8 {
            let (release, held) = mpsc::channel::<()>();
            releases.push(release);
            let held = Mutex::new(held);
            let lookup: Lookup = Arc::new(move |host: &str| {
                let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
                MOST.fetch_max(now, Ordering::SeqCst);
                let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(30));
                RUNNING.fetch_sub(1, Ordering::SeqCst);
                host.to_owned()
            });
            let started = Instant::now();
            assert_eq!(
                canonical_within(&SLOTS, Duration::from_millis(20), "db01", &lookup),
                None
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }
        assert_eq!(SLOTS.taken(), 2);
        let never = Arc::new(AtomicUsize::new(0));
        let past_the_cap = answering("db01.corp.example", Arc::clone(&never));
        assert_eq!(
            canonical_within(&SLOTS, Duration::from_millis(20), "db01", &past_the_cap),
            None
        );
        assert_eq!(never.load(Ordering::SeqCst), 0, "a lookup ran past the cap");
        // Both held lookups have to be running before they are released, or they never overlap.
        eventually("both held lookups running", || {
            RUNNING.load(Ordering::SeqCst) == 2
        });
        drop(releases);
        let freed = Instant::now();
        while SLOTS.taken() > 0 {
            assert!(
                freed.elapsed() < Duration::from_secs(10),
                "a slot was never given back"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(RUNNING.load(Ordering::SeqCst), 0);
        assert_eq!(MOST.load(Ordering::SeqCst), 2);
        assert_eq!(
            canonical_within(&SLOTS, Duration::from_secs(10), "db01", &past_the_cap).as_deref(),
            Some("db01.corp.example")
        );
    }

    // A slot freed within a waiting call's budget is taken by it.
    #[test]
    fn a_slot_freed_within_the_budget_is_taken() {
        static SLOTS: Slots = Slots::new(1);
        let (lookup, release, _) = held();
        assert_eq!(
            canonical_within(&SLOTS, Duration::from_millis(20), "db01", &lookup),
            None
        );
        assert_eq!(SLOTS.taken(), 1);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(release);
        });
        let answer = answering("db01.corp.example", Arc::new(AtomicUsize::new(0)));
        assert_eq!(
            canonical_within(&SLOTS, Duration::from_secs(5), "db01", &answer).as_deref(),
            Some("db01.corp.example")
        );
    }
}
