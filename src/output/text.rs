//! The default format (DESIGN.md §5.1): `-- ` metadata lines, a `name:type` header per
//! result set, tab-separated rows. The metadata-line helpers are shared with csv, which
//! writes the same lines to stderr.

use std::fmt::Write as _;
use std::io::{self, Write};

use super::{
    BatchInfo, Limits, Output, Summary, Tally, blocked_message, escaped, hex, push_escaped,
    push_type,
};
use crate::db::{BatchSink, Column, ServerMessage, Value};

pub struct TextSink<W: Write> {
    out: W,
    tally: Tally,
    line: String,
    /// Bytes written so far, for the output budget.
    written: u64,
}

impl<W: Write> TextSink<W> {
    pub fn new(out: W, limits: Limits) -> Self {
        TextSink {
            out,
            tally: Tally::new(limits),
            line: String::with_capacity(1024),
            written: 0,
        }
    }

    /// Writes `text` (whole lines, without the last newline) if the output budget admits
    /// it, and says whether it did.
    fn emit(&mut self, text: &str) -> io::Result<bool> {
        if !self.tally.admit(self.written, text.len() + 1) {
            return Ok(false);
        }
        self.write_line(text)?;
        Ok(true)
    }

    /// Writes a line whatever the budget: the footer of a result set that was shown, notes,
    /// the guard's refusal. There are few of these, and the closing lines use the reserve.
    fn write_line(&mut self, text: &str) -> io::Result<()> {
        self.written += text.len() as u64 + 1;
        self.out.write_all(text.as_bytes())?;
        self.out.write_all(b"\n")
    }

    /// Writes the pending run of row counts as one line, before any other event.
    fn flush_affected(&mut self) -> io::Result<()> {
        if let Some((statements, rows)) = self.tally.take_affected()
            && !self.emit(&rows_affected_line(statements, rows))?
        {
            self.tally.withhold_row_counts(statements);
        }
        Ok(())
    }
}

/// Appends one value in the text format. Returns true when `--max-cell` cut it.
fn push_value(line: &mut String, max: usize, value: &Value, first: bool) -> bool {
    match value {
        Value::Null => line.push_str("NULL"),
        Value::Int(v) => {
            let _ = write!(line, "{v}");
        }
        Value::Num(s) | Value::Lit(s) => line.push_str(s),
        Value::Text(s) if s == "NULL" => line.push_str("\\NULL"),
        Value::Text(s) => {
            if first && s.starts_with("--") {
                line.push('\\');
            }
            return push_escaped(line, s, max);
        }
        Value::Bin(bytes) => {
            let (text, cut) = hex(bytes, max);
            line.push_str(&text);
            if cut > 0 {
                line.push_str(&super::cut_marker(cut, true));
                return true;
            }
        }
    }
    false
}

/// `-- msg: text`
pub fn message_line(message: &ServerMessage) -> String {
    let mut line = String::from("-- msg: ");
    push_escaped(&mut line, &message.text, 0);
    line
}

/// `-- error 208 (severity 16, state 1, line 14): text`, with the line mapped to the input.
pub fn error_line(tally: &Tally, error: &ServerMessage) -> String {
    let location = match (&error.procedure, error.line) {
        (Some(procedure), Some(line)) => format!(", proc {}, line {line}", escaped(procedure)),
        (Some(procedure), None) => format!(", proc {}", escaped(procedure)),
        (None, _) => match (tally.source_line(error), tally.source_name()) {
            (Some(line), Some(name)) => format!(", {}:{line}", escaped(name)),
            (Some(line), None) => format!(", line {line}"),
            (None, _) => String::new(),
        },
    };
    let mut line = format!(
        "-- error {} (severity {}, state {}{location}): ",
        error.number, error.severity, error.state
    );
    push_escaped(&mut line, &error.text, 0);
    line
}

/// `-- 5 rows affected`, or for a run of statements `-- 300 statements, 300 rows affected`.
pub fn rows_affected_line(statements: u64, rows: u64) -> String {
    let s = if rows == 1 { "" } else { "s" };
    if statements == 1 {
        format!("-- {rows} row{s} affected")
    } else {
        format!("-- {statements} statements, {rows} row{s} affected")
    }
}

pub fn blocked_line(location: &str, reason: &str, advice: &str) -> String {
    format!(
        "-- blocked: {}",
        escaped(&blocked_message(location, reason, advice))
    )
}

pub fn note_line(text: &str) -> String {
    format!("-- note: {}", escaped(text))
}

pub fn batch_line(batch: &BatchInfo) -> String {
    format!("-- batch {} ({})", batch.number, escaped(&batch.location()))
}

