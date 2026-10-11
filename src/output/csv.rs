//! `--format csv` (DESIGN.md §5.3): RFC 4180 fields with LF line endings. The data stream
//! (stdout or `-o`) holds only CSV: a header row of column names, the rows, and a blank line
//! between result sets. Everything else (result markers with types, row counts, messages,
//! errors, the status) goes to `meta` (stderr) as text-format `-- ` lines.

use std::io::{self, Write};

use super::text::{
    batch_line, blocked_layout, closing_lines, error_line, message_line, note_layout,
    rows_affected_line, write_diagnostic,
};
use super::{
    BatchInfo, Layout, Limits, Output, Summary, Tally, cut, cut_marker, escaped, hex, push_char,
    push_type,
};
use crate::db::{BatchSink, Column, ServerMessage, Value};

pub struct CsvSink<W: Write> {
    data: W,
    meta: Box<dyn Write>,
    tally: Tally,
    line: String,
    results_written: usize,
    /// Bytes written to both streams so far: the output budget covers what the agent reads.
    written: u64,
}

impl<W: Write> CsvSink<W> {
    pub fn new(data: W, meta: Box<dyn Write>, limits: Limits) -> Self {
        CsvSink {
            data,
            meta,
            tally: Tally::new(limits),
            line: String::with_capacity(1024),
            results_written: 0,
            written: 0,
        }
    }

    /// Writes a metadata line whatever the budget: the marker of a result the budget
    /// admitted, or its footer, which the reserve holds (see `TextSink::write_line`).
    fn meta_line(&mut self, line: &str) -> io::Result<()> {
        self.written += line.len() as u64 + 1;
        writeln!(self.meta, "{line}")
    }

    /// Writes a note or a refusal to the metadata stream, shortened to the room the budget
    /// has left before the status reserve (see `TextSink::write_fitted`).
    fn write_fitted(&mut self, layout: &Layout) -> io::Result<()> {
        match layout.fit(self.tally.note_room(self.written), push_char) {
            Some(line) => {
                self.written += line.len() as u64;
                self.meta.write_all(line.as_bytes())
            }
            None => {
                self.tally.withhold_note();
                Ok(())
            }
        }
    }

    /// Writes a metadata line if the output budget admits it, and says whether it did.
    fn emit_meta(&mut self, line: &str) -> io::Result<bool> {
        if !self.tally.admit(self.written, line.len() + 1) {
            return Ok(false);
        }
        self.meta_line(line)?;
        Ok(true)
    }

    /// Writes the pending run of row counts as one line, before any other event.
    fn flush_affected(&mut self) -> io::Result<()> {
        if let Some((statements, rows)) = self.tally.take_affected()
            && !self.emit_meta(&rows_affected_line(statements, rows))?
        {
            self.tally.withhold_row_counts(statements);
        }
        Ok(())
    }
}

