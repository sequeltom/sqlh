//! `--format json` (DESIGN.md §5.2): one JSON document, streamed. It starts with the contract
//! version and, once connected, a description of the connection and how long that took; each
//! batch has an ordered `events` array (result, msg, rows_affected, error) and its duration;
//! the run's outcome comes last because it is only known at the end. `finish` closes whatever
//! is open, so a run that fails mid-result still yields valid JSON.
//!
//! The output budget (`--max-output`) covers the whole document, which the sink counts
//! piece by piece: the head with the connection, and events, are admitted or taken back
//! whole; a note is shortened when it is recorded and its bytes count from then on, though
//! it is written last; the closers of the result and batch still open and the outcome fields
//! come out of the reserve.

use std::fmt::Write as _;
use std::io::{self, Write};

use super::text::write_diagnostic;
use super::{
    BatchInfo, Layout, Limits, NAME_CHARS, OutOfOrder, Output, Summary, Tally, cut, cut_marker,
    hex, push_blocked, push_within,
};
use crate::db::{BatchSink, Column, Connection, ServerMessage, Stop, Value};

/// The version of the JSON document (DESIGN.md §5.2): `major.minor`, the document's first
/// field, and what `sqlh --version` reports. The major changes only for an incompatible
/// change, such as a field or event removed or renamed, or its type or meaning changed. A
/// minor may add optional fields, event types or enum values, so a reader written for a
/// version has to ignore what it doesn't know. 1.0 is the document with `contract_version`,
/// `connection`, `connect_ms` and the batches' `elapsed_ms`. 1.1 added `row_count_exact` to
/// every result and `counting_stopped` to every batch (the bounded count of hidden rows,
/// PLAN.md 2.15) but was never released as a stable contract. 2.0 is the major bump it
/// needed: a result's `row_count` may now be a lower bound, which a reader of 1.x would take
/// for the exact total, so that reader must reject the document at its major version (code
/// review R16). 2.0 keeps 1.1's fields.
pub const CONTRACT_VERSION: &str = "2.0";

/// Integers beyond ±2^53 lose precision as JSON numbers in most parsers, so they become strings.
const MAX_SAFE_INTEGER: i64 = 1 << 53;

/// What the `notes` array costs once, however many notes it holds: `,"notes":[` and `]`.
const NOTES_ARRAY: usize = ",\"notes\":[]".len();

pub struct JsonSink<W: Write> {
    out: W,
    /// stderr, for `Output::diagnostic` lines only.
    meta: Box<dyn Write>,
    tally: Tally,
    buf: String,
    /// Bytes already written from `buf`, for the output budget.
    drained: u64,
    /// Bytes of diagnostics written to `meta`.
    meta_written: u64,
    opened: bool,
    batches: usize,
    in_batch: bool,
    /// The run clock when the open batch started, for its `elapsed_ms`; none for a batch
    /// opened without `begin_batch`, which has no duration to report.
    batch_started: Option<u128>,
    events: usize,
    in_result: bool,
    /// The notes, already shortened to fit and written as JSON string content (escaped,
    /// without the quotes). `notes_bytes` is what they will take in the document.
    notes: Vec<String>,
    notes_bytes: u64,
    /// A guard refusal as (location, reason, advice); it becomes the document's `error`.
    blocked: Option<(String, String, String)>,
}

/// The buffer and document state before an event, so that an event the output budget
/// refuses can be taken back whole, including a batch it opened and its comma.
struct Mark {
    len: usize,
    opened: bool,
    batches: usize,
    in_batch: bool,
    events: usize,
}

impl<W: Write> JsonSink<W> {
    /// A sink whose diagnostics go nowhere, for tests that look at the document only.
    #[cfg(test)]
    pub fn new(out: W, limits: Limits) -> Self {
        Self::with_meta(out, Box::new(io::sink()), limits)
    }

    pub fn with_meta(out: W, meta: Box<dyn Write>, limits: Limits) -> Self {
        JsonSink {
            out,
            meta,
            tally: Tally::new(limits),
            buf: String::with_capacity(1024),
            drained: 0,
            meta_written: 0,
            opened: false,
            batches: 0,
            in_batch: false,
            batch_started: None,
            events: 0,
            in_result: false,
            notes: Vec::new(),
            notes_bytes: 0,
            blocked: None,
        }
    }

    fn mark(&self) -> Mark {
        Mark {
            len: self.buf.len(),
            opened: self.opened,
            batches: self.batches,
            in_batch: self.in_batch,
            events: self.events,
        }
    }

    /// Bytes counted against the output budget with `pending` bytes of `buf` not yet
    /// accepted: what was written to either stream, plus what the notes will take.
    fn spent(&self, pending: usize) -> u64 {
        self.drained + pending as u64 + self.meta_written + self.notes_bytes
    }

    /// Whether what was appended since `mark` fits the output budget; if not, takes it back.
    fn admit_since(&mut self, mark: Mark) -> bool {
        let used = self.spent(mark.len);
        if self.tally.admit(used, self.buf.len() - mark.len) {
            return true;
        }
        self.buf.truncate(mark.len);
        self.opened = mark.opened;
        self.batches = mark.batches;
        self.in_batch = mark.in_batch;
        self.events = mark.events;
        false
    }

    /// Writes the pending run of row counts as one event, before any other event.
    fn flush_affected(&mut self) -> io::Result<()> {
        let Some((statements, rows)) = self.tally.take_affected() else {
            return Ok(());
        };
        let mark = self.mark();
        self.start_event();
        let _ = write!(self.buf, "{{\"type\":\"rows_affected\",\"count\":{rows}");
        if statements > 1 {
            let _ = write!(self.buf, ",\"statements\":{statements}");
        }
        self.buf.push('}');
        if !self.admit_since(mark) {
            self.tally.withhold_row_counts(statements);
        }
        self.drain(false)
    }

    /// Writes the document head once, if nothing has: `{"contract_version":"2.0","batches":[`.
    fn open(&mut self) {
        if !self.opened {
            self.push_head(None);
        }
    }

    /// The document head, with the description of the connection and the time it took in
    /// front of the batches when they are known.
    fn push_head(&mut self, connection: Option<(&Connection, u128)>) {
        self.buf.push_str("{\"contract_version\":\"");
        self.buf.push_str(CONTRACT_VERSION);
        self.buf.push('"');
        if let Some((connection, connect_ms)) = connection {
            self.buf.push_str(",\"connection\":");
            push_connection(&mut self.buf, connection);
            let _ = write!(self.buf, ",\"connect_ms\":{connect_ms}");
        }
        self.buf.push_str(",\"batches\":[");
        self.opened = true;
    }

    /// Starts an event in the current batch, opening an anonymous batch if none is open.
    fn start_event(&mut self) {
        self.open();
        if !self.in_batch {
            if self.batches > 0 {
                self.buf.push(',');
            }
            self.buf.push_str("{\"events\":[");
            self.batches += 1;
            self.in_batch = true;
            self.events = 0;
        }
        if self.events > 0 {
            self.buf.push(',');
        }
        self.events += 1;
    }

    fn end_result(&mut self, interrupted: bool) {
        if !self.in_result {
            return;
        }
        // A result a failure cut short has only the rows that arrived, so its count is not the
        // total either, whether or not a count was stopped. `truncated` is the raw question:
        // did rows arrive that were not shown? The bound of a stopped count can't answer it.
        let _ = write!(
            self.buf,
            "],\"row_count\":{},\"row_count_exact\":{},\"truncated\":{},\"cells_cut\":{}",
            self.tally.rows_total(),
            !interrupted && self.tally.rows_exact(),
            self.tally.rows_hidden(),
            self.tally.cells_cut()
        );
        if interrupted {
            self.buf.push_str(",\"interrupted\":true");
        }
        self.buf.push('}');
        self.in_result = false;
    }

    /// Closes the open result, which was cut short, and the open batch, which ended at
    /// `ended_ms` on the run clock.
    fn close_batch(&mut self, ended_ms: u128) {
        self.end_result(true);
        if self.in_batch {
            self.buf.push(']');
            if let Some(started_ms) = self.batch_started.take() {
                let _ = write!(
                    self.buf,
                    ",\"elapsed_ms\":{}",
                    ended_ms.saturating_sub(started_ms)
                );
            }
            let _ = write!(
                self.buf,
                ",\"counting_stopped\":{}}}",
                self.tally.batch_stopped()
            );
            self.in_batch = false;
        }
    }