/// The closing lines: the error that stopped the run, limit notes, then the status.
pub fn closing_lines(tally: &Tally, summary: &Summary) -> String {
    let mut lines = String::new();
    if let Some(error) = summary.error {
        lines.push_str("-- error: ");
        push_escaped(&mut lines, error, 0);
        lines.push('\n');
    }
    for note in tally.limit_notes() {
        let _ = writeln!(lines, "{}", note_line(&note));
    }
    let _ = writeln!(lines, "-- {}", summary.status);
    lines
}

impl<W: Write> BatchSink for TextSink<W> {
    fn columns(&mut self, columns: &[Column]) -> io::Result<()> {
        self.flush_affected()?;
        let Some(number) = self.tally.start_result() else {
            return Ok(());
        };
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        let _ = writeln!(line, "-- result {number}");
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                line.push('\t');
            }
            // Like values: a header can't start with `--` and pass for a metadata line.
            if i == 0 && column.name.starts_with("--") {
                line.push('\\');
            }
            push_escaped(&mut line, &column.name, 0);
            line.push(':');
            push_type(&mut line, &column.type_name);
        }
        let shown = self.emit(&line)?;
        self.line = line;
        if !shown {
            self.tally.withhold_result();
        }
        Ok(())
    }

    fn wants_values(&self) -> bool {
        self.tally.wants_values()
    }

    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        let max = self.tally.limits().max_cell;
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        let mut cut = 0;
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                line.push('\t');
            }
            if push_value(&mut line, max, value, i == 0) {
                cut += 1;
            }
        }
        let shown = self.emit(&line)?;
        self.line = line;
        if shown {
            self.tally.row_shown();
            for _ in 0..cut {
                self.tally.cell_cut();
            }
        } else {
            self.tally.row_withheld();
        }
        Ok(())
    }

    fn skipped_row(&mut self) {
        self.tally.row_skipped();
    }

    fn end_rows(&mut self) -> io::Result<()> {
        if self.tally.result_hidden() {
            return Ok(());
        }
        let footer = format!("-- {}", self.tally.footer());
        self.write_line(&footer)
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
        if !self.emit(&message_line(message))? {
            self.tally.withhold_message();
        }
        Ok(())
    }

    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        self.flush_affected()?;
        self.tally.error(error.severity);
        let line = error_line(&self.tally, error);
        if !self.emit(&line)? {
            self.tally.withhold_error();
        }
        Ok(())
    }
}

impl<W: Write> Output for TextSink<W> {
    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()> {
        self.flush_affected()?;
        self.tally.begin_batch(batch);
        if batch.total > 1 && !self.emit(&batch_line(batch))? {
            self.tally.withhold_batch();
        }
        Ok(())
    }

    fn end_batch(&mut self) -> io::Result<()> {
        self.flush_affected()
    }

    fn note(&mut self, text: &str) -> io::Result<()> {
        self.flush_affected()?;
        self.write_line(&note_line(text))
    }

    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()> {
        self.flush_affected()?;
        self.write_line(&blocked_line(location, reason, advice))
    }

    fn finish(&mut self, summary: &Summary) -> io::Result<()> {
        self.flush_affected()?;
        let lines = closing_lines(&self.tally, summary);
        self.written += lines.len() as u64;
        self.out.write_all(lines.as_bytes())
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn limits() -> Limits {
        Limits {
            max_rows: 2,
            max_cell: 5,
            max_results: 1,
            max_messages: 1,
            max_output: 0,
        }
    }

    pub fn col(name: &str, type_name: &str) -> Column {
        Column {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
        }
    }

    pub fn msg(text: &str) -> ServerMessage {
        ServerMessage {
            number: 0,
            severity: 0,
            state: 1,
            line: Some(1),
            procedure: None,
            text: text.into(),
        }
    }

    pub fn error(line: i32) -> ServerMessage {
        ServerMessage {
            number: 208,
            severity: 16,
            state: 1,
            line: Some(line),
            procedure: None,
            text: "Invalid object name 'dbo.Nope'.".into(),
        }
    }

    pub fn batch(source: &str, multi_source: bool, line: u32) -> BatchInfo<'_> {
        BatchInfo {
            number: 1,
            total: 1,
            source,
            multi_source,
            line,
            line_map: &[],
            run: 1,
            runs: 1,
        }
    }

