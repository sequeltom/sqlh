//! `--format json` (DESIGN.md §5.2): one JSON document, streamed. Each batch has an ordered
//! `events` array (result, msg, rows_affected, error); the run's outcome comes last because
//! it is only known at the end. `finish` closes whatever is open, so a run that fails
//! mid-result still yields valid JSON.
//!
//! The output budget (`--max-output`) covers the whole document, which the sink counts
//! piece by piece: events are admitted or taken back whole; a note is shortened when it is
//! recorded and its bytes count from then on, though it is written last; the closers of
//! the result and batch still open and the outcome fields come out of the reserve.

use std::fmt::Write as _;
use std::io::{self, Write};

use super::text::write_diagnostic;
use super::{
    BatchInfo, Layout, Limits, Output, Summary, Tally, cut, cut_marker, hex, push_blocked,
    push_within,
};
use crate::db::{BatchSink, Column, ServerMessage, Value};

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

    /// Writes the document head once: `{"batches":[`.
    fn open(&mut self) {
        if !self.opened {
            self.buf.push_str("{\"batches\":[");
            self.opened = true;
        }
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
        let _ = write!(
            self.buf,
            "],\"row_count\":{},\"truncated\":{},\"cells_cut\":{}",
            self.tally.rows_total(),
            self.tally.rows_shown() < self.tally.rows_total(),
            self.tally.cells_cut()
        );
        if interrupted {
            self.buf.push_str(",\"interrupted\":true");
        }
        self.buf.push('}');
        self.in_result = false;
    }

    fn close_batch(&mut self) {
        self.end_result(true);
        if self.in_batch {
            self.buf.push_str("]}");
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

    fn end_rows(&mut self) -> io::Result<()> {
        self.end_result(false);
        self.drain(false)
    }

    fn rows_affected(&mut self, count: u64) -> io::Result<()> {
        self.tally.add_affected(count);
        Ok(())
    }

    fn message(&mut self, message: &ServerMessage) -> io::Result<()> {
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
    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()> {
        self.flush_affected()?;
        self.tally.begin_batch(batch);
        // The previous batch's closers are not up for admission. The document head is: it
        // comes back out with the batch if the budget refuses both.
        self.close_batch();
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
        if !self.admit_since(mark) {
            self.tally.withhold_batch();
        }
        self.drain(false)
    }

    fn end_batch(&mut self) -> io::Result<()> {
        self.flush_affected()?;
        self.close_batch();
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
        self.close_batch();
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
        ADVICE, INTERRUPTED_STATUS, RAGGED_STATUS, Shared, batch, capped, col, cut_by, error,
        interrupted_run, limits, msg, quoting, ragged_run, summary,
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
        s.begin_batch(&batch("q.sql", false, 1)).unwrap();
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
        s.end_batch().unwrap();
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
                         "row_count": 3, "truncated": true, "cells_cut": 2},
                        {"type": "rows_affected", "count": 3},
                        {"type": "error", "number": 208, "severity": 16, "state": 1,
                         "line": 2, "text": "Invalid object name 'dbo.Nope'."}
                    ]
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
        s.end_batch().unwrap();
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
        s.end_batch().unwrap();
        s.begin_batch(&BatchInfo {
            number: 2,
            total: 2,
            ..batch("-Q", false, 3)
        })
        .unwrap();
        s.rows_affected(1).unwrap();
        s.end_batch().unwrap();
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
    // the 512-byte status reserve, 600 - 512 = 88 bytes, 13 of them for the array and quotes
    // around the first note: 66 characters and "…[+134]". The document head and `]` take 13,
    // and what the notes array, the error's wrapper and the outcome leave of the other 587
    // goes to the error: 404 characters and "…[+596]".
    #[test]
    fn a_tight_cap_shortens_exactly() {
        let (mut s, main, _) = sink(capped(600));
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
                "{{\"batches\":[],\"notes\":[\"{}…[+134]\"],\"error\":\"{}…[+596]\",\
                 \"ok\":false,\"exit_code\":1,\"summary\":\"ok: 0 batches, 3 ms\",\
                 \"elapsed_ms\":3}}\n",
                "x".repeat(66),
                "e".repeat(404)
            )
        );
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
        assert!(meta.bytes() <= 4096 - 512);

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
            "{\"batches\":[],\"ok\":true,\"exit_code\":0,\"summary\":\"ok: 0 batches, 3 ms\",\
             \"elapsed_ms\":0}\n"
        );
    }

    // The reserve is sized for this: the closers of an open result and batch, the end of the
    // batches, and the notes and error wrappers, with every number at its widest and the
    // longest status main.rs builds. The closers' template is checked against a real run.
    #[test]
    fn the_worst_case_ending_fits_the_status_reserve() {
        let closers = |rows: u64, cut: u64, truncated: bool| {
            format!(
                "],\"row_count\":{rows},\"truncated\":{truncated},\"cells_cut\":{cut},\
                 \"interrupted\":true}}]}}"
            )
        };
        let (mut s, main, _) = sink(capped(0));
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("v", "int")]).unwrap();
        s.row(&[Value::Int(7)]).unwrap();
        s.finish(&refused(1, "x")).unwrap();
        assert!(
            main.text().contains(&closers(1, 0, false)),
            "{}",
            main.text()
        );

        let status = format!(
            "failed (exit 1): {} errors, first in batch {}; {} later batches not run \
             (--continue-on-error runs them), {} ms",
            u32::MAX,
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
        let worst = closers(u64::MAX, u64::MAX, false).len()
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
