//! Output formats (DESIGN.md §5): text (default), json and csv. Each is an `Output`. The
//! shared `Tally` applies the limits and keeps the counts, so every format agrees on what
//! was shown, cut or hidden.
//!
//! `--max-output` caps every byte a run writes, on both streams (DESIGN.md §4.4). The body
//! (rows, messages, errors, ...) is admitted piece by piece up to `OUTPUT_RESERVE` short of
//! the cap and is withheld whole past it. Everything else is text of unbounded length that
//! must still be written: notes, a guard refusal, the closing error, `--verbose` lines.
//! `Layout` shortens such text to what is left, ending it with `…[+N]`, and keeps the
//! status line (JSON: the outcome fields) out of reach of all of it. In text and csv the
//! status line ends with a tail that says where the run went; its length depends on the names
//! in it, so the budget holds exactly that much back on top of the two reserves
//! (`Tally::reserve_ending`).

pub mod csv;
pub mod delivery;
pub mod json;
pub mod text;

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::time::Duration;

use crate::db::{BatchSink, Connection, ServerMessage, Stop};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    Csv,
}

impl Format {
    pub fn parse(text: &str) -> Option<Format> {
        match text {
            "text" => Some(Format::Text),
            "json" => Some(Format::Json),
            "csv" => Some(Format::Csv),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Rows shown per result set; 0 = all.
    pub max_rows: u64,
    /// Characters shown per value; 0 = all.
    pub max_cell: usize,
    /// Result sets shown per run; 0 = all.
    pub max_results: usize,
    /// Server messages shown per run; 0 = all.
    pub max_messages: usize,
    /// Bytes of output per run, across all streams; 0 = no limit (DESIGN.md §4.4).
    pub max_output: u64,
    /// Seconds the reader may take no output before delivery fails (exit 7); 0 = wait as
    /// long as it takes. It follows the query timeout, `-t` (DESIGN.md §6).
    pub stall_secs: u32,
    /// Whose flags the output's advice names.
    pub advice: Advice,
    /// In `sqlh mcp`: the call's timeout is the server's limit, so the advice offers no
    /// larger one.
    pub timeout_at_limit: bool,
}

/// Whose flags a run's advice names: the command line's (`--exact-count`, `-t`), or, in
/// `sqlh mcp`, a call's arguments and the profile's settings (docs/mcp-mode.md §4), so a
/// result never offers a flag its reader can't pass (as 2.16 found for `deny` profiles).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Advice {
    #[default]
    Cli,
    Mcp,
}

impl Advice {
    /// The row limit as a result names it: the flag, or in `sqlh mcp` the call's argument.
    pub fn max_rows(self) -> &'static str {
        match self {
            Advice::Cli => "--max-rows",
            Advice::Mcp => "max_rows",
        }
    }

    /// The value limit, as `max_rows` names the row limit.
    pub fn max_cell(self) -> &'static str {
        match self {
            Advice::Cli => "--max-cell",
            Advice::Mcp => "max_cell",
        }
    }

    /// The output limit: in `sqlh mcp` a launch flag, which a call can't change.
    pub fn max_output(self) -> &'static str {
        match self {
            Advice::Cli => "--max-output",
            Advice::Mcp => "the server's --max-output",
        }
    }

    /// The result-set limit, `n`: in `sqlh mcp` it is fixed.
    pub fn max_results(self, n: usize) -> String {
        match self {
            Advice::Cli => format!("--max-results {n}"),
            Advice::Mcp => format!("at most {n} per call"),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_rows: 100,
            max_cell: 500,
            max_results: 20,
            max_messages: 100,
            max_output: 1_000_000,
            stall_secs: 60,
            advice: Advice::Cli,
            timeout_at_limit: false,
        }
    }
}

/// The part of `max_output` kept for the end of the run: the body (result sets, rows,
/// messages, errors, row counts, batch markers) stops this many bytes short of the cap, so
/// the footer of the result being shown, the closing error and notes, and the status line
/// always fit.
pub const OUTPUT_RESERVE: u64 = 1024;

/// The last part of `OUTPUT_RESERVE`: notes, a guard refusal and `--verbose` lines are
/// shortened to stop this many bytes short of the cap, so that the status line (JSON: the
/// outcome fields) and the closers of the result or batch still open always fit. The worst
/// case for those, with every number at its widest, is well under it (tested per format).
///
/// In text and csv the status line also ends with where the run went (`Tally::reserve_ending`).
/// That tail is as long as the names in it, so it isn't part of this constant: both reserves
/// grow by exactly its length once the sink has it.
pub const STATUS_RESERVE: u64 = 640;

/// The most characters of a name from the server (the server, the database, the version) that
/// the output carries: the `connection` object of JSON and the tail of the status line in
/// text and csv (DESIGN.md §5). SQL Server names stop at 128, so a longer one isn't from a SQL
/// Server; it is cut like a long value. That bounds both: at six bytes for a character that has
/// to be escaped, the status line's tail takes at most 1,609 bytes (`text::WIDEST_ENDING` in
/// the tests) and JSON's object under 2,000, which fit the smallest `--max-output` with room
/// to spare.
pub const NAME_CHARS: usize = 128;

/// The smallest `--max-output` other than 0: room for some output plus the reserve.
pub const MIN_MAX_OUTPUT: u64 = 4096;

/// The error a sink returns for an event that came out of order (`Tally::outside_rows`, and
/// `Output::connected` of the JSON sink). It is an `io::Error` of kind `InvalidInput` that
/// carries this, so the caller can tell it from a failure to write: the order is a promise of
/// sqlh and its driver, a broken one is a bug, and the run ends as an internal error (exit 10)
/// rather than as undeliverable output (exit 7; DESIGN.md §5.1 and §6).
#[derive(Debug)]
pub struct OutOfOrder(String);

impl OutOfOrder {
    pub fn error(message: String) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, OutOfOrder(message))
    }

    /// Whether `error` is a sink's refusal of an event out of order.
    pub fn is(error: &io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(|inner| inner.is::<OutOfOrder>())
    }
}

impl std::fmt::Display for OutOfOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OutOfOrder {}

/// Where a batch comes from, for markers and error line mapping.
pub struct BatchInfo<'a> {
    /// 1-based position in the run, counting `GO n` repeats.
    pub number: usize,
    /// Batches in the run, counting repeats.
    pub total: usize,
    pub source: &'a str,
    pub multi_source: bool,
    /// Source line where the batch text starts.
    pub line: u32,
    /// The batch's line map (`script::Batch::line_map`); empty when lines map one to one.
    pub line_map: &'a [u32],
    pub run: u32,
    pub runs: u32,
    /// The run clock when the batch started, in milliseconds: the clock that
    /// `Summary::elapsed_ms` and `Output::end_batch` read, so a batch's duration is a
    /// difference of two readings and no sink has to look at the time itself.
    pub started_ms: u128,
}

impl BatchInfo<'_> {
    /// `line 14` or `file.sql:14`, plus `, run 2 of 3` for a repeated batch.
    pub fn location(&self) -> String {
        let mut location = if self.multi_source {
            format!("{}:{}", self.source, self.line)
        } else {
            format!("line {}", self.line)
        };
        if self.runs > 1 {
            let _ = write!(location, ", run {} of {}", self.run, self.runs);
        }
        location
    }
}

/// How the run ended, for the last lines of output.
pub struct Summary<'a> {
    pub exit: u8,
    /// The status without the `-- ` prefix: `ok: 2 batches, 41 ms` or `failed (exit 1): ...`.
    pub status: &'a str,
    /// Why the run stopped early, if it did.
    pub error: Option<&'a str>,
    pub elapsed_ms: u128,
}

pub trait Output: BatchSink {
    /// The connection is up, after `connect_ms` milliseconds. The facts come from the login
    /// itself, so there is no extra round trip. Called once, before the first batch, and not
    /// at all when nothing connected (a usage error, a guard refusal, a failed connect). A
    /// sink refuses a call that comes too late, or a second one (`OutOfOrder`).
    ///
    /// JSON carries the server, the database the run started in and the connect time as
    /// `connection` and `connect_ms` (DESIGN.md §5.2). Text and csv carry the server, the
    /// database and the connect time as the tail of the status line, `; db01/master, connect
    /// 195 ms` (§5.1; owner decision 10, PLAN.md §6). They do not carry the auth method or
    /// whether the traffic is encrypted. The default does nothing, for a sink that carries
    /// none of it.
    fn connected(&mut self, connection: &Connection, connect_ms: u128) -> io::Result<()> {
        let _ = (connection, connect_ms);
        Ok(())
    }
    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()>;
    /// The batch is over. `ended_ms` is the run clock (see `BatchInfo::started_ms`).
    fn end_batch(&mut self, ended_ms: u128) -> io::Result<()>;
    /// A note about the run. Shortened (`…[+N]`) to fit what is left of `--max-output`, or
    /// counted as withheld when not even that fits.
    fn note(&mut self, text: &str) -> io::Result<()>;
    /// The read-only guard refused the input; nothing ran. `advice` says how to get past it.
    /// Shortened to fit like a note, in its variable parts first.
    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()>;
    /// A line about sqlh itself for whoever runs it (`--verbose`): `sqlh: <text>` on stderr
    /// in every format, flushed at once. It counts against `--max-output` and is shortened
    /// like a note. In text and json, stderr carries nothing else and a stderr that can't
    /// take the line just drops it; in csv, stderr is the metadata stream (DESIGN.md §5.3).
    fn diagnostic(&mut self, text: &str) -> io::Result<()>;
    /// Writes the end of the output: limit notes, any error, the status. Always called last,
    /// also after a failure mid-result, and must leave well-formed output. A batch still
    /// open ended when the run did: at `summary.elapsed_ms` on the run clock. The status is
    /// never shortened; the error and notes are, to fit the budget.
    fn finish(&mut self, summary: &Summary) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    /// The sections the next result sets fill, in order (schema commands, schema-commands.md
    /// §4): each takes the next label. Called before the batch that returns them begins.
    fn set_sections(&mut self, sections: &[Section]);
}

/// A labeled result set of a schema command (schema-commands.md §4). In text and csv its
/// header is the column names only and comes with its first row, and one with no rows is
/// listed on a `-- no rows: ...` line instead; JSON gives its result event a `label`. A
/// `definition` section is one row (name, type, definition) whose definition text prints
/// as a block framed by its line count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub label: &'static str,
    pub definition: bool,
}