    /// Writes buffered JSON once it grows, to keep memory flat on big results.
    fn drain(&mut self, force: bool) -> io::Result<()> {
        if force || self.buf.len() >= 32 * 1024 {
            self.out.write_all(self.buf.as_bytes())?;
            self.drained += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }

    /// Appends one value. Returns true when `--max-cell` cut it.
    fn push_value(&mut self, value: &Value) -> bool {
        let max = self.tally.limits().max_cell;
        match value {
            Value::Null => self.buf.push_str("null"),
            Value::Int(v) if v.unsigned_abs() <= MAX_SAFE_INTEGER as u64 => {
                let _ = write!(self.buf, "{v}");
            }
            Value::Int(v) => {
                let _ = write!(self.buf, "\"{v}\"");
            }
            Value::Num(s) => self.buf.push_str(s),
            Value::Lit(s) => push_str(&mut self.buf, s),
            Value::Text(s) => {
                let (kept, cut_count) = cut(s, max);
                self.buf.push('"');
                push_escaped(&mut self.buf, kept);
                if cut_count > 0 {
                    self.buf.push_str(&cut_marker(cut_count, false));
                }
                self.buf.push('"');
                return cut_count > 0;
            }
            Value::Bin(bytes) => {
                let (text, cut_count) = hex(bytes, max);
                self.buf.push('"');
                self.buf.push_str(&text);
                if cut_count > 0 {
                    self.buf.push_str(&cut_marker(cut_count, true));
                }
                self.buf.push('"');
                return cut_count > 0;
            }
            // Cut by the read, to the sink's own limit: the marker is all that's left to add.
            Value::TextCut(s, more) => {
                self.buf.push('"');
                push_escaped(&mut self.buf, s);
                self.buf.push_str(&cut_marker(*more, false));
                self.buf.push('"');
                return true;
            }
            Value::BinCut(bytes, more) => {
                self.buf.push('"');
                self.buf.push_str(&hex(bytes, 0).0);
                self.buf.push_str(&cut_marker(*more, true));
                self.buf.push('"');
                return true;
            }
        }
        false
    }
}

/// Appends `s` as a JSON string literal.
fn push_str(out: &mut String, s: &str) {
    out.push('"');
    push_escaped(out, s);
    out.push('"');
}

/// Appends the JSON-escaped content of `s`, without quotes.
fn push_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        push_char(out, c);
    }
}

/// Appends one character as it is written inside a JSON string.
fn push_char(out: &mut String, c: char) {
    match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c if (c as u32) < 0x20 => {
            let _ = write!(out, "\\u{:04x}", c as u32);
        }
        c => out.push(c),
    }
}

/// Appends a name from the server as a JSON string, cut after `NAME_CHARS` characters (the
/// cut ends in `…[+N]`, like a cut value).
fn push_name(out: &mut String, name: &str) {
    let (kept, cut_count) = cut(name, NAME_CHARS);
    out.push('"');
    push_escaped(out, kept);
    if cut_count > 0 {
        out.push_str(&cut_marker(cut_count, false));
    }
    out.push('"');
}

/// `push_name`, or `null` for a name the server didn't give.
fn push_name_or_null(out: &mut String, name: Option<&str>) {
    match name {
        Some(name) => push_name(out, name),
        None => out.push_str("null"),
    }
}

/// The `connection` object (DESIGN.md §5.2). Every field that comes from the server goes
/// through `push_name`; `auth` is one of two fixed words, and there is no login name here, no
/// connection string, nothing from a credential.
fn push_connection(out: &mut String, connection: &Connection) {
    out.push_str("{\"server\":");
    push_name_or_null(out, connection.server.as_deref());
    out.push_str(",\"database\":");
    push_name(out, &connection.database);
    out.push_str(",\"auth\":");
    push_str(out, connection.auth);
    let _ = write!(
        out,
        ",\"encrypted\":{},\"server_version\":",
        connection.encrypted
    );
    push_name_or_null(out, connection.version.as_deref());
    out.push('}');
}

/// The document's last fields: `,"ok":...,"elapsed_ms":N}` and a newline. The status is
/// never shortened.
fn outcome(summary: &Summary) -> String {
    let mut out = format!(
        ",\"ok\":{},\"exit_code\":{},\"summary\":",
        summary.exit == 0,
        summary.exit
    );
    push_str(&mut out, summary.status);
    let _ = writeln!(out, ",\"elapsed_ms\":{}}}", summary.elapsed_ms);
    out
}

impl<W: Write> BatchSink for JsonSink<W> {
    fn columns(&mut self, columns: &[Column]) -> io::Result<()> {
        self.flush_affected()?;
        if self.tally.start_result().is_none() {
            return Ok(());
        }
        let mark = self.mark();
        self.start_event();
        self.buf.push_str("{\"type\":\"result\",\"columns\":[");
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                self.buf.push(',');
            }
            self.buf.push_str("{\"name\":");
            push_str(&mut self.buf, &column.name);
            self.buf.push_str(",\"type\":");
            push_str(&mut self.buf, &column.type_name);
            let _ = write!(self.buf, ",\"nullable\":{}}}", column.nullable);
        }
        self.buf.push_str("],\"rows\":[");
        if self.admit_since(mark) {
            self.in_result = true;
        } else {
            self.tally.withhold_result();
        }
        self.drain(false)
    }

    fn wants_values(&self) -> bool {
        self.tally.wants_values()
    }

    fn cell_limit(&self) -> usize {
        self.tally.cell_limit()
    }

    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        let mark = self.mark();
        if self.tally.rows_shown() > 0 {
            self.buf.push(',');
        }
        self.buf.push('[');
        let mut cells_cut = 0;
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                self.buf.push(',');
            }
            if self.push_value(value) {
                cells_cut += 1;
            }
        }
        self.buf.push(']');
        if self.admit_since(mark) {
            self.tally.row_shown();
            for _ in 0..cells_cut {
                self.tally.cell_cut();
            }
        } else {
            self.tally.row_withheld();
        }
        self.drain(false)
    }

    fn skipped_row(&mut self) {
        self.tally.row_skipped();
    }

    fn counting_stopped(&mut self, by: Stop) {
        self.tally.stop_counting(by);
    }

    fn end_rows(&mut self) -> io::Result<()> {
        self.tally.end_rows();
        self.end_result(false);
        self.drain(false)
    }

    fn end_rows_incomplete(&mut self) -> io::Result<()> {
        self.tally.end_rows();
        self.end_result(true);
        self.drain(false)
    }

    fn rows_affected(&mut self, count: u64) -> io::Result<()> {
        self.tally.outside_rows("a row count")?;
        self.tally.add_affected(count);
        Ok(())
    }

    fn message(&mut self, message: &ServerMessage) -> io::Result<()> {
        self.tally.outside_rows("a message")?;
        self.flush_affected()?;
        if !self.tally.message_visible() {
            return Ok(());
        }
        let mark = self.mark();
        self.start_event();
        self.buf.push_str("{\"type\":\"msg\",\"text\":");
        push_str(&mut self.buf, &message.text);
        self.buf.push('}');
        if !self.admit_since(mark) {
            self.tally.withhold_message();
        }
        self.drain(false)
    }

    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        self.tally.outside_rows("an error")?;
        self.flush_affected()?;
        self.tally.error(error.severity);
        let mark = self.mark();
        self.start_event();
        let _ = write!(
            self.buf,
            "{{\"type\":\"error\",\"number\":{},\"severity\":{},\"state\":{}",
            error.number, error.severity, error.state
        );
        match &error.procedure {
            Some(procedure) => {
                self.buf.push_str(",\"procedure\":");
                push_str(&mut self.buf, procedure);
                match error.line {
                    Some(line) => {
                        let _ = write!(self.buf, ",\"line\":{line}");
                    }
                    None => self.buf.push_str(",\"line\":null"),
                }
            }
            None => {
                match self.tally.source_line(error) {
                    Some(line) => {
                        let _ = write!(self.buf, ",\"line\":{line}");
                    }
                    None => self.buf.push_str(",\"line\":null"),
                }
                if let Some(source) = self.tally.source_name() {
                    let source = source.to_owned();
                    self.buf.push_str(",\"source\":");
                    push_str(&mut self.buf, &source);
                }
            }
        }
        self.buf.push_str(",\"text\":");
        push_str(&mut self.buf, &error.text);
        self.buf.push('}');
        if !self.admit_since(mark) {
            self.tally.withhold_error();
        }
        self.drain(false)
    }
}

impl<W: Write> Output for JsonSink<W> {
    /// Writes the head of the document with the connection in it, as a piece of the body: the
    /// budget can refuse it (when notes and diagnostics have used it up), and it is then
    /// counted as withheld, and the head comes later without it. It has to come before
    /// anything else in the document; a call after that is refused.
    fn connected(&mut self, connection: &Connection, connect_ms: u128) -> io::Result<()> {
        if self.opened {
            return Err(OutOfOrder::error(
                "the connection was described after the document had started; it comes before \
                 the first batch (Output order, DESIGN.md §5.2)"
                    .into(),
            ));
        }
        let mark = self.mark();
        self.push_head(Some((connection, connect_ms)));
        if !self.admit_since(mark) {
            self.tally.withhold_connection();
        }
        self.drain(false)
    }

    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()> {
        self.flush_affected()?;
        // The previous batch's closers are not up for admission (they read its tally, which
        // `begin_batch` resets). The document head is: it comes back out with the batch if the
        // budget refuses both.
        self.close_batch(batch.started_ms);
        self.tally.begin_batch(batch);
        let mark = self.mark();
        self.open();
        if self.batches > 0 {
            self.buf.push(',');
        }
        let _ = write!(self.buf, "{{\"batch\":{},\"source\":", batch.number);
        push_str(&mut self.buf, batch.source);
        let _ = write!(self.buf, ",\"line\":{}", batch.line);
        if batch.runs > 1 {
            let _ = write!(self.buf, ",\"run\":{},\"runs\":{}", batch.run, batch.runs);
        }
        self.buf.push_str(",\"events\":[");
        self.batches += 1;
        self.in_batch = true;
        self.events = 0;
        if self.admit_since(mark) {
            self.batch_started = Some(batch.started_ms);
        } else {
            self.tally.withhold_batch();
        }
        self.drain(false)
    }

    fn end_batch(&mut self, ended_ms: u128) -> io::Result<()> {
        self.flush_affected()?;
        self.close_batch(ended_ms);
        self.drain(false)
    }