/// Appends one CSV field, quoted when it must be: empty (so it differs from NULL), or
/// holding a comma, quote, line break, or edge whitespace that readers might trim.
fn push_field(out: &mut String, s: &str) {
    let quote = s.is_empty()
        || s.contains([',', '"', '\n', '\r'])
        || s.starts_with(char::is_whitespace)
        || s.ends_with(char::is_whitespace);
    if !quote {
        out.push_str(s);
        return;
    }
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

impl<W: Write> BatchSink for CsvSink<W> {
    fn columns(&mut self, columns: &[Column]) -> io::Result<()> {
        self.flush_affected()?;
        let Some(number) = self.tally.start_result() else {
            return Ok(());
        };
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        if self.results_written > 0 {
            line.push('\n');
        }
        let mut types = String::new();
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                line.push(',');
                types.push_str(", ");
            }
            push_field(&mut line, &column.name);
            types.push_str(&escaped(&column.name));
            types.push(':');
            push_type(&mut types, &column.type_name);
        }
        line.push('\n');
        let marker = format!("-- result {number}: {types}");
        // The header row and its metadata line are shown together or not at all.
        if self
            .tally
            .admit(self.written, line.len() + marker.len() + 1)
        {
            self.results_written += 1;
            self.written += line.len() as u64;
            self.data.write_all(line.as_bytes())?;
            self.meta_line(&marker)?;
        } else {
            self.tally.withhold_result();
        }
        self.line = line;
        Ok(())
    }

    fn wants_values(&self) -> bool {
        self.tally.wants_values()
    }

    fn cell_limit(&self) -> usize {
        self.tally.cell_limit()
    }

    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        let max = self.tally.limits().max_cell;
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        let mut cells_cut = 0;
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            match value {
                Value::Null => {}
                Value::Int(v) => line.push_str(&v.to_string()),
                Value::Num(s) | Value::Lit(s) => push_field(&mut line, s),
                Value::Text(s) => {
                    let (kept, cut_count) = cut(s, max);
                    if cut_count > 0 {
                        cells_cut += 1;
                        push_field(
                            &mut line,
                            &format!("{kept}{}", cut_marker(cut_count, false)),
                        );
                    } else {
                        push_field(&mut line, kept);
                    }
                }
                Value::Bin(bytes) => {
                    let (mut text, cut_count) = hex(bytes, max);
                    if cut_count > 0 {
                        cells_cut += 1;
                        text.push_str(&cut_marker(cut_count, true));
                    }
                    line.push_str(&text);
                }
                // Cut by the read, to the sink's own limit: the marker is all that's left to add.
                Value::TextCut(s, more) => {
                    cells_cut += 1;
                    push_field(&mut line, &format!("{s}{}", cut_marker(*more, false)));
                }
                Value::BinCut(bytes, more) => {
                    cells_cut += 1;
                    line.push_str(&hex(bytes, 0).0);
                    line.push_str(&cut_marker(*more, true));
                }
            }
        }
        line.push('\n');
        let result = if self.tally.admit(self.written, line.len()) {
            self.tally.row_shown();
            for _ in 0..cells_cut {
                self.tally.cell_cut();
            }
            self.written += line.len() as u64;
            self.data.write_all(line.as_bytes())
        } else {
            self.tally.row_withheld();
            Ok(())
        };
        self.line = line;
        result
    }

    fn skipped_row(&mut self) {
        self.tally.row_skipped();
    }

    fn end_rows(&mut self) -> io::Result<()> {
        if self.tally.result_hidden() {
            return Ok(());
        }
        let footer = format!("-- {}", self.tally.footer());
        self.meta_line(&footer)
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
        if !self.emit_meta(&message_line(message))? {
            self.tally.withhold_message();
        }
        Ok(())
    }

    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        self.flush_affected()?;
        self.tally.error(error.severity);
        let line = error_line(&self.tally, error);
        if !self.emit_meta(&line)? {
            self.tally.withhold_error();
        }
        Ok(())
    }
}

impl<W: Write> Output for CsvSink<W> {
    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()> {
        self.flush_affected()?;
        self.tally.begin_batch(batch);
        if batch.total > 1 && !self.emit_meta(&batch_line(batch))? {
            self.tally.withhold_batch();
        }
        Ok(())
    }

    fn end_batch(&mut self) -> io::Result<()> {
        self.flush_affected()
    }

    fn note(&mut self, text: &str) -> io::Result<()> {
        self.flush_affected()?;
        self.write_fitted(&note_layout(text))
    }

    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()> {
        self.flush_affected()?;
        self.write_fitted(&blocked_layout(location, reason, advice))
    }

    fn diagnostic(&mut self, text: &str) -> io::Result<()> {
        self.written += write_diagnostic(&mut self.meta, &mut self.tally, self.written, text)?;
        Ok(())
    }

    fn finish(&mut self, summary: &Summary) -> io::Result<()> {
        self.flush_affected()?;
        self.data.flush()?;
        let lines = closing_lines(&self.tally, summary, self.written);
        self.written += lines.len() as u64;
        self.meta.write_all(lines.as_bytes())
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }

    fn flush(&mut self) -> io::Result<()> {
        self.data.flush()?;
        self.meta.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::super::text::tests::{
        ADVICE, INTERRUPTED_STATUS, RAGGED_STATUS, Shared, capped, col, cut_by, error,
        interrupted_run, limits, msg, quoting, ragged_run, summary,
    };
    use super::*;

    #[test]
    fn data_and_meta_streams() {
        let (data, meta) = (Shared::default(), Shared::default());
        let limits = Limits {
            max_cell: 20,
            ..limits()
        };
        let mut s = CsvSink::new(data.clone(), Box::new(meta.clone()), limits);
        s.columns(&[
            col("id", "int"),
            col("name", "nvarchar(20)"),
            col("note", "varchar(max)"),
        ])
        .unwrap();
        s.row(&[
            Value::Null,
            Value::Text("Smith, \"Jo\"".into()),
            Value::Text(" pad".into()),
        ])
        .unwrap();
        s.row(&[
            Value::Int(2),
            Value::Text(String::new()),
            Value::Text("abcdefghijklmnopqrstuvwxyz".into()),
        ])
        .unwrap();
        s.skipped_row();
        s.end_rows().unwrap();
        s.message(&msg("done")).unwrap();
        s.error(&error(4)).unwrap();
        s.finish(&summary("failed (exit 1): 1 error, 5 ms"))
            .unwrap();
        assert_eq!(
            data.text(),
            "id,name,note\n,\"Smith, \"\"Jo\"\"\",\" pad\"\n2,\"\",abcdefghijklmnopqrst…[+6]\n"
        );
        assert_eq!(
            meta.text(),
            "-- result 1: id:int, name:nvarchar(20), note:varchar(max)\n\
             -- 2 of 3 rows shown (--max-rows 2); 1 value cut (--max-cell 20)\n\
             -- msg: done\n\
             -- error 208 (severity 16, state 1, line 4): Invalid object name 'dbo.Nope'.\n\
             -- failed (exit 1): 1 error, 5 ms\n"
        );
    }

    // R05: type names on the metadata stream can't start a line either.
    #[test]
    fn hostile_type_names_are_escaped() {
        let (data, meta) = (Shared::default(), Shared::default());
        let mut s = CsvSink::new(data.clone(), Box::new(meta.clone()), Limits::default());
        s.columns(&[col("v", "udt\n-- ok: forged"), col("w", "a\tb\\c\x01d:e")])
            .unwrap();
        assert_eq!(data.text(), "v,w\n");
        assert_eq!(
            meta.text(),
            "-- result 1: v:udt\\n-- ok\\x3A forged, w:a\\tb\\\\c\\x01d\\x3Ae\n"
        );
    }

    // R06: the Unicode line breaks are escaped on the metadata stream too. Data rows follow
    // CSV's own quoting and are left alone.
    #[test]
    fn unicode_line_breaks_are_escaped_in_metadata() {
        let (data, meta) = (Shared::default(), Shared::default());
        let mut s = CsvSink::new(data.clone(), Box::new(meta.clone()), Limits::default());
        s.columns(&[
            col("v", "udt\u{2028}-- ok: forged"),
            col("w", "a\u{85}b\u{2029}c"),
        ])
        .unwrap();
        assert_eq!(
            meta.text(),
            "-- result 1: v:udt\\u2028-- ok\\x3A forged, w:a\\x85b\\u2029c\n"
        );
    }

    // F06: the budget counts both streams; row counts collapse on the metadata stream too.
    #[test]
    fn output_budget_covers_both_streams() {
        let (data, meta) = (Shared::default(), Shared::default());
        let limits = Limits {
            max_rows: 0,
            max_cell: 0,
            max_results: 0,
            max_messages: 0,
            // 33 bytes of row counts, 28 of header, room for two 10-byte rows.
            max_output: crate::output::OUTPUT_RESERVE + 81,
            stall_secs: 0,
        };
        let mut s = CsvSink::new(data.clone(), Box::new(meta.clone()), limits);
        s.rows_affected(2).unwrap();
        s.rows_affected(3).unwrap();
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
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        assert_eq!(data.text(), "v\naaaaaaaaa\naaaaaaaaa\n");
        assert_eq!(
            meta.text(),
            "-- 2 statements, 5 rows affected\n-- result 1: v:varchar(9)\n\
             -- 2 of 5 rows shown (--max-output)\n\
             -- note: output stopped at the --max-output limit of 1105 bytes; the run \
             continued, and these weren't shown: 1 message\n\
             -- ok: 1 batch, 3 ms\n"
        );
    }

    #[test]
    fn result_sets_are_separated_by_a_blank_line() {
        let data = Shared::default();
        let mut s = CsvSink::new(data.clone(), Box::new(Shared::default()), Limits::default());
        for value in ["a", "b"] {
            s.columns(&[col("x", "varchar(1)")]).unwrap();
            s.row(&[Value::Text(value.into())]).unwrap();
            s.end_rows().unwrap();
        }
        assert_eq!(data.text(), "x\na\n\nx\nb\n");
    }

    // The whole-response budget (R08): the same cases as the text format, on the metadata
    // stream, which is where csv puts notes, refusals, diagnostics and the status.

    fn sink(limits: Limits) -> (CsvSink<Shared>, Shared, Shared) {
        let (data, meta) = (Shared::default(), Shared::default());
        let sink = CsvSink::new(data.clone(), Box::new(meta.clone()), limits);
        (sink, data, meta)
    }

    fn total(data: &Shared, meta: &Shared) -> usize {
        data.bytes() + meta.bytes()
    }

    #[test]
    fn a_huge_refusal_is_shortened_to_the_budget() {
        let (mut s, data, meta) = sink(capped(4096));
        s.blocked("line 1", &quoting(20_000), ADVICE).unwrap();
        s.finish(&Summary {
            exit: 5,
            status: "failed (exit 5): blocked by the read-only guard, 0 ms",
            error: None,
            elapsed_ms: 0,
        })
        .unwrap();
        assert_eq!(data.bytes(), 0);
        let out = meta.text();
        assert!(total(&data, &meta) <= 4096, "{} bytes", total(&data, &meta));
        let blocked = out.lines().next().unwrap();
        assert!(
            blocked.starts_with("-- blocked: line 1: UPDATE [qqqq"),
            "{blocked}"
        );
        assert!(
            blocked.ends_with(ADVICE),
            "the advice must survive: {blocked}"
        );
        assert_eq!(
            blocked.matches('q').count() + cut_by(blocked),
            20_000 + "] would write".len()
        );
        assert_eq!(
            out.lines().last().unwrap(),
            "-- failed (exit 5): blocked by the read-only guard, 0 ms"
        );
        assert_eq!(out.lines().count(), 2);
    }

    // Exact output, as in the text format: the arithmetic of a tight cap, on the metadata
    // stream. The data stream is empty.
    #[test]
    fn a_tight_cap_shortens_exactly() {
        let (mut s, data, meta) = sink(capped(600));
        s.note(&"x".repeat(200)).unwrap();
        let error = "e".repeat(1000);
        s.finish(&Summary {
            exit: 1,
            status: "ok: 0 batches, 3 ms",
            error: Some(&error),
            elapsed_ms: 3,
        })
        .unwrap();
        assert_eq!(data.bytes(), 0);
        assert_eq!(
            meta.text(),
            format!(
                "-- note: {}…[+131]\n-- error: {}…[+532]\n-- ok: 0 batches, 3 ms\n",
                "x".repeat(69),
                "e".repeat(468)
            )
        );
    }

    #[test]
    fn a_huge_closing_error_is_shortened_and_the_status_is_not() {
        let (mut s, data, meta) = sink(capped(4096));
        let error = "z".repeat(30_000);
        s.finish(&Summary {
            exit: 2,
            status: "failed (exit 2): connection failed, 5 ms",
            error: Some(&error),
            elapsed_ms: 5,
        })
        .unwrap();
        let out = meta.text();
        assert_eq!(data.bytes(), 0);
        assert!(total(&data, &meta) <= 4096, "{} bytes", out.len());
        assert!(
            out.len() >= 4096 - 8,
            "the error takes the room: {}",
            out.len()
        );
        let first = out.lines().next().unwrap();
        assert_eq!(first.matches('z').count() + cut_by(first), 30_000);
        assert_eq!(
            out.lines().last().unwrap(),
            "-- failed (exit 2): connection failed, 5 ms"
        );
    }

    #[test]
    fn long_notes_are_shortened_and_late_ones_counted() {
        let (mut s, data, meta) = sink(capped(4096));
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
        assert!(total(&data, &meta) <= 4096);
        let out = meta.text();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert_eq!(lines[0].matches('k').count() + cut_by(lines[0]), 10_000);
        assert_eq!(lines[1].matches('z').count() + cut_by(lines[1]), 10_000);
        assert!(
            lines[2].ends_with("these weren't shown: 1 note"),
            "{}",
            lines[2]
        );
        assert_eq!(lines[3], "-- failed (exit 1): 1 error, 3 ms");
    }

    #[test]
    fn escapes_that_expand_are_counted_after_escaping() {
        for (ch, written) in [
            ('\u{1}', "\\x01"),
            ('\u{2028}', "\\u2028"),
            ('\\', "\\\\"),
            ('日', "日"),
            ('😀', "😀"),
        ] {
            let (mut s, data, meta) = sink(capped(4096));
            let reason = format!("[{}]", ch.to_string().repeat(5000));
            s.blocked("line 1", &reason, ADVICE).unwrap();
            s.finish(&summary("failed (exit 5): x, 0 ms")).unwrap();
            assert!(total(&data, &meta) <= 4096, "{ch:?}");
            let out = meta.text();
            let blocked = out.lines().next().unwrap();
            let marker = blocked.rfind("…[+").unwrap();
            assert!(blocked[..marker].ends_with(written), "{ch:?}: {blocked}");
            let kept = blocked[..marker].matches(written).count();
            assert_eq!(kept + cut_by(blocked), 5001, "{ch:?}");
        }
    }

    // Wherever the body ends, the data and the metadata together stay within the cap, and
    // the metadata ends with the status.
    #[test]
    fn the_cap_holds_wherever_the_body_ends() {
        use crate::output::OUTPUT_RESERVE;
        for max in OUTPUT_RESERVE..OUTPUT_RESERVE + 1800 {
            for (run, error, status) in [
                (
                    ragged_run as fn(&mut dyn Output),
                    "-- error: closing error ",
                    RAGGED_STATUS,
                ),
                (
                    interrupted_run,
                    "-- error: query timeout ",
                    INTERRUPTED_STATUS,
                ),
            ] {
                let (mut s, data, meta) = sink(capped(max));
                run(&mut s);
                let spent = total(&data, &meta) as u64;
                assert!(
                    spent <= max,
                    "cap {max}: {spent} bytes\n{}{}",
                    data.text(),
                    meta.text()
                );
                let out = meta.text();
                assert!(out.contains(error), "cap {max}: no closing error\n{out}");
                assert!(
                    out.lines().last().unwrap().ends_with(status),
                    "cap {max}: {out}"
                );
                assert!(out.ends_with('\n'));
            }
        }
        let (mut s, data, meta) = sink(capped(0));
        ragged_run(&mut s);
        assert!(!data.text().contains("…[+") && !meta.text().contains("…[+"));
    }

    // Diagnostics are lines of the metadata stream: `sqlh: ...` among the `-- ` lines.
    #[test]
    fn diagnostics_join_the_metadata_stream_and_count_against_the_budget() {
        let (mut s, data, meta) = sink(capped(4096));
        s.diagnostic("connected in 5 ms; server SQL01\\INST")
            .unwrap();
        s.note("a note").unwrap();
        s.diagnostic(&"d".repeat(20_000)).unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        let out = meta.text();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "sqlh: connected in 5 ms; server SQL01\\\\INST");
        assert_eq!(lines[1], "-- note: a note");
        assert_eq!(lines[2].matches('d').count() + cut_by(lines[2]), 20_000);
        assert_eq!(lines[3], "-- ok: 0 batches, 3 ms");
        assert_eq!(data.bytes(), 0);
        assert!(total(&data, &meta) <= 4096);
    }
}
