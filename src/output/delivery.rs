//! Output delivery (PLAN.md 2.3, review F07): a thread does the writing, so a reader that
//! stops reading can't block the async runtime.
//!
//! - The sinks write into `ChannelWriter`s, which hand full chunks (never more than `CHUNK`
//!   bytes, however much a caller writes at once) to the thread through a queue and never
//!   block.
//! - While the queue holds more than `QUEUE_LIMIT` bytes, the batch loop awaits
//!   `Gate::room` instead of reading more rows, so the runtime keeps handling timers and
//!   Ctrl+C, and the queue stays about that size. The limit is a threshold checked between
//!   rows, not a hard ceiling.
//! - The thread writes a chunk in units of at most `WRITE_UNIT` bytes. Each write that takes
//!   bytes is progress: the bytes stop counting as queued, so room opens as the reader
//!   drains, and the stall clock restarts. A reader that keeps taking output isn't stalled,
//!   however slowly.
//! - When the reader makes no progress on waiting output for the stall limit (the query
//!   timeout, `-t`), the output couldn't be delivered: exit 7. Time with nothing to write
//!   doesn't count.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bytes a `ChannelWriter` collects before handing them to the thread.
const CHUNK: usize = 64 * 1024;

/// The most the thread hands to one `write` call. A write to a pipe returns only when the
/// reader has taken all of it, and the stall clock restarts after each write, so a reader
/// must take this much within the stall limit to not be a stall. 4 KiB is the buffer of a
/// small pipe (the ones `std::process` makes). Smaller units mean more system calls: 9 MB
/// written to a file took about 15 ms longer than with 64 KiB writes, and a run within the
/// default 1 MB output cap showed no difference.
const WRITE_UNIT: usize = 4 * 1024;

/// Bytes queued for the thread before the batch loop waits for the reader.
const QUEUE_LIMIT: usize = 256 * 1024;

/// Which stream a chunk goes to: csv sends its metadata to stderr, apart from the data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest {
    Main,
    Meta,
}

struct State {
    queue: VecDeque<(Dest, Vec<u8>)>,
    /// No more chunks will come; the thread flushes and stops once the queue is empty.
    closed: bool,
    /// The thread has stopped: everything was written and flushed, or a write failed.
    done: bool,
    /// The first write error, kept as kind and text because `io::Error` isn't `Clone`.
    error: Option<(io::ErrorKind, String)>,
    /// When the reader last took a write (of at most `WRITE_UNIT` bytes), or the thread was
    /// handed work while it waited for some: a reader that has made no progress since then
    /// for the stall limit has stalled. Time with nothing to write doesn't count.
    progress: Instant,
    /// The thread is waiting for work, not for the reader.
    idle: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Bytes in the queue or being written, readable without the lock on every row.
    queued: AtomicUsize,
    /// A write has failed (`State::error` says how), checked without the lock on every write.
    failed: AtomicBool,
    /// Wakes the thread when a chunk arrives or the queue closes.
    work: Condvar,
    /// Wakes `Delivery::finish` when the thread makes progress or stops.
    progress: Condvar,
    /// Wakes `Gate::room` and `Delivery::finish_async` (never both at once: the first waits
    /// during the run, the second after it) when the thread makes progress or stops.
    room: tokio::sync::Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock means the thread panicked; the panic hook ends the process anyway.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn error(&self) -> Option<io::Error> {
        if !self.failed.load(Ordering::Acquire) {
            return None;
        }
        self.lock()
            .error
            .as_ref()
            .map(|(kind, text)| io::Error::new(*kind, text.clone()))
    }

    /// Wakes the thread for new work, a chunk or the close. If it was waiting for work rather
    /// than for the reader, the stall clock starts now.
    fn give_work(&self, state: &mut State) {
        if state.idle {
            state.idle = false;
            state.progress = Instant::now();
        }
        self.work.notify_one();
    }