    /// Shortened to fit what the budget has left before the status reserve, and counted at
    /// once; the document gets it at the end.
    fn note(&mut self, text: &str) -> io::Result<()> {
        self.flush_affected()?;
        // The array costs `,"notes":[` and `]` once, plus quotes and a comma for each note.
        let overhead = if self.notes.is_empty() {
            NOTES_ARRAY + 2
        } else {
            3
        };
        let mut encoded = String::new();
        let fitted = self
            .tally
            .note_room(self.spent(self.buf.len()))
            .checked_sub(overhead)
            .and_then(|room| push_within(&mut encoded, text, room, push_char));
        if fitted.is_some() {
            self.notes_bytes += (overhead + encoded.len()) as u64;
            self.notes.push(encoded);
        } else {
            self.tally.withhold_note();
        }
        Ok(())
    }

    /// Kept for the end, where it becomes `error` and is shortened to the room that is left.
    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()> {
        self.flush_affected()?;
        self.blocked = Some((location.to_owned(), reason.to_owned(), advice.to_owned()));
        Ok(())
    }

    fn diagnostic(&mut self, text: &str) -> io::Result<()> {
        let used = self.spent(self.buf.len());
        self.meta_written += write_diagnostic(&mut self.meta, &mut self.tally, used, text)?;
        Ok(())
    }

