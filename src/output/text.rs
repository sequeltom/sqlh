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
}

impl<W: Write> TextSink<W> {
    pub fn new(out: W, limits: Limits) -> Self {
        TextSink {
            out,
            tally: Tally::new(limits),
            line: String::with_capacity(1024),
        }
    }

    fn push_value(&mut self, value: &Value, first: bool) {
        let max = self.tally.limits().max_cell;
        match value {
            Value::Null => self.line.push_str("NULL"),
            Value::Int(v) => {
                let _ = write!(self.line, "{v}");
            }
            Value::Num(s) | Value::Lit(s) => self.line.push_str(s),
            Value::Text(s) if s == "NULL" => self.line.push_str("\\NULL"),
            Value::Text(s) => {
                if first && s.starts_with("--") {
                    self.line.push('\\');
                }
                if push_escaped(&mut self.line, s, max) {
                    self.tally.cell_cut();
                }
            }
            Value::Bin(bytes) => {
                let (text, cut) = hex(bytes, max);
                self.line.push_str(&text);
                if cut > 0 {
                    self.line.push_str(&super::cut_marker(cut, true));
                    self.tally.cell_cut();
                }
            }
        }
    }
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

pub fn rows_affected_line(count: u64) -> String {
    let s = if count == 1 { "" } else { "s" };
    format!("-- {count} row{s} affected")
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
        let Some(number) = self.tally.start_result() else {
            return Ok(());
        };
        self.line.clear();
        let _ = writeln!(self.line, "-- result {number}");
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                self.line.push('\t');
            }
            // Like values: a header can't start with `--` and pass for a metadata line.
            if i == 0 && column.name.starts_with("--") {
                self.line.push('\\');
            }
            push_escaped(&mut self.line, &column.name, 0);
            self.line.push(':');
            push_type(&mut self.line, &column.type_name);
        }
        writeln!(self.out, "{}", self.line)
    }

    fn wants_values(&self) -> bool {
        self.tally.wants_values()
    }

    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        self.tally.row_shown();
        self.line.clear();
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                self.line.push('\t');
            }
            self.push_value(value, i == 0);
        }
        writeln!(self.out, "{}", self.line)
    }

    fn skipped_row(&mut self) {
        self.tally.row_skipped();
    }

    fn end_rows(&mut self) -> io::Result<()> {
        if self.tally.result_hidden() {
            return Ok(());
        }
        writeln!(self.out, "-- {}", self.tally.footer())
    }

    fn rows_affected(&mut self, count: u64) -> io::Result<()> {
        writeln!(self.out, "{}", rows_affected_line(count))
    }

    fn message(&mut self, message: &ServerMessage) -> io::Result<()> {
        if !self.tally.message_visible() {
            return Ok(());
        }
        writeln!(self.out, "{}", message_line(message))
    }

    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        self.tally.error(error.severity);
        writeln!(self.out, "{}", error_line(&self.tally, error))
    }
}

impl<W: Write> Output for TextSink<W> {
    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()> {
        self.tally.begin_batch(batch);
        if batch.total > 1 {
            writeln!(self.out, "{}", batch_line(batch))?;
        }
        Ok(())
    }

    fn end_batch(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn note(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.out, "{}", note_line(text))
    }

    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()> {
        writeln!(self.out, "{}", blocked_line(location, reason, advice))
    }

    fn finish(&mut self, summary: &Summary) -> io::Result<()> {
        self.out
            .write_all(closing_lines(&self.tally, summary).as_bytes())
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
