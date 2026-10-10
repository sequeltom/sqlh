//! `--format csv` (DESIGN.md §5.3): RFC 4180 fields with LF line endings. The data stream
//! (stdout or `-o`) holds only CSV: a header row of column names, the rows, and a blank line
//! between result sets. Everything else (result markers with types, row counts, messages,
//! errors, the status) goes to `meta` (stderr) as text-format `-- ` lines.

use std::io::{self, Write};

use super::text::{
    batch_line, blocked_line, closing_lines, error_line, message_line, note_line,
    rows_affected_line,
};
use super::{BatchInfo, Limits, Output, Summary, Tally, cut, cut_marker, escaped, hex, push_type};
use crate::db::{BatchSink, Column, ServerMessage, Value};

pub struct CsvSink<W: Write> {
    data: W,
    meta: Box<dyn Write>,
    tally: Tally,
    line: String,
    results_written: usize,
}

impl<W: Write> CsvSink<W> {
    pub fn new(data: W, meta: Box<dyn Write>, limits: Limits) -> Self {
        CsvSink {
            data,
            meta,
            tally: Tally::new(limits),
            line: String::with_capacity(1024),
            results_written: 0,
        }
    }

    fn meta_line(&mut self, line: &str) -> io::Result<()> {
        writeln!(self.meta, "{line}")
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
        let Some(number) = self.tally.start_result() else {
            return Ok(());
        };
        if self.results_written > 0 {
            self.data.write_all(b"\n")?;
        }
        self.results_written += 1;
        self.line.clear();
        let mut types = String::new();
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                self.line.push(',');
                types.push_str(", ");
            }
            push_field(&mut self.line, &column.name);
            types.push_str(&escaped(&column.name));
            types.push(':');
            push_type(&mut types, &column.type_name);
        }
        self.line.push('\n');
        self.data.write_all(self.line.as_bytes())?;
        self.meta_line(&format!("-- result {number}: {types}"))
    }

    fn wants_values(&self) -> bool {
        self.tally.wants_values()
    }

    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        self.tally.row_shown();
        let max = self.tally.limits().max_cell;
        self.line.clear();
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                self.line.push(',');
            }
            match value {
                Value::Null => {}
                Value::Int(v) => self.line.push_str(&v.to_string()),
                Value::Num(s) | Value::Lit(s) => push_field(&mut self.line, s),
                Value::Text(s) => {
                    let (kept, cut_count) = cut(s, max);
                    if cut_count > 0 {
                        self.tally.cell_cut();
                        push_field(
                            &mut self.line,
                            &format!("{kept}{}", cut_marker(cut_count, false)),
                        );
                    } else {
                        push_field(&mut self.line, kept);
                    }
                }
                Value::Bin(bytes) => {
                    let (mut text, cut_count) = hex(bytes, max);
                    if cut_count > 0 {
                        self.tally.cell_cut();
                        text.push_str(&cut_marker(cut_count, true));
                    }
                    self.line.push_str(&text);
                }
            }
        }
        self.line.push('\n');
        self.data.write_all(self.line.as_bytes())
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
        self.meta_line(&rows_affected_line(count))
    }

    fn message(&mut self, message: &ServerMessage) -> io::Result<()> {
        if !self.tally.message_visible() {
            return Ok(());
        }
        self.meta_line(&message_line(message))
    }

    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        self.tally.error(error.severity);
        let line = error_line(&self.tally, error);
        self.meta_line(&line)
    }
}

impl<W: Write> Output for CsvSink<W> {
    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()> {
        self.tally.begin_batch(batch);
        if batch.total > 1 {
            let marker = batch_line(batch);
            self.meta_line(&marker)?;
        }
        Ok(())
    }

    fn end_batch(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn note(&mut self, text: &str) -> io::Result<()> {
        self.meta_line(&note_line(text))
    }

    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()> {
        self.meta_line(&blocked_line(location, reason, advice))
    }

    fn finish(&mut self, summary: &Summary) -> io::Result<()> {
        self.data.flush()?;
        let lines = closing_lines(&self.tally, summary);
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
    use super::super::text::tests::{col, error, limits, msg, summary};
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A `Write` whose bytes stay readable after the sink takes ownership of a clone.
    #[derive(Clone, Default)]
    struct Shared(Rc<RefCell<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

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
}
