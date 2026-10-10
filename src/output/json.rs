//! `--format json` (DESIGN.md §5.2): one JSON document, streamed. Each batch has an ordered
//! `events` array (result, msg, rows_affected, error); the run's outcome comes last because
//! it is only known at the end. `finish` closes whatever is open, so a run that fails
//! mid-result still yields valid JSON.

use std::fmt::Write as _;
use std::io::{self, Write};

use super::{BatchInfo, Limits, Output, Summary, Tally, blocked_message, cut, cut_marker, hex};
use crate::db::{BatchSink, Column, ServerMessage, Value};

/// Integers beyond ±2^53 lose precision as JSON numbers in most parsers, so they become strings.
const MAX_SAFE_INTEGER: i64 = 1 << 53;

pub struct JsonSink<W: Write> {
    out: W,
    tally: Tally,
    buf: String,
    /// Bytes already written from `buf`, for the output budget.
    drained: u64,
    opened: bool,
    batches: usize,
    in_batch: bool,
    events: usize,
    in_result: bool,
    notes: Vec<String>,
    blocked: Option<String>,
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
    pub fn new(out: W, limits: Limits) -> Self {
        JsonSink {
            out,
            tally: Tally::new(limits),
            buf: String::with_capacity(1024),
            drained: 0,
            opened: false,
            batches: 0,
            in_batch: false,
            events: 0,
            in_result: false,
            notes: Vec::new(),
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

    /// Whether what was appended since `mark` fits the output budget; if not, takes it back.
    fn admit_since(&mut self, mark: Mark) -> bool {
        let used = self.drained + mark.len as u64;
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
        self.open();
        self.close_batch();
        let mark = self.mark();
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

    fn note(&mut self, text: &str) -> io::Result<()> {
        self.flush_affected()?;
        self.notes.push(text.to_owned());
        Ok(())
    }

    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()> {
        self.flush_affected()?;
        self.blocked = Some(blocked_message(location, reason, advice));
        Ok(())
    }

    fn finish(&mut self, summary: &Summary) -> io::Result<()> {
        self.flush_affected()?;
        self.open();
        self.close_batch();
        self.buf.push(']');
        let mut notes = std::mem::take(&mut self.notes);
        notes.extend(self.tally.limit_notes());
        if !notes.is_empty() {
            self.buf.push_str(",\"notes\":[");
            for (i, note) in notes.iter().enumerate() {
                if i > 0 {
                    self.buf.push(',');
                }
                push_str(&mut self.buf, note);
            }
            self.buf.push(']');
        }
        if let Some(error) = summary.error.or(self.blocked.as_deref()) {
            let error = error.to_owned();
            self.buf.push_str(",\"error\":");
            push_str(&mut self.buf, &error);
        }
        let _ = write!(
            self.buf,
            ",\"ok\":{},\"exit_code\":{},\"summary\":",
            summary.exit == 0,
            summary.exit
        );
        push_str(&mut self.buf, summary.status);
        let _ = writeln!(self.buf, ",\"elapsed_ms\":{}}}", summary.elapsed_ms);
        self.drain(true)
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain(true)?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::super::text::tests::{batch, col, error, limits, msg, summary};
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
}