impl Section {
    pub const fn rows(label: &'static str) -> Section {
        Section {
            label,
            definition: false,
        }
    }

    pub const fn definition(label: &'static str) -> Section {
        Section {
            label,
            definition: true,
        }
    }
}

/// The output for `format`. CSV keeps stdout (or `-o`) for data only, so its metadata goes
/// to stderr. The other formats use stderr for `Output::diagnostic` lines only.
pub fn create(format: Format, out: Box<dyn Write + Send>, limits: Limits) -> Stream {
    create_with(format, out, Box::new(io::stderr()), limits)
}

/// `create` with a stderr of the caller's choosing, so that tests can break it.
fn create_with(
    format: Format,
    out: Box<dyn Write + Send>,
    stderr: Box<dyn Write + Send>,
    limits: Limits,
) -> Stream {
    let stall = (limits.stall_secs > 0).then(|| Duration::from_secs(limits.stall_secs.into()));
    match format {
        // csv's stderr carries the metadata and the status, so it is delivered with the data,
        // by the same thread and in order, and a failure there is a delivery failure (exit 7).
        Format::Csv => {
            let delivery = delivery::Delivery::start(out, Some(stderr), stall);
            let main = delivery.writer(delivery::Dest::Main);
            let meta = Box::new(delivery.writer(delivery::Dest::Meta));
            Stream {
                output: Box::new(csv::CsvSink::new(main, meta, limits)),
                delivery,
                side: None,
            }
        }
        // In the other formats stderr carries diagnostics only, which are best effort
        // (DESIGN.md §6). They get a thread of their own, so an unread stderr can't hold up
        // the response on stdout (review R12). They still count against `--max-output`: the
        // sink counts what it hands over.
        Format::Text | Format::Json => {
            let delivery = delivery::Delivery::start(out, None, stall);
            let side = delivery::Side::start(stderr);
            let main = delivery.writer(delivery::Dest::Main);
            let meta = Box::new(side.writer());
            let output: Box<dyn Output> = match format {
                Format::Text => Box::new(text::TextSink::with_meta(main, meta, limits)),
                _ => Box::new(json::JsonSink::with_meta(main, meta, limits)),
            };
            Stream {
                output,
                delivery,
                side: Some(side),
            }
        }
    }
}

/// An output held in memory, with no delivery thread: an MCP call's result, which is sent as
/// one message once the call is over (docs/mcp-mode.md §4). Its size is bounded by the
/// limits' `max_output`, which `sqlh mcp` never lets be 0. Diagnostics go nowhere: MCP calls
/// run without `--verbose` lines (`Report::Off`).
pub enum Collected {
    Text(text::TextSink<Vec<u8>>),
    Json(json::JsonSink<Vec<u8>>),
}

impl Collected {
    /// An empty output in `format` (text or json; csv has no place in a tool result).
    pub fn new(format: Format, limits: Limits) -> Collected {
        match format {
            Format::Json => Collected::Json(json::JsonSink::with_meta(
                Vec::new(),
                Box::new(io::sink()),
                limits,
            )),
            Format::Text | Format::Csv => Collected::Text(text::TextSink::with_meta(
                Vec::new(),
                Box::new(io::sink()),
                limits,
            )),
        }
    }

    pub fn output(&mut self) -> &mut dyn Output {
        match self {
            Collected::Text(sink) => sink,
            Collected::Json(sink) => sink,
        }
    }

    /// What was written, once the output is finished. The sinks write UTF-8, so a lossy
    /// conversion changes nothing.
    pub fn into_text(self) -> String {
        let bytes = match self {
            Collected::Text(sink) => sink.into_inner(),
            Collected::Json(sink) => sink.into_inner(),
        };
        String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into())
    }
}

/// A run's output: the sink for its format, the thread that delivers what it writes
/// (`delivery`), and in text and json the diagnostics' own thread (`side`). Derefs to the
/// sink.
pub struct Stream {
    // Dropped in this order: the sink first, which hands the side writer's last bytes over.
    output: Box<dyn Output>,
    delivery: delivery::Delivery,
    side: Option<delivery::Side>,
}

impl Stream {
    /// For the batch loop, to wait while the reader is behind.
    pub fn gate(&self) -> delivery::Gate {
        self.delivery.gate()
    }

    /// Waits until everything the sink has flushed reached its destination; see
    /// `Delivery::finish`. Then gives the diagnostics on stderr, in text and json, at most
    /// `delivery::SIDE_WAIT`; whether they arrive never changes the result.
    pub fn wait_delivered(&mut self, stall: Option<Duration>) -> io::Result<()> {
        let delivered = self.delivery.finish(stall);
        self.finish_side();
        delivered
    }

    /// `wait_delivered` for the async runtime: it doesn't block the thread, so a signal can
    /// cut the wait short, and `within` caps the whole wait (R10); see
    /// `Delivery::finish_async`. Dropping the future early is safe. The wait for the
    /// diagnostics that follows does block the thread, for at most `delivery::SIDE_WAIT`.
    pub async fn wait_delivered_async(
        &mut self,
        stall: Option<Duration>,
        within: Option<Duration>,
    ) -> io::Result<()> {
        let delivered = self.delivery.finish_async(stall, within).await;
        self.finish_side();
        delivered
    }

    fn finish_side(&mut self) {
        if let Some(side) = &mut self.side {
            side.finish();
        }
    }
}

impl Deref for Stream {
    type Target = dyn Output;

    fn deref(&self) -> &Self::Target {
        &*self.output
    }
}

impl DerefMut for Stream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.output
    }
}

/// Adds a guard refusal to `layout` as one message, the same in every format:
/// `line 3: UPDATE dbo.T would write; nothing was run. <advice>`. The location, the reason
/// (which can quote a huge identifier) and the advice are the parts that get shortened when
/// the budget is short; the words between them never are.
pub fn push_blocked<'a>(
    layout: &mut Layout<'a>,
    location: &'a str,
    reason: &'a str,
    advice: &'a str,
) {
    layout
        .part(location)
        .text(": ")
        .part(reason)
        .text("; nothing was run. ")
        .part(advice);
}

/// How one character is written in an output format: appends it to `out`. Text and csv
/// metadata use `push_char`, json strings their own.
pub type PushChar = fn(&mut String, char);

/// The bytes `s` takes when written with `push`.
pub fn encoded_len(s: &str, push: PushChar) -> usize {
    let mut scratch = String::new();
    s.chars()
        .map(|c| {
            scratch.clear();
            push(&mut scratch, c);
            scratch.len()
        })
        .sum()
}

/// Appends `s` written with `push` in at most `room` bytes. If it fits, all of it; if not,
/// a prefix and the marker `…[+N]`, N being the characters cut (the style of a cut value,
/// DESIGN.md §4.4). Returns N, or `None`, appending nothing, when not even the marker fits.
/// `room == usize::MAX` means no limit.
pub fn push_within(out: &mut String, s: &str, room: usize, push: PushChar) -> Option<usize> {
    let whole = room == usize::MAX || encoded_len(s, push) <= room;
    if whole {
        s.chars().for_each(|c| push(out, c));
        return Some(0);
    }
    // The marker is sized for the most it can say (every character cut), so the real one,
    // which says fewer, always fits.
    let total = s.chars().count();
    let keep = room.checked_sub(cut_marker(total, false).len())?;
    let (mut size, mut kept) = (0, 0);
    let mut scratch = String::new();
    for c in s.chars() {
        scratch.clear();
        push(&mut scratch, c);
        if size + scratch.len() > keep {
            break;
        }
        out.push_str(&scratch);
        size += scratch.len();
        kept += 1;
    }
    out.push_str(&cut_marker(total - kept, false));
    Some(total - kept)
}

/// Splits `room` bytes among texts that want `needs[i]` each, as evenly as it can: a text
/// that wants no more than an equal share gets all it wants, and what that leaves is split
/// among the rest. The shares add up to at most `room`.
fn shares(needs: &[usize], room: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..needs.len()).collect();
    order.sort_by_key(|&i| needs[i]);
    let mut left = room;
    let mut shares = vec![0; needs.len()];
    for (done, &i) in order.iter().enumerate() {
        let share = needs[i].min(left / (needs.len() - done));
        shares[i] = share;
        left -= share;
    }
    shares
}

/// Text to write within a byte budget: fixed pieces, already in their output encoding,
/// around parts of variable text (raw, written with the format's `PushChar` when laid out).
/// What `fit` shortens are the parts, never the pieces.
pub struct Layout<'a> {
    fixed: Vec<String>,
    parts: Vec<&'a str>,
}

impl Default for Layout<'_> {
    fn default() -> Self {
        Layout::new()
    }
}

impl<'a> Layout<'a> {
    pub fn new() -> Self {
        Layout {
            fixed: vec![String::new()],
            parts: Vec::new(),
        }
    }

    /// Fixed text, written as it is.
    pub fn text(&mut self, s: &str) -> &mut Self {
        if let Some(last) = self.fixed.last_mut() {
            last.push_str(s);
        }
        self
    }

    /// Variable text, written with the format's escaping and shortened if it must be.
    pub fn part(&mut self, s: &'a str) -> &mut Self {
        self.parts.push(s);
        self.fixed.push(String::new());
        self
    }

    /// The layout in at most `room` bytes (`usize::MAX`: no limit). The parts get what the
    /// fixed text leaves, shared as evenly as `shares` can, so a short part stays whole and
    /// the long ones are cut. `None` when the fixed text alone doesn't fit, or a part would
    /// have to be cut to less than its marker; nothing is then half written.
    pub fn fit(&self, room: usize, push: PushChar) -> Option<String> {
        let fixed: usize = self.fixed.iter().map(String::len).sum();
        let needs: Vec<usize> = self.parts.iter().map(|p| encoded_len(p, push)).collect();
        let allowed = shares(&needs, room.checked_sub(fixed)?);
        let mut out = String::with_capacity(fixed + allowed.iter().sum::<usize>());
        for (i, part) in self.parts.iter().enumerate() {
            out.push_str(&self.fixed[i]);
            push_within(&mut out, part, allowed[i], push)?;
        }
        out.push_str(&self.fixed[self.parts.len()]);
        Some(out)
    }
}

