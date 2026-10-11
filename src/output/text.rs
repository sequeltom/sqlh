//! The default format (DESIGN.md §5.1): `-- ` metadata lines, a `name:type` header per
//! result set, tab-separated rows. The metadata-line helpers are shared with csv, which
//! writes the same lines to stderr.

use std::fmt::Write as _;
use std::io::{self, Write};

use super::{
    BatchInfo, Layout, Limits, Output, Summary, Tally, escaped, hex, push_blocked, push_char,
    push_escaped, push_type,
};
use crate::db::{BatchSink, Column, ServerMessage, Value};

pub struct TextSink<W: Write> {
    out: W,
    /// stderr, for `Output::diagnostic` lines only.
    meta: Box<dyn Write>,
    tally: Tally,
    line: String,
    /// Bytes written so far to both streams, for the output budget.
    written: u64,
}

impl<W: Write> TextSink<W> {
    /// A sink whose diagnostics go nowhere, for tests that look at the main stream only.
    #[cfg(test)]
    pub fn new(out: W, limits: Limits) -> Self {
        Self::with_meta(out, Box::new(io::sink()), limits)
    }

    pub fn with_meta(out: W, meta: Box<dyn Write>, limits: Limits) -> Self {
        TextSink {
            out,
            meta,
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

    /// Writes a line whatever the budget: the footer of a result set that was shown. The
    /// budget admitted that result's header, so the footer comes out of the reserve, which
    /// holds one (`STATUS_RESERVE`, and the room the closing lines get).
    fn write_line(&mut self, text: &str) -> io::Result<()> {
        self.written += text.len() as u64 + 1;
        self.out.write_all(text.as_bytes())?;
        self.out.write_all(b"\n")
    }

    /// Writes a note or a refusal (`layout` is the whole line, newline included), shortened
    /// to the room the budget has left before the status reserve; counted as withheld when
    /// not even that fits.
    fn write_fitted(&mut self, layout: &Layout) -> io::Result<()> {
        match layout.fit(self.tally.note_room(self.written), push_char) {
            Some(line) => {
                self.written += line.len() as u64;
                self.out.write_all(line.as_bytes())
            }
            None => {
                self.tally.withhold_note();
                Ok(())
            }
        }
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
        // Cut by the read, to the sink's own limit: the marker is all that's left to add.
        Value::TextCut(s, more) => {
            if first && s.starts_with("--") {
                line.push('\\');
            }
            push_escaped(line, s, 0);
            line.push_str(&super::cut_marker(*more, false));
            return true;
        }
        Value::BinCut(bytes, more) => {
            line.push_str(&hex(bytes, 0).0);
            line.push_str(&super::cut_marker(*more, true));
            return true;
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

/// `-- blocked: line 3: UPDATE dbo.T would write; nothing was run. <advice>` as a line to
/// fit into the budget (see `Layout`).
pub fn blocked_layout<'a>(location: &'a str, reason: &'a str, advice: &'a str) -> Layout<'a> {
    let mut layout = Layout::new();
    layout.text("-- blocked: ");
    push_blocked(&mut layout, location, reason, advice);
    layout.text("\n");
    layout
}

/// `-- note: text` as a line to fit into the budget.
pub fn note_layout(text: &str) -> Layout<'_> {
    let mut layout = Layout::new();
    layout.text("-- note: ").part(text).text("\n");
    layout
}

pub fn batch_line(batch: &BatchInfo) -> String {
    format!("-- batch {} ({})", batch.number, escaped(&batch.location()))
}

/// `sqlh: text` on the diagnostics stream, flushed at once so that it shows while the run
/// goes on. Shortened to the room the budget has left before the status reserve; counted as
/// withheld when not even that fits. Returns the bytes written, for the sink's count.
pub fn write_diagnostic(
    meta: &mut dyn Write,
    tally: &mut Tally,
    used: u64,
    text: &str,
) -> io::Result<u64> {
    let mut layout = Layout::new();
    layout.text("sqlh: ").part(text).text("\n");
    match layout.fit(tally.note_room(used), push_char) {
        Some(line) => {
            meta.write_all(line.as_bytes())?;
            meta.flush()?;
            Ok(line.len() as u64)
        }
        None => {
            tally.withhold_note();
            Ok(0)
        }
    }
}

/// The closing lines: the error that stopped the run, limit notes, then the status. Given
/// the `used` bytes written so far, the status is laid out first and never shortened; the
/// error and the notes share what it leaves, each shortened (`…[+N]`) only as far as needed.
pub fn closing_lines(tally: &Tally, summary: &Summary, used: u64) -> String {
    let notes = tally.limit_notes();
    let mut layout = Layout::new();
    if let Some(error) = summary.error {
        layout.text("-- error: ").part(error).text("\n");
    }
    for note in &notes {
        layout.text("-- note: ").part(note).text("\n");
    }
    layout.text("-- ").text(summary.status).text("\n");
    layout
        .fit(tally.closing_room(used), push_char)
        // No room for even a cut error or note (the reserve makes this unreachable): the
        // status alone.
        .unwrap_or_else(|| format!("-- {}\n", summary.status))
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

    fn cell_limit(&self) -> usize {
        self.tally.cell_limit()
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
        let lines = closing_lines(&self.tally, summary, self.written);
        self.written += lines.len() as u64;
        self.out.write_all(lines.as_bytes())
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()?;
        self.meta.flush()
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
            stall_secs: 0,
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

    // Helpers for the tests of the whole-response budget, shared by all three formats.

    /// A `Write` whose bytes stay readable after a sink takes a clone of it.
    #[derive(Clone, Default)]
    pub struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

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
        pub fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("UTF-8 output")
        }
        pub fn bytes(&self) -> usize {
            self.0.borrow().len()
        }
    }

    /// No limits but the total output, `max_output` bytes.
    pub fn capped(max_output: u64) -> Limits {
        Limits {
            max_rows: 0,
            max_cell: 0,
            max_results: 0,
            max_messages: 0,
            max_output,
            stall_secs: 0,
        }
    }

    pub const ADVICE: &str = "Rerun with --allow-write if the write is intended";

    /// A refusal's reason that quotes an identifier of `n` characters, `q` being a letter
    /// that nothing else in the message or the status line contains.
    pub fn quoting(n: usize) -> String {
        format!("UPDATE [{}] would write", "q".repeat(n))
    }

    /// The number N of the last cut marker `…[+N]` in `text`.
    pub fn cut_by(text: &str) -> usize {
        let start = text.rfind("…[+").expect("a cut marker") + "…[+".len();
        let digits: String = text[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().expect("a count")
    }

    /// A run that makes every kind of write: a diagnostic and a note first, a batch with a
    /// result of many rows, a message, a row count and an error, a second batch, a late
    /// note, and a closing error and status. Run it with any budget; the response must
    /// stay within it and end with the status.
    pub fn ragged_run(out: &mut dyn Output) {
        out.diagnostic("connected in 12 ms; server SQL01 version 17.0.1000, database master")
            .unwrap();
        out.note("ignored sqlcmd flags -W -s (sqlh output is always compact)")
            .unwrap();
        let first = BatchInfo {
            total: 2,
            ..batch("-Q", false, 1)
        };
        out.begin_batch(&first).unwrap();
        out.columns(&[col("id", "int"), col("v", "varchar(20)")])
            .unwrap();
        for i in 0..60 {
            if out.wants_values() {
                out.row(&[Value::Int(i), Value::Text(format!("value {i}"))])
                    .unwrap();
            } else {
                out.skipped_row();
            }
        }
        out.end_rows().unwrap();
        out.message(&msg("a message after the rows")).unwrap();
        out.rows_affected(3).unwrap();
        out.error(&error(1)).unwrap();
        out.end_batch().unwrap();
        let second = BatchInfo {
            number: 2,
            total: 2,
            ..batch("-Q", false, 3)
        };
        out.begin_batch(&second).unwrap();
        out.columns(&[col("w", "int")]).unwrap();
        out.row(&[Value::Int(1)]).unwrap();
        out.end_rows().unwrap();
        out.end_batch().unwrap();
        out.note(&"a late note ".repeat(40)).unwrap();
        let closing = "closing error ".repeat(60);
        out.finish(&Summary {
            exit: 1,
            status: RAGGED_STATUS,
            error: Some(&closing),
            elapsed_ms: 7,
        })
        .unwrap();
    }

    pub const RAGGED_STATUS: &str = "failed (exit 1): 1 error, first in batch 1, 7 ms";

    /// A run that dies in the middle of a result (a timeout): no footer, and JSON has a
    /// result and a batch to close.
    pub fn interrupted_run(out: &mut dyn Output) {
        out.begin_batch(&batch("-Q", false, 1)).unwrap();
        out.columns(&[col("id", "int"), col("v", "varchar(20)")])
            .unwrap();
        for i in 0..60 {
            if out.wants_values() {
                out.row(&[Value::Int(i), Value::Text(format!("value {i}"))])
                    .unwrap();
            } else {
                out.skipped_row();
            }
        }
        let closing = "query timeout ".repeat(80);
        out.finish(&Summary {
            exit: 4,
            status: INTERRUPTED_STATUS,
            error: Some(&closing),
            elapsed_ms: 2000,
        })
        .unwrap();
    }

    pub const INTERRUPTED_STATUS: &str = "failed (exit 4): timeout, 2000 ms";

    /// A text sink with a stream each for the main output and the diagnostics.
    fn sink(limits: Limits) -> (TextSink<Shared>, Shared, Shared) {
        let (main, meta) = (Shared::default(), Shared::default());
        let sink = TextSink::with_meta(main.clone(), Box::new(meta.clone()), limits);
        (sink, main, meta)
    }

    // R08: a guard refusal that quotes a 20,000-character identifier is shortened in its
    // variable part, so the response stays within --max-output, the advice survives, and
    // the status line is still last.
    #[test]
    fn a_huge_refusal_is_shortened_to_the_budget() {
        let (mut s, main, meta) = sink(capped(4096));
        s.blocked("line 1", &quoting(20_000), ADVICE).unwrap();
        s.finish(&Summary {
            exit: 5,
            status: "failed (exit 5): blocked by the read-only guard, 0 ms",
            error: None,
            elapsed_ms: 0,
        })
        .unwrap();
        let out = main.text();
        assert_eq!(meta.bytes(), 0);
        assert!(out.len() <= 4096, "{} bytes", out.len());
        let blocked = out.lines().next().unwrap();
        assert!(
            blocked.starts_with("-- blocked: line 1: UPDATE [qqqq"),
            "{blocked}"
        );
        assert!(
            blocked.contains(&format!("]; nothing was run. {ADVICE}")) && blocked.ends_with(ADVICE),
            "the advice must survive: {blocked}"
        );
        // The count is exact: the identifier and the "] would write" after it.
        assert_eq!(
            blocked.matches('q').count() + cut_by(blocked),
            20_000 + "] would write".len()
        );
        // It takes the room that is left before the status reserve, give or take the digits
        // of the count: the cut isn't wasteful.
        let room = (4096 - crate::output::STATUS_RESERVE) as usize;
        assert!(
            (room - 8..=room).contains(&(blocked.len() + 1)),
            "{} of {room}",
            blocked.len() + 1
        );
        assert_eq!(
            out.lines().last().unwrap(),
            "-- failed (exit 5): blocked by the read-only guard, 0 ms"
        );
        assert_eq!(out.lines().count(), 2);
    }

    // R08, exact output: the arithmetic of a tight cap. A note may use what the cap has left
    // less the 512-byte status reserve: 600 - 512 = 88 bytes, of which "-- note: " and the
    // newline take 10, "…[+131]" 9 and 69 characters the other 69. The error and the status
    // then share the 512 bytes that are left: 23 for the status line, 11 around the error,
    // 10 for "…[+1000]"'s widest form, so 468 characters of the error and "…[+532]".
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
                "-- note: {}…[+131]\n-- error: {}…[+532]\n-- ok: 0 batches, 3 ms\n",
                "x".repeat(69),
                "e".repeat(468)
            )
        );
    }

    // R08: a huge closing error is shortened, the status line is not.
    #[test]
    fn a_huge_closing_error_is_shortened_and_the_status_is_not() {
        let (mut s, main, meta) = sink(capped(4096));
        let error = "z".repeat(30_000);
        s.finish(&Summary {
            exit: 2,
            status: "failed (exit 2): connection failed, 5 ms",
            error: Some(&error),
            elapsed_ms: 5,
        })
        .unwrap();
        let out = main.text();
        assert_eq!(meta.bytes(), 0);
        assert!(out.len() <= 4096, "{} bytes", out.len());
        assert!(
            out.len() >= 4096 - 8,
            "the error takes the room: {}",
            out.len()
        );
        let first = out.lines().next().unwrap();
        assert!(first.starts_with("-- error: zzzz"), "{first}");
        assert_eq!(first.matches('z').count() + cut_by(first), 30_000);
        assert_eq!(
            out.lines().last().unwrap(),
            "-- failed (exit 2): connection failed, 5 ms"
        );
        assert_eq!(out.lines().count(), 2);
    }

    // R08: a long note is shortened to the room before the status reserve and counts as it
    // is written, so a second long one finds no room and is counted as withheld; the closing
    // note says so, and the closing error shares what is left with it.
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
        let out = main.text();
        assert!(out.len() <= 4096, "{} bytes", out.len());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(lines[0].starts_with("-- note: kkkk"), "{}", lines[0]);
        assert_eq!(lines[0].matches('k').count() + cut_by(lines[0]), 10_000);
        assert!(lines[1].starts_with("-- error: zzzz"), "{}", lines[1]);
        assert_eq!(lines[1].matches('z').count() + cut_by(lines[1]), 10_000);
        assert_eq!(
            lines[2],
            "-- note: output stopped at the --max-output limit of 4096 bytes; the run \
             continued, and these weren't shown: 1 note"
        );
        assert_eq!(lines[3], "-- failed (exit 1): 1 error, 3 ms");
    }

    // R08: the budget counts encoded bytes, after escaping: a control character becomes four
    // bytes, U+2028 six, and a wide character keeps its UTF-8 length. A cut never splits an
    // escape or a character, and the count N is in characters, not bytes.
    #[test]
    fn escapes_that_expand_are_counted_after_escaping() {
        for (ch, written) in [
            ('\u{1}', "\\x01"),
            ('\u{85}', "\\x85"),
            ('\u{2028}', "\\u2028"),
            ('\\', "\\\\"),
            ('é', "é"),
            ('日', "日"),
            ('😀', "😀"),
        ] {
            let (mut s, main, _) = sink(capped(4096));
            let reason = format!("[{}]", ch.to_string().repeat(5000));
            s.blocked("line 1", &reason, ADVICE).unwrap();
            s.finish(&summary("failed (exit 5): x, 0 ms")).unwrap();
            let out = main.text();
            assert!(out.len() <= 4096, "{ch:?}: {} bytes", out.len());
            let blocked = out.lines().next().unwrap();
            let marker = blocked.rfind("…[+").unwrap();
            assert!(blocked[..marker].ends_with(written), "{ch:?}: {blocked}");
            // The "[" in front is kept, the "]" behind is cut, and the count is exact.
            let kept = blocked[..marker].matches(written).count();
            assert_eq!(kept + cut_by(blocked), 5001, "{ch:?}");
            assert!(blocked.ends_with(ADVICE), "{ch:?}: {blocked}");
        }
    }

    // R08: wherever the body ends, the whole response stays within the cap and ends with the
    // status. The cap runs from just the reserve (nothing of the body fits) up past the
    // point where everything but the closing text fits.
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
                let (mut s, main, meta) = sink(capped(max));
                run(&mut s);
                let out = main.text();
                let total = (main.bytes() + meta.bytes()) as u64;
                assert!(
                    total <= max,
                    "cap {max}: {total} bytes\n{out}{}",
                    meta.text()
                );
                // The reserve leaves room for the closing error (shortened if need be) and
                // the status, wherever the body stopped.
                assert!(out.contains(error), "cap {max}: no closing error\n{out}");
                assert!(
                    out.lines().last().unwrap().ends_with(status),
                    "cap {max}: {out}"
                );
                assert!(out.ends_with('\n'));
            }
        }
        // Without a cap nothing is shortened.
        let (mut s, main, meta) = sink(capped(0));
        ragged_run(&mut s);
        assert!(!main.text().contains("…[+") && !meta.text().contains("…[+"));
    }

    // R08: `--verbose` lines go to the second stream as `sqlh: ...`, escaped like a note,
    // and count against the budget; a huge one is shortened, one that cannot fit is counted.
    #[test]
    fn diagnostics_go_to_the_second_stream_and_count_against_the_budget() {
        let (mut s, main, meta) = sink(capped(4096));
        s.diagnostic("connected in 5 ms; server SQL01\\INST, a\nb")
            .unwrap();
        assert_eq!(
            meta.text(),
            "sqlh: connected in 5 ms; server SQL01\\\\INST, a\\nb\n"
        );
        assert_eq!(main.bytes(), 0);
        s.diagnostic(&"d".repeat(20_000)).unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        let lines = meta.text();
        let long = lines.lines().nth(1).unwrap();
        assert!(long.starts_with("sqlh: dddd"), "{long}");
        assert_eq!(long.matches('d').count() + cut_by(long), 20_000);
        assert_eq!(main.text(), "-- ok: 0 batches, 3 ms\n");
        assert!(main.bytes() + meta.bytes() <= 4096);

        // Too little room for even a marker: the line is dropped and counted.
        let (mut s, main, meta) = sink(capped(520));
        s.diagnostic(&"d".repeat(100)).unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(meta.bytes(), 0);
        assert_eq!(
            main.text(),
            "-- note: output stopped at the --max-output limit of 520 bytes; the run \
             continued, and these weren't shown: 1 note\n-- ok: 0 batches, 3 ms\n"
        );
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
            stall_secs: 0,
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
