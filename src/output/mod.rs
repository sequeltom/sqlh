//! Output formats (DESIGN.md §5): text (default), json and csv. Each is an `Output`. The
//! shared `Tally` applies the limits and keeps the counts, so every format agrees on what
//! was shown, cut or hidden.

pub mod csv;
pub mod json;
pub mod text;

use std::fmt::Write as _;
use std::io::{self, BufWriter, Write};

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
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_rows: 100,
            max_cell: 500,
            max_results: 20,
            max_messages: 100,
        }
    }
}

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
    fn note(&mut self, text: &str) -> io::Result<()>;
    /// The read-only guard refused the input; nothing ran. `advice` says how to get past it.
    fn blocked(&mut self, location: &str, reason: &str, advice: &str) -> io::Result<()>;
    /// Writes the end of the output: limit notes, any error, the status. Always called last,
    /// also after a failure mid-result, and must leave well-formed output.
    fn finish(&mut self, summary: &Summary) -> io::Result<()>;
    fn tally(&self) -> &Tally;
    fn flush(&mut self) -> io::Result<()>;
}

/// The output for `format`. CSV keeps stdout (or `-o`) for data only, so its metadata goes
/// to stderr.
pub fn create(format: Format, out: Box<dyn Write>, limits: Limits) -> Box<dyn Output> {
    let out = BufWriter::with_capacity(64 * 1024, out);
    match format {
        Format::Text => Box::new(text::TextSink::new(out, limits)),
        Format::Json => Box::new(json::JsonSink::new(out, limits)),
        Format::Csv => Box::new(csv::CsvSink::new(out, Box::new(io::stderr()), limits)),
    }
}

/// A guard refusal as one message, for every format:
/// `line 3: UPDATE dbo.T would write; nothing was run. <advice>`.
pub fn blocked_message(location: &str, reason: &str, advice: &str) -> String {
    format!("{location}: {reason}; nothing was run. {advice}")
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
            rows_total: 0,
            rows_shown: 0,
            cells_cut: 0,
            messages_seen: 0,
            errors: 0,
            batch_max_severity: 0,
        }
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
        let max = self.limits.max_results;
        self.hidden = max > 0 && self.results_seen > max;
        (!self.hidden).then_some(self.results_seen)
    }

    pub fn result_hidden(&self) -> bool {
        self.hidden
    }

    pub fn wants_values(&self) -> bool {
        !self.hidden && (self.limits.max_rows == 0 || self.rows_shown < self.limits.max_rows)
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
    if cut_count > 0 {
        out.push_str(&cut_marker(cut_count, false));
    }
    cut_count > 0
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
