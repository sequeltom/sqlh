//! Output formats (DESIGN.md §5): text (default), json and csv. Each is an `Output`. The
//! shared `Tally` applies the limits and keeps the counts, so every format agrees on what
//! was shown, cut or hidden.
//!
//! `--max-output` caps every byte a run writes, on both streams (DESIGN.md §4.4). The body
//! (rows, messages, errors, ...) is admitted piece by piece up to `OUTPUT_RESERVE` short of
//! the cap and is withheld whole past it. Everything else is text of unbounded length that
//! must still be written: notes, a guard refusal, the closing error, `--verbose` lines.
//! `Layout` shortens such text to what is left, ending it with `…[+N]`, and keeps the
//! status line (JSON: the outcome fields) out of reach of all of it.

pub mod csv;
pub mod delivery;
pub mod json;
pub mod text;

use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::time::Duration;

use crate::db::{BatchSink, ServerMessage};

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
pub const STATUS_RESERVE: u64 = 512;

/// The smallest `--max-output` other than 0: room for some output plus the reserve.
pub const MIN_MAX_OUTPUT: u64 = 4096;

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
    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()>;
    fn end_batch(&mut self) -> io::Result<()>;
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
    /// also after a failure mid-result, and must leave well-formed output. The status is
    /// never shortened; the error and notes are, to fit the budget.
    fn finish(&mut self, summary: &Summary) -> io::Result<()>;
    fn tally(&self) -> &Tally;
    fn flush(&mut self) -> io::Result<()>;
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
    /// The output budget ran out: nothing more of the body is written.
    exhausted: bool,
    /// The budget ran out while the current result set was being shown.
    cut: bool,
    withheld: Withheld,
    /// Consecutive row counts not written yet, as (statements, rows): they become one line.
    affected: Option<(u64, u64)>,
    rows_total: u64,
    rows_shown: u64,
    cells_cut: u64,
    messages_seen: usize,
    errors: u32,
    batch_max_severity: i32,
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
            exhausted: false,
            cut: false,
            withheld: Withheld::default(),
            affected: None,
            rows_total: 0,
            rows_shown: 0,
            cells_cut: 0,
            messages_seen: 0,
            errors: 0,
            batch_max_severity: 0,
        }
    }

    /// Whether `bytes` more of the body fit in the output budget, given the `used` bytes
    /// written so far (both streams, and anything promised to the end). The budget keeps
    /// `OUTPUT_RESERVE` for the end of the run. Once something doesn't fit, nothing more
    /// does, so output never resumes after a gap.
    pub fn admit(&mut self, used: u64, bytes: usize) -> bool {
        if self.exhausted {
            return false;
        }
        let max = self.limits.max_output;
        if max == 0 || used + bytes as u64 <= max.saturating_sub(OUTPUT_RESERVE) {
            return true;
        }
        self.exhausted = true;
        false
    }

    /// Bytes a note, a guard refusal or a diagnostic may take, given the `used` bytes
    /// written so far: what the budget has left, less `STATUS_RESERVE` (`usize::MAX`: no
    /// limit).
    pub fn note_room(&self, used: u64) -> usize {
        self.room(used, STATUS_RESERVE)
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
        self.source.clear();
        self.source.push_str(batch.source);
        self.multi_source = batch.multi_source;
        self.batch_line = batch.line;
        self.line_map.clear();
        self.line_map.extend_from_slice(batch.line_map);
        self.batch_max_severity = 0;
    }

    /// Starts a result set. Returns its number, or `None` when `--max-results` hides it.
    pub fn start_result(&mut self) -> Option<usize> {
        self.results_seen += 1;
        self.rows_total = 0;
        self.rows_shown = 0;
        self.cells_cut = 0;
        self.cut = false;
        let max = self.limits.max_results;
        self.hidden = max > 0 && self.results_seen > max;
        (!self.hidden).then_some(self.results_seen)
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

    pub fn rows_total(&self) -> u64 {
        self.rows_total
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

    pub fn error(&mut self, severity: i32) {
        self.errors += 1;
        self.batch_max_severity = self.batch_max_severity.max(severity);
    }

    pub fn errors(&self) -> u32 {
        self.errors
    }

    /// Highest error severity in the current batch (0 if none).
    pub fn batch_max_severity(&self) -> i32 {
        self.batch_max_severity
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
        let mut footer = if self.rows_shown == self.rows_total {
            let s = if self.rows_total == 1 { "" } else { "s" };
            format!("{} row{s}", self.rows_total)
        } else if self.cut {
            format!(
                "{} of {} rows shown (--max-output)",
                self.rows_shown, self.rows_total
            )
        } else {
            format!(
                "{} of {} rows shown (--max-rows {})",
                self.rows_shown, self.rows_total, self.limits.max_rows
            )
        };
        if self.cells_cut > 0 {
            let s = if self.cells_cut == 1 { "" } else { "s" };
            let _ = write!(
                footer,
                "; {} value{s} cut (--max-cell {})",
                self.cells_cut, self.limits.max_cell
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
                "{} more result sets not shown (--max-results {max_results})",
                self.results_seen - max_results
            ));
        }
        let max_messages = self.limits.max_messages;
        if max_messages > 0 && self.messages_seen > max_messages {
            notes.push(format!(
                "{} more messages not shown",
                self.messages_seen - max_messages
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
            ]
            .iter()
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, one, many)| format!("{n} {}", if *n == 1 { one } else { many }))
            .collect();
            let mut note = format!(
                "output stopped at the --max-output limit of {} bytes; the run continued",
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

    // R08: the reserve is sized for the widest footer plus the longest status line main.rs
    // builds (the JSON equivalent is tested in json.rs). If a status line grows, this is the
    // test that says the reserve must too.
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
        let footer = format!("-- {}\n", tally.footer());
        let status = format!(
            "-- failed (exit 1): {} errors, first in batch {}; {} later batches not run \
             (--continue-on-error runs them), {} ms\n",
            u32::MAX,
            usize::MAX,
            usize::MAX,
            u128::MAX
        );
        let worst = footer.len() + status.len();
        assert!(worst <= STATUS_RESERVE as usize, "{worst} bytes");
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
        };
        assert_eq!(batch.location(), "line 14");
        batch.multi_source = true;
        batch.run = 2;
        batch.runs = 3;
        assert_eq!(batch.location(), "b.sql:14, run 2 of 3");
    }
}
