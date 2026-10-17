//! The default format (DESIGN.md §5.1): `-- ` metadata lines, a `name:type` header per
//! result set, tab-separated rows. The metadata-line helpers are shared with csv, which
//! writes the same lines to stderr.

use std::fmt::Write as _;
use std::io::{self, Write};

use super::{
    BatchInfo, Layout, Limits, NAME_CHARS, Output, Section, Summary, Tally, escaped, hex,
    push_blocked, push_char, push_escaped, push_type,
};
use crate::db::{BatchSink, Column, Connection, ServerMessage, Stop, Value};

pub struct TextSink<W: Write> {
    out: W,
    /// stderr, for `Output::diagnostic` lines only.
    meta: Box<dyn Write>,
    tally: Tally,
    line: String,
    /// Bytes written so far to both streams, for the output budget.
    written: u64,
    /// The tail of the status line that says where the run went (`status_ending`); empty
    /// until the connection is described, and when nothing connected.
    ending: String,
    /// A section's `-- label` line and names-only header, held back until its first row, so a
    /// section with no rows prints nothing (schema-commands.md §4).
    held: Option<String>,
}

impl<W: Write> TextSink<W> {
    /// A sink whose diagnostics go nowhere, for tests that look at the main stream only.
    #[cfg(test)]
    pub fn new(out: W, limits: Limits) -> Self {
        Self::with_meta(out, Box::new(io::sink()), limits)
    }

    /// The writer back, once the output is finished: an MCP call's text, in memory.
    pub fn into_inner(self) -> W {
        self.out
    }

    pub fn with_meta(out: W, meta: Box<dyn Write>, limits: Limits) -> Self {
        TextSink {
            out,
            meta,
            tally: Tally::new(limits),
            line: String::with_capacity(1024),
            written: 0,
            ending: String::new(),
            held: None,
        }
    }