/// What the output budget kept back after it ran out, for the closing note. The result set
/// that was being shown reports its own rows in its footer.
#[derive(Default)]
struct Withheld {
    results: u64,
    messages: u64,
    errors: u64,
    /// Statements whose row counts weren't shown.
    row_counts: u64,
    batches: u64,
    /// Notes and diagnostics that didn't fit, not even shortened.
    notes: u64,
    /// The description of the connection, when the budget was already used up: JSON's head, or
    /// the tail of the status line in text and csv.
    connection: u64,
    /// Lines of a definition block (`def`) that didn't fit.
    definition_lines: u64,
    /// The read of a definition was cut too, so more lines than `definition_lines` weren't
    /// shown: the count is a lower bound.
    definition_lines_more: bool,
}

/// Limits and counts shared by all formats.
pub struct Tally {
    limits: Limits,
    source: String,
    multi_source: bool,
    batch_line: u32,
    line_map: Vec<u32>,
    results_seen: usize,
    hidden: bool,
    /// A result set's rows are streaming: `columns` has been called and `end_rows` has not.
    /// True for a hidden result set too, because the order is the caller's, not the display's.
    rows_open: bool,
    /// The output budget ran out: nothing more of the body is written.
    exhausted: bool,
    /// The budget ran out while the current result set was being shown.
    cut: bool,
    /// The count of the current result set's rows was stopped early (`stop_counting`): its
    /// total is a lower bound.
    stopped: bool,
    /// The current batch's number in the run, for the note and the status line.
    batch_number: usize,
    /// The current batch had a count stopped (JSON's `counting_stopped`).
    batch_stopped: bool,
    /// The batches (by number, in order) in which a count was stopped.
    stopped_batches: Vec<usize>,
    /// What stopped them, for the note's advice: the budget (`--exact-count` lifts it), `-t`
    /// (raising it does).
    stopped_by_budget: bool,
    stopped_by_timeout: bool,
    withheld: Withheld,
    /// Consecutive row counts not written yet, as (statements, rows): they become one line.
    affected: Option<(u64, u64)>,
    rows_total: u64,
    rows_shown: u64,
    cells_cut: u64,
    messages_seen: usize,
    /// Bytes the end of the run needs on top of the two reserves: the tail of the status line
    /// that names the connection (`reserve_ending`). Zero in JSON, which has no such tail.
    ending: u64,
    /// `reserve_ending` has been called: the connection is described once.
    connection_told: bool,
    /// A batch has begun, so the connection can no longer be described.
    batch_begun: bool,
    /// The sections the next result sets fill (`Output::set_sections`).
    sections: VecDeque<Section>,
    /// The section the current result set fills, if any.
    section: Option<Section>,
    /// Sections of the current batch that ended with no rows, for its `-- no rows:` line.
    empty_sections: Vec<&'static str>,
}

impl Tally {
    pub fn new(limits: Limits) -> Self {
        Tally {
            limits,
            source: String::new(),
            multi_source: false,
            batch_line: 1,
            line_map: Vec::new(),
            results_seen: 0,
            hidden: false,
            rows_open: false,
            exhausted: false,
            cut: false,
            stopped: false,
            batch_number: 0,
            batch_stopped: false,
            stopped_batches: Vec::new(),
            stopped_by_budget: false,
            stopped_by_timeout: false,
            withheld: Withheld::default(),
            affected: None,
            rows_total: 0,
            rows_shown: 0,
            cells_cut: 0,
            messages_seen: 0,
            ending: 0,
            connection_told: false,
            batch_begun: false,
            sections: VecDeque::new(),
            section: None,
            empty_sections: Vec::new(),
        }
    }

    /// The sections the next result sets fill, in order (`Output::set_sections`).
    pub fn set_sections(&mut self, sections: &[Section]) {
        self.sections = sections.iter().copied().collect();
    }

    /// The section the current result set fills.
    pub fn section(&self) -> Option<Section> {
        self.section
    }

    /// The current section ended with no rows.
    pub fn empty_section(&mut self, label: &'static str) {
        self.empty_sections.push(label);
    }