    /// Records the first error; later ones add nothing.
    fn fail(&self, state: &mut State, error: &io::Error) {
        state.error.get_or_insert((error.kind(), error.to_string()));
        self.failed.store(true, Ordering::Release);
    }
}

/// `why` names the limit when it is the stall limit (-t), and is empty for a one-off limit.
fn stalled(limit: Duration, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "the reader stopped reading for {} s{why}",
            limit.as_secs().max(1)
        ),
    )
}

/// The writing thread and its queue. Create the writers with `writer`, then call `finish`
/// once the sinks have flushed everything into them.
pub struct Delivery {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// How long the reader may make no progress before delivery fails (None: forever).
    stall: Option<Duration>,
}

impl Delivery {
    /// Starts the thread. `meta` is the second stream, if the format has one.
    pub fn start(
        main: Box<dyn Write + Send>,
        meta: Option<Box<dyn Write + Send>>,
        stall: Option<Duration>,
    ) -> Delivery {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                closed: false,
                done: false,
                error: None,
                progress: Instant::now(),
                idle: true,
            }),
            queued: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            work: Condvar::new(),
            progress: Condvar::new(),
            room: tokio::sync::Notify::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("sqlh-output".into())
            .spawn(move || write_out(&thread_shared, main, meta))
            .ok();
        if thread.is_none() {
            let mut state = shared.lock();
            shared.fail(
                &mut state,
                &io::Error::other("cannot start the output thread"),
            );
            state.done = true;
        }
        Delivery {
            shared,
            thread,
            stall,
        }
    }

    pub fn writer(&self, dest: Dest) -> ChannelWriter {
        ChannelWriter {
            shared: Arc::clone(&self.shared),
            dest,
            buf: Vec::with_capacity(CHUNK),
        }
    }

    pub fn gate(&self) -> Gate {
        Gate {
            shared: Arc::clone(&self.shared),
            stall: self.stall,
        }
    }

    /// Waits until everything queued has been written and flushed, the files closed. Fails
    /// with the first write error, or when the reader makes no progress for `stall`
    /// (`None`: the delivery's own limit). On a stall the thread is left blocked; the
    /// process ends without it.
    pub fn finish(&mut self, stall: Option<Duration>) -> io::Result<()> {
        let (stall, why) = match stall {
            Some(limit) => (Some(limit), ""),
            None => (self.stall, " (-t)"),
        };
        let mut state = self.shared.lock();
        state.closed = true;
        self.shared.give_work(&mut state);
        while !state.done {
            match stall {
                Some(limit) => {
                    let waited = state.progress.elapsed();
                    if waited >= limit {
                        // Detached for good, so dropping the delivery doesn't wait again
                        // (for the full stall limit, after a cancel's shorter one).
                        self.thread = None;
                        return Err(stalled(limit, why));
                    }
                    state = self
                        .shared
                        .progress
                        .wait_timeout(state, limit - waited)
                        .map(|(guard, _)| guard)
                        .unwrap_or_else(|e| e.into_inner().0);
                }
                None => {
                    state = self
                        .shared
                        .progress
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
        }
        drop(state);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        match self.shared.error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// `finish` for the async runtime (R10): the same conditions and the same stall rule, but
    /// the wait doesn't block the thread, so a signal can cut it short. `within` also caps the
    /// whole wait, whatever the reader is doing (`None`: no cap beyond `stall`). Giving up
    /// detaches the thread for good, as in `finish`.
    ///
    /// Cancel-safe: dropping the future leaves the delivery closed but unfinished, and a
    /// later `finish` or `finish_async` carries on from there.
    pub async fn finish_async(
        &mut self,
        stall: Option<Duration>,
        within: Option<Duration>,
    ) -> io::Result<()> {
        let (stall, why) = match stall {
            Some(limit) => (Some(limit), ""),
            None => (self.stall, " (-t)"),
        };
        let cap = within.map(|within| (Instant::now() + within, within));
        {
            let mut state = self.shared.lock();
            state.closed = true;
            self.shared.give_work(&mut state);
        }
        loop {
            // Registered before looking at the state, so a wakeup in between isn't lost.
            let notified = self.shared.room.notified();
            let wait: Option<Duration> = {
                let state = self.shared.lock();
                if state.done {
                    break;
                }
                let mut wait = None;
                if let Some(limit) = stall {
                    let waited = state.progress.elapsed();
                    if waited >= limit {
                        self.thread = None;
                        return Err(stalled(limit, why));
                    }
                    wait = Some(limit - waited);
                }
                if let Some((deadline, within)) = cap {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        self.thread = None;
                        return Err(unfinished(within));
                    }
                    wait = Some(wait.map_or(left, |w: Duration| w.min(left)));
                }
                wait
            };
            match wait {
                Some(left) => {
                    let _ = tokio::time::timeout(left, notified).await;
                }
                None => notified.await,
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        match self.shared.error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// The cap on a wait (`finish_async`'s `within`) ran out while the reader was still taking
/// output, too slowly to finish in time.
fn unfinished(within: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "the reader didn't take the rest of the output within {} s",
            within.as_secs().max(1)
        ),
    )
}

impl Drop for Delivery {
    /// Delivers what's left when nobody called `finish` (a profile command's notes before its
    /// test), as far as the stall limit allows.
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.finish(None);
        }
    }
}

/// The thread: writes chunks in order until the queue closes, then flushes both streams and
/// drops them (closing a `-o` file before it is renamed into place).
fn write_out(
    shared: &Shared,
    mut main: Box<dyn Write + Send>,
    mut meta: Option<Box<dyn Write + Send>>,
) {
    loop {
        let (dest, chunk) = {
            let mut state = shared.lock();
            loop {
                if let Some(item) = state.queue.pop_front() {
                    break item;
                }
                if state.closed {
                    drop(state);
                    let flushed = main
                        .flush()
                        .and_then(|()| meta.as_mut().map_or(Ok(()), |m| m.flush()));
                    drop(main);
                    drop(meta);
                    let mut state = shared.lock();
                    if let Err(e) = flushed {
                        shared.fail(&mut state, &e);
                    }
                    state.done = true;
                    shared.progress.notify_all();
                    shared.room.notify_one();
                    return;
                }
                state.idle = true;
                state = shared.work.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let out: &mut dyn Write = match (dest, meta.as_mut()) {
            (Dest::Meta, Some(meta)) => &mut **meta,
            _ => &mut *main,
        };
        if let Err(e) = write_units(shared, out, &chunk) {
            let mut state = shared.lock();
            shared.fail(&mut state, &e);
            state.done = true;
            // Nothing is written from here on, and `ChannelWriter::send` queues nothing once
            // `done` is set (under this lock), so nothing is left counted as queued.
            state.queue.clear();
            shared.queued.store(0, Ordering::Relaxed);
            drop(state);
            shared.progress.notify_all();
            shared.room.notify_one();
            return;
        }
    }
}

/// Writes `chunk` to `out` in units of at most `WRITE_UNIT` bytes, like `write_all`: it goes
/// on after a partial write, retries an interrupted one, and fails on a write that takes
/// nothing. After every write that took `n` bytes, those bytes stop counting as queued, the
/// stall clock restarts and the waiters are woken. Progress is therefore what the reader
/// actually takes, not whole chunks: a reader that keeps reading is never a stall, however
/// slowly, and one that blocks a single write for the stall limit is.
fn write_units(shared: &Shared, out: &mut dyn Write, chunk: &[u8]) -> io::Result<()> {
    let mut rest = chunk;
    while !rest.is_empty() {
        match out.write(&rest[..rest.len().min(WRITE_UNIT)]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(n) => {
                rest = &rest[n..];
                shared.queued.fetch_sub(n, Ordering::Relaxed);
                let now = Instant::now();
                shared.lock().progress = now;
                shared.progress.notify_all();
                shared.room.notify_one();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A sink's end of the delivery: collects bytes and hands full chunks to the thread, none
/// larger than `CHUNK`. It never blocks. A write fails once the thread has hit an error, such
/// as a reader that closed the pipe, so the run sees it at once (exit 7).
pub struct ChannelWriter {
    shared: Arc<Shared>,
    dest: Dest,
    buf: Vec<u8>,
}

impl ChannelWriter {
    fn send(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        let mut state = self.shared.lock();
        if state.done {
            return;
        }
        self.shared.queued.fetch_add(chunk.len(), Ordering::Relaxed);
        state.queue.push_back((self.dest, chunk));
        self.shared.give_work(&mut state);
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(error) = self.shared.error() {
            return Err(error);
        }
        // Fill the buffer up to a chunk, send it, and go on with the rest: a caller's buffer
        // of any size never becomes one queued item larger than a chunk.
        let mut rest = buf;
        while !rest.is_empty() {
            let room = CHUNK.saturating_sub(self.buf.len());
            let (head, tail) = rest.split_at(rest.len().min(room));
            self.buf.extend_from_slice(head);
            rest = tail;
            if self.buf.len() >= CHUNK {
                self.send();
            }
        }
        Ok(buf.len())
    }

    /// Hands what's collected to the thread; doesn't wait for it to be written.
    fn flush(&mut self) -> io::Result<()> {
        if let Some(error) = self.shared.error() {
            return Err(error);
        }
        self.send();
        Ok(())
    }
}

impl Drop for ChannelWriter {
    fn drop(&mut self) {
        self.send();
    }
}

/// The batch loop's view of the queue: whether it's full, and a way to wait for room
/// without blocking the runtime.
#[derive(Clone)]
pub struct Gate {
    shared: Arc<Shared>,
    stall: Option<Duration>,
}

impl Gate {
    pub fn is_full(&self) -> bool {
        self.shared.queued.load(Ordering::Relaxed) > QUEUE_LIMIT
    }

    /// Waits until the queue has room. Fails with the thread's write error, or when the
    /// reader makes no progress for the stall limit.
    pub async fn room(&self) -> io::Result<()> {
        loop {
            // Registered before checking, so a wakeup in between isn't lost.
            let notified = self.shared.room.notified();
            let wait = {
                let state = self.shared.lock();
                if let Some((kind, text)) = &state.error {
                    return Err(io::Error::new(*kind, text.clone()));
                }
                if state.done || !self.is_full() {
                    return Ok(());
                }
                match self.stall {
                    Some(limit) => {
                        let waited = state.progress.elapsed();
                        if waited >= limit {
                            return Err(stalled(limit, " (-t)"));
                        }
                        Some(limit - waited)
                    }
                    None => None,
                }
            };
            match wait {
                Some(left) => {
                    let _ = tokio::time::timeout(left, notified).await;
                }
                None => notified.await,
            }
        }
    }
}

impl crate::db::Backpressure for Gate {
    fn is_full(&self) -> bool {
        Gate::is_full(self)
    }

    fn room(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + '_>> {
        Box::pin(Gate::room(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Collects what the thread writes, shared with the test.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A reader that never reads: every write blocks until the test lets it go.
    struct Stalled(Mutex<mpsc::Receiver<()>>);

    impl Write for Stalled {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let _ = self.0.lock().unwrap().recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "reader went away",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn delivers_both_streams_in_order() {
        let (main, meta) = (Sink::default(), Sink::default());
        let mut delivery =
            Delivery::start(Box::new(main.clone()), Some(Box::new(meta.clone())), None);
        let mut data = delivery.writer(Dest::Main);
        let mut info = delivery.writer(Dest::Meta);
        for i in 0..20_000 {
            writeln!(data, "row {i}").unwrap();
        }
        writeln!(info, "-- 20000 rows").unwrap();
        data.flush().unwrap();
        info.flush().unwrap();
        delivery.finish(None).unwrap();
        let main = String::from_utf8(main.0.lock().unwrap().clone()).unwrap();
        assert_eq!(main.lines().count(), 20_000);
        assert!(main.starts_with("row 0\nrow 1\n") && main.ends_with("row 19999\n"));
        assert_eq!(*meta.0.lock().unwrap(), b"-- 20000 rows\n");
    }

    // F07: with a reader that doesn't read, writing never blocks, the gate fills, waiting
    // for room gives up after the stall limit, and so does finishing.
    #[test]
    fn a_stalled_reader_never_blocks_the_writer_and_times_out() {
        let (release, blocked) = mpsc::channel();
        let stall = Duration::from_millis(300);
        let mut delivery =
            Delivery::start(Box::new(Stalled(Mutex::new(blocked))), None, Some(stall));
        let gate = delivery.gate();
        let mut out = delivery.writer(Dest::Main);
        let started = Instant::now();
        let line = [b'x'; 1000];
        while !gate.is_full() {
            out.write_all(&line).unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "writes didn't block"
        );
        let error = runtime().block_on(gate.room()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("stopped reading"), "{error}");
        out.flush().unwrap();
        let error = delivery.finish(None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().ends_with("for 1 s (-t)"), "{error}");
        drop(release);
    }

    // PLAN.md 2.4: after a cancel, finishing waits a second, not the stall limit, and once
    // it has given up, dropping the delivery doesn't wait again.
    #[test]
    fn a_shorter_wait_gives_up_for_good() {
        let (release, blocked) = mpsc::channel();
        let mut delivery = Delivery::start(
            Box::new(Stalled(Mutex::new(blocked))),
            None,
            Some(Duration::from_secs(60)),
        );
        let mut out = delivery.writer(Dest::Main);
        out.write_all(&[b'x'; CHUNK]).unwrap();
        drop(out);
        let started = Instant::now();
        let error = delivery
            .finish(Some(Duration::from_millis(200)))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "the reader stopped reading for 1 s");
        drop(delivery);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        drop(release);
    }

    // R10: the async wait ends when everything is written, like `finish`.
    #[test]
    fn finish_async_waits_for_delivery() {
        let sink = Sink::default();
        let mut delivery = Delivery::start(Box::new(sink.clone()), None, None);
        let mut out = delivery.writer(Dest::Main);
        out.write_all(b"one\n").unwrap();
        drop(out);
        runtime()
            .block_on(delivery.finish_async(None, None))
            .unwrap();
        assert_eq!(*sink.0.lock().unwrap(), b"one\n");
    }

    // R10: a write error comes out of the async wait too.
    #[test]
    fn finish_async_reports_a_write_error() {
        let mut delivery = Delivery::start(Box::new(Broken), None, None);
        let mut out = delivery.writer(Dest::Main);
        out.write_all(b"one\n").unwrap();
        drop(out);
        let error = runtime()
            .block_on(delivery.finish_async(None, None))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    // R10: the same stall rule as `finish`, and the same detaching: dropping the delivery
    // afterwards doesn't wait again, however long the stall limit is.
    #[test]
    fn finish_async_gives_up_on_a_stalled_reader_for_good() {
        let (release, blocked) = mpsc::channel();
        let mut delivery = Delivery::start(
            Box::new(Stalled(Mutex::new(blocked))),
            None,
            Some(Duration::from_secs(60)),
        );
        let mut out = delivery.writer(Dest::Main);
        out.write_all(&[b'x'; CHUNK]).unwrap();
        drop(out);
        let started = Instant::now();
        let error = runtime()
            .block_on(delivery.finish_async(Some(Duration::from_millis(200)), None))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "the reader stopped reading for 1 s");
        drop(delivery);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        drop(release);
    }

    // R10: with no stall limit (-t 0) and no cap the wait has no end of its own; the signal
    // that cuts it short is the caller's `select!`. A shorter wait asked for afterwards
    // still works, and the stalled thread is detached when it gives up.
    #[test]
    fn finish_async_without_limits_waits_until_it_is_dropped() {
        let (release, blocked) = mpsc::channel();
        let mut delivery = Delivery::start(Box::new(Stalled(Mutex::new(blocked))), None, None);
        let mut out = delivery.writer(Dest::Main);
        out.write_all(&[b'x'; CHUNK]).unwrap();
        drop(out);
        runtime().block_on(async {
            let waited = tokio::time::timeout(
                Duration::from_millis(300),
                delivery.finish_async(None, None),
            )
            .await;
            assert!(waited.is_err(), "no limit: still waiting");
            let started = Instant::now();
            let error = delivery
                .finish_async(
                    Some(Duration::from_millis(200)),
                    Some(Duration::from_secs(1)),
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(1));
        });
        drop(delivery);
        drop(release);
    }

    // R10: the cap ends the wait even for a reader that keeps making progress.
    #[test]
    fn finish_async_stops_at_its_cap() {
        /// Takes every chunk, but slowly: 100 ms each.
        struct Slow;

        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_millis(100));
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut delivery = Delivery::start(Box::new(Slow), None, Some(Duration::from_secs(1)));
        let mut out = delivery.writer(Dest::Main);
        for _ in 0..20 {
            out.write_all(&[b'x'; CHUNK]).unwrap();
        }
        drop(out);
        let started = Instant::now();
        let error = runtime()
            .block_on(delivery.finish_async(None, Some(Duration::from_millis(400))))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "the reader didn't take the rest of the output within 1 s"
        );
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
        // Detached: dropping the delivery doesn't wait for the other chunks.
        drop(delivery);
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
    }

    // R10: a wait whose future was dropped (a signal won the `select!`) can be resumed, and
    // delivers what is left once the reader reads again.
    #[test]
    fn a_dropped_async_wait_can_be_resumed() {
        let (release, blocked) = mpsc::channel();
        let mut delivery = Delivery::start(Box::new(Stalled(Mutex::new(blocked))), None, None);
        let mut out = delivery.writer(Dest::Main);
        out.write_all(b"late\n").unwrap();
        drop(out);
        runtime().block_on(async {
            let cut = tokio::time::timeout(
                Duration::from_millis(100),
                delivery.finish_async(None, None),
            )
            .await;
            assert!(cut.is_err(), "the reader is stalled, so the wait is open");
            release.send(()).unwrap();
            delivery.finish_async(None, None).await.unwrap();
        });
    }

    // PLAN.md 2.4: time with nothing to write isn't a stall. A run that was quiet for longer
    // than the limit, then wrote its last lines to a reader that keeps up, is delivered.
    #[test]
    fn quiet_time_is_not_a_stall() {
        let sink = Sink::default();
        let limit = Duration::from_millis(100);
        let mut delivery = Delivery::start(Box::new(sink.clone()), None, Some(limit));
        let mut out = delivery.writer(Dest::Main);
        out.write_all(b"first\n").unwrap();
        out.flush().unwrap();
        std::thread::sleep(limit * 3);
        out.write_all(b"last\n").unwrap();
        drop(out);
        delivery.finish(Some(limit)).unwrap();
        assert_eq!(*sink.0.lock().unwrap(), b"first\nlast\n");
    }

    // F07: once the reader catches up, waiting for room ends without an error.
    #[test]
    fn room_comes_back_when_the_reader_catches_up() {
        let (release, blocked) = mpsc::channel();
        let mut delivery = Delivery::start(
            Box::new(Stalled(Mutex::new(blocked))),
            None,
            Some(Duration::from_secs(10)),
        );
        let gate = delivery.gate();
        let mut out = delivery.writer(Dest::Main);
        while !gate.is_full() {
            out.write_all(&[b'x'; 1000]).unwrap();
        }
        let releaser = std::thread::spawn(move || {
            for _ in 0..1000 {
                if release.send(()).is_err() {
                    break;
                }
            }
        });
        runtime().block_on(gate.room()).unwrap();
        assert!(!gate.is_full());
        out.flush().unwrap();
        delivery.finish(None).unwrap();
        releaser.join().unwrap();
    }

    /// A reader that is slow but never stops: it takes at most 100 bytes per call and
    /// dawdles over each one.
    struct Trickle {
        got: Arc<Mutex<Vec<u8>>>,
        delay: Duration,
    }

    impl Write for Trickle {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(self.delay);
            let n = buf.len().min(100);
            self.got.lock().unwrap().extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // R11: the stall clock measures what the reader takes, not finished chunks. Delivering
    // this takes the reader longer than the stall limit, but it takes some of the output
    // every few milliseconds, so it never stalls, and everything arrives in order.
    #[test]
    fn a_slow_reader_that_keeps_taking_output_is_not_a_stall() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let reader = Trickle {
            got: Arc::clone(&got),
            delay: Duration::from_millis(3),
        };
        let limit = Duration::from_millis(200);
        let data: Vec<u8> = (0..2000)
            .flat_map(|i| format!("row {i}\n").into_bytes())
            .collect();
        let started = Instant::now();
        let mut delivery = Delivery::start(Box::new(reader), None, Some(limit));
        let mut out = delivery.writer(Dest::Main);
        out.write_all(&data).unwrap();
        drop(out);
        delivery.finish(None).unwrap();
        assert!(
            started.elapsed() > limit * 2,
            "the delivery was meant to outlast the limit: {:?}",
            started.elapsed()
        );
        assert!(*got.lock().unwrap() == data, "bytes lost or out of order");
    }

    /// Waits for something the thread does in its own time.
    fn eventually(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // R11: a write bigger than a chunk is queued in chunks, whatever the caller's buffer.
    #[test]
    fn a_big_write_is_queued_in_chunks() {
        let (release, blocked) = mpsc::channel();
        let mut delivery = Delivery::start(Box::new(Stalled(Mutex::new(blocked))), None, None);
        let mut out = delivery.writer(Dest::Main);
        // Park the thread inside a write, so that nothing else leaves the queue.
        out.write_all(b"first\n").unwrap();
        out.flush().unwrap();
        eventually("the thread to take the first chunk", || {
            delivery.shared.lock().queue.is_empty()
        });
        let big = vec![b'x'; 3 * CHUNK + 5];
        assert_eq!(out.write(&big).unwrap(), big.len());
        let sizes: Vec<usize> = delivery
            .shared
            .lock()
            .queue
            .iter()
            .map(|(_, chunk)| chunk.len())
            .collect();
        let queued = delivery.shared.queued.load(Ordering::Relaxed);
        // Let the thread go before asserting, so a failure can't leave it parked.
        drop(release);
        assert_eq!(sizes, [CHUNK; 3]);
        // The 5 bytes left over are still with the writer; the rest is queued or in flight.
        assert_eq!(queued, 6 + 3 * CHUNK);
        out.flush().unwrap();
        delivery.finish(None).unwrap();
        assert_eq!(delivery.shared.queued.load(Ordering::Relaxed), 0);
    }

    // R11: bytes stop counting as queued as the reader takes them, not when a whole chunk is
    // through, so the batch loop's wait for room ends as soon as the queue is back under its
    // limit. Here the queue is 10,000 bytes over, and the reader takes one 4 KiB write at a
    // time, on cue: room opens at the third, with the 64 KiB chunk in flight far from done.
    #[test]
    fn room_opens_as_bytes_drain() {
        let (release, blocked) = mpsc::channel();
        let mut delivery = Delivery::start(Box::new(Stalled(Mutex::new(blocked))), None, None);
        let gate = delivery.gate();
        let mut out = delivery.writer(Dest::Main);
        out.write_all(&vec![b'x'; QUEUE_LIMIT + 10_000]).unwrap();
        out.flush().unwrap();
        let shared = Arc::clone(&delivery.shared);
        let queued = move || shared.queued.load(Ordering::Relaxed);
        let full = queued();
        assert_eq!(full, QUEUE_LIMIT + 10_000);
        assert!(gate.is_full());
        for _ in 0..2 {
            release.send(()).unwrap();
        }
        eventually("two writes to leave the queue", || {
            queued() == full - 2 * WRITE_UNIT
        });
        assert!(gate.is_full());
        release.send(()).unwrap();
        runtime().block_on(gate.room()).unwrap();
        assert_eq!(queued(), full - 3 * WRITE_UNIT);
        assert!(!gate.is_full());
        drop(release);
        delivery.finish(None).unwrap();
        assert_eq!(queued(), 0);
    }

    /// Takes every write at once, but its flush never returns.
    struct FlushStalls(Mutex<mpsc::Receiver<()>>);

    impl Write for FlushStalls {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            let _ = self.0.lock().unwrap().recv();
            Ok(())
        }
    }

    // R11: the flush when the delivery closes is part of delivering, and it doesn't restart
    // the stall clock, so a reader that blocks it is a stall like any other.
    #[test]
    fn a_flush_that_never_returns_is_a_stall() {
        let (release, blocked) = mpsc::channel();
        let limit = Duration::from_millis(300);
        let mut delivery = Delivery::start(
            Box::new(FlushStalls(Mutex::new(blocked))),
            None,
            Some(limit),
        );
        let mut out = delivery.writer(Dest::Main);
        out.write_all(b"all written\n").unwrap();
        drop(out);
        let started = Instant::now();
        let error = delivery.finish(None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(release);
    }

    // R11: one thread writes both streams in order, so a stalled stream holds up the other
    // one and delivery fails, however well the other reader keeps up.
    #[test]
    fn a_stalled_stream_is_caught_although_the_other_keeps_up() {
        let (release, blocked) = mpsc::channel();
        let data = Sink::default();
        let mut delivery = Delivery::start(
            Box::new(data.clone()),
            Some(Box::new(Stalled(Mutex::new(blocked)))),
            Some(Duration::from_millis(300)),
        );
        let mut info = delivery.writer(Dest::Meta);
        let mut rows = delivery.writer(Dest::Main);
        info.write_all(b"-- metadata\n").unwrap();
        info.flush().unwrap();
        rows.write_all(b"row\n").unwrap();
        rows.flush().unwrap();
        let error = delivery.finish(None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(data.0.lock().unwrap().is_empty(), "written out of order");
        drop(release);
    }

    /// Takes a few bytes per call, and every third call is interrupted.
    struct Interrupted {
        got: Arc<Mutex<Vec<u8>>>,
        calls: usize,
    }

    impl Write for Interrupted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls.is_multiple_of(3) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(7);
            self.got.lock().unwrap().extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Never takes anything, and says so with `Ok(0)`.
    struct Takes0;

    impl Write for Takes0 {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // R11: the thread's own write loop does what `write_all` does: it continues after a
    // partial write, retries an interrupted one, and fails on a write that takes nothing.
    #[test]
    fn interrupted_writes_are_retried_and_a_write_of_zero_is_an_error() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let reader = Interrupted {
            got: Arc::clone(&got),
            calls: 0,
        };
        let mut delivery = Delivery::start(Box::new(reader), None, None);
        let mut out = delivery.writer(Dest::Main);
        let data: Vec<u8> = (0..3 * WRITE_UNIT).map(|i| (i % 251) as u8).collect();
        out.write_all(&data).unwrap();
        drop(out);
        delivery.finish(None).unwrap();
        assert!(*got.lock().unwrap() == data, "bytes lost or out of order");

        let mut delivery = Delivery::start(Box::new(Takes0), None, None);
        let mut out = delivery.writer(Dest::Main);
        out.write_all(b"x").unwrap();
        out.flush().unwrap();
        let error = delivery.finish(None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    }

    // F02 still holds: a reader that closed the pipe fails the next write and `finish`.
    #[test]
    fn a_write_error_reaches_the_writer_and_finish() {
        let mut delivery = Delivery::start(Box::new(Broken), None, None);
        let mut out = delivery.writer(Dest::Main);
        out.write_all(b"one\n").unwrap();
        out.flush().unwrap();
        assert_eq!(
            delivery.finish(None).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            out.write_all(b"two\n").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