    fn finish(&mut self, summary: &Summary) -> io::Result<()> {
        self.flush_affected()?;
        self.open();
        // A batch still open ended with the run.
        self.close_batch(summary.elapsed_ms);
        self.buf.push(']');
        let limit_notes = self.tally.limit_notes();
        let notes = std::mem::take(&mut self.notes);
        let blocked = self.blocked.take();
        let closing = outcome(summary);
        // The notes already recorded, the limit notes and the error, then the outcome. Only
        // the limit notes and the error can still be shortened: the recorded notes were
        // fitted when they arrived, and the outcome never is.
        let mut layout = Layout::new();
        if !notes.is_empty() || !limit_notes.is_empty() {
            layout.text(",\"notes\":[");
            let mut first = true;
            for note in &notes {
                layout
                    .text(if first { "\"" } else { ",\"" })
                    .text(note)
                    .text("\"");
                first = false;
            }
            for note in &limit_notes {
                layout
                    .text(if first { "\"" } else { ",\"" })
                    .part(note)
                    .text("\"");
                first = false;
            }
            layout.text("]");
        }
        match (summary.error, &blocked) {
            (Some(error), _) => {
                layout.text(",\"error\":\"").part(error).text("\"");
            }
            (None, Some((location, reason, advice))) => {
                layout.text(",\"error\":\"");
                push_blocked(&mut layout, location, reason, advice);
                layout.text("\"");
            }
            (None, None) => {}
        }
        layout.text(&closing);
        // The recorded notes are in the layout now, so they are not counted twice.
        let room = self
            .tally
            .closing_room(self.drained + self.buf.len() as u64 + self.meta_written);
        // No room for even a cut error or note (the reserve makes this unreachable): the
        // outcome alone.
        let tail = layout.fit(room, push_char).unwrap_or(closing);
        self.buf.push_str(&tail);
        self.drain(true)
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain(true)?;
        self.out.flush()?;
        self.meta.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::super::text::tests::{
        ADVICE, FILLING_STATUS, INTERRUPTED_STATUS, RAGGED_STATUS, Shared, batch, capped, col,
        cut_by, error, filling_run, interrupted_run, limits, mid_result_events_are_refused, msg,
        quoting, ragged_run, summary,
    };
    use super::*;
    use serde_json::{Value as Json, json};

    fn parse(sink: JsonSink<Vec<u8>>) -> Json {
        let text = String::from_utf8(sink.out).unwrap();
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
    }

    #[test]
    fn document_shape() {
        let mut s = JsonSink::new(Vec::new(), limits());
        s.begin_batch(&BatchInfo {
            started_ms: 4,
            ..batch("q.sql", false, 1)
        })
        .unwrap();
        s.message(&msg("hello \"world\"")).unwrap();
        s.columns(&[
            col("id", "bigint"),
            col("v", "nvarchar(10)"),
            col("b", "varbinary(4)"),
        ])
        .unwrap();
        s.row(&[
            Value::Int(9_007_199_254_740_993),
            Value::Text("a\nbcdefg".into()),
            Value::Bin(vec![1, 2, 3]),
        ])
        .unwrap();
        s.row(&[Value::Int(-5), Value::Null, Value::Lit("12.50".into())])
            .unwrap();
        s.skipped_row();
        s.end_rows().unwrap();
        s.rows_affected(3).unwrap();
        s.error(&error(2)).unwrap();
        s.end_batch(9).unwrap();
        s.note("ignored sqlcmd flags -W").unwrap();
        s.finish(&Summary {
            exit: 1,
            status: "failed (exit 1): 1 error, 9 ms",
            error: None,
            elapsed_ms: 9,
        })
        .unwrap();
        assert_eq!(
            parse(s),
            json!({
                "contract_version": "2.0",
                "batches": [{
                    "batch": 1, "source": "q.sql", "line": 1,
                    "events": [
                        {"type": "msg", "text": "hello \"world\""},
                        {"type": "result",
                         "columns": [
                             {"name": "id", "type": "bigint", "nullable": true},
                             {"name": "v", "type": "nvarchar(10)", "nullable": true},
                             {"name": "b", "type": "varbinary(4)", "nullable": true}],
                         "rows": [["9007199254740993", "a\nbcd…[+3]", "0x0102…[+1 bytes]"],
                                  [-5, null, "12.50"]],
                         "row_count": 3, "row_count_exact": true, "truncated": true, "cells_cut": 2},
                        {"type": "rows_affected", "count": 3},
                        {"type": "error", "number": 208, "severity": 16, "state": 1,
                         "line": 2, "text": "Invalid object name 'dbo.Nope'."}
                    ],
                    "elapsed_ms": 5, "counting_stopped": false
                }],
                "notes": ["ignored sqlcmd flags -W"],
                "ok": false, "exit_code": 1, "summary": "failed (exit 1): 1 error, 9 ms",
                "elapsed_ms": 9
            })
        );
    }

    // F06: a run of row counts is one event; `statements` appears only when it's a run.
    #[test]
    fn row_counts_collapse_into_one_event() {
        let mut s = JsonSink::new(Vec::new(), limits());
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        for rows in [1, 2, 3] {
            s.rows_affected(rows).unwrap();
        }
        s.message(&msg("done")).unwrap();
        s.rows_affected(5).unwrap();
        s.end_batch(0).unwrap();
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        assert_eq!(
            parse(s)["batches"][0]["events"],
            json!([
                {"type": "rows_affected", "count": 6, "statements": 3},
                {"type": "msg", "text": "done"},
                {"type": "rows_affected", "count": 5}
            ])
        );
    }

    // F06: past the budget the document stays valid: the cut result closes with exact
    // counts, later events and batches are left out, and a note says so.
    #[test]
    fn output_budget_keeps_the_document_valid() {
        let max_output = crate::output::OUTPUT_RESERVE + 200;
        let limits = Limits {
            max_rows: 0,
            max_cell: 0,
            max_results: 0,
            max_messages: 0,
            max_output,
            stall_secs: 0,
        };
        let mut s = JsonSink::new(Vec::new(), limits);
        s.begin_batch(&BatchInfo {
            total: 2,
            ..batch("-Q", false, 1)
        })
        .unwrap();
        s.columns(&[col("v", "varchar(9)")]).unwrap();
        for _ in 0..5 {
            if s.wants_values() {
                s.row(&[Value::Text("aaaaaaaaa".into())]).unwrap();
            } else {
                s.skipped_row();
            }
        }
        s.end_rows().unwrap();
        s.message(&msg("later")).unwrap();
        s.error(&error(1)).unwrap();
        s.end_batch(0).unwrap();
        s.begin_batch(&BatchInfo {
            number: 2,
            total: 2,
            ..batch("-Q", false, 3)
        })
        .unwrap();
        s.rows_affected(1).unwrap();
        s.end_batch(0).unwrap();
        s.finish(&summary("failed (exit 1): 1 error, 3 ms"))
            .unwrap();
        let length = s.out.len() as u64;
        let doc = parse(s);
        assert!(length <= max_output, "{length} bytes");
        let batches = doc["batches"].as_array().unwrap();
        assert_eq!(batches.len(), 1, "{doc}");
        let events = batches[0]["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "only the result: {doc}");
        let shown = events[0]["rows"].as_array().unwrap().len();
        assert!((1..5).contains(&shown), "{doc}");
        assert_eq!(events[0]["row_count"], 5);
        assert_eq!(events[0]["truncated"], true);
        let note = doc["notes"][0].as_str().unwrap();
        assert!(
            note.contains("--max-output limit")
                && note.contains("1 message, 1 error, 1 statement's row count, 1 batch"),
            "{note}"
        );
        assert_eq!(doc["exit_code"], 0);
    }

    #[test]
    fn failure_mid_result_is_still_valid_json() {
        let mut s = JsonSink::new(Vec::new(), limits());
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.finish(&Summary {
            exit: 4,
            status: "failed (exit 4): timeout, 2000 ms",
            error: Some("query timeout after 2 s in batch 1"),
            elapsed_ms: 2000,
        })
        .unwrap();
        let doc = parse(s);
        assert_eq!(doc["batches"][0]["events"][0]["interrupted"], json!(true));
        assert_eq!(
            doc["batches"][0]["events"][0]["row_count_exact"],
            json!(false)
        );
        assert_eq!(doc["error"], json!("query timeout after 2 s in batch 1"));
        assert_eq!(doc["exit_code"], json!(4));
    }

    #[test]
    fn early_failure_and_block() {
        let mut s = JsonSink::new(Vec::new(), limits());
        s.finish(&summary("failed (exit 3): usage error, 0 ms"))
            .unwrap();
        assert_eq!(parse(s)["batches"], json!([]));

        let mut s = JsonSink::new(Vec::new(), limits());
        s.blocked(
            "line 2",
            "INSERT ##t would write to a global temp table",
            "Use a #temp table",
        )
        .unwrap();
        s.finish(&summary("failed (exit 5): blocked, 0 ms"))
            .unwrap();
        let doc = parse(s);
        assert_eq!(
            doc["error"],
            "line 2: INSERT ##t would write to a global temp table; nothing was run. Use a #temp table"
        );
    }

    #[test]
    fn errors_in_procedures_and_other_sources() {
        let mut s = JsonSink::new(Vec::new(), limits());
        s.begin_batch(&batch("b.sql", true, 10)).unwrap();
        s.error(&error(3)).unwrap();
        s.error(&ServerMessage {
            procedure: Some("dbo.p".into()),
            ..error(7)
        })
        .unwrap();
        s.finish(&summary("x")).unwrap();
        let events = parse(s)["batches"][0]["events"].clone();
        assert_eq!(events[0]["line"], json!(12));
        assert_eq!(events[0]["source"], json!("b.sql"));
        assert_eq!(events[1]["procedure"], json!("dbo.p"));
        assert_eq!(events[1]["line"], json!(7));
    }

    // PLAN.md 2.15, exact output: a stopped count says so in the result (`row_count` is the
    // floored lower bound and `row_count_exact` false) and in its batch (`counting_stopped`);
    // the batch after it is exact. The note and the status say it in words.
    #[test]
    fn a_stopped_count_is_a_lower_bound_in_the_result_batch_note_and_status() {
        use super::super::text::tests::{stopped_limits, stopped_run};
        let (mut s, main, _) = sink(stopped_limits());
        stopped_run(&mut s);
        assert_eq!(
            main.text(),
            concat!(
                r#"{"contract_version":"2.0","batches":["#,
                r#"{"batch":1,"source":"-Q","line":1,"events":["#,
                r#"{"type":"rows_affected","count":2},"#,
                r#"{"type":"result","columns":[{"name":"id","type":"int","nullable":true},"#,
                r#"{"name":"v","type":"varchar(20)","nullable":true}],"#,
                r#""rows":[[0,"v0"],[1,"v1"],[2,"v2"]],"row_count":1000,"#,
                r#""row_count_exact":false,"truncated":true,"cells_cut":0}],"#,
                r#""elapsed_ms":5,"counting_stopped":true},"#,
                r#"{"batch":2,"source":"-Q","line":6,"events":["#,
                r#"{"type":"result","columns":[{"name":"next","type":"int","nullable":true}],"#,
                r#""rows":[[1]],"row_count":1,"row_count_exact":true,"truncated":false,"#,
                r#""cells_cut":0}],"elapsed_ms":1,"counting_stopped":false}],"#,
                r#""notes":["counting stopped in batch 1: the rest of that batch was cancelled, "#,
                r#"so its later statements did not run; --exact-count counts every row"],"#,
                r#""ok":true,"exit_code":0,"summary":"ok: 2 batches, counting stopped in 1, 7 ms","#,
                r#""elapsed_ms":7}"#,
                "\n"
            )
        );
    }

    // R16: a stopped count's `row_count` is a lower bound that a reader of the 1.x contract
    // would take for the total (the document has `ok: true`, no `interrupted`, and 1.0 never
    // had the fields that qualify it). The major version is what stops it: a reader that
    // follows DESIGN.md §5.2 checks `contract_version` first and rejects what it doesn't know.
    #[test]
    fn a_reader_of_the_1_x_contract_rejects_a_stopped_count_document_by_its_major_version() {
        use super::super::text::tests::{stopped_limits, stopped_run};
        // The check DESIGN.md §5.2 prescribes: accept the supported major, reject any other.
        fn reads(supported_major: u64, doc: &head_reader::Doc) -> Result<(), String> {
            if doc.major != supported_major {
                return Err(format!(
                    "unsupported contract_version {} (this reader handles {supported_major}.x)",
                    doc.version
                ));
            }
            Ok(())
        }
        let (mut s, main, _) = sink(stopped_limits());
        stopped_run(&mut s);
        let text = main.text();
        let doc = head_reader::parse(&text);
        // The document is the one a 1.x reader would misread: successful, no `interrupted`, and
        // a first result whose `row_count` is not the total.
        let parsed = document(&main);
        let result = &parsed["batches"][0]["events"][1];
        assert_eq!(parsed["ok"], true);
        assert!(result.get("interrupted").is_none(), "{result}");
        assert_eq!(result["row_count_exact"], false);
        assert_eq!(parsed["contract_version"], CONTRACT_VERSION);
        assert_eq!(
            reads(1, &doc),
            Err(format!(
                "unsupported contract_version {CONTRACT_VERSION} (this reader handles 1.x)"
            ))
        );
        assert_eq!(reads(doc.major, &doc), Ok(()));
        // The bump is a major one: it can't be 1.x again while a stopped count is a bound.
        assert!(doc.major >= 2, "{}", doc.version);
    }

    /// Just enough of a reader to apply the major-version check to a document's head.
    mod head_reader {
        pub struct Doc {
            pub version: String,
            pub major: u64,
        }

        pub fn parse(text: &str) -> Doc {
            let head = text.strip_prefix("{\"contract_version\":\"").expect("head");
            let version = head.split('"').next().expect("version").to_owned();
            let major = version
                .split('.')
                .next()
                .expect("major")
                .parse()
                .expect("n");
            Doc { version, major }
        }
    }

    // PLAN.md 2.15 (review), exact output: a stopped count is `truncated` whenever a hidden row
    // was counted, even when flooring to one digit leaves the bound no higher than the rows
    // shown: eleven rows counted with ten shown are `row_count` 11 and `truncated`, not 10 and
    // not truncated. A count `-t` stopped before any hidden row arrived has none to report: the
    // rows shown are the bound, `truncated` is false, and `row_count_exact` still says that
    // more rows may exist.
    #[test]
    fn a_stopped_count_is_truncated_when_a_hidden_row_was_counted() {
        use super::super::text::tests::{short_limits, stopped_short_run};
        let (mut s, main, _) = sink(short_limits());
        stopped_short_run(&mut s);
        let result = |count: u32, truncated: bool| {
            format!(
                concat!(
                    r#"{{"type":"result","columns":[{{"name":"n","type":"int","nullable":true}}],"#,
                    r#""rows":[[0],[1],[2],[3],[4],[5],[6],[7],[8],[9]],"row_count":{},"#,
                    r#""row_count_exact":false,"truncated":{},"cells_cut":0}}"#
                ),
                count, truncated
            )
        };
        assert_eq!(
            main.text(),
            format!(
                concat!(
                    r#"{{"contract_version":"2.0","batches":["#,
                    r#"{{"batch":1,"source":"-Q","line":1,"events":[{}],"#,
                    r#""elapsed_ms":5,"counting_stopped":true}},"#,
                    r#"{{"batch":2,"source":"-Q","line":6,"events":[{}],"#,
                    r#""elapsed_ms":5,"counting_stopped":true}}],"#,
                    r#""notes":["counting stopped in batches 1, 2: the rest of each was "#,
                    r#"cancelled, so their later statements did not run; --exact-count counts "#,
                    r#"every row; raise -t to count longer"],"#,
                    r#""ok":true,"exit_code":0,"#,
                    r#""summary":"ok: 2 batches, counting stopped in 2, 7 ms","elapsed_ms":7}}"#,
                    "\n"
                ),
                result(11, true),
                result(10, false)
            )
        );
    }

    // R15 A, second round, end to end: the real `Session::run` against a fake TDS server
    // (`db::fake_server`), into this sink, whose own `--max-rows` turns `wants_values` false on
    // the row that fills it. That row's work crosses `-t` with the final DONE already there,
    // and no hidden row exists. The run is a timeout, and the document closes the result as
    // interrupted and not exact, at a quota of 1 and at the default 100.
    #[test]
    fn a_deadline_crossed_by_the_row_that_fills_the_quota_closes_the_result_interrupted() {
        use crate::db::fake_server::{Script, int_rows, run_once, serve};
        use crate::db::{Counting, DbError};
        use std::time::{Duration, Instant};

        /// The JSON sink, with 1.2 s of work after its `slow`th row.
        struct Slow {
            inner: JsonSink<Vec<u8>>,
            rows: u64,
            slow: u64,
        }
        impl BatchSink for Slow {
            fn columns(&mut self, columns: &[Column]) -> io::Result<()> {
                self.inner.columns(columns)
            }
            fn wants_values(&self) -> bool {
                self.inner.wants_values()
            }
            fn cell_limit(&self) -> usize {
                self.inner.cell_limit()
            }
            fn row(&mut self, values: &[Value]) -> io::Result<()> {
                self.inner.row(values)?;
                self.rows += 1;
                if self.rows == self.slow {
                    let until = Instant::now() + Duration::from_millis(1200);
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                }
                Ok(())
            }
            fn skipped_row(&mut self) {
                self.inner.skipped_row();
            }
            fn counting_stopped(&mut self, by: Stop) {
                self.inner.counting_stopped(by);
            }
            fn end_rows(&mut self) -> io::Result<()> {
                self.inner.end_rows()
            }
            fn end_rows_incomplete(&mut self) -> io::Result<()> {
                self.inner.end_rows_incomplete()
            }
            fn rows_affected(&mut self, count: u64) -> io::Result<()> {
                self.inner.rows_affected(count)
            }
            fn message(&mut self, message: &ServerMessage) -> io::Result<()> {
                self.inner.message(message)
            }
            fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
                self.inner.error(error)
            }
        }

        for quota in [1, 100] {
            let mut inner = JsonSink::new(
                Vec::new(),
                Limits {
                    max_rows: quota,
                    ..limits()
                },
            );
            inner.begin_batch(&batch("-Q", false, 1)).unwrap();
            let sink = Slow {
                inner,
                rows: 0,
                slow: quota,
            };
            let ran = run_once(
                serve(Script::Reply(int_rows(quota))),
                "SELECT x".into(),
                Counting::BOUNDED,
                sink,
            );
            assert!(
                matches!(ran.outcome, Err(DbError::Timeout(_))),
                "quota {quota}: {:?}",
                ran.outcome
            );
            // What the caller does with a failed batch: the closing lines, no `end_batch`.
            let mut s = ran.sink.inner;
            s.finish(&Summary {
                exit: 4,
                status: "failed (exit 4): timeout, 1200 ms",
                error: Some("query timeout after 1 s in batch 1"),
                elapsed_ms: 1200,
            })
            .unwrap();
            let doc = parse(s);
            let result = &doc["batches"][0]["events"][0];
            assert_eq!(result["row_count"], json!(quota), "{doc}");
            assert_eq!(result["row_count_exact"], json!(false), "{doc}");
            assert_eq!(result["interrupted"], json!(true), "{doc}");
            assert_eq!(doc["ok"], json!(false), "{doc}");
        }
    }

    // PLAN.md 2.15 (review): a result a failure cut short has only the rows that arrived, so
    // its count is never exact, whether or not anything was hidden. A reader that looks only at
    // `row_count_exact` is not told that a partial count is the total.
    #[test]
    fn an_interrupted_result_never_claims_an_exact_count() {
        let (mut s, main, _) = sink(Limits {
            max_rows: 2,
            ..capped(0)
        });
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.row(&[Value::Int(2)]).unwrap();
        s.skipped_row();
        s.finish(&Summary {
            exit: 4,
            status: "failed (exit 4): timeout, 2000 ms",
            error: Some("query timeout after 2 s in batch 1"),
            elapsed_ms: 2000,
        })
        .unwrap();
        assert_eq!(
            main.text(),
            concat!(
                r#"{"contract_version":"2.0","batches":[{"batch":1,"source":"-Q","line":1,"#,
                r#""events":[{"type":"result","columns":[{"name":"x","type":"int","nullable":true}],"#,
                r#""rows":[[1],[2]],"row_count":3,"row_count_exact":false,"truncated":true,"#,
                r#""cells_cut":0,"interrupted":true}],"elapsed_ms":2000,"counting_stopped":false}],"#,
                r#""error":"query timeout after 2 s in batch 1","ok":false,"exit_code":4,"#,
                r#""summary":"failed (exit 4): timeout, 2000 ms","elapsed_ms":2000}"#,
                "\n"
            )
        );
        // The same result that ended on its own is exact.
        let (mut s, main, _) = sink(Limits {
            max_rows: 2,
            ..capped(0)
        });
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.row(&[Value::Int(2)]).unwrap();
        s.skipped_row();
        s.end_rows().unwrap();
        s.end_batch(1).unwrap();
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        let result = &document(&main)["batches"][0]["events"][0];
        assert_eq!(result["row_count_exact"], true, "{result}");
        assert!(result.get("interrupted").is_none(), "{result}");
    }

    // R17: a SQL error that ends a result's rows closes it as incomplete, in the order
    // `BatchSink` promises (the result, the messages held, the error). The incomplete state
    // is the result's own: a later result in the same batch, and one in the next batch, are
    // exact again, and a result that ended on its own before the error stays exact.
    #[test]
    fn a_result_a_sql_error_ended_is_incomplete_and_the_state_does_not_leak() {
        let (mut s, main, _) = sink(capped(0));
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.end_rows().unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.row(&[Value::Int(2)]).unwrap();
        s.end_rows_incomplete().unwrap();
        s.message(&msg("held")).unwrap();
        s.error(&error(2)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(7)]).unwrap();
        s.end_rows().unwrap();
        s.end_batch(1).unwrap();
        s.begin_batch(&batch("-Q", false, 3)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(8)]).unwrap();
        s.end_rows().unwrap();
        s.end_batch(2).unwrap();
        s.finish(&summary("failed (exit 1): 1 error, 3 ms"))
            .unwrap();
        let doc = document(&main);
        let events = doc["batches"][0]["events"].as_array().unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["result", "result", "msg", "error", "result"]);
        let exact: Vec<_> = [0, 1, 4]
            .iter()
            .map(|&i| events[i]["row_count_exact"].as_bool().unwrap())
            .collect();
        assert_eq!(exact, [true, false, true], "{doc}");
        assert_eq!(events[1]["row_count"], 2);
        assert_eq!(events[1]["interrupted"], true);
        assert_eq!(events[1]["truncated"], false);
        for i in [0, 4] {
            assert!(events[i].get("interrupted").is_none(), "{doc}");
        }
        let later = &doc["batches"][1]["events"][0];
        assert_eq!(later["row_count_exact"], true, "{doc}");
        assert!(later.get("interrupted").is_none(), "{doc}");
        // Exact output of the interrupted result: the marker comes last in the object.
        assert!(
            main.text().contains(concat!(
                r#"{"type":"result","columns":[{"name":"x","type":"int","nullable":true}],"#,
                r#""rows":[[1],[2]],"row_count":2,"row_count_exact":false,"truncated":false,"#,
                r#""cells_cut":0,"interrupted":true},{"type":"msg","text":"held"}"#
            )),
            "{}",
            main.text()
        );
    }

    // The whole-response budget (R08). A json sink with a stream each for the document and
    // the diagnostics; the document is parsed, so every test also proves it is valid JSON.

    fn sink(limits: Limits) -> (JsonSink<Shared>, Shared, Shared) {
        let (main, meta) = (Shared::default(), Shared::default());
        let sink = JsonSink::with_meta(main.clone(), Box::new(meta.clone()), limits);
        (sink, main, meta)
    }

    fn document(main: &Shared) -> Json {
        let text = main.text();
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
    }

    // PLAN.md 2.11: the contract version, the description of the connection and the times.

    fn connection() -> Connection {
        Connection {
            server: Some("SQL01".into()),
            version: Some("17.0.1000".into()),
            database: "master".into(),
            auth: "sql",
            encrypted: true,
        }
    }

    /// A connection whose server and database names are `name` and `name`, both hostile.
    fn named(name: &str) -> Connection {
        Connection {
            server: Some(name.into()),
            database: name.into(),
            ..connection()
        }
    }

    // The version is `major.minor`, and it is the document's first field.
    #[test]
    fn the_contract_version_is_major_dot_minor_and_comes_first() {
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        let (major, minor) = CONTRACT_VERSION.split_once('.').expect("major.minor");
        assert!(digits(major) && digits(minor), "{CONTRACT_VERSION}");
        for connected in [false, true] {
            let (mut s, main, _) = sink(capped(0));
            if connected {
                s.connected(&connection(), 5).unwrap();
            }
            s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
            assert!(
                main.text()
                    .starts_with(&format!("{{\"contract_version\":\"{CONTRACT_VERSION}\",")),
                "{}",
                main.text()
            );
        }
    }

    // Exact output, with a connection: the description and its time come right after the
    // version, in front of the batches, and each batch ends with how long it took.
    #[test]
    fn a_document_starts_with_the_connection_and_each_batch_ends_with_its_time() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection(), 195).unwrap();
        s.begin_batch(&BatchInfo {
            started_ms: 200,
            ..batch("-Q", false, 1)
        })
        .unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.end_rows().unwrap();
        s.end_batch(212).unwrap();
        s.finish(&Summary {
            elapsed_ms: 215,
            ..summary("ok: 1 batch, 215 ms")
        })
        .unwrap();
        assert_eq!(
            main.text(),
            concat!(
                r#"{"contract_version":"2.0","#,
                r#""connection":{"server":"SQL01","database":"master","auth":"sql","#,
                r#""encrypted":true,"server_version":"17.0.1000"},"connect_ms":195,"#,
                r#""batches":[{"batch":1,"source":"-Q","line":1,"events":["#,
                r#"{"type":"result","columns":[{"name":"x","type":"int","nullable":true}],"#,
                r#""rows":[[1]],"row_count":1,"row_count_exact":true,"truncated":false,"cells_cut":0}],"#,
                r#""elapsed_ms":12,"counting_stopped":false}],"#,
                r#""ok":true,"exit_code":0,"summary":"ok: 1 batch, 215 ms","elapsed_ms":215}"#,
                "\n"
            )
        );
    }

    // Exact output, without one: a run that never connected (a usage error, a guard refusal,
    // a failed connect) has the version and nothing about a connection or its time.
    #[test]
    fn without_a_connection_there_is_neither_connection_nor_connect_ms() {
        let (mut s, main, _) = sink(capped(0));
        s.finish(&refused(3, "failed (exit 3): usage error, 0 ms"))
            .unwrap();
        assert_eq!(
            main.text(),
            concat!(
                r#"{"contract_version":"2.0","batches":[],"ok":false,"exit_code":3,"#,
                r#""summary":"failed (exit 3): usage error, 0 ms","elapsed_ms":0}"#,
                "\n"
            )
        );
        let (mut s, main, _) = sink(capped(0));
        s.blocked("line 2", "INSERT ##t would write", "Use a #temp table")
            .unwrap();
        s.finish(&refused(5, "failed (exit 5): blocked, 0 ms"))
            .unwrap();
        let doc = document(&main);
        assert!(doc.get("connection").is_none() && doc.get("connect_ms").is_none());
        assert_eq!(doc["batches"], json!([]));
        assert_eq!(doc["exit_code"], 5);
    }

    // A server that gave no name or version has `null`s, not placeholders.
    #[test]
    fn a_server_that_reports_no_name_or_version_gives_nulls() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(
            &Connection {
                server: None,
                version: None,
                auth: "windows",
                encrypted: false,
                ..connection()
            },
            0,
        )
        .unwrap();
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(
            document(&main)["connection"],
            json!({"server": null, "database": "master", "auth": "windows",
                   "encrypted": false, "server_version": null})
        );
        assert_eq!(document(&main)["connect_ms"], 0);
    }

    // Every field that comes from the server is escaped, whatever it holds: quotes, backslashes,
    // control characters, line breaks, and characters that JSON allows raw (U+007F, U+0085,
    // U+2028 and U+2029, as in every json string; DESIGN.md §5.1). The document stays valid,
    // the names read back unchanged, and the only raw line break is the final newline.
    #[test]
    fn hostile_server_and_database_names_are_escaped() {
        let name = "a\"b\\c\nd\re\tf\u{1}g\u{7f}h\u{85}i\u{2028}j\u{2029}k]}{,\"x\":1";
        let (mut s, main, _) = sink(capped(0));
        s.connected(&named(name), 1).unwrap();
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        let text = main.text();
        let doc: Json = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        assert_eq!(doc["connection"]["server"], name);
        assert_eq!(doc["connection"]["database"], name);
        assert_eq!(doc["connection"].as_object().unwrap().len(), 5, "{doc}");
        // Only the final newline is a raw one.
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        assert!(!text.contains(['\r', '\t', '\u{1}']), "{text:?}");
    }

    // A name longer than SQL Server's own limit is cut like a long value, with the number of
    // characters left out; one of exactly 128 characters is not.
    #[test]
    fn a_name_beyond_128_characters_is_cut() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(
            &Connection {
                server: Some("s".repeat(128)),
                database: "d".repeat(300),
                ..connection()
            },
            1,
        )
        .unwrap();
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        let doc = document(&main);
        assert_eq!(doc["connection"]["server"], "s".repeat(128));
        assert_eq!(
            doc["connection"]["database"],
            format!("{}…[+172]", "d".repeat(128))
        );
    }

    // The widest description there can be, every character of two cut names a six-byte
    // escape, is under 2,000 bytes: it fits the smallest cap with most of the body to spare,
    // and the whole document still ends within the cap with its outcome.
    #[test]
    fn the_widest_connection_fits_the_smallest_cap() {
        let (mut s, main, _) = sink(capped(4096));
        s.connected(&named(&"\u{1}".repeat(255)), u128::MAX)
            .unwrap();
        let head = main.bytes() + s.buf.len();
        assert!(head < 2000, "{head} bytes");
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        assert!(main.bytes() <= 4096, "{} bytes", main.bytes());
        let doc = document(&main);
        let server = doc["connection"]["server"].as_str().unwrap();
        assert_eq!(server.matches('\u{1}').count(), 128);
        assert!(server.ends_with("…[+127]"), "{server}");
        // The head is in the document, not withheld.
        assert!(doc["connect_ms"].is_number() && doc.get("notes").is_none());
    }

    // The description is a piece of the body. When notes and diagnostics have used the room
    // it would need, the budget refuses it: it is left out, counted in the closing note, and
    // the document stays valid and within the cap, with no connection in it.
    #[test]
    fn a_connection_the_budget_has_no_room_for_is_left_out_and_counted() {
        let (mut s, main, meta) = sink(capped(4096));
        s.diagnostic(&"d".repeat(3_000)).unwrap();
        s.connected(&connection(), 7).unwrap();
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.end_batch(1).unwrap();
        s.finish(&refused(0, "ok: 1 batch, 3 ms")).unwrap();
        assert!(main.bytes() + meta.bytes() <= 4096);
        let doc = document(&main);
        assert!(doc.get("connection").is_none() && doc.get("connect_ms").is_none());
        assert_eq!(doc["batches"], json!([]));
        assert_eq!(doc["exit_code"], 0);
        assert_eq!(
            doc["notes"][0],
            "output stopped at the --max-output limit of 4096 bytes; the run continued, and \
             these weren't shown: 1 batch, 1 connection description"
        );
    }

    // The description comes first or not at all: after the document has started, or a
    // second time, the sink refuses it and changes nothing.
    #[test]
    fn the_connection_is_described_once_before_anything_else() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection(), 1).unwrap();
        for _ in 0..2 {
            let error = s.connected(&named("other"), 2).unwrap_err();
            assert!(OutOfOrder::is(&error), "{error}");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(
                error.to_string().contains("before the first batch"),
                "{error}"
            );
        }
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(main.text().matches("\"connection\"").count(), 1);
        let doc = document(&main);
        assert_eq!(doc["connect_ms"], 1);
        assert_eq!(doc["connection"]["server"], "SQL01");

        let (mut s, main, _) = sink(capped(0));
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.connected(&connection(), 1).unwrap_err();
        s.finish(&refused(0, "ok: 1 batch, 3 ms")).unwrap();
        assert!(document(&main).get("connection").is_none());
    }

    // A batch lasts from its start to its end on the run clock; one that ends with the run
    // (a timeout, a cancel) ends at the run's own elapsed time; one opened without
    // `begin_batch` has no start to measure from, so it reports no time.
    #[test]
    fn a_batch_reports_the_time_between_its_start_and_its_end() {
        let (mut s, main, _) = sink(capped(0));
        let at = |number, started_ms| BatchInfo {
            number,
            total: 3,
            started_ms,
            ..batch("-Q", false, 1)
        };
        s.begin_batch(&at(1, 10)).unwrap();
        s.end_batch(25).unwrap();
        s.begin_batch(&at(2, 30)).unwrap();
        s.end_batch(31).unwrap();
        s.begin_batch(&at(3, 40)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.finish(&Summary {
            exit: 4,
            status: "failed (exit 4): timeout, 100 ms",
            error: Some("query timeout after 1 s in batch 3"),
            elapsed_ms: 100,
        })
        .unwrap();
        let doc = document(&main);
        let times: Vec<u64> = doc["batches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["elapsed_ms"].as_u64().expect("a time"))
            .collect();
        assert_eq!(times, [15, 1, 60]);
        assert_eq!(doc["batches"][2]["events"][0]["interrupted"], true);

        let (mut s, main, _) = sink(capped(0));
        s.message(&msg("outside any batch")).unwrap();
        s.end_batch(99).unwrap();
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        let batch = &document(&main)["batches"][0];
        assert!(batch.get("batch").is_none() && batch.get("elapsed_ms").is_none());
    }

    // A clock that ran backwards (it can't, but a time is only a difference) is 0, not a wrap.
    #[test]
    fn a_batch_never_lasts_a_negative_time() {
        let (mut s, main, _) = sink(capped(0));
        s.begin_batch(&BatchInfo {
            started_ms: 50,
            ..batch("-Q", false, 1)
        })
        .unwrap();
        s.end_batch(40).unwrap();
        s.finish(&refused(0, "ok: 1 batch, 3 ms")).unwrap();
        assert_eq!(document(&main)["batches"][0]["elapsed_ms"], 0);
    }

    // A run cut by --max-output keeps what the head promised: the connection and its time come
    // first and are counted in the cap, the cut result closes with exact counts, the batch
    // still says how long it took, and the outcome is whole.
    #[test]
    fn a_run_cut_by_the_cap_keeps_its_connection_and_times() {
        let max = crate::output::OUTPUT_RESERVE + 600;
        let (mut s, main, _) = sink(capped(max));
        s.connected(&connection(), 31).unwrap();
        s.begin_batch(&BatchInfo {
            started_ms: 40,
            ..batch("-Q", false, 1)
        })
        .unwrap();
        s.columns(&[col("v", "varchar(20)")]).unwrap();
        for i in 0..100 {
            if s.wants_values() {
                s.row(&[Value::Text(format!("row {i}"))]).unwrap();
            } else {
                s.skipped_row();
            }
        }
        s.end_rows().unwrap();
        s.message(&msg("too late for the cap")).unwrap();
        s.end_batch(95).unwrap();
        s.finish(&Summary {
            elapsed_ms: 99,
            ..summary("ok: 1 batch, 99 ms")
        })
        .unwrap();
        assert!(main.bytes() as u64 <= max, "{} bytes", main.bytes());
        let doc = document(&main);
        assert_eq!(doc["connection"]["database"], "master");
        assert_eq!(doc["connect_ms"], 31);
        let batch = &doc["batches"][0];
        assert_eq!(batch["elapsed_ms"], 55);
        let result = &batch["events"][0];
        assert_eq!(result["row_count"], 100);
        assert_eq!(result["truncated"], true);
        let shown = result["rows"].as_array().unwrap().len();
        assert!((1..100).contains(&shown), "{shown} rows");
        assert_eq!(batch["events"].as_array().unwrap().len(), 1);
        assert!(
            doc["notes"][0]
                .as_str()
                .is_some_and(|n| n.ends_with("these weren't shown: 1 message")),
            "{doc}"
        );
        assert_eq!(doc["summary"], "ok: 1 batch, 99 ms");
        assert_eq!(doc["elapsed_ms"], 99);
    }

    // A result that a failure cut short is closed as interrupted when the next batch begins,
    // ending that batch there, and the next batch takes its events. (A failed run goes no
    // further today; the sink doesn't depend on that.)
    #[test]
    fn a_cut_short_result_does_not_refuse_the_next_batch() {
        let (mut s, main, _) = sink(capped(0));
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.begin_batch(&BatchInfo {
            number: 2,
            total: 2,
            started_ms: 8,
            ..batch("-Q", false, 2)
        })
        .unwrap();
        s.message(&msg("second batch")).unwrap();
        s.end_batch(9).unwrap();
        s.finish(&refused(0, "ok: 2 batches, 3 ms")).unwrap();
        let doc = document(&main);
        assert_eq!(doc["batches"][0]["events"][0]["interrupted"], true);
        assert_eq!(doc["batches"][0]["elapsed_ms"], 8);
        assert_eq!(doc["batches"][1]["events"][0]["text"], "second batch");
        assert_eq!(doc["batches"][1]["elapsed_ms"], 1);
    }

    // The sweep of `the_cap_holds_wherever_the_body_ends`, with the description of the
    // connection written first: at every cap from the reserve up, the document is valid, no
    // longer than the cap, and has its outcome; the description is there or counted as withheld.
    #[test]
    fn the_cap_holds_with_the_connection_in_the_document() {
        use crate::output::OUTPUT_RESERVE;
        for max in OUTPUT_RESERVE..OUTPUT_RESERVE + 1800 {
            for (run, exit) in [
                (ragged_run as fn(&mut dyn Output), 1),
                (interrupted_run, 4),
                (filling_run, 1),
            ] {
                let (mut s, main, meta) = sink(capped(max));
                s.connected(&named("N"), 12).unwrap();
                run(&mut s);
                let total = (main.bytes() + meta.bytes()) as u64;
                assert!(total <= max, "cap {max}: {total} bytes\n{}", main.text());
                let doc = document(&main);
                assert_eq!(doc["exit_code"], exit, "cap {max}");
                // Either the description is there, or the closing note says the output
                // stopped first (its start is never cut; its counts may be).
                let stopped = doc["notes"].as_array().is_some_and(|notes| {
                    notes
                        .iter()
                        .filter_map(Json::as_str)
                        .any(|note| note.starts_with("output stopped at the --max-output limit"))
                });
                assert!(
                    doc.get("connection").is_some() || stopped,
                    "cap {max}: {doc}"
                );
                assert_eq!(
                    doc.get("connection").is_some(),
                    doc.get("connect_ms").is_some(),
                    "cap {max}"
                );
            }
        }
    }

    // PLAN.md 2.12, DESIGN.md §5.2: nothing but rows arrives inside a result set. What did is
    // refused and changes nothing, and a message the server sent during the result is an
    // event after it in the batch's events, never one of its rows.
    #[test]
    fn nothing_but_rows_arrives_inside_a_result_set() {
        let (mut s, main, meta) = sink(limits());
        mid_result_events_are_refused(&mut s);
        s.finish(&summary("failed (exit 1): 1 error, 3 ms"))
            .unwrap();
        assert_eq!(meta.bytes(), 0);
        assert_eq!(
            document(&main),
            json!({
                "contract_version": "2.0",
                "batches": [{"events": [
                    {"type": "result",
                     "columns": [{"name": "v", "type": "int", "nullable": true}],
                     "rows": [[1], [2]], "row_count": 2, "row_count_exact": true,
                     "truncated": false, "cells_cut": 0},
                    {"type": "msg", "text": "kept"},
                    {"type": "rows_affected", "count": 3},
                    {"type": "error", "number": 208, "severity": 16, "state": 1, "line": 2,
                     "text": "Invalid object name 'dbo.Nope'."}
                ], "counting_stopped": false}],
                "notes": ["1 more result sets not shown (--max-results 1)"],
                "ok": true, "exit_code": 0,
                "summary": "failed (exit 1): 1 error, 3 ms", "elapsed_ms": 3
            })
        );
    }

    // A refused call writes nothing: the result it came in is still there, and the run that
    // ends there closes it as interrupted and leaves a valid document, with none of the
    // refused events in it.
    #[test]
    fn a_refused_event_leaves_a_valid_document() {
        type Call = fn(&mut JsonSink<Shared>) -> io::Result<()>;
        let calls: [(&str, Call); 3] = [
            ("message", |s| s.message(&msg("too early"))),
            ("error", |s| s.error(&error(1))),
            ("row count", |s| s.rows_affected(4)),
        ];
        for (what, call) in calls {
            let (mut s, main, _) = sink(capped(0));
            s.begin_batch(&batch("-Q", false, 1)).unwrap();
            s.columns(&[col("x", "int")]).unwrap();
            s.row(&[Value::Int(1)]).unwrap();
            let written = main.bytes() + s.buf.len();
            call(&mut s).unwrap_err();
            assert_eq!(main.bytes() + s.buf.len(), written, "{what}");
            s.row(&[Value::Int(2)]).unwrap();
            s.finish(&Summary {
                exit: 10,
                status: "failed (exit 10): internal error, 3 ms",
                error: Some("internal error: the sink refused it"),
                elapsed_ms: 3,
            })
            .unwrap();
            let doc = document(&main);
            let events = doc["batches"][0]["events"].as_array().unwrap();
            assert_eq!(events.len(), 1, "{what}: {doc}");
            assert_eq!(events[0]["rows"], json!([[1], [2]]), "{what}");
            assert_eq!(events[0]["interrupted"], true, "{what}");
            assert_eq!(doc["exit_code"], 10);
        }
    }

    fn refused(exit: u8, status: &str) -> Summary<'_> {
        Summary {
            exit,
            status,
            error: None,
            elapsed_ms: 0,
        }
    }

    // A guard refusal that quotes a 20,000-character identifier is shortened in its variable
    // part: the document stays within --max-output, valid, with the advice and the outcome.
    #[test]
    fn a_huge_refusal_is_shortened_to_the_budget() {
        let (mut s, main, meta) = sink(capped(4096));
        s.blocked("line 1", &quoting(20_000), ADVICE).unwrap();
        s.finish(&refused(
            5,
            "failed (exit 5): blocked by the read-only guard, 0 ms",
        ))
        .unwrap();
        assert_eq!(meta.bytes(), 0);
        assert!(main.bytes() <= 4096, "{} bytes", main.bytes());
        assert!(main.bytes() >= 4096 - 8, "the error takes the room");
        let doc = document(&main);
        assert_eq!(doc["batches"], json!([]));
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["exit_code"], 5);
        assert_eq!(
            doc["summary"],
            "failed (exit 5): blocked by the read-only guard, 0 ms"
        );
        let error = doc["error"].as_str().unwrap();
        assert!(error.starts_with("line 1: UPDATE [qqqq"), "{error}");
        assert!(error.ends_with(ADVICE), "{error}");
        assert!(error.contains("]; nothing was run. "), "{error}");
        assert_eq!(
            error.matches('q').count() + cut_by(error),
            20_000 + "] would write".len()
        );
    }

    // Exact output: the arithmetic of a tight cap. A note may use what the cap has left less
    // the 640-byte status reserve, 728 - 640 = 88 bytes, 13 of them for the array and quotes
    // around the first note: 66 characters and "…[+134]". The document head
    // (`{"contract_version":"2.0","batches":[`) and `]` take 38, and what the notes array,
    // the error's wrapper and the outcome leave of the other 690 goes to the error: 507
    // characters and "…[+493]".
    #[test]
    fn a_tight_cap_shortens_exactly() {
        let (mut s, main, _) = sink(capped(728));
        s.note(&"x".repeat(200)).unwrap();
        let error = "e".repeat(1000);
        s.finish(&Summary {
            exit: 1,
            status: "ok: 0 batches, 3 ms",
            error: Some(&error),
            elapsed_ms: 3,
        })
        .unwrap();
        assert_eq!(
            main.text(),
            format!(
                "{{\"contract_version\":\"2.0\",\"batches\":[],\"notes\":[\"{}…[+134]\"],\
                 \"error\":\"{}…[+493]\",\"ok\":false,\"exit_code\":1,\
                 \"summary\":\"ok: 0 batches, 3 ms\",\"elapsed_ms\":3}}\n",
                "x".repeat(66),
                "e".repeat(507)
            )
        );
        // One byte of the cap is left over: the marker is sized for the widest count.
        assert_eq!(main.bytes(), 727);
    }

    // A huge closing error is shortened; the outcome fields are not.
    #[test]
    fn a_huge_closing_error_is_shortened_and_the_outcome_is_not() {
        let (mut s, main, _) = sink(capped(4096));
        let error = "z".repeat(30_000);
        s.finish(&Summary {
            exit: 2,
            status: "failed (exit 2): connection failed, 5 ms",
            error: Some(&error),
            elapsed_ms: 5,
        })
        .unwrap();
        assert!(main.bytes() <= 4096, "{} bytes", main.bytes());
        assert!(main.bytes() >= 4096 - 8, "the error takes the room");
        let doc = document(&main);
        let error = doc["error"].as_str().unwrap();
        assert_eq!(error.matches('z').count() + cut_by(error), 30_000);
        assert_eq!(doc["exit_code"], 2);
        assert_eq!(doc["summary"], "failed (exit 2): connection failed, 5 ms");
        assert_eq!(doc["elapsed_ms"], 5);
    }

    // Notes are shortened when they arrive and count from then on, though the document gets
    // them last: the second long one finds no room and is counted as withheld.
    #[test]
    fn long_notes_are_shortened_and_late_ones_counted() {
        let (mut s, main, _) = sink(capped(4096));
        s.note(&"k".repeat(10_000)).unwrap();
        s.note(&"k".repeat(10_000)).unwrap();
        let error = "z".repeat(10_000);
        s.finish(&Summary {
            exit: 1,
            status: "failed (exit 1): 1 error, 3 ms",
            error: Some(&error),
            elapsed_ms: 3,
        })
        .unwrap();
        assert!(main.bytes() <= 4096, "{} bytes", main.bytes());
        let doc = document(&main);
        let notes = doc["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 2, "{doc}");
        let first = notes[0].as_str().unwrap();
        assert_eq!(first.matches('k').count() + cut_by(first), 10_000);
        assert_eq!(
            notes[1],
            "output stopped at the --max-output limit of 4096 bytes; the run continued, and \
             these weren't shown: 1 note"
        );
        let error = doc["error"].as_str().unwrap();
        assert_eq!(error.matches('z').count() + cut_by(error), 10_000);
        assert_eq!(doc["summary"], "failed (exit 1): 1 error, 3 ms");
    }

    // The budget counts bytes as the document writes them: `\u0001` is six bytes, a quote or
    // backslash two, a wide character its UTF-8 length. A cut never splits an escape or a
    // character, and the count N is in characters.
    #[test]
    fn escapes_that_expand_are_counted_after_escaping() {
        for ch in ['\u{1}', '"', '\\', '\n', 'é', '日', '😀'] {
            let (mut s, main, _) = sink(capped(4096));
            let reason = format!("[{}]", ch.to_string().repeat(5000));
            s.blocked("line 1", &reason, ADVICE).unwrap();
            s.finish(&refused(5, "failed (exit 5): x, 0 ms")).unwrap();
            assert!(main.bytes() <= 4096, "{ch:?}: {} bytes", main.bytes());
            let doc = document(&main);
            let error = doc["error"].as_str().unwrap();
            let kept = error.matches(ch).count();
            // The "[" in front is kept, the "]" behind is cut, and the count is exact.
            assert_eq!(kept + cut_by(error), 5001, "{ch:?}");
            assert!(error.ends_with(ADVICE), "{ch:?}");
        }
    }

    // Wherever the body ends, the document stays within the cap and valid, and its outcome
    // fields are there.
    #[test]
    fn the_cap_holds_wherever_the_body_ends() {
        use crate::output::OUTPUT_RESERVE;
        for max in OUTPUT_RESERVE..OUTPUT_RESERVE + 1800 {
            for (run, exit, error, status) in [
                (
                    ragged_run as fn(&mut dyn Output),
                    1,
                    "closing error ",
                    RAGGED_STATUS,
                ),
                (interrupted_run, 4, "query timeout ", INTERRUPTED_STATUS),
                // A stopped count at the widest numbers: the longest closers and status.
                (filling_run, 1, "closing error ", FILLING_STATUS),
            ] {
                let (mut s, main, meta) = sink(capped(max));
                run(&mut s);
                let total = (main.bytes() + meta.bytes()) as u64;
                assert!(total <= max, "cap {max}: {total} bytes\n{}", main.text());
                let doc = document(&main);
                assert_eq!(doc["ok"], false, "cap {max}");
                assert_eq!(doc["exit_code"], exit, "cap {max}");
                assert_eq!(doc["summary"], status, "cap {max}");
                assert!(doc["batches"].is_array(), "cap {max}");
                // The reserve leaves room for the closing error, shortened if need be.
                assert!(
                    doc["error"].as_str().is_some_and(|e| e.starts_with(error)),
                    "cap {max}: {doc}"
                );
            }
        }
        let (mut s, main, meta) = sink(capped(0));
        ragged_run(&mut s);
        assert!(!main.text().contains("…[+") && !meta.text().contains("…[+"));
    }

    // `--verbose` lines go to the second stream as plain `sqlh: ...` text and count against
    // the budget, which includes what the notes will take.
    #[test]
    fn diagnostics_go_to_the_second_stream_and_count_against_the_budget() {
        let (mut s, main, meta) = sink(capped(4096));
        s.diagnostic("connected in 5 ms; server SQL01\\INST")
            .unwrap();
        assert_eq!(
            meta.text(),
            "sqlh: connected in 5 ms; server SQL01\\\\INST\n"
        );
        assert_eq!(main.bytes(), 0);
        // A diagnostic is counted: the note that follows gets only what it leaves.
        s.diagnostic(&"d".repeat(2_000)).unwrap();
        s.note(&"k".repeat(20_000)).unwrap();
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        let lines = meta.text();
        let long = lines.lines().nth(1).unwrap();
        assert_eq!(long.matches('d').count(), 2_000);
        assert!(main.bytes() + meta.bytes() <= 4096);
        let doc = document(&main);
        assert_eq!(doc["ok"], true);
        let note = doc["notes"][0].as_str().unwrap();
        assert_eq!(note.matches('k').count() + cut_by(note), 20_000);
        // The diagnostics and the note together take the room before the status reserve.
        let room = 4096 - crate::output::STATUS_RESERVE as usize;
        let spent = meta.bytes() + NOTES_ARRAY + 2 + note.len();
        assert!((room - 8..=room).contains(&spent), "{spent} of {room}");
        // A huge diagnostic is shortened like a note.
        let (mut s, _, meta) = sink(capped(4096));
        s.diagnostic(&"d".repeat(20_000)).unwrap();
        let line = meta.text();
        assert_eq!(line.matches('d').count() + cut_by(&line), 20_000);
        assert!(meta.bytes() <= 4096 - 640);

        // Too little room for even a marker: the line is dropped and counted.
        let (mut s, main, meta) = sink(capped(520));
        s.diagnostic(&"d".repeat(100)).unwrap();
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(meta.bytes(), 0);
        let doc = document(&main);
        assert!(
            doc["notes"][0]
                .as_str()
                .unwrap()
                .ends_with("these weren't shown: 1 note"),
            "{doc}"
        );
    }

    // A cap too small for the outcome (the command line asks for 4096 at least) still gets a
    // valid document with the outcome: the cap is the thing that gives way.
    #[test]
    fn a_cap_too_small_for_the_ending_still_yields_the_outcome() {
        let (mut s, main, _) = sink(capped(100));
        s.note("n").unwrap();
        s.finish(&refused(0, "ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(
            main.text(),
            "{\"contract_version\":\"2.0\",\"batches\":[],\"ok\":true,\"exit_code\":0,\
             \"summary\":\"ok: 0 batches, 3 ms\",\"elapsed_ms\":0}\n"
        );
    }

    // The reserve is sized for this: the closers of an open result and batch (the batch's
    // `elapsed_ms` among them), the end of the batches, and the notes and error wrappers, with
    // every number at its widest and the longest status main.rs builds. The closers' template
    // is checked against a real run. The head with the connection is not in it: it is a piece
    // of the body, admitted when it is written.
    #[test]
    fn the_worst_case_ending_fits_the_status_reserve() {
        // An interrupted result is never exact (`false`, the longer word), whatever else holds.
        let closers = |rows: u64, cut: u64, truncated: bool, elapsed: u128, stopped: bool| {
            format!(
                "],\"row_count\":{rows},\"row_count_exact\":false,\"truncated\":{truncated},\
                 \"cells_cut\":{cut},\"interrupted\":true}}],\"elapsed_ms\":{elapsed},\
                 \"counting_stopped\":{stopped}}}"
            )
        };
        let (mut s, main, _) = sink(capped(0));
        s.begin_batch(&BatchInfo {
            started_ms: 3,
            ..batch("-Q", false, 1)
        })
        .unwrap();
        s.columns(&[col("v", "int")]).unwrap();
        s.row(&[Value::Int(7)]).unwrap();
        s.finish(&Summary {
            elapsed_ms: 10,
            ..refused(1, "x")
        })
        .unwrap();
        assert!(
            main.text().contains(&closers(1, 0, false, 7, false)),
            "{}",
            main.text()
        );

        let status = format!(
            "failed (exit 1): {} errors, first in batch {}; {} later batches not run \
             (--continue-on-error runs them), counting stopped in {}, {} ms",
            u32::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX,
            u128::MAX
        );
        let outcome = outcome(&Summary {
            exit: 10,
            status: &status,
            error: None,
            elapsed_ms: u128::MAX,
        });
        let worst = closers(u64::MAX, u64::MAX, false, u128::MAX, false).len()
            + "]".len()
            + ",\"notes\":[\"\"]".len()
            + ",\"error\":\"\"".len()
            + outcome.len();
        assert!(
            worst <= crate::output::STATUS_RESERVE as usize,
            "{worst} bytes"
        );
    }
}