    /// The batch's sections that had no rows, for its `-- no rows:` line; clears them.
    pub fn take_empty_sections(&mut self) -> Vec<&'static str> {
        std::mem::take(&mut self.empty_sections)
    }

    /// `lines` lines of a definition block didn't fit the budget, which is then used up;
    /// `more`: the read was cut, so more followed them, how many unknown.
    pub fn withhold_definition_lines(&mut self, lines: u64, more: bool) {
        self.exhausted = true;
        self.withheld.definition_lines += lines;
        self.withheld.definition_lines_more |= more;
    }

    /// Whether `bytes` more of the body fit in the output budget, given the `used` bytes
    /// written so far (both streams, and anything promised to the end). The budget keeps
    /// `OUTPUT_RESERVE` for the end of the run, and the status line's tail on top of it.
    /// Once something doesn't fit, nothing more does, so output never resumes after a gap.
    pub fn admit(&mut self, used: u64, bytes: usize) -> bool {
        if self.exhausted {
            return false;
        }
        let max = self.limits.max_output;
        if max == 0 || used + bytes as u64 <= max.saturating_sub(OUTPUT_RESERVE + self.ending) {
            return true;
        }
        self.exhausted = true;
        false
    }

    /// Bytes a note, a guard refusal or a diagnostic may take, given the `used` bytes
    /// written so far: what the budget has left, less `STATUS_RESERVE` and the status line's
    /// tail (`usize::MAX`: no limit).
    pub fn note_room(&self, used: u64) -> usize {
        self.room(used, STATUS_RESERVE + self.ending)
    }

    /// Text and csv, when the connection is described (`Output::connected`): the status line
    /// will end with `bytes` more than the reserves were sized for, the tail that says where
    /// the run went. Its length depends on the names in it, so it is held back from the
    /// budget exactly, on top of both reserves, instead of being added to a constant that
    /// every run would pay for at its widest. The status line is never shortened (DESIGN.md
    /// §4.4), so the room is promised here and `admit` and `note_room` keep it.
    ///
    /// Returns whether the tail has room. When notes written before the connection (the one
    /// about ignored sqlcmd flags) have left less than `bytes` before the status reserve, the
    /// tail is left out and counted among what wasn't shown, the way JSON's head is, and the
    /// body stops: the cap is the thing that gives way, and the status line stays whole.
    ///
    /// A second call, or one after a batch has begun, is the caller's bug: `OutOfOrder`.
    pub fn reserve_ending(&mut self, used: u64, bytes: usize) -> io::Result<bool> {
        if self.connection_told || self.batch_begun {
            return Err(OutOfOrder::error(
                "the connection was described twice, or after the first batch had begun; it \
                 is described once, before the first batch (Output order, DESIGN.md §5.1)"
                    .into(),
            ));
        }
        self.connection_told = true;
        if self.note_room(used) >= bytes {
            self.ending = bytes as u64;
            return Ok(true);
        }
        self.exhausted = true;
        self.withhold_connection();
        Ok(false)
    }

    /// Bytes more of the body may take, by `admit`'s rule: what the budget has left before the
    /// end-of-run reserve and the status line's tail; 0 once the budget is used up
    /// (`usize::MAX`: no limit).
    pub fn body_room(&self, used: u64) -> usize {
        if self.exhausted {
            return 0;
        }
        self.room(used, OUTPUT_RESERVE + self.ending)
    }

    /// Bytes the closing lines may take: what the budget has left (`usize::MAX`: no limit).
    /// The status line comes out of it first and is never shortened.
    pub fn closing_room(&self, used: u64) -> usize {
        self.room(used, 0)
    }

    fn room(&self, used: u64, keep: u64) -> usize {
        match self.limits.max_output {
            0 => usize::MAX,
            max => {
                usize::try_from(max.saturating_sub(used).saturating_sub(keep)).unwrap_or(usize::MAX)
            }
        }
    }

    /// A note or diagnostic didn't fit even shortened. The budget is then used up: nothing
    /// more of the body is written, and the closing note counts what was lost.
    pub fn withhold_note(&mut self) {
        self.exhausted = true;
        self.withheld.notes += 1;
    }

    /// A result set's header didn't fit the budget: the whole result set is withheld.
    pub fn withhold_result(&mut self) {
        self.hidden = true;
        self.withheld.results += 1;
    }

    /// A row didn't fit the budget; it still counts toward the result set's total.
    pub fn row_withheld(&mut self) {
        self.rows_total += 1;
        self.cut = true;
    }

    pub fn withhold_message(&mut self) {
        self.withheld.messages += 1;
    }

    pub fn withhold_error(&mut self) {
        self.withheld.errors += 1;
    }

    pub fn withhold_batch(&mut self) {
        self.withheld.batches += 1;
    }

    /// The description of the connection didn't fit the budget (JSON's head; text and csv go
    /// through `reserve_ending`).
    pub fn withhold_connection(&mut self) {
        self.withheld.connection += 1;
    }

    /// Row counts of `statements` statements didn't fit the budget.
    pub fn withhold_row_counts(&mut self, statements: u64) {
        self.withheld.row_counts += statements;
    }

    /// Adds one statement's row count to the run that becomes one line.
    pub fn add_affected(&mut self, rows: u64) {
        let (statements, total) = self.affected.unwrap_or((0, 0));
        self.affected = Some((statements + 1, total + rows));
    }

    /// The pending run of row counts as (statements, rows), to be written before any other
    /// event.
    pub fn take_affected(&mut self) -> Option<(u64, u64)> {
        self.affected.take()
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn begin_batch(&mut self, batch: &BatchInfo) {
        self.batch_begun = true;
        self.source.clear();
        self.source.push_str(batch.source);
        self.multi_source = batch.multi_source;
        self.batch_line = batch.line;
        self.line_map.clear();
        self.line_map.extend_from_slice(batch.line_map);
        self.batch_number = batch.number;
        self.batch_stopped = false;
        // A batch starts with no result open. One that a failure cut short is closed by the
        // sink (json marks it interrupted), and must not leave every event refused.
        self.rows_open = false;
    }

    /// Starts a result set. Returns its number, or `None` when `--max-results` hides it.
    pub fn start_result(&mut self) -> Option<usize> {
        self.section = self.sections.pop_front();
        self.rows_open = true;
        self.results_seen += 1;
        self.rows_total = 0;
        self.rows_shown = 0;
        self.cells_cut = 0;
        self.cut = false;
        self.stopped = false;
        let max = self.limits.max_results;
        self.hidden = max > 0 && self.results_seen > max;
        (!self.hidden).then_some(self.results_seen)
    }

    /// The rows of the current result set are over (`end_rows`), shown or hidden.
    pub fn end_rows(&mut self) {
        self.rows_open = false;
    }

    /// The order of a batch's events (DESIGN.md §5.1, and `BatchSink` in `db`): between a
    /// result set's `columns` and its `end_rows` the sink gets rows and nothing else. A
    /// message, an error or a row count arrives before the result set or after it, so the
    /// open result is never interrupted. `what` names the call for the error, which says so
    /// and changes nothing: a sink asks this first and returns the error.
    ///
    /// It is an error in every build rather than a debug assertion: the events come from the
    /// server, and a release build must neither panic on them nor write a document that
    /// isn't valid. The run then ends as an internal error (exit 10, DESIGN.md §6): the
    /// order is a promise of sqlh and its driver, so a broken one is a bug.
    pub fn outside_rows(&self, what: &str) -> io::Result<()> {
        if self.rows_open {
            return Err(OutOfOrder::error(format!(
                "{what} arrived while a result set's rows were streaming; the sink takes it \
                 before the result set or after its end (BatchSink order, DESIGN.md §5.1)"
            )));
        }
        Ok(())
    }

    pub fn result_hidden(&self) -> bool {
        self.hidden
    }

    pub fn wants_values(&self) -> bool {
        !self.hidden
            && !self.exhausted
            && (self.limits.max_rows == 0 || self.rows_shown < self.limits.max_rows)
    }

    /// The most characters of a value worth reading (`BatchSink::cell_limit`): `--max-cell`.
    /// With no cell limit, a value longer than the whole output budget in characters: every
    /// character takes at least a byte of the response, so such a row can only be withheld, and
    /// the read stops keeping the value there (DESIGN.md §11). 0 when neither is limited.
    pub fn cell_limit(&self) -> usize {
        match (self.limits.max_cell, self.limits.max_output) {
            (0, 0) => 0,
            (0, output) => usize::try_from(output).unwrap_or(usize::MAX),
            (cell, _) => cell,
        }
    }

    pub fn row_shown(&mut self) {
        self.rows_total += 1;
        self.rows_shown += 1;
    }

    pub fn row_skipped(&mut self) {
        self.rows_total += 1;
    }

    pub fn cell_cut(&mut self) {
        self.cells_cut += 1;
    }

    /// The count of hidden rows was stopped early (`BatchSink::counting_stopped`), by `by`: the
    /// rest of the batch was cancelled and the result set's total is a lower bound.
    pub fn stop_counting(&mut self, by: Stop) {
        self.stopped = true;
        self.batch_stopped = true;
        match by {
            Stop::Budget => self.stopped_by_budget = true,
            Stop::Timeout => self.stopped_by_timeout = true,
        }
        if self.stopped_batches.last() != Some(&self.batch_number) {
            self.stopped_batches.push(self.batch_number);
        }
    }

    /// Whether the current result set's total is exact. False once its count was stopped.
    pub fn rows_exact(&self) -> bool {
        !self.stopped
    }

    /// Whether rows were hidden from the current result set that the count reached: more rows
    /// arrived than were shown. This is the raw count, which the floored bound of a stopped
    /// count (`rows_total`) can't contradict: JSON's `truncated` reads it.
    pub fn rows_hidden(&self) -> bool {
        self.rows_total > self.rows_shown
    }

    /// Whether a count was stopped in the current batch.
    pub fn batch_stopped(&self) -> bool {
        self.batch_stopped
    }

    /// How many batches had a count stopped. The status line takes the run's own count
    /// (`run::Accounting`), not this.
    #[cfg(test)]
    pub fn stopped_batches(&self) -> usize {
        self.stopped_batches.len()
    }

    /// The result set's total as the output reports it: the rows counted, or when the count
    /// was stopped, a lower bound.
    /// - The rows counted floored to one significant digit (`floor_digit`). A count that ran
    ///   for a fixed time differs from run to run by the speed of the link; the first digit
    ///   mostly doesn't, so the same query mostly reads the same.
    /// - But once a hidden row was counted, never less than one more than the rows shown, so
    ///   flooring can't make the output say that nothing was hidden (154 rows counted with
    ///   100 shown read `100 of ≥101`, not `≥100`).
    /// - When no hidden row arrived (`-t` ended the wait for the first one), the rows shown:
    ///   that many rows are known to exist, and the stopped count says there may be more.
    pub fn rows_total(&self) -> u64 {
        if !self.stopped {
            return self.rows_total;
        }
        if self.rows_hidden() {
            floor_digit(self.rows_total).max(self.rows_shown.saturating_add(1))
        } else {
            self.rows_shown
        }
    }

    pub fn rows_shown(&self) -> u64 {
        self.rows_shown
    }

    pub fn cells_cut(&self) -> u64 {
        self.cells_cut
    }

    /// Counts a server message; false when `max_messages` says to hide it.
    pub fn message_visible(&mut self) -> bool {
        self.messages_seen += 1;
        let max = self.limits.max_messages;
        max == 0 || self.messages_seen <= max
    }

    /// The input line a server message points at. `None` inside procedures (their lines are
    /// the procedure's own) or when the server gave no line.
    pub fn source_line(&self, message: &ServerMessage) -> Option<u32> {
        if message.procedure.is_some() {
            return None;
        }
        message
            .line
            .filter(|line| *line > 0)
            .map(|line| crate::script::source_line(self.batch_line, &self.line_map, line as u32))
    }

    /// The current source's name, when there are several sources.
    pub fn source_name(&self) -> Option<&str> {
        self.multi_source.then_some(self.source.as_str())
    }

    /// `3 rows`, or `5 of 2775 rows shown (--max-rows 5); 2 values cut (--max-cell 40)`.
    pub fn footer(&self) -> String {
        self.footer_for(false)
    }

    /// The footer of a result a SQL error ended (`end_rows_incomplete`, DESIGN.md §5.1): the
    /// rows that arrived, and `interrupted`, so a reader doesn't take the count for the total.
    /// `3 rows so far (interrupted)`, or `5 of ≥2775 rows shown (--max-rows 5; interrupted)`.
    pub fn footer_incomplete(&self) -> String {
        self.footer_for(true)
    }

    fn footer_for(&self, incomplete: bool) -> String {
        let total = self.rows_total();
        let mut footer = if self.stopped || (incomplete && self.rows_shown != total) {
            // A lower bound: `≥`, and what ended the count after the limit that hid the rows.
            let advice = self.limits.advice;
            let limit = if self.cut {
                advice.max_output().to_owned()
            } else {
                format!("{} {}", advice.max_rows(), self.limits.max_rows)
            };
            let why = match (self.stopped, incomplete) {
                (true, true) => "counting stopped; interrupted",
                (true, false) => "counting stopped",
                _ => "interrupted",
            };
            format!(
                "{} of ≥{total} rows shown ({limit}; {why})",
                self.rows_shown
            )
        } else if incomplete {
            let s = if total == 1 { "" } else { "s" };
            format!("{total} row{s} so far (interrupted)")
        } else if self.rows_shown == total {
            let s = if total == 1 { "" } else { "s" };
            format!("{total} row{s}")
        } else if self.cut {
            format!(
                "{} of {total} rows shown ({})",
                self.rows_shown,
                self.limits.advice.max_output()
            )
        } else {
            format!(
                "{} of {total} rows shown ({} {})",
                self.rows_shown,
                self.limits.advice.max_rows(),
                self.limits.max_rows
            )
        };
        if self.cells_cut > 0 {
            let s = if self.cells_cut == 1 { "" } else { "s" };
            let _ = write!(
                footer,
                "; {} value{s} cut ({} {})",
                self.cells_cut,
                self.limits.advice.max_cell(),
                self.limits.max_cell
            );
        }
        footer
    }

    /// Notes about anything the run-wide limits hid.
    pub fn limit_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let max_results = self.limits.max_results;
        if max_results > 0 && self.results_seen > max_results {
            notes.push(format!(
                "{} more result sets not shown ({})",
                self.results_seen - max_results,
                self.limits.advice.max_results(max_results)
            ));
        }
        let max_messages = self.limits.max_messages;
        if max_messages > 0 && self.messages_seen > max_messages {
            notes.push(format!(
                "{} more messages not shown",
                self.messages_seen - max_messages
            ));
        }
        if !self.stopped_batches.is_empty() {
            notes.push(stopped_note(
                &self.stopped_batches,
                self.stopped_by_budget,
                self.stopped_by_timeout,
                self.limits.advice,
                self.limits.timeout_at_limit,
            ));
        }
        if self.exhausted {
            let w = &self.withheld;
            let counts: Vec<String> = [
                (w.results, "result set", "result sets"),
                (w.messages, "message", "messages"),
                (w.errors, "error", "errors"),
                (
                    w.row_counts,
                    "statement's row count",
                    "statements' row counts",
                ),
                (w.batches, "batch", "batches"),
                (w.notes, "note", "notes"),
                (
                    w.connection,
                    "connection description",
                    "connection descriptions",
                ),
            ]
            .iter()
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, one, many)| format!("{n} {}", if *n == 1 { one } else { many }))
            .chain((w.definition_lines > 0).then(|| {
                let at_least = if w.definition_lines_more { "≥" } else { "" };
                let s = if w.definition_lines == 1 && !w.definition_lines_more {
                    ""
                } else {
                    "s"
                };
                format!("{at_least}{} definition line{s}", w.definition_lines)
            }))
            .collect();
            let mut note = format!(
                "output stopped at {} limit of {} bytes; the run continued",
                match self.limits.advice {
                    Advice::Cli => "the --max-output",
                    Advice::Mcp => "the server's --max-output",
                },
                self.limits.max_output
            );
            if !counts.is_empty() {
                let _ = write!(note, ", and these weren't shown: {}", counts.join(", "));
            }
            notes.push(note);
        }
        notes
    }
}

/// `n` floored to one significant digit: 852359 gives 800000, 1003 gives 1000, 99 gives 90, and
/// anything under 10 stays as it is.
pub fn floor_digit(n: u64) -> u64 {
    let mut unit = 1;
    while n / unit >= 10 {
        unit *= 10;
    }
    n / unit * unit
}

/// The most batches the note about stopped counts names; the rest are counted.
const NOTE_BATCHES: usize = 5;