    /// A definition section's row (name, type, definition): the definition as a block,
    /// `-- definition: dbo.p (SQL_STORED_PROCEDURE), 42 lines` and then exactly that many
    /// lines (schema-commands.md §4). Each line is escaped like a value, except that a tab
    /// stays a tab, so nothing in it can break a line; the count tells a reader where the
    /// block ends, so a line that starts with `--` (a T-SQL comment) can't pass for metadata.
    ///
    /// When not all of it is shown, the header counts the lines that are, and says what wasn't
    /// and why (DESIGN.md §5.1): whole lines the output budget had no room for (`--max-output`,
    /// which then stops the body), and the part the read cut (`--max-cell N`, or the output
    /// budget when there is no cell limit). A read cut leaves the number of lines after it
    /// unknown, so that count is a lower bound, and a line it cut in two is marked.
    fn definition(&mut self, values: &[Value]) -> io::Result<()> {
        fn text(value: Option<&Value>) -> &str {
            match value {
                Some(Value::Text(s)) => s,
                _ => "?",
            }
        }
        let (name, kind) = (escaped(text(values.first())), escaped(text(values.get(1))));
        let (body, more) = match values.get(2) {
            Some(Value::Text(s)) => (s.as_str(), None),
            Some(Value::TextCut(s, more)) => (s.as_str(), Some(*more)),
            _ => ("", None),
        };
        let mut lines: Vec<String> = body
            .split('\n')
            .map(|line| {
                let line = line.strip_suffix('\r').unwrap_or(line);
                let mut out = String::with_capacity(line.len());
                for ch in line.chars() {
                    if ch == '\t' {
                        out.push('\t');
                    } else {
                        push_char(&mut out, ch);
                    }
                }
                out
            })
            .collect();
        if body.ends_with('\n') {
            lines.pop();
        }
        let read = lines.len();
        // The read stopped inside a line: the last line read is only its start.
        let partial = more.is_some() && !body.is_empty() && !body.ends_with('\n');
        let read_cut = more.map(|_| match self.tally.limits().max_cell {
            0 => "--max-output".to_owned(),
            cell => format!("--max-cell {cell}"),
        });
        let header = |shown: usize| {
            let plural = |n: usize| if n == 1 { "" } else { "s" };
            if shown == read && more.is_none() {
                return format!(
                    "-- definition: {name} ({kind}), {read} line{}",
                    plural(read)
                );
            }
            let mut header = format!(
                "-- definition: {name} ({kind}), first {shown} line{} shown",
                plural(shown)
            );
            if partial && shown == read {
                header.push_str(", the last one cut short");
            }
            let mut why = Vec::new();
            let rest = if shown < read {
                why.push("--max-output");
                let left = read - shown;
                let at_least = if more.is_some() { "≥" } else { "" };
                format!("{at_least}{left} more line{}", plural(left))
            } else {
                let more = more.unwrap_or(0);
                format!("{more} more character{}", plural(more))
            };
            if let Some(cut) = &read_cut
                && !why.contains(&cut.as_str())
            {
                why.push(cut);
            }
            let _ = write!(header, "; {rest} not shown ({})", why.join(", "));
            header
        };
        // The longest the header can be, so the lines counted in it are sure to fit after it:
        // its counts take at most as many digits as `read`, twice.
        let longest = header(0).len().max(header(read).len()) + read.to_string().len() + 1;
        let room = self.tally.body_room(self.written);
        if longest > room {
            self.tally.withhold_result();
            self.tally.row_withheld();
            return Ok(());
        }
        let mut used = longest;
        let mut shown = 0;
        for line in &lines {
            used += line.len() + 1;
            if used > room {
                break;
            }
            shown += 1;
        }
        self.write_line(&header(shown))?;
        for line in &lines[..shown] {
            self.write_line(line)?;
        }
        self.tally.row_shown();
        if shown < read {
            self.tally
                .withhold_definition_lines((read - shown) as u64, more.is_some());
        }
        Ok(())
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

/// The tail a connected run's status line ends with, in text and csv (DESIGN.md §5.1):
/// `; db01/master, connect 195 ms`, the server, the database the run started in and the
/// connect time. A name is escaped like every variable field of a metadata line and cut after
/// `NAME_CHARS` characters with `…[+N]`, as in JSON's `connection`; a server that reported no
/// name is `?`. The server never holds a `/` (it is a machine name and an instance name), so
/// the database is everything after the first one. Nothing from a credential, no login name.
///
/// Its bytes are reserved in the budget on top of the reserves (`Tally::reserve_ending`), so
/// the status line, which is never shortened, still fits whatever the names hold. Empty when
/// there was no room left for it; the closing note then counts it as not shown.
pub fn status_ending(
    tally: &mut Tally,
    used: u64,
    connection: &Connection,
    connect_ms: u128,
) -> io::Result<String> {
    let mut ending = String::from("; ");
    match &connection.server {
        Some(server) => {
            push_escaped(&mut ending, server, NAME_CHARS);
        }
        None => ending.push('?'),
    }
    ending.push('/');
    push_escaped(&mut ending, &connection.database, NAME_CHARS);
    let _ = write!(ending, ", connect {connect_ms} ms");
    Ok(if tally.reserve_ending(used, ending.len())? {
        ending
    } else {
        String::new()
    })
}

/// The closing lines: the error that stopped the run, limit notes, then the status, which
/// ends with `ending` (see `status_ending`) when the run connected. Given the `used` bytes
/// written so far, the status is laid out first and never shortened; the error and the
/// notes share what it leaves, each shortened (`…[+N]`) only as far as needed.
pub fn closing_lines(tally: &Tally, summary: &Summary, ending: &str, used: u64) -> String {
    let notes = tally.limit_notes();
    let mut layout = Layout::new();
    if let Some(error) = summary.error {
        layout.text("-- error: ").part(error).text("\n");
    }
    for note in &notes {
        layout.text("-- note: ").part(note).text("\n");
    }
    layout
        .text("-- ")
        .text(summary.status)
        .text(ending)
        .text("\n");
    layout
        .fit(tally.closing_room(used), push_char)
        // No room for even a cut error or note (the reserve makes this unreachable): the
        // status alone.
        .unwrap_or_else(|| format!("-- {}{ending}\n", summary.status))
}

impl<W: Write> BatchSink for TextSink<W> {
    fn columns(&mut self, columns: &[Column]) -> io::Result<()> {
        self.flush_affected()?;
        let Some(number) = self.tally.start_result() else {
            return Ok(());
        };
        if let Some(section) = self.tally.section() {
            if !section.definition {
                let mut held = format!("-- {}\n", section.label);
                for (i, column) in columns.iter().enumerate() {
                    if i > 0 {
                        held.push('\t');
                    }
                    if i == 0 && column.name.starts_with("--") {
                        held.push('\\');
                    }
                    push_escaped(&mut held, &column.name, 0);
                }
                self.held = Some(held);
            }
            return Ok(());
        }
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
        if self.tally.section().is_some_and(|s| s.definition) {
            return self.definition(values);
        }
        if let Some(held) = self.held.take()
            && !self.emit(&held)?
        {
            self.tally.withhold_result();
            self.tally.row_withheld();
            return Ok(());
        }
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

    fn counting_stopped(&mut self, by: Stop) {
        self.tally.stop_counting(by);
    }

    fn end_rows(&mut self) -> io::Result<()> {
        self.tally.end_rows();
        if let Some(section) = self.tally.section() {
            let held = self.held.take();
            if self.tally.result_hidden() || section.definition {
                return Ok(());
            }
            if self.tally.rows_total() == 0 && self.tally.rows_exact() {
                self.tally.empty_section(section.label);
                return Ok(());
            }
            if held.is_some() {
                // Rows came, but the budget ran out before the first could be shown.
                self.tally.withhold_result();
                return Ok(());
            }
            // A section's footer is only for what it didn't show.
            if self.tally.rows_exact() && !self.tally.rows_hidden() && self.tally.cells_cut() == 0 {
                return Ok(());
            }
        }
        if self.tally.result_hidden() {
            return Ok(());
        }
        let footer = format!("-- {}", self.tally.footer());
        self.write_line(&footer)
    }

    fn end_rows_incomplete(&mut self) -> io::Result<()> {
        self.tally.end_rows();
        if let Some(held) = self.held.take()
            && !self.tally.result_hidden()
            && !self.emit(&held)?
        {
            self.tally.withhold_result();
        }
        if self.tally.result_hidden() {
            return Ok(());
        }
        let footer = format!("-- {}", self.tally.footer_incomplete());
        self.write_line(&footer)
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
        if !self.emit(&message_line(message))? {
            self.tally.withhold_message();
        }
        Ok(())
    }

    fn error(&mut self, error: &ServerMessage) -> io::Result<()> {
        self.tally.outside_rows("an error")?;
        self.flush_affected()?;
        let line = error_line(&self.tally, error);
        if !self.emit(&line)? {
            self.tally.withhold_error();
        }
        Ok(())
    }
}

impl<W: Write> Output for TextSink<W> {
    fn connected(&mut self, connection: &Connection, connect_ms: u128) -> io::Result<()> {
        self.ending = status_ending(&mut self.tally, self.written, connection, connect_ms)?;
        Ok(())
    }

    fn begin_batch(&mut self, batch: &BatchInfo) -> io::Result<()> {
        self.flush_affected()?;
        self.tally.begin_batch(batch);
        if batch.total > 1 && !self.emit(&batch_line(batch))? {
            self.tally.withhold_batch();
        }
        Ok(())
    }

    fn end_batch(&mut self, _ended_ms: u128) -> io::Result<()> {
        self.flush_affected()?;
        let empty = self.tally.take_empty_sections();
        if let Some(line) = no_rows_line(&empty)
            && !self.emit(&line)?
        {
            self.tally.withhold_note();
        }
        Ok(())
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
        let lines = closing_lines(&self.tally, summary, &self.ending, self.written);
        self.written += lines.len() as u64;
        self.out.write_all(lines.as_bytes())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()?;
        self.meta.flush()
    }

    fn set_sections(&mut self, sections: &[Section]) {
        self.tally.set_sections(sections);
    }
}

/// `-- no rows: foreign_keys, triggers`: a batch's sections that had no rows (text and csv);
/// `None` when every section had some.
pub fn no_rows_line(empty: &[&str]) -> Option<String> {
    (!empty.is_empty()).then(|| format!("-- no rows: {}", empty.join(", ")))
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
            started_ms: 0,
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

    /// The facts of a login to `server`, in `database`. Text and csv use only those two.
    pub fn connection_to(server: &str, database: &str) -> Connection {
        Connection {
            server: Some(server.into()),
            version: Some("17.0.1000".into()),
            database: database.into(),
            auth: "sql",
            encrypted: true,
        }
    }

    /// Server and database names that make the widest status tail: 255 characters (the most a
    /// TDS name field holds), each cut to 128 and each written as a six-byte escape.
    pub fn widest_names() -> Connection {
        let name = "\u{2028}".repeat(255);
        connection_to(&name, &name)
    }

    /// The tail `widest_names` and a connect time of `u128::MAX` give: `; `, two names of 128
    /// escapes (768 bytes) and `…[+127]` (9), the `/`, `, connect `, 39 digits and ` ms`.
    pub const WIDEST_ENDING: usize = 2 + 777 + 1 + 777 + 10 + 39 + 3;

    fn text(sink: TextSink<Vec<u8>>) -> String {
        String::from_utf8(sink.out).unwrap()
    }

    /// A schema command's batch (schema-commands.md §4), shared by the three formats' tests:
    /// three sections, the middle one with no rows; the last one's second row starts with `--`.
    pub fn sectioned_run(s: &mut dyn Output) {
        s.set_sections(&[
            Section::rows("object"),
            Section::rows("indexes"),
            Section::rows("columns"),
        ]);
        s.begin_batch(&batch("describe", false, 1)).unwrap();
        s.columns(&[col("name", "nvarchar(257)"), col("type", "nvarchar(60)")])
            .unwrap();
        s.row(&[
            Value::Text("dbo.T".into()),
            Value::Text("USER_TABLE".into()),
        ])
        .unwrap();
        s.end_rows().unwrap();
        s.columns(&[col("name", "sysname")]).unwrap();
        s.end_rows().unwrap();
        s.columns(&[col("column", "sysname"), col("type", "nvarchar(271)")])
            .unwrap();
        for (column, kind) in [("id", "int"), ("--x", "varchar(5)")] {
            s.row(&[Value::Text(column.into()), Value::Text(kind.into())])
                .unwrap();
        }
        s.end_rows().unwrap();
        s.end_batch(1).unwrap();
    }

    // schema-commands.md §4: a section is `-- label` and a header of names only, written with
    // its first row; one with no rows is listed on the batch's `-- no rows:` line; a section
    // that showed all its rows has no footer.
    #[test]
    fn sections_are_labeled_and_empty_ones_collapse() {
        let mut s = TextSink::new(Vec::new(), Limits::default());
        sectioned_run(&mut s);
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        assert_eq!(
            text(s),
            "-- object\nname\ttype\ndbo.T\tUSER_TABLE\n\
             -- columns\ncolumn\ttype\nid\tint\n\\--x\tvarchar(5)\n\
             -- no rows: indexes\n\
             -- ok: 1 batch, 3 ms\n"
        );
    }

    // A section keeps its footer when it didn't show everything.
    #[test]
    fn a_section_says_what_it_did_not_show() {
        let mut s = TextSink::new(
            Vec::new(),
            Limits {
                max_rows: 1,
                ..Limits::default()
            },
        );
        s.set_sections(&[Section::rows("columns")]);
        s.columns(&[col("column", "sysname")]).unwrap();
        for name in ["a", "b"] {
            if s.wants_values() {
                s.row(&[Value::Text(name.into())]).unwrap();
            } else {
                s.skipped_row();
            }
        }
        s.end_rows().unwrap();
        assert_eq!(
            text(s),
            "-- columns\ncolumn\na\n-- 1 of 2 rows shown (--max-rows 1)\n"
        );
    }

    fn definition_row(body: &str) -> Vec<Value> {
        vec![
            Value::Text("dbo.p".into()),
            Value::Text("SQL_STORED_PROCEDURE".into()),
            Value::Text(body.into()),
        ]
    }

    // schema-commands.md §4, with the review's correction: a definition is a block framed by
    // its line count. Its lines are escaped like values (a backslash doubled, a control
    // character as `\xNN`) except that a tab stays a tab, so nothing breaks a line; a line
    // that starts with `--`, as T-SQL comments do, stays inside the counted block.
    #[test]
    fn a_definition_is_a_counted_block() {
        let mut s = TextSink::new(Vec::new(), Limits::default());
        s.set_sections(&[Section::definition("definition")]);
        s.columns(&[col("name", "nvarchar(257)")]).unwrap();
        s.row(&definition_row(
            "CREATE PROC dbo.p AS\r\n-- ok: 1 batch, 3 ms\r\n\tSELECT 'a\\b\x07'\u{2028};\r\n",
        ))
        .unwrap();
        s.end_rows().unwrap();
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        assert_eq!(
            text(s),
            "-- definition: dbo.p (SQL_STORED_PROCEDURE), 3 lines\n\
             CREATE PROC dbo.p AS\n\
             -- ok: 1 batch, 3 ms\n\
             \tSELECT 'a\\\\b\\x07'\\u2028;\n\
             -- ok: 1 batch, 3 ms\n"
        );
    }

    fn definition_out(limits: Limits, value: Value) -> String {
        let mut s = TextSink::new(Vec::new(), limits);
        s.set_sections(&[Section::definition("definition")]);
        s.columns(&[col("name", "nvarchar(257)")]).unwrap();
        let mut row = definition_row("");
        row[2] = value;
        s.row(&row).unwrap();
        s.end_rows().unwrap();
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        text(s)
    }

    // A definition longer than the output budget shows the lines that fit, and its header and
    // the closing note say how many there were.
    #[test]
    fn a_definition_is_cut_at_whole_lines_and_says_so() {
        let limits = Limits {
            max_output: crate::output::OUTPUT_RESERVE + 140,
            ..Limits::default()
        };
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let out = definition_out(limits, Value::Text(body));
        assert!(
            out.starts_with(
                "-- definition: dbo.p (SQL_STORED_PROCEDURE), first 4 lines shown; 16 more \
                 lines not shown (--max-output)\nline 1\nline 2\nline 3\nline 4\n\
                 -- note: output stopped at the --max-output"
            ),
            "{out}"
        );
        assert!(
            out.contains("these weren't shown: 16 definition lines"),
            "{out}"
        );
    }

    // R29: a definition the read cut says which limit cut it and what is left, marks a line
    // it cut in two, and doesn't claim an output limit that isn't there.
    #[test]
    fn a_definition_cut_while_read_says_by_what() {
        let cell = |max_cell, max_output| Limits {
            max_cell,
            max_output,
            ..Limits::default()
        };
        // Cut inside the first line, with no output limit: no --max-output anywhere.
        let out = definition_out(cell(10, 0), Value::TextCut("CREATE PRO".into(), 52));
        assert_eq!(
            out,
            "-- definition: dbo.p (SQL_STORED_PROCEDURE), first 1 line shown, the last one cut \
             short; 52 more characters not shown (--max-cell 10)\n\
             CREATE PRO\n\
             -- ok: 1 batch, 3 ms\n"
        );
        // Cut just after a line break: every line read is whole.
        let out = definition_out(cell(5, 0), Value::TextCut("a\nbc\n".into(), 1));
        assert_eq!(
            out,
            "-- definition: dbo.p (SQL_STORED_PROCEDURE), first 2 lines shown; 1 more \
             character not shown (--max-cell 5)\na\nbc\n-- ok: 1 batch, 3 ms\n"
        );
        // Both: the read was cut and the budget had no room for all of what was read, so the
        // lines left are a lower bound, here and in the note.
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let out = definition_out(
            cell(150, crate::output::OUTPUT_RESERVE + 160),
            Value::TextCut(body[..150].into(), 1000),
        );
        assert!(
            out.starts_with(
                "-- definition: dbo.p (SQL_STORED_PROCEDURE), first 2 lines shown; ≥18 more \
                 lines not shown (--max-output, --max-cell 150)\nline 1\nline 2\n-- note"
            ),
            "{out}"
        );
        assert!(
            out.contains("these weren't shown: ≥18 definition lines"),
            "{out}"
        );
        // No cell limit: the read was cut at the output budget.
        let out = definition_out(
            cell(0, crate::output::OUTPUT_RESERVE + 160),
            Value::TextCut(body[..150].into(), 1000),
        );
        assert!(out.contains("lines not shown (--max-output)\n"), "{out}");
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
        out.end_batch(5).unwrap();
        let second = BatchInfo {
            number: 2,
            total: 2,
            started_ms: 5,
            ..batch("-Q", false, 3)
        };
        out.begin_batch(&second).unwrap();
        out.columns(&[col("w", "int")]).unwrap();
        out.row(&[Value::Int(1)]).unwrap();
        out.end_rows().unwrap();
        out.end_batch(6).unwrap();
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

    /// A run whose body is far larger than any cap the sweeps use, so that the cap is what
    /// ends it and the ending has to fit where the body stopped: 500 rows, a message, a note
    /// and a closing error longer than the room, and a status line at the widest numbers.
    pub fn filling_run(out: &mut dyn Output) {
        out.begin_batch(&batch("-Q", false, 1)).unwrap();
        out.columns(&[col("id", "int"), col("v", "varchar(20)")])
            .unwrap();
        for i in 0..500 {
            if out.wants_values() {
                out.row(&[Value::Int(i), Value::Text(format!("value {i}"))])
                    .unwrap();
            } else {
                out.skipped_row();
            }
        }
        // The count stopped: the footer and the notes are at their longest.
        out.counting_stopped(Stop::Budget);
        out.end_rows().unwrap();
        out.message(&msg("a message after the rows")).unwrap();
        out.end_batch(5).unwrap();
        out.note(&"a late note ".repeat(40)).unwrap();
        let closing = "closing error ".repeat(400);
        out.finish(&Summary {
            exit: 1,
            status: FILLING_STATUS,
            error: Some(&closing),
            elapsed_ms: 7,
        })
        .unwrap();
    }

    /// PLAN.md 2.15: two batches, the first with a result whose count was stopped after 1,003
    /// rows (`--max-rows 3`), and a status line that says so. The same calls for every format.
    pub fn stopped_run(out: &mut dyn Output) {
        let first = BatchInfo {
            total: 2,
            ..batch("-Q", false, 1)
        };
        out.begin_batch(&first).unwrap();
        out.rows_affected(2).unwrap();
        out.columns(&[col("id", "int"), col("v", "varchar(20)")])
            .unwrap();
        for i in 0..1003 {
            if out.wants_values() {
                out.row(&[Value::Int(i), Value::Text(format!("v{i}"))])
                    .unwrap();
            } else {
                out.skipped_row();
            }
        }
        out.counting_stopped(Stop::Budget);
        out.end_rows().unwrap();
        out.end_batch(5).unwrap();
        let second = BatchInfo {
            number: 2,
            total: 2,
            started_ms: 5,
            ..batch("-Q", false, 6)
        };
        out.begin_batch(&second).unwrap();
        out.columns(&[col("next", "int")]).unwrap();
        out.row(&[Value::Int(1)]).unwrap();
        out.end_rows().unwrap();
        out.end_batch(6).unwrap();
        out.finish(&Summary {
            exit: 0,
            status: STOPPED_STATUS,
            error: None,
            elapsed_ms: 7,
        })
        .unwrap();
    }

    pub const STOPPED_STATUS: &str = "ok: 2 batches, counting stopped in 1, 7 ms";

    pub fn stopped_limits() -> Limits {
        Limits {
            max_rows: 3,
            ..capped(0)
        }
    }

    /// `--max-rows 10`, for `stopped_short_run`.
    pub fn short_limits() -> Limits {
        Limits {
            max_rows: 10,
            ..capped(0)
        }
    }

    /// PLAN.md 2.15 (review), with `short_limits()`: the two stopped counts whose bound one
    /// significant digit would flatten to "nothing was hidden". Batch 1 was stopped by the budget
    /// after one hidden row: 11 rows counted floor to 10, which is the rows shown, so the bound
    /// is one more, 11. Batch 2 was stopped by `-t` before any hidden row arrived: the bound is
    /// the 10 rows shown, and none is said to be hidden. The same calls for every format.
    pub fn stopped_short_run(out: &mut dyn Output) {
        for (number, hidden, by) in [(1, 1, Stop::Budget), (2, 0, Stop::Timeout)] {
            let info = BatchInfo {
                number,
                total: 2,
                started_ms: (number as u128 - 1) * 5,
                ..batch("-Q", false, 1 + (number as u32 - 1) * 5)
            };
            out.begin_batch(&info).unwrap();
            out.columns(&[col("n", "int")]).unwrap();
            for i in 0..10 + hidden {
                if out.wants_values() {
                    out.row(&[Value::Int(i)]).unwrap();
                } else {
                    out.skipped_row();
                }
            }
            out.counting_stopped(by);
            out.end_rows().unwrap();
            out.end_batch(number as u128 * 5).unwrap();
        }
        out.finish(&Summary {
            exit: 0,
            status: STOPPED_SHORT_STATUS,
            error: None,
            elapsed_ms: 7,
        })
        .unwrap();
    }

    pub const STOPPED_SHORT_STATUS: &str = "ok: 2 batches, counting stopped in 2, 7 ms";

    pub const FILLING_STATUS: &str = "failed (exit 1): 4294967295 errors, first in batch \
        18446744073709551615; 18446744073709551615 later batches not run (--continue-on-error \
        runs them), counting stopped in 18446744073709551615, 340282366920938463463374607431768211455 ms";

    /// PLAN.md 2.12, the same calls for every format (with `limits()`: one result set and one
    /// message shown). A message, an error and a row count that arrive between a result set's
    /// `columns` and its `end_rows` are refused with an error, in a shown result set and in a
    /// hidden one, and change nothing: not the counts, and not what the limits still allow.
    /// Once the result set is closed the same calls are taken, so a message the server sent
    /// during the result lands after it. Leaves the run open: the caller finishes it.
    pub fn mid_result_events_are_refused(out: &mut dyn Output) {
        fn refused(result: io::Result<()>, what: &str) {
            let error = result.expect_err(what);
            assert!(super::super::OutOfOrder::is(&error), "{what}: {error}");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{what}");
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("{what} arrived while a result set's rows were")),
                "{error}"
            );
        }
        fn mid_result(out: &mut dyn Output) {
            refused(out.message(&msg("too early")), "a message");
            refused(out.error(&error(1)), "an error");
            refused(out.rows_affected(9), "a row count");
        }
        // The first result set is shown.
        out.columns(&[col("v", "int")]).unwrap();
        out.row(&[Value::Int(1)]).unwrap();
        mid_result(out);
        out.row(&[Value::Int(2)]).unwrap();
        out.end_rows().unwrap();
        // The refused message was not counted: the one message `limits()` allows is still free.
        out.message(&msg("kept")).unwrap();
        // The second is hidden by --max-results, and the order is the caller's all the same.
        out.columns(&[col("w", "int")]).unwrap();
        mid_result(out);
        out.end_rows().unwrap();
        out.rows_affected(3).unwrap();
        out.error(&error(2)).unwrap();
    }

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
    // less the 640-byte status reserve: 728 - 640 = 88 bytes, of which "-- note: " and the
    // newline take 10, "…[+131]" 9 and 69 characters the other 69. The error and the status
    // then share the 640 bytes that are left: 23 for the status line, 11 around the error,
    // 10 for "…[+1000]"'s widest form, so 596 characters of the error and "…[+404]".
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
                "-- note: {}…[+131]\n-- error: {}…[+404]\n-- ok: 0 batches, 3 ms\n",
                "x".repeat(69),
                "e".repeat(596)
            )
        );
    }

    // PLAN.md 2.15, exact output: a stopped count reads `≥` with the total floored to one
    // digit, says what stopped it inside the parentheses, and the closing note and the status
    // line say that the rest of the batch was cancelled. The batch after it runs as usual.
    #[test]
    fn a_stopped_count_is_a_lower_bound_in_the_footer_note_and_status() {
        let (mut s, main, _) = sink(stopped_limits());
        stopped_run(&mut s);
        assert_eq!(
            main.text(),
            "-- batch 1 (line 1)\n\
             -- 2 rows affected\n\
             -- result 1\n\
             id:int\tv:varchar(20)\n\
             0\tv0\n\
             1\tv1\n\
             2\tv2\n\
             -- 3 of ≥1000 rows shown (--max-rows 3; counting stopped)\n\
             -- batch 2 (line 6)\n\
             -- result 2\n\
             next:int\n\
             1\n\
             -- 1 row\n\
             -- note: counting stopped in batch 1: the rest of that batch was cancelled, so its \
             later statements did not run; --exact-count counts every row\n\
             -- ok: 2 batches, counting stopped in 1, 7 ms\n"
        );
    }

    // R17, exact output: a SQL error that ends a result's rows closes it with an incomplete
    // footer (`so far`, or `≥` with the hidden rows counted, and `interrupted`), before the
    // messages held and the error. A result that ended on its own, and a later result in the
    // same batch, keep the exact footer.
    #[test]
    fn a_result_a_sql_error_ended_has_an_incomplete_footer() {
        let (mut s, main, _) = sink(Limits {
            max_rows: 2,
            max_cell: 0,
            ..capped(0)
        });
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.end_rows().unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.end_rows_incomplete().unwrap();
        s.message(&msg("held")).unwrap();
        s.error(&error(2)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.row(&[Value::Int(2)]).unwrap();
        s.skipped_row();
        s.skipped_row();
        s.end_rows_incomplete().unwrap();
        s.error(&error(3)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(9)]).unwrap();
        s.end_rows().unwrap();
        s.end_batch(1).unwrap();
        s.finish(&summary("failed (exit 1): 2 errors, 3 ms"))
            .unwrap();
        assert_eq!(
            main.text(),
            "-- result 1\n\
             x:int\n\
             1\n\
             -- 1 row\n\
             -- result 2\n\
             x:int\n\
             1\n\
             -- 1 row so far (interrupted)\n\
             -- msg: held\n\
             -- error 208 (severity 16, state 1, line 2): Invalid object name 'dbo.Nope'.\n\
             -- result 3\n\
             x:int\n\
             1\n\
             2\n\
             -- 2 of ≥4 rows shown (--max-rows 2; interrupted)\n\
             -- error 208 (severity 16, state 1, line 3): Invalid object name 'dbo.Nope'.\n\
             -- result 4\n\
             x:int\n\
             9\n\
             -- 1 row\n\
             -- failed (exit 1): 2 errors, 3 ms\n"
        );
    }

    // PLAN.md 2.15 (review), exact output: flooring to one digit never makes a stopped count
    // say that nothing was hidden. Eleven rows counted with ten shown read `≥11`, not `≥10`;
    // a count that `-t` stopped before any hidden row arrived reads `≥10`, the rows shown. The
    // note gives each kind of stop its own advice, and the status counts both batches.
    #[test]
    fn a_stopped_count_never_says_that_nothing_was_hidden() {
        let (mut s, main, _) = sink(short_limits());
        stopped_short_run(&mut s);
        let rows = "0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        assert_eq!(
            main.text(),
            format!(
                "-- batch 1 (line 1)\n\
                 -- result 1\n\
                 n:int\n\
                 {rows}\
                 -- 10 of ≥11 rows shown (--max-rows 10; counting stopped)\n\
                 -- batch 2 (line 6)\n\
                 -- result 2\n\
                 n:int\n\
                 {rows}\
                 -- 10 of ≥10 rows shown (--max-rows 10; counting stopped)\n\
                 -- note: counting stopped in batches 1, 2: the rest of each was cancelled, so \
                 their later statements did not run; --exact-count counts every row; raise -t \
                 to count longer\n\
                 -- ok: 2 batches, counting stopped in 2, 7 ms\n"
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

    // PLAN.md 2.11, owner decision 10, DESIGN.md §5.1: a run that connected ends its status
    // line with the server, the database it started in and the connect time. The elapsed time
    // before the tail is the run's own and includes the connect.
    #[test]
    fn a_connected_run_ends_its_status_line_with_where_it_ran() {
        let (mut s, main, meta) = sink(capped(0));
        s.connected(&connection_to("db01", "master"), 195).unwrap();
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.columns(&[col("x", "int")]).unwrap();
        s.row(&[Value::Int(1)]).unwrap();
        s.end_rows().unwrap();
        s.end_batch(40).unwrap();
        s.finish(&summary("ok: 1 batch, 241 ms")).unwrap();
        assert_eq!(
            main.text(),
            "-- result 1\nx:int\n1\n-- 1 row\n\
             -- ok: 1 batch, 241 ms; db01/master, connect 195 ms\n"
        );
        // Nothing else carries it, and nothing goes to the diagnostics stream.
        assert_eq!(meta.bytes(), 0);
    }

    // A run that failed after connecting says where it was too: the closing error comes first,
    // the status line last, with the tail at its end.
    #[test]
    fn a_failed_run_that_connected_keeps_the_tail_after_its_error() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection_to("db01", "AppDb"), 0).unwrap();
        s.finish(&Summary {
            exit: 4,
            status: "failed (exit 4): timeout, 60012 ms",
            error: Some("query timeout after 60 s in batch 1; raise -t or narrow the query"),
            elapsed_ms: 60012,
        })
        .unwrap();
        assert_eq!(
            main.text(),
            "-- error: query timeout after 60 s in batch 1; raise -t or narrow the query\n\
             -- failed (exit 4): timeout, 60012 ms; db01/AppDb, connect 0 ms\n"
        );
    }

    // Nothing connected (a usage error, a guard refusal, a failed connect): the status line is
    // what it always was.
    #[test]
    fn a_run_that_never_connected_has_no_tail() {
        let (mut s, main, _) = sink(capped(0));
        s.finish(&Summary {
            exit: 3,
            status: "failed (exit 3): usage error, 0 ms",
            error: Some("-P is rejected"),
            elapsed_ms: 0,
        })
        .unwrap();
        assert_eq!(
            main.text(),
            "-- error: -P is rejected\n-- failed (exit 3): usage error, 0 ms\n"
        );
        let (mut s, main, _) = sink(capped(0));
        s.blocked("line 2", "INSERT ##t would write", "Use a #temp table")
            .unwrap();
        s.finish(&Summary {
            exit: 5,
            status: "failed (exit 5): blocked by the read-only guard, 0 ms",
            error: None,
            elapsed_ms: 0,
        })
        .unwrap();
        assert_eq!(
            main.text(),
            "-- blocked: line 2: INSERT ##t would write; nothing was run. Use a #temp table\n\
             -- failed (exit 5): blocked by the read-only guard, 0 ms\n"
        );
    }

    // A server that gave no name is `?`, as in `--verbose`; the database is always known.
    #[test]
    fn a_server_that_reported_no_name_is_a_question_mark() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(
            &Connection {
                server: None,
                ..connection_to("", "master")
            },
            7,
        )
        .unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(
            main.text(),
            "-- ok: 0 batches, 3 ms; ?/master, connect 7 ms\n"
        );
    }

    // Names are escaped like every variable field of a metadata line (DESIGN.md §5.1): they
    // can't start a line or pass for a `-- ` line, whether the consumer splits at `\n` only or
    // at every Unicode line break, and a backslash is doubled. A bracket-quoted database name
    // can hold any of this.
    #[test]
    fn hostile_server_and_database_names_are_escaped() {
        let name = "a\"b\\c\nd\re\tf\u{1}g\u{7f}h\u{85}i\u{2028}j\u{2029}k]}{,\"x\":1";
        let escaped = "a\"b\\\\c\\nd\\re\\tf\\x01g\\x7Fh\\x85i\\u2028j\\u2029k]}{,\"x\":1";
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection_to(name, name), 1).unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(
            main.text(),
            format!("-- ok: 0 batches, 3 ms; {escaped}/{escaped}, connect 1 ms\n")
        );

        // The forged line is one line of metadata, the real status line.
        let forged = "x\n-- ok: forged\u{2028}-- ok: forged";
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection_to(forged, forged), 1).unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        let out = main.text();
        let breaks = |c: char| matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}');
        assert_eq!(out.matches('\n').count(), 1, "{out:?}");
        assert_eq!(out.split(breaks).count(), 2, "{out:?}");
        assert_eq!(
            out.split(breaks).filter(|l| l.starts_with("-- ")).count(),
            1
        );
    }

    // A name longer than SQL Server's own limit is cut like a long value, with the number of
    // characters left out, as JSON's `connection` cuts it; one of exactly 128 characters is not.
    #[test]
    fn a_name_beyond_128_characters_is_cut() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection_to(&"s".repeat(128), &"d".repeat(300)), 1)
            .unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(
            main.text(),
            format!(
                "-- ok: 0 batches, 3 ms; {}/{}…[+172], connect 1 ms\n",
                "s".repeat(128),
                "d".repeat(128)
            )
        );
        // The cut counts characters before escaping, and never splits an escape.
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection_to("a", &"\u{1}".repeat(130)), 1)
            .unwrap();
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(
            main.text(),
            format!(
                "-- ok: 0 batches, 3 ms; a/{}…[+2], connect 1 ms\n",
                "\\x01".repeat(128)
            )
        );
    }

    // A run cut by --max-output: the tail is held back from the budget to the byte. The body
    // stops exactly as much sooner as the tail is long, so the same run under a cap larger by
    // that many bytes writes the same body, and the status line, whole with its tail, is
    // still the last line.
    #[test]
    fn a_run_cut_by_the_cap_keeps_the_tail_and_gives_it_its_room() {
        fn run(s: &mut TextSink<Shared>) {
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
            s.finish(&summary("failed (exit 1): 1 error, 3 ms"))
                .unwrap();
        }
        let tail = "; db01/master, connect 195 ms";
        let (mut plain, without, _) = sink(budget(50));
        run(&mut plain);

        let (mut s, main, meta) = sink(budget(50 + tail.len() as u64));
        s.connected(&connection_to("db01", "master"), 195).unwrap();
        run(&mut s);
        let max = crate::output::OUTPUT_RESERVE + 50 + tail.len() as u64;
        assert_eq!(
            main.text(),
            format!(
                "-- result 1\nv:varchar(9)\naaaaaaaaa\naaaaaaaaa\n\
                 -- 2 of 5 rows shown (--max-output)\n\
                 -- note: output stopped at the --max-output limit of {max} bytes; the run \
                 continued, and these weren't shown: 1 message, 1 error\n\
                 -- failed (exit 1): 1 error, 3 ms{tail}\n"
            )
        );
        assert_eq!(meta.bytes(), 0);
        // The same rows as without a connection and the smaller cap; only the cap, and with it
        // the note that names it, and the tail differ.
        let plain = without.text();
        assert!(plain.starts_with("-- result 1\nv:varchar(9)\naaaaaaaaa\naaaaaaaaa\n"));
        assert_eq!(
            main.text().replace(&format!("{max} bytes"), "1074 bytes"),
            format!("{}{tail}\n", plain.strip_suffix('\n').unwrap())
        );
    }

    // The tail is a promise of room made when the connection is described. Notes written
    // before that (the one about ignored sqlcmd flags) may already have used it, only with a cap
    // near the minimum: the tail is then left out, counted among what wasn't shown, and the
    // status line is whole without it, within the cap.
    #[test]
    fn a_tail_the_budget_has_no_room_for_is_left_out_and_counted() {
        let (mut s, main, _) = sink(capped(4096));
        s.note(&"n".repeat(10_000)).unwrap();
        s.connected(&connection_to("db01", "master"), 195).unwrap();
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        s.end_batch(1).unwrap();
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        let out = main.text();
        assert!(out.len() <= 4096, "{} bytes", out.len());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].starts_with("-- note: nnnn"), "{}", lines[0]);
        assert_eq!(
            lines[1],
            "-- note: output stopped at the --max-output limit of 4096 bytes; the run \
             continued, and these weren't shown: 1 connection description"
        );
        assert_eq!(lines[2], "-- ok: 1 batch, 3 ms");
    }

    // The connection is described once, before the first batch (`Output::connected`): a second
    // call, or one that comes after a batch began, is refused and changes nothing.
    #[test]
    fn the_connection_is_described_once_before_anything_else() {
        let (mut s, main, _) = sink(capped(0));
        s.connected(&connection_to("db01", "master"), 1).unwrap();
        let error = s.connected(&connection_to("other", "x"), 2).unwrap_err();
        assert!(crate::output::OutOfOrder::is(&error), "{error}");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("before the first batch"));
        s.finish(&summary("ok: 0 batches, 3 ms")).unwrap();
        assert_eq!(
            main.text(),
            "-- ok: 0 batches, 3 ms; db01/master, connect 1 ms\n"
        );

        let (mut s, main, _) = sink(capped(0));
        s.begin_batch(&batch("-Q", false, 1)).unwrap();
        let error = s
            .connected(&connection_to("db01", "master"), 1)
            .unwrap_err();
        assert!(crate::output::OutOfOrder::is(&error), "{error}");
        s.finish(&summary("ok: 1 batch, 3 ms")).unwrap();
        assert_eq!(main.text(), "-- ok: 1 batch, 3 ms\n");
    }

    // The widest tail there can be is the one pinned in `WIDEST_ENDING`: 1,609 bytes. It is
    // reserved, so at every cap the CLI accepts (4096 up) the whole response stays within the
    // cap and ends with the status line, whole, with the tail at its end.
    #[test]
    fn the_cap_holds_with_the_widest_status_tail() {
        let mut ending = String::new();
        {
            let mut tally = Tally::new(capped(0));
            ending.push_str(&status_ending(&mut tally, 0, &widest_names(), u128::MAX).unwrap());
        }
        assert_eq!(ending.len(), WIDEST_ENDING);
        assert!(ending.starts_with("; \\u2028") && ending.ends_with(" ms"));
        for max in 4096..4096 + 1800 {
            for (run, status) in [
                (ragged_run as fn(&mut dyn Output), RAGGED_STATUS),
                (interrupted_run, INTERRUPTED_STATUS),
                (filling_run, FILLING_STATUS),
            ] {
                let (mut s, main, meta) = sink(capped(max));
                s.connected(&widest_names(), u128::MAX).unwrap();
                run(&mut s);
                let out = main.text();
                let total = (main.bytes() + meta.bytes()) as u64;
                assert!(
                    total <= max,
                    "cap {max}: {total} bytes\n{out}{}",
                    meta.text()
                );
                assert_eq!(
                    out.lines().last().unwrap(),
                    format!("-- {status}{ending}"),
                    "cap {max}"
                );
            }
        }
    }

    // The sweep of `the_cap_holds_wherever_the_body_ends` with a short tail, down to the
    // smallest caps the sinks can be given: the tail is always reserved there, and the
    // response is within the cap and ends with the status line and its tail.
    #[test]
    fn the_cap_holds_wherever_the_body_ends_with_a_tail() {
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
                (filling_run, "-- error: closing error ", FILLING_STATUS),
            ] {
                let (mut s, main, meta) = sink(capped(max));
                s.connected(&connection_to("N", "N"), 12).unwrap();
                run(&mut s);
                let out = main.text();
                let total = (main.bytes() + meta.bytes()) as u64;
                assert!(
                    total <= max,
                    "cap {max}: {total} bytes\n{out}{}",
                    meta.text()
                );
                assert!(out.contains(error), "cap {max}: no closing error\n{out}");
                assert_eq!(
                    out.lines().last().unwrap(),
                    format!("-- {status}; N/N, connect 12 ms"),
                    "cap {max}"
                );
            }
        }
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

    // PLAN.md 2.12, DESIGN.md §5.1: nothing but rows arrives inside a result set. A message,
    // an error or a row count that did is refused and changes nothing; a message the server
    // sent during the result is written after its footer.
    #[test]
    fn nothing_but_rows_arrives_inside_a_result_set() {
        let (mut s, main, meta) = sink(limits());
        mid_result_events_are_refused(&mut s);
        s.finish(&summary("failed (exit 1): 1 error, 3 ms"))
            .unwrap();
        assert_eq!(meta.bytes(), 0);
        assert_eq!(
            main.text(),
            "-- result 1\nv:int\n1\n2\n-- 2 rows\n-- msg: kept\n-- 3 rows affected\n\
             -- error 208 (severity 16, state 1, line 2): Invalid object name 'dbo.Nope'.\n\
             -- note: 1 more result sets not shown (--max-results 1)\n\
             -- failed (exit 1): 1 error, 3 ms\n"
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
        s.end_batch(0).unwrap();
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
        assert_eq!(
            text(s),
            "-- error 208 (severity 16, state 1, b.sql:12): Invalid object name 'dbo.Nope'.\n\
             -- error 208 (severity 16, state 1, proc dbo.p, line 3): Invalid object name 'dbo.Nope'.\n"
        );
    }
}