    pub fn summary(status: &str) -> Summary<'_> {
        Summary {
            exit: 0,
            status,
            error: None,
            elapsed_ms: 3,
        }
    }

    fn text(sink: TextSink<Vec<u8>>) -> String {
        String::from_utf8(sink.out).unwrap()
    }

    #[test]
    fn result_set_format() {
        let mut s = TextSink::new(Vec::new(), limits());
        s.columns(&[
            col("id", "int"),
            col("", "nvarchar(50)"),
            col("b", "varbinary(max)"),
        ])
        .unwrap();
        s.row(&[
            Value::Int(1),
            Value::Text("a\tb".into()),
            Value::Bin(vec![0xAB, 0xCD, 0xEF]),
        ])
        .unwrap();
        assert!(s.wants_values());
        s.row(&[
            Value::Text("--x".into()),
            Value::Text("NULL".into()),
            Value::Null,
        ])
        .unwrap();
        assert!(!s.wants_values());
        s.skipped_row();
        s.end_rows().unwrap();
        assert_eq!(
            text(s),
            "-- result 1\nid:int\t:nvarchar(50)\tb:varbinary(max)\n\
             1\ta\\tb\t0xABCD…[+1 bytes]\n\
             \\--x\t\\NULL\tNULL\n\
             -- 2 of 3 rows shown (--max-rows 2); 1 value cut (--max-cell 5)\n"
        );
    }

    #[test]
    fn limits_hide_extra_results_and_messages() {
        let mut s = TextSink::new(Vec::new(), limits());
        for _ in 0..2 {
            s.columns(&[col("x", "int")]).unwrap();
            if s.wants_values() {
                s.row(&[Value::Int(1)]).unwrap();
            } else {
                s.skipped_row();
            }
            s.end_rows().unwrap();
        }
        s.message(&msg("first")).unwrap();
        s.message(&msg("second")).unwrap();
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        assert_eq!(
            text(s),
            "-- result 1\nx:int\n1\n-- 1 row\n-- msg: first\n\
             -- note: 1 more result sets not shown (--max-results 1)\n\
             -- note: 1 more messages not shown\n-- ok: 1 batch, 3 ms\n"
        );
    }

    // F11: no name, location, reason or note can start a line or pass for metadata.
    #[test]
    fn hostile_names_cannot_forge_metadata() {
        let mut s = TextSink::new(Vec::new(), limits());
        s.columns(&[col("-- forged", "int"), col("a\nb", "int")])
            .unwrap();
        s.end_rows().unwrap();
        s.begin_batch(&BatchInfo {
            number: 2,
            total: 2,
            ..batch("x\n-- ok: fake", true, 1)
        })
        .unwrap();
        s.error(&ServerMessage {
            procedure: Some("dbo.p\n-- ok: fake".into()),
            ..error(1)
        })
        .unwrap();
        s.blocked(
            "line 1",
            "UPDATE [a\n-- ok: fake] would write",
            "Rerun\n-- ok: fake",
        )
        .unwrap();
        s.note("one\n-- ok: fake").unwrap();
        let out = text(s);
        assert!(
            out.starts_with("-- result 1\n\\-- forged:int\ta\\nb:int\n"),
            "{out}"
        );
        assert!(out.contains("(x\\n-- ok: fake:1)"), "{out}");
        assert!(out.contains("proc dbo.p\\n-- ok: fake, line 1"), "{out}");
        assert!(out.contains("[a\\n-- ok: fake]"), "{out}");
        assert!(
            out.contains("nothing was run. Rerun\\n-- ok: fake"),
            "{out}"
        );
        assert!(out.contains("-- note: one\\n-- ok: fake"), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("-- ok")), "{out}");
    }

    // F06: a loop's row counts become one line per run; a message or batch end splits runs.
    #[test]
    fn consecutive_row_counts_become_one_line() {
        let mut s = TextSink::new(Vec::new(), limits());
        for rows in [1, 5, 6] {
            s.rows_affected(rows).unwrap();
        }
        s.message(&msg("done")).unwrap();
        s.rows_affected(1).unwrap();
        s.end_batch().unwrap();
        s.rows_affected(0).unwrap();
        s.rows_affected(0).unwrap();
        s.finish(&summary("ok: 2 batches, 3 ms")).unwrap();
        assert_eq!(
            text(s),
            "-- 3 statements, 12 rows affected\n-- msg: done\n-- 1 row affected\n\
             -- 2 statements, 0 rows affected\n-- ok: 2 batches, 3 ms\n"
        );
    }

    /// Limits that leave only the output budget: `body` bytes before the closing reserve.
    fn budget(body: u64) -> Limits {
        Limits {
            max_rows: 0,
            max_cell: 0,
            max_results: 0,
            max_messages: 0,
            max_output: crate::output::OUTPUT_RESERVE + body,
        }
    }

    // F06: past the budget nothing more of the body is written, the run's counts stay
    // exact, the cut result set says why in its footer, and the closing note says what else
    // was withheld.
    #[test]
    fn output_budget_stops_the_body_and_says_what_was_withheld() {
        let mut s = TextSink::new(Vec::new(), budget(50));
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
        s.columns(&[col("w", "int")]).unwrap();
        assert!(!s.wants_values());
        s.end_rows().unwrap();
        s.rows_affected(3).unwrap();
        s.rows_affected(4).unwrap();
        assert_eq!(s.tally().errors(), 1, "a withheld error still counts");
        s.finish(&summary("failed (exit 1): 1 error, 3 ms"))
            .unwrap();
        assert_eq!(
            text(s),
            "-- result 1\nv:varchar(9)\naaaaaaaaa\naaaaaaaaa\n\
             -- 2 of 5 rows shown (--max-output)\n\
             -- note: output stopped at the --max-output limit of 1074 bytes; the run \
             continued, and these weren't shown: 1 result set, 1 message, 1 error, \
             2 statements' row counts\n\
             -- failed (exit 1): 1 error, 3 ms\n"
        );
    }

    // R05: a CLR UDT's type name comes from the server, so it's escaped like a column name,
    // and a `:` in it can't move where the type starts.
    #[test]
    fn hostile_type_names_are_escaped() {
        let mut s = TextSink::new(Vec::new(), limits());
        s.columns(&[col("v", "udt\n-- ok: forged"), col("w", "a\tb\\c\x01d:e")])
            .unwrap();
        assert_eq!(
            text(s),
            "-- result 1\nv:udt\\n-- ok\\x3A forged\tw:a\\tb\\\\c\\x01d\\x3Ae\n"
        );
    }

    // R06: U+0085, U+2028 and U+2029 are line breaks to some consumers (Python's
    // `splitlines`), so they're escaped everywhere `\n` is: values, names, type names and
    // every metadata field.
    #[test]
    fn unicode_line_breaks_cannot_forge_metadata() {
        let mut s = TextSink::new(Vec::new(), limits());
        s.columns(&[
            col("a\u{2028}-- ok: forged", "int"),
            col("v", "udt\u{85}-- ok: forged"),
        ])
        .unwrap();
        s.row(&[
            Value::Text("a\u{2029}b".into()),
            Value::Text("c\u{85}d".into()),
        ])
        .unwrap();
        s.blocked(
            "line 1",
            "UPDATE [t\u{2028}-- ok: forged] would write",
            "Rerun\u{2029}-- ok: forged",
        )
        .unwrap();
        s.note("one\u{85}-- ok: forged").unwrap();
        let out = text(s);
        assert!(
            out.starts_with(
                "-- result 1\na\\u2028-- ok: forged:int\tv:udt\\x85-- ok\\x3A forged\n\
                 a\\u2029b\tc\\x85d\n"
            ),
            "{out}"
        );
        assert!(out.contains("[t\\u2028-- ok: forged]"), "{out}");
        assert!(
            out.contains("nothing was run. Rerun\\u2029-- ok: forged"),
            "{out}"
        );
        assert!(out.contains("-- note: one\\x85-- ok: forged"), "{out}");
        // Splitting at every Unicode line break gives exactly the lines that `\n` does.
        let breaks = |c: char| {
            matches!(
                c,
                '\n' | '\r' | '\x0b' | '\x0c' | '\x1c'
                    ..='\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        };
        assert_eq!(
            out.split(breaks).collect::<Vec<_>>(),
            out.split('\n').collect::<Vec<_>>(),
            "{out}"
        );
        assert!(!out.split(breaks).any(|l| l.starts_with("-- ok")), "{out}");
    }

    #[test]
    fn errors_map_lines_to_the_source() {
        let mut s = TextSink::new(Vec::new(), limits());
        s.begin_batch(&batch("b.sql", true, 10)).unwrap();
        s.error(&error(3)).unwrap();
        let in_proc = ServerMessage {
            procedure: Some("dbo.p".into()),
            ..error(3)
        };
        s.error(&in_proc).unwrap();
        assert_eq!(s.tally().batch_max_severity(), 16);
        assert_eq!(
            text(s),
            "-- error 208 (severity 16, state 1, b.sql:12): Invalid object name 'dbo.Nope'.\n\
             -- error 208 (severity 16, state 1, proc dbo.p, line 3): Invalid object name 'dbo.Nope'.\n"
        );
    }
}