/// The run's note that counting stopped (DESIGN.md §5.1): where, what that cost, and the way
/// to count further. A stopped count cancels the rest of its batch, and nothing else says its
/// later statements didn't run. The advice follows what stopped the counts: the budget, which
/// `--exact-count` lifts (a run that has it can't be stopped by the budget, so it is never
/// advised to it again), or `-t`, which only raising it lifts. A run with both kinds gets both.
fn stopped_note(
    batches: &[usize],
    by_budget: bool,
    by_timeout: bool,
    advice: Advice,
    timeout_at_limit: bool,
) -> String {
    let shown: Vec<String> = batches
        .iter()
        .take(NOTE_BATCHES)
        .map(usize::to_string)
        .collect();
    let mut note = if batches.len() == 1 {
        format!(
            "counting stopped in batch {}: the rest of that batch was cancelled, so its later \
             statements did not run",
            shown[0]
        )
    } else {
        let more = batches.len() - shown.len();
        let more = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        format!(
            "counting stopped in batches {}{more}: the rest of each was cancelled, so their \
             later statements did not run",
            shown.join(", ")
        )
    };
    let (exact, longer) = match advice {
        Advice::Cli => ("--exact-count counts every row", "raise -t to count longer"),
        Advice::Mcp => (
            "the profile's exact_count = true counts every row",
            "a larger timeout counts longer",
        ),
    };
    // A call already at the server's limit has no larger timeout to ask for.
    let by_timeout = by_timeout && !(advice == Advice::Mcp && timeout_at_limit);
    for advice in [(by_budget, exact), (by_timeout, longer)]
        .iter()
        .filter(|(applies, _)| *applies)
        .map(|(_, advice)| advice)
    {
        note.push_str("; ");
        note.push_str(advice);
    }
    note
}

/// Cuts `s` after `max` characters (0 = no limit). Returns the kept prefix and how many
/// characters were cut.
pub fn cut(s: &str, max: usize) -> (&str, usize) {
    if max == 0 {
        return (s, 0);
    }
    match s.char_indices().nth(max) {
        Some((index, _)) => (&s[..index], s[index..].chars().count()),
        None => (s, 0),
    }
}

/// `0x` and uppercase hex for at most `max / 2` bytes (0 = all). Returns the text and how
/// many bytes were cut.
pub fn hex(bytes: &[u8], max: usize) -> (String, usize) {
    let shown = if max == 0 {
        bytes.len()
    } else {
        bytes.len().min(max / 2)
    };
    let mut out = String::with_capacity(2 + shown * 2);
    out.push_str("0x");
    for b in &bytes[..shown] {
        let _ = write!(out, "{b:02X}");
    }
    (out, bytes.len() - shown)
}

/// The marker that ends a cut value: `…[+N]` characters, or `…[+N bytes]` for binary.
pub fn cut_marker(cut: usize, bytes: bool) -> String {
    if bytes {
        format!("…[+{cut} bytes]")
    } else {
        format!("…[+{cut}]")
    }
}

/// `s` with the text format's escaping and no limit: for variable fields in metadata lines
/// (names, locations, reasons, notes), so no value can start a line or fake a `-- ` line.
pub fn escaped(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_escaped(&mut out, s, 0);
    out
}

/// Appends a column's type name for a `name:type` header entry: escaped like a name, with
/// `:` also written as `\x3A`, so the type is always everything after the last `:`. Type
/// names are mostly built by sqlh, but a CLR UDT's comes from the server as is.
pub fn push_type(out: &mut String, type_name: &str) {
    for (i, part) in type_name.split(':').enumerate() {
        if i > 0 {
            out.push_str("\\x3A");
        }
        push_escaped(out, part, 0);
    }
}

/// Appends `s` with `\\ \t \n \r` and other control characters escaped (the text format's
/// escaping), cut after `max` characters with a marker. Returns true when cut.
/// - Every control character, C0 and DEL and C1 (U+0080..U+009F), becomes `\xNN`. U+0085 is
///   one of Unicode's line breaks, and U+009B is a control sequence introducer on some
///   8-bit terminals.
/// - The other two Unicode line breaks, U+2028 and U+2029, become `\u2028` and `\u2029`.
///   Together with `\n`, `\r` and the C0 breaks, that leaves no character a consumer could
///   split a line at (Python's `splitlines`, say), so a value can't pass for a `-- ` line.
pub fn push_escaped(out: &mut String, s: &str, max: usize) -> bool {
    let (kept, cut_count) = cut(s, max);
    for ch in kept.chars() {
        push_char(out, ch);
    }
    if cut_count > 0 {
        out.push_str(&cut_marker(cut_count, false));
    }
    cut_count > 0
}

/// Appends one character in the text format's escaping (see `push_escaped`).
pub fn push_char(out: &mut String, ch: char) {
    match ch {
        '\\' => out.push_str("\\\\"),
        '\t' => out.push_str("\\t"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        // Control characters are all below U+0100, so two hex digits are enough.
        c if c.is_control() => {
            let _ = write!(out, "\\x{:02X}", c as u32);
        }
        '\u{2028}' => out.push_str("\\u2028"),
        '\u{2029}' => out.push_str("\\u2029"),
        c => out.push(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escaped(s: &str, max: usize) -> (String, bool) {
        let mut out = String::new();
        let cut = push_escaped(&mut out, s, max);
        (out, cut)
    }

    #[test]
    fn escapes() {
        assert_eq!(
            escaped("a\tb\nc\r\\d\x01", 0),
            ("a\\tb\\nc\\r\\\\d\\x01".into(), false)
        );
        assert_eq!(escaped("héllo wörld", 5), ("héllo…[+6]".into(), true));
        assert_eq!(escaped("abc", 3), ("abc".into(), false));
    }

    // R06: C1 controls and the two Unicode-only line breaks are escaped, ordinary Unicode
    // text (accents, a no-break space, CJK) is not.
    #[test]
    fn escapes_c1_controls_and_unicode_line_breaks() {
        assert_eq!(
            escaped("a\u{7f}b\u{80}c\u{85}d\u{9b}e\u{9f}f", 0),
            ("a\\x7Fb\\x80c\\x85d\\x9Be\\x9Ff".into(), false)
        );
        assert_eq!(
            escaped("x\u{2028}y\u{2029}z", 0),
            ("x\\u2028y\\u2029z".into(), false)
        );
        assert_eq!(
            escaped("é\u{a0}日本\u{2027}\u{202a}", 0),
            ("é\u{a0}日本\u{2027}\u{202a}".into(), false)
        );
        // The cut counts characters before escaping, as it always did.
        assert_eq!(escaped("a\u{2028}b", 2), ("a\\u2028…[+1]".into(), true));
    }

    // R06: whatever the character, its escaped form holds nothing a consumer splits lines at
    // (the set is Python's `str.splitlines`, the widest in common use).
    #[test]
    fn no_character_survives_as_a_line_break() {
        const BREAKS: [char; 10] = [
            '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
        ];
        for c in '\0'..=char::MAX {
            let (out, _) = escaped(&c.to_string(), 0);
            assert!(
                !out.chars().any(|o| BREAKS.contains(&o)),
                "U+{:04X} -> {out:?}",
                c as u32
            );
        }
    }

    #[test]
    fn cuts_and_hex() {
        assert_eq!(cut("héllo", 0), ("héllo", 0));
        assert_eq!(cut("héllo", 2), ("hé", 3));
        assert_eq!(hex(&[0xAB, 0xCD, 0xEF], 0), ("0xABCDEF".into(), 0));
        assert_eq!(hex(&[0xAB, 0xCD, 0xEF], 5), ("0xABCD".into(), 1));
    }

    // PLAN.md 2.15: a stopped count reports its first digit.
    #[test]
    fn a_count_is_floored_to_one_significant_digit() {
        for (n, floored) in [
            (0, 0),
            (7, 7),
            (9, 9),
            (10, 10),
            (99, 90),
            (100, 100),
            (101, 100),
            (852_359, 800_000),
            (1_003, 1_000),
            (1_999_999, 1_000_000),
            (u64::MAX, 10_000_000_000_000_000_000),
        ] {
            assert_eq!(floor_digit(n), floored, "{n}");
        }
    }

    /// A result of `shown` rows, `--max-rows` equal to that, and `hidden` rows counted past
    /// them before the count was stopped by `by`.
    fn stopped_result(shown: u64, hidden: u64, by: Stop) -> Tally {
        let mut t = Tally::new(Limits {
            max_rows: shown,
            ..Limits::default()
        });
        t.start_result();
        for _ in 0..shown {
            t.row_shown();
        }
        for _ in 0..hidden {
            t.row_skipped();
        }
        t.stop_counting(by);
        t
    }

    // PLAN.md 2.15: flooring must not make the output say that nothing was hidden. Once a
    // hidden row was counted the bound is above the rows shown, whatever the floor says, and
    // `rows_hidden` (JSON's `truncated`) reads the raw count.
    #[test]
    fn a_stopped_count_never_hides_that_rows_were_hidden() {
        // Plenty hidden: one significant digit.
        let t = stopped_result(100, 852_259, Stop::Budget);
        assert_eq!(
            (t.rows_total(), t.rows_hidden(), t.rows_exact()),
            (800_000, true, false)
        );
        assert_eq!(
            t.footer(),
            "100 of ≥800000 rows shown (--max-rows 100; counting stopped)"
        );
        // A few hidden: 101 to 199 floor to 100, which is not above the 100 shown, so the
        // bound is one more than the rows shown. 200 floors to 200 and stays.
        for hidden in [1, 54, 99] {
            let t = stopped_result(100, hidden, Stop::Budget);
            assert_eq!((t.rows_total(), t.rows_hidden()), (101, true), "{hidden}");
            assert_eq!(
                t.footer(),
                "100 of ≥101 rows shown (--max-rows 100; counting stopped)",
                "{hidden}"
            );
        }
        assert_eq!(stopped_result(100, 100, Stop::Budget).rows_total(), 200);
        // The floor can land under the rows shown: 19 counted floor to 10, 15 were shown.
        assert_eq!(stopped_result(15, 4, Stop::Budget).rows_total(), 16);
        // Nothing hidden arrived (`-t` ended the wait for the first hidden row): the rows
        // shown are the bound, and no rows are said to be hidden.
        let t = stopped_result(7, 0, Stop::Timeout);
        assert_eq!(
            (t.rows_total(), t.rows_hidden(), t.rows_exact()),
            (7, false, false)
        );
        assert_eq!(
            t.footer(),
            "7 of ≥7 rows shown (--max-rows 7; counting stopped)"
        );
        // The output budget as the limit that hid them: it is named in place of --max-rows.
        let mut t = stopped_result(100, 0, Stop::Budget);
        t.row_withheld();
        assert_eq!(
            t.footer(),
            "100 of ≥101 rows shown (--max-output; counting stopped)"
        );
        // A count that ended on its own is exact, however many rows were hidden.
        let mut t = Tally::new(Limits::default());
        t.start_result();
        for _ in 0..105 {
            t.row_skipped();
        }
        assert_eq!(
            (t.rows_total(), t.rows_exact(), t.rows_hidden()),
            (105, true, true)
        );
        // A new result is exact again; the batch keeps what happened.
        let mut t = stopped_result(100, 5, Stop::Budget);
        t.start_result();
        assert!(t.rows_exact());
        assert_eq!((t.batch_stopped(), t.stopped_batches()), (true, 1));
    }

    #[test]
    fn the_note_names_the_batches_where_counting_stopped() {
        let note = |batches: &[usize]| stopped_note(batches, true, false, Advice::Cli, false);
        let tail = "; --exact-count counts every row";
        assert_eq!(
            note(&[4]),
            format!(
                "counting stopped in batch 4: the rest of that batch was cancelled, so its \
                 later statements did not run{tail}"
            )
        );
        assert_eq!(
            note(&[2, 3, 4, 5, 6]),
            format!(
                "counting stopped in batches 2, 3, 4, 5, 6: the rest of each was cancelled, so \
                 their later statements did not run{tail}"
            )
        );
        assert_eq!(
            note(&[2, 3, 4, 5, 6, 7, 8, 9]),
            format!(
                "counting stopped in batches 2, 3, 4, 5, 6 (+3 more): the rest of each was \
                 cancelled, so their later statements did not run{tail}"
            )
        );
    }

    // PLAN.md 2.15: the advice follows what stopped the count. `--exact-count` lifts the
    // budget and nothing else, so a run that `-t` stopped is told to raise `-t` instead,
    // which is also what a run that already has `--exact-count` needs; a run with both kinds
    // of stop gets both.
    #[test]
    fn the_notes_advice_follows_what_stopped_the_count() {
        let first = "counting stopped in batch 2: the rest of that batch was cancelled, so its \
                     later statements did not run";
        assert_eq!(
            stopped_note(&[2], true, false, Advice::Cli, false),
            format!("{first}; --exact-count counts every row")
        );
        assert_eq!(
            stopped_note(&[2], false, true, Advice::Cli, false),
            format!("{first}; raise -t to count longer")
        );
        assert_eq!(
            stopped_note(&[2], true, true, Advice::Cli, false),
            format!("{first}; --exact-count counts every row; raise -t to count longer")
        );
        // sqlh mcp names what its reader can change: the profile, and the call's timeout.
        assert_eq!(
            stopped_note(&[2], true, true, Advice::Mcp, false),
            format!(
                "{first}; the profile's exact_count = true counts every row; a larger timeout \
                 counts longer"
            )
        );
        // A call already at the server's limit is offered no larger timeout.
        assert_eq!(
            stopped_note(&[2], true, true, Advice::Mcp, true),
            format!("{first}; the profile's exact_count = true counts every row")
        );
        assert_eq!(stopped_note(&[2], false, true, Advice::Mcp, true), first);
        // Through the tally: batch 1 stopped by the budget, batch 2 by `-t`.
        let batch = |number| BatchInfo {
            number,
            total: 2,
            source: "-Q",
            multi_source: false,
            line: 1,
            line_map: &[],
            run: 1,
            runs: 1,
            started_ms: 0,
        };
        let mut t = Tally::new(Limits::default());
        t.begin_batch(&batch(1));
        t.start_result();
        t.stop_counting(Stop::Timeout);
        assert_eq!(
            t.limit_notes(),
            [
                "counting stopped in batch 1: the rest of that batch was cancelled, so its \
                 later statements did not run; raise -t to count longer"
            ]
        );
        t.begin_batch(&batch(2));
        t.start_result();
        t.stop_counting(Stop::Budget);
        assert_eq!(
            t.limit_notes(),
            [
                "counting stopped in batches 1, 2: the rest of each was cancelled, so their \
                 later statements did not run; --exact-count counts every row; raise -t to \
                 count longer"
            ]
        );
        assert_eq!(t.stopped_batches(), 2);
    }

    // docs/mcp-mode.md §4, What a result advises, exactly and beside the CLI's: in sqlh mcp a
    // footer names the limit that hid rows or cut values as the call's argument (`max_rows N`,
    // `max_cell N`) and the output limit as the server's launch flag, which no call changes,
    // with `counting stopped` and `interrupted` as in the CLI; the result sets past the limit
    // are a fixed number per call.
    #[test]
    fn in_mcp_footers_and_the_result_set_note_name_the_calls_limits() {
        // 2775 rows, 5 shown (the row limit 5), two of their values cut (the value limit 40).
        let by_rows = |advice| {
            let mut t = Tally::new(Limits {
                max_rows: 5,
                max_cell: 40,
                advice,
                ..Limits::default()
            });
            t.start_result();
            for _ in 0..5 {
                t.row_shown();
            }
            t.cell_cut();
            t.cell_cut();
            for _ in 0..2770 {
                t.row_skipped();
            }
            t
        };
        // 50 rows, 3 shown before the output budget ran out.
        let by_output = |advice| {
            let mut t = Tally::new(Limits {
                advice,
                ..Limits::default()
            });
            t.start_result();
            for _ in 0..3 {
                t.row_shown();
            }
            t.row_withheld();
            for _ in 0..46 {
                t.row_skipped();
            }
            t
        };
        let stopped = |mut t: Tally| {
            t.stop_counting(Stop::Budget);
            t
        };
        for (advice, rows, cell, output, results) in [
            (
                Advice::Cli,
                "--max-rows 5",
                "--max-cell 40",
                "--max-output",
                "--max-results 1",
            ),
            (
                Advice::Mcp,
                "max_rows 5",
                "max_cell 40",
                "the server's --max-output",
                "at most 1 per call",
            ),
        ] {
            let values = format!("; 2 values cut ({cell})");
            assert_eq!(
                by_rows(advice).footer(),
                format!("5 of 2775 rows shown ({rows}){values}")
            );
            assert_eq!(
                by_rows(advice).footer_incomplete(),
                format!("5 of ≥2775 rows shown ({rows}; interrupted){values}")
            );
            assert_eq!(
                stopped(by_rows(advice)).footer(),
                format!("5 of ≥2000 rows shown ({rows}; counting stopped){values}")
            );
            assert_eq!(
                stopped(by_rows(advice)).footer_incomplete(),
                format!("5 of ≥2000 rows shown ({rows}; counting stopped; interrupted){values}")
            );
            assert_eq!(
                by_output(advice).footer(),
                format!("3 of 50 rows shown ({output})")
            );
            assert_eq!(
                stopped(by_output(advice)).footer(),
                format!("3 of ≥50 rows shown ({output}; counting stopped)")
            );
            let mut t = Tally::new(Limits {
                max_results: 1,
                advice,
                ..Limits::default()
            });
            for _ in 0..3 {
                t.start_result();
                t.end_rows();
            }
            assert_eq!(
                t.limit_notes(),
                [format!("2 more result sets not shown ({results})")]
            );
        }
    }

    // F06: a refusal is final, and the closing note doesn't list counts that are all zero.
    #[test]
    fn the_output_budget_is_sticky() {
        let mut tally = Tally::new(Limits {
            max_output: OUTPUT_RESERVE + 100,
            ..Limits::default()
        });
        assert!(tally.admit(0, 100));
        assert!(!tally.admit(100, 1));
        assert!(
            !tally.admit(0, 1),
            "nothing fits once the budget has run out"
        );
        assert!(!tally.wants_values());
        assert_eq!(
            tally.limit_notes(),
            ["output stopped at the --max-output limit of 1124 bytes; the run continued"]
        );
        // In sqlh mcp it is the server's launch flag (docs/mcp-mode.md §4).
        let mut mcp = Tally::new(Limits {
            max_output: OUTPUT_RESERVE + 100,
            advice: Advice::Mcp,
            ..Limits::default()
        });
        assert!(!mcp.admit(101, 1));
        assert_eq!(
            mcp.limit_notes(),
            ["output stopped at the server's --max-output limit of 1124 bytes; the run continued"]
        );
        let mut unlimited = Tally::new(Limits {
            max_output: 0,
            ..Limits::default()
        });
        assert!(unlimited.admit(u64::MAX / 2, usize::MAX / 2));
    }

    // R08: text that must be written whatever its length is cut to the room it is given,
    // marker included, and the marker says how many characters went.
    #[test]
    fn push_within_cuts_to_the_room() {
        let within = |s: &str, room: usize| {
            let mut out = String::new();
            let cut = push_within(&mut out, s, room, push_char);
            (out, cut)
        };
        assert_eq!(within("hello", 5), ("hello".into(), Some(0)));
        assert_eq!(within("hello", usize::MAX), ("hello".into(), Some(0)));
        assert_eq!(within("", 0), (String::new(), Some(0)));
        // "…[+8]" is seven bytes, so 12 bytes keep "hél" (four) and one is left over: the
        // marker is sized for the widest count, "…[+11]".
        assert_eq!(within("héllo wörld", 12), ("hél…[+8]".into(), Some(8)));
        // Escapes count as written: four bytes each, never half of one.
        assert_eq!(
            within("\u{1}\u{1}\u{1}\u{1}", 12),
            ("\\x01…[+3]".into(), Some(3))
        );
        // Not even the marker fits: nothing is appended.
        assert_eq!(within("abcdefg", 6), (String::new(), None));
        assert_eq!(within("a", 0), (String::new(), None));
        // Appends to what is there.
        let mut out = String::from("> ");
        assert_eq!(push_within(&mut out, "abc", 3, push_char), Some(0));
        assert_eq!(out, "> abc");
    }

    #[test]
    fn shares_split_a_room_evenly_without_wasting_it() {
        assert_eq!(shares(&[10, 200, 30], 100), [10, 60, 30]);
        assert_eq!(shares(&[50, 50], 60), [30, 30]);
        assert_eq!(shares(&[50, 50], 1000), [50, 50]);
        assert_eq!(shares(&[0, 100], 10), [0, 10]);
        assert_eq!(shares(&[5], 0), [0]);
        assert_eq!(shares(&[], 7), Vec::<usize>::new());
        // What an even split leaves over goes to the last of the biggest.
        assert_eq!(shares(&[100, 100, 100], 10), [3, 3, 4]);
        for (needs, room) in [(vec![7, 300, 9, 300], 50), (vec![1, 2, 3], 4)] {
            let given = shares(&needs, room);
            assert!(given.iter().sum::<usize>() <= room, "{given:?}");
            assert!(given.iter().zip(&needs).all(|(g, n)| g <= n), "{given:?}");
        }
    }

    #[test]
    fn a_layout_shortens_its_parts_and_never_its_fixed_text() {
        let mut layout = Layout::new();
        layout
            .text("[")
            .part("aaaaaaaaaaaaaaaaaaaa")
            .text("|")
            .part("bb")
            .text("]");
        let fit = |room| layout.fit(room, push_char);
        assert_eq!(fit(usize::MAX).unwrap(), "[aaaaaaaaaaaaaaaaaaaa|bb]");
        assert_eq!(fit(25).unwrap(), "[aaaaaaaaaaaaaaaaaaaa|bb]");
        // Three bytes of fixed text leave 21 for the parts: "bb" keeps its two, and the
        // other 19 hold eleven characters and "…[+9]".
        assert_eq!(fit(24).unwrap(), "[aaaaaaaaaaa…[+9]|bb]");
        // Fixed text alone doesn't fit, or a part would get less than its marker.
        assert_eq!(fit(2), None);
        assert_eq!(fit(10), None);
        assert!(fit(25).unwrap().len() <= 25 && fit(24).unwrap().len() <= 24);
    }

    #[test]
    fn rooms_leave_the_reserve() {
        let tally = |max_output| {
            Tally::new(Limits {
                max_output,
                ..Limits::default()
            })
        };
        let t = tally(5000);
        assert_eq!(t.note_room(1000), 5000 - 1000 - STATUS_RESERVE as usize);
        assert_eq!(t.closing_room(1000), 4000);
        assert_eq!(t.note_room(4800), 0);
        assert_eq!(t.closing_room(6000), 0);
        let unlimited = tally(0);
        assert_eq!(unlimited.note_room(u64::MAX), usize::MAX);
        assert_eq!(unlimited.closing_room(7), usize::MAX);
    }

    // PLAN.md 2.7: the read cuts a large value to `cell_limit()` characters as it arrives. With
    // `--max-cell` that is the limit itself; without, the output budget in characters, since a
    // value longer than the whole response can only be withheld; with neither, no limit.
    #[test]
    fn the_cell_limit_is_what_the_sink_can_show() {
        let limit = |max_cell, max_output| {
            Tally::new(Limits {
                max_cell,
                max_output,
                ..Limits::default()
            })
            .cell_limit()
        };
        assert_eq!(limit(500, 1_000_000), 500);
        assert_eq!(limit(500, 0), 500);
        assert_eq!(limit(0, 1_000_000), 1_000_000);
        assert_eq!(limit(0, MIN_MAX_OUTPUT), 4096);
        assert_eq!(limit(0, 0), 0);
        assert_eq!(Tally::new(Limits::default()).cell_limit(), 500);
    }

    // PLAN.md 2.7: a value the read cut (`TextCut`, `BinCut`) is written exactly as the same
    // value is when the sink cuts it: the same bytes and the same count of values cut, in
    // every format. The first column starts with `--`, which the text format guards.
    #[test]
    fn a_value_cut_by_the_read_is_written_like_one_cut_by_the_sink() {
        use crate::db::Value;
        let limits = Limits {
            max_rows: 0,
            max_cell: 5,
            max_results: 0,
            max_messages: 0,
            max_output: 0,
            stall_secs: 0,
            ..Limits::default()
        };
        let columns = [
            crate::db::Column {
                name: "p".into(),
                type_name: "nvarchar(max)".into(),
                nullable: true,
            },
            crate::db::Column {
                name: "t".into(),
                type_name: "varchar(max)".into(),
                nullable: true,
            },
            crate::db::Column {
                name: "b".into(),
                type_name: "varbinary(max)".into(),
                nullable: true,
            },
        ];
        // Cut by the sink, and the same rows cut by the read (five characters or two bytes
        // kept, the rest counted).
        let by_sink = [
            Value::Text("--abc\"defg".into()),
            Value::Text("héllo, wörld".into()),
            Value::Bin(vec![0xAB, 0xCD, 0xEF]),
        ];
        let by_read = [
            Value::TextCut("--abc".into(), 5),
            Value::TextCut("héllo".into(), 7),
            Value::BinCut(vec![0xAB, 0xCD], 1),
        ];
        // What a sink writes for one row, to the data stream and to the metadata stream.
        let write = |format, row: &[Value]| {
            let (main, meta) = (Capture::default(), Capture::default());
            let mut out: Box<dyn Output> = match format {
                Format::Text => Box::new(text::TextSink::with_meta(
                    main.clone(),
                    Box::new(meta.clone()),
                    limits,
                )),
                Format::Json => Box::new(json::JsonSink::with_meta(
                    main.clone(),
                    Box::new(meta.clone()),
                    limits,
                )),
                Format::Csv => Box::new(csv::CsvSink::new(
                    main.clone(),
                    Box::new(meta.clone()),
                    limits,
                )),
            };
            out.columns(&columns).unwrap();
            assert_eq!(out.cell_limit(), 5);
            out.row(row).unwrap();
            out.end_rows().unwrap();
            out.finish(&Summary {
                exit: 0,
                status: "ok: 1 batch, 3 ms",
                error: None,
                elapsed_ms: 3,
            })
            .unwrap();
            out.flush().unwrap();
            drop(out);
            let text = |c: &Capture| String::from_utf8(c.0.lock().unwrap().clone()).unwrap();
            (text(&main), text(&meta))
        };
        for format in [Format::Text, Format::Json, Format::Csv] {
            let sunk = write(format, &by_sink);
            let read = write(format, &by_read);
            assert_eq!(read, sunk, "{format:?}");
        }
        // The text format, exactly: the guard on the first column's `--`, the markers' counts,
        // and the footer's.
        assert_eq!(
            write(Format::Text, &by_read).0,
            "-- result 1\np:nvarchar(max)\tt:varchar(max)\tb:varbinary(max)\n\
             \\--abc…[+5]\théllo…[+7]\t0xABCD…[+1 bytes]\n\
             -- 1 row; 3 values cut (--max-cell 5)\n-- ok: 1 batch, 3 ms\n"
        );
        // csv: the data, then the footer on the metadata stream.
        let (data, meta) = write(Format::Csv, &by_read);
        assert_eq!(data, "p,t,b\n--abc…[+5],héllo…[+7],0xABCD…[+1 bytes]\n");
        assert!(
            meta.contains("-- 1 row; 3 values cut (--max-cell 5)\n"),
            "{meta}"
        );
        // json: one row of three strings, and the count of cut cells.
        let doc: serde_json::Value =
            serde_json::from_str(&write(Format::Json, &by_read).0).expect("a JSON document");
        let result = &doc["batches"][0]["events"][0];
        assert_eq!(
            result["rows"],
            serde_json::json!([["--abc…[+5]", "héllo…[+7]", "0xABCD…[+1 bytes]"]])
        );
        assert_eq!(result["cells_cut"], 3);
    }

    // A note that cannot fit exhausts the budget and shows in the closing note's counts.
    #[test]
    fn a_withheld_note_is_counted_and_ends_the_body() {
        let mut tally = Tally::new(Limits {
            max_output: 5000,
            ..Limits::default()
        });
        tally.withhold_note();
        tally.withhold_note();
        assert!(!tally.admit(0, 1));
        assert_eq!(
            tally.limit_notes(),
            [
                "output stopped at the --max-output limit of 5000 bytes; the run continued, and \
              these weren't shown: 2 notes"
            ]
        );
    }

    // PLAN.md 2.11: the description of the connection is a piece of the body, counted when
    // the budget has no room for it.
    #[test]
    fn a_withheld_connection_description_is_counted() {
        let mut tally = Tally::new(Limits {
            max_output: 5000,
            ..Limits::default()
        });
        assert!(!tally.admit(4000, 1));
        tally.withhold_connection();
        assert_eq!(
            tally.limit_notes(),
            [
                "output stopped at the --max-output limit of 5000 bytes; the run continued, and \
                 these weren't shown: 1 connection description"
            ]
        );
    }

    // PLAN.md 2.12: a result set is open from `columns` to `end_rows`, whether it is shown
    // or hidden, and while it is open nothing but rows is taken. Refusing changes nothing.
    #[test]
    fn events_are_refused_while_a_result_set_is_open() {
        let mut tally = Tally::new(Limits {
            max_results: 1,
            ..Limits::default()
        });
        assert!(tally.outside_rows("a message").is_ok());
        for shown in [true, false] {
            assert_eq!(tally.start_result().is_some(), shown);
            for what in ["a message", "an error", "a row count"] {
                let error = tally.outside_rows(what).unwrap_err();
                assert!(OutOfOrder::is(&error), "{error}");
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                assert!(
                    error
                        .to_string()
                        .starts_with(&format!("{what} arrived while a result set's rows were")),
                    "{error}"
                );
            }
            tally.end_rows();
            assert!(tally.outside_rows("a message").is_ok());
        }
        // A result set that a failure cut short doesn't carry over into the next batch.
        tally.start_result();
        assert!(tally.outside_rows("a message").is_err());
        tally.begin_batch(&BatchInfo {
            number: 2,
            total: 2,
            source: "-Q",
            multi_source: false,
            line: 1,
            line_map: &[],
            run: 1,
            runs: 1,
            started_ms: 0,
        });
        assert!(tally.outside_rows("a message").is_ok());
    }

    // Only a sink's refusal is an `OutOfOrder`: an `InvalidInput` from a writer is not, so it
    // stays a failure to write.
    #[test]
    fn only_a_refusal_is_out_of_order() {
        assert!(OutOfOrder::is(&OutOfOrder::error("x".into())));
        assert!(!OutOfOrder::is(&io::Error::from(
            io::ErrorKind::InvalidInput
        )));
        assert!(!OutOfOrder::is(&io::Error::new(
            io::ErrorKind::InvalidInput,
            "x"
        )));
        assert_eq!(OutOfOrder::error("why".into()).to_string(), "why");
    }

    // R08: the reserve is sized for the widest footer plus the longest status line main.rs
    // builds (the JSON equivalent is tested in json.rs). If a status line grows, this is the
    // test that says the reserve must too. The tail that says where the run went (PLAN.md
    // 2.11) is not in it: its length depends on the names, so it is reserved exactly, on top
    // (`Tally::reserve_ending`, tested below).
    #[test]
    fn the_worst_case_footer_and_status_fit_the_status_reserve() {
        let mut tally = Tally::new(Limits {
            max_rows: u64::MAX,
            max_cell: usize::MAX,
            ..Limits::default()
        });
        tally.rows_total = u64::MAX;
        tally.rows_shown = u64::MAX - 1;
        tally.cells_cut = u64::MAX;
        // A stopped count, cut by the output budget: the longest footer.
        tally.stopped = true;
        tally.cut = true;
        let footer = format!("-- {}\n", tally.footer());
        let status = format!("-- {}\n", text::tests::FILLING_STATUS);
        // The status the sweeps use is the longest main.rs builds: every count at its widest.
        let widest = format!(
            "failed (exit 1): {} errors, first in batch {}; {} later batches not run \
             (--continue-on-error runs them), counting stopped in {}, {} ms",
            u32::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX,
            u128::MAX
        );
        assert_eq!(status, format!("-- {widest}\n"));
        let worst = footer.len() + status.len();
        assert!(worst <= STATUS_RESERVE as usize, "{worst} bytes");
        // R17: the footer of a result a SQL error ended is no wider, even in the combination
        // the database layer never produces (a stopped count that is also incomplete).
        for stopped in [false, true] {
            tally.stopped = stopped;
            let incomplete = format!("-- {}\n", tally.footer_incomplete());
            let worst = incomplete.len() + status.len();
            assert!(worst <= STATUS_RESERVE as usize, "{worst} bytes");
        }
    }

    // PLAN.md 2.11, DESIGN.md §4.4: the status line's tail (`; db01/master, connect 195 ms`)
    // is held back from the budget by its own length, on top of both reserves, so the status
    // line, which is never shortened, fits whatever the names hold, and a short tail costs the
    // body a few bytes rather than the widest one's worth.
    #[test]
    fn the_status_tail_is_reserved_on_top_of_both_reserves() {
        let tally = |max_output| {
            Tally::new(Limits {
                max_output,
                ..Limits::default()
            })
        };
        let mut t = tally(5000);
        assert!(t.reserve_ending(0, 100).unwrap());
        // The body stops 100 bytes sooner, notes 100 bytes sooner, and the closing lines keep
        // everything the cap has left, the tail's room included: they hold the status line.
        assert_eq!(
            t.note_room(1000),
            5000 - 1000 - STATUS_RESERVE as usize - 100
        );
        assert_eq!(t.closing_room(1000), 4000);
        let body = 5000 - OUTPUT_RESERVE as usize - 100;
        assert!(t.admit(0, body));
        assert!(!t.admit(body as u64, 1));
        let mut without = tally(5000);
        assert!(without.admit(0, body + 100));
        // No cap, no reserve.
        let mut unlimited = tally(0);
        assert!(
            unlimited
                .reserve_ending(u64::MAX / 2, usize::MAX / 2)
                .unwrap()
        );
        assert_eq!(unlimited.note_room(7), usize::MAX);
        assert!(unlimited.admit(u64::MAX / 2, usize::MAX / 2));
    }

    // The tail fits when the cap has that much left before the status reserve, exactly; when it
    // doesn't, it is left out, the budget is spent, and the closing note counts it, as it
    // counts JSON's head (`1 connection description`). The reserves stay as they were.
    #[test]
    fn a_status_tail_that_does_not_fit_is_left_out_and_counted() {
        let tally = || {
            Tally::new(Limits {
                max_output: 5000,
                ..Limits::default()
            })
        };
        let room = 5000 - 4000 - STATUS_RESERVE;
        let mut fits = tally();
        assert!(fits.reserve_ending(4000, room as usize).unwrap());
        assert_eq!(fits.note_room(4000), 0);
        let mut tight = tally();
        assert!(!tight.reserve_ending(4000, room as usize + 1).unwrap());
        assert_eq!(tight.note_room(4000), room as usize);
        assert!(!tight.admit(0, 1), "the budget is spent");
        assert_eq!(
            tight.limit_notes(),
            [
                "output stopped at the --max-output limit of 5000 bytes; the run continued, and \
                 these weren't shown: 1 connection description"
            ]
        );
    }

    // The connection is described once, and before the first batch: a second call, or one
    // after a batch began, is a bug of the caller's and changes nothing.
    #[test]
    fn a_status_tail_is_reserved_once_and_before_the_first_batch() {
        let mut t = Tally::new(Limits::default());
        assert!(t.reserve_ending(0, 10).unwrap());
        let error = t.reserve_ending(0, 10).unwrap_err();
        assert!(OutOfOrder::is(&error), "{error}");
        assert_eq!(t.note_room(0), 1_000_000 - STATUS_RESERVE as usize - 10);

        let mut t = Tally::new(Limits::default());
        t.begin_batch(&BatchInfo {
            number: 1,
            total: 1,
            source: "-Q",
            multi_source: false,
            line: 1,
            line_map: &[],
            run: 1,
            runs: 1,
            started_ms: 0,
        });
        assert!(OutOfOrder::is(&t.reserve_ending(0, 10).unwrap_err()));
        assert_eq!(t.note_room(0), 1_000_000 - STATUS_RESERVE as usize);
    }

    // The widest tail, two names of 255 characters that are all six-byte escapes (cut to 128
    // each) and a connect time of 39 digits, is 1,609 bytes. With the widest status line
    // (above) it is the widest ending there can be, and the smallest cap the CLI accepts still
    // leaves the body more than a thousand bytes after both: the tail costs a short tail's
    // few bytes in a normal run and this only for names no SQL Server hands out.
    #[test]
    fn the_widest_status_tail_leaves_the_smallest_cap_a_body() {
        use text::tests::{WIDEST_ENDING, widest_names};
        let mut tally = Tally::new(Limits::default());
        let ending = text::status_ending(&mut tally, 0, &widest_names(), u128::MAX).unwrap();
        assert_eq!(ending.len(), WIDEST_ENDING);
        assert_eq!(WIDEST_ENDING, 1609);
        let body = MIN_MAX_OUTPUT - OUTPUT_RESERVE - WIDEST_ENDING as u64;
        assert!(body > 1000, "{body} bytes");
        // A normal tail costs the body about thirty bytes.
        let usual = text::status_ending(
            &mut Tally::new(Limits::default()),
            0,
            &text::tests::connection_to("db01", "master"),
            195,
        )
        .unwrap();
        assert_eq!(usual, "; db01/master, connect 195 ms");
    }

    /// What the delivery thread wrote, readable from the test (the thread needs `Send`).
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A stderr whose reader has gone.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    // DESIGN.md §6, wired through `create`: in text and json, stderr carries diagnostics
    // only, so a stderr that fails loses them and the output is still delivered. In csv it
    // carries the metadata and the status, so the same failure is a delivery failure.
    #[test]
    fn a_failing_stderr_costs_only_the_diagnostics_except_in_csv() {
        let summary = Summary {
            exit: 0,
            status: "ok: 0 batches, 3 ms",
            error: None,
            elapsed_ms: 3,
        };
        for format in [Format::Text, Format::Json] {
            let stdout = Capture::default();
            let mut stream = create_with(
                format,
                Box::new(stdout.clone()),
                Box::new(Broken),
                Limits::default(),
            );
            stream.diagnostic("connected in 5 ms").unwrap();
            stream.note("a note").unwrap();
            stream.finish(&summary).unwrap();
            stream.flush().unwrap();
            stream.wait_delivered(None).unwrap();
            let written = String::from_utf8(stdout.0.lock().unwrap().clone()).unwrap();
            assert!(
                written.contains("a note") && written.contains("ok: 0 batches, 3 ms"),
                "{format:?}: {written}"
            );
        }
        let stdout = Capture::default();
        let mut stream = create_with(
            Format::Csv,
            Box::new(stdout),
            Box::new(Broken),
            Limits::default(),
        );
        // Later writes may already fail once the thread has seen the broken stderr.
        let _ = stream.diagnostic("connected in 5 ms");
        let _ = stream.finish(&summary);
        let _ = stream.flush();
        let error = stream.wait_delivered(None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    // R12: in text and json, a stderr that is open but never read (here: a write that blocks
    // until the test ends) costs at most the diagnostics' own wait. The response on stdout is
    // delivered whole and on time, even with no stall limit (`-t 0`).
    #[test]
    fn an_unread_stderr_cannot_hold_up_the_response() {
        struct Unread(std::sync::Mutex<std::sync::mpsc::Receiver<()>>);
        impl Write for Unread {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                let _ = self.0.lock().unwrap().recv();
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let summary = Summary {
            exit: 0,
            status: "ok: 1 batch, 3 ms",
            error: None,
            elapsed_ms: 3,
        };
        for format in [Format::Text, Format::Json] {
            let (release, blocked) = std::sync::mpsc::channel::<()>();
            let stdout = Capture::default();
            let mut stream = create_with(
                format,
                Box::new(stdout.clone()),
                Box::new(Unread(std::sync::Mutex::new(blocked))),
                Limits {
                    stall_secs: 0,
                    ..Limits::default()
                },
            );
            let started = std::time::Instant::now();
            stream.diagnostic("connected in 5 ms").unwrap();
            stream.finish(&summary).unwrap();
            stream.flush().unwrap();
            stream.wait_delivered(None).unwrap();
            let waited = started.elapsed();
            assert!(waited < Duration::from_secs(2), "{format:?}: {waited:?}");
            let written = String::from_utf8(stdout.0.lock().unwrap().clone()).unwrap();
            assert!(
                written.contains("ok: 1 batch, 3 ms"),
                "{format:?}: {written}"
            );
            drop(stream);
            drop(release);
        }
    }

    #[test]
    fn batch_locations() {
        let mut batch = BatchInfo {
            number: 2,
            total: 3,
            source: "b.sql",
            multi_source: false,
            line: 14,
            line_map: &[],
            run: 1,
            runs: 1,
            started_ms: 0,
        };
        assert_eq!(batch.location(), "line 14");
        batch.multi_source = true;
        batch.run = 2;
        batch.runs = 3;
        assert_eq!(batch.location(), "b.sql:14, run 2 of 3");
    }
}
