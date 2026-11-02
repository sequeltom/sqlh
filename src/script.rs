//! Input text to batches: decoding, GO splitting, $(var) substitution, line mapping.
//!
//! Batches are split first and substituted after (DESIGN.md §4.3), so a variable can't
//! create a batch separator: a value of `GO` stays inside its batch.

use std::borrow::Cow;
use std::path::PathBuf;

use crate::guard;

/// The most SQL a run takes, before and after `$(var)` substitution (DESIGN.md §4.4).
pub const MAX_INPUT_BYTES: usize = 10_000_000;

/// The most times `GO n` may run a batch.
pub const MAX_REPEAT: u32 = 1000;

pub struct Source {
    /// How the source is named in locations: a file path, `stdin` or `-Q`.
    pub name: String,
    /// The file it was read from, for `-i`; used to refuse `-o` onto an input.
    pub path: Option<PathBuf>,
    pub text: String,
}

pub struct Batch {
    pub sql: String,
    /// Index into the sources slice.
    pub source: usize,
    /// 1-based line in the source where the batch text starts.
    pub line: u32,
    /// `GO 3` runs a batch three times.
    pub repeat: u32,
    /// For each line of `sql`, the 0-based line of the source batch it came from. Empty when
    /// lines map one to one, which is the case unless a value contained line breaks.
    pub line_map: Vec<u32>,
}

impl Batch {
    /// The source line for a 1-based line of the substituted batch text, which is what the
    /// server's line numbers refer to.
    pub fn source_line(&self, line_in_batch: u32) -> u32 {
        source_line(self.line, &self.line_map, line_in_batch)
    }
}

/// Maps a 1-based line of a batch's substituted text to its source line, given where the
/// batch starts and its line map (empty when lines map one to one).
pub fn source_line(batch_line: u32, line_map: &[u32], line_in_batch: u32) -> u32 {
    let index = line_in_batch.saturating_sub(1);
    let offset = match line_map.get(index as usize) {
        Some(original) => *original,
        None if line_map.is_empty() => index,
        None => line_map.last().copied().unwrap_or(0),
    };
    batch_line + offset
}

/// Decodes input bytes: UTF-8 (BOM optional) or UTF-16 with a BOM. Anything else is an
/// error rather than a lossy guess that would silently corrupt string literals.
pub fn decode(bytes: Vec<u8>, name: &str) -> Result<String, String> {
    let utf16 = |body: &[u8], from: fn([u8; 2]) -> u16| {
        let (units, rest) = body.as_chunks::<2>();
        if !rest.is_empty() {
            return Err(format!(
                "{name}: invalid UTF-16 (odd number of bytes); the file is truncated or not UTF-16"
            ));
        }
        let units: Vec<u16> = units.iter().map(|c| from(*c)).collect();
        String::from_utf16(&units).map_err(|_| format!("{name}: invalid UTF-16"))
    };
    match bytes.as_slice() {
        [0xEF, 0xBB, 0xBF, rest @ ..] => std::str::from_utf8(rest)
            .map(str::to_owned)
            .map_err(|e| not_utf8(name, e.valid_up_to() + 3)),
        // UTF-32's BOMs, the first of which starts as UTF-16 LE's does.
        [0xFF, 0xFE, 0, 0, ..] | [0, 0, 0xFE, 0xFF, ..] => Err(wide_without_bom(name)),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        // NUL is valid UTF-8, so this text would decode, and the guard would refuse it for a false
        // reason (`S starts the batch`) and offer a flag that sends it to the server. One NUL
        // beside each of the first two characters is no one's SQL: the servers read NUL as
        // whitespace, but nobody writes it so.
        [_, 0, _, 0, ..] | [0, _, 0, _, ..] => Err(wide_without_bom(name)),
        _ => String::from_utf8(bytes).map_err(|e| not_utf8(name, e.utf8_error().valid_up_to())),
    }
}

fn wide_without_bom(name: &str) -> String {
    format!(
        "{name}: looks like UTF-16 without a BOM, or UTF-32 (NUL bytes among its first characters); \
         save it as UTF-8, or UTF-16 with a BOM"
    )
}

fn not_utf8(name: &str, at: usize) -> String {
    format!("{name}: not valid UTF-8 (byte {at}); save it as UTF-8, or UTF-16 with a BOM")
}

pub fn prepare(
    sources: &[Source],
    terminator: &str,
    vars: Option<&[(String, String)]>,
) -> Result<Vec<Batch>, String> {
    let mut batches = Vec::new();
    let mut total = 0usize;
    for (index, source) in sources.iter().enumerate() {
        for raw in split(&source.text, terminator) {
            if raw.repeat > MAX_REPEAT {
                return Err(format!(
                    "{}:{}: {terminator} {} would run the batch {} times; the most is {MAX_REPEAT}",
                    source.name, raw.terminator_line, raw.repeat, raw.repeat
                ));
            }
            let line_at =
                |offset: usize| raw.line + raw.text[..offset].matches('\n').count() as u32;
            let (sql, line_map) = match vars {
                // Told how much of the limit is left, so that a value repeated a million times
                // is refused when it reaches the limit, not after it has been built.
                Some(vars) => {
                    substitute(&raw.text, vars, MAX_INPUT_BYTES.saturating_sub(total)).map_err(
                        |stop| match stop {
                            Stop::TooLarge => too_large(),
                            Stop::Password(offset) => {
                                let line = line_at(offset);
                                format!(
                                    "{}:{line}: $({PASSWORD_VAR}) isn't substituted, since rows, \
                                     refusals and the server's errors would print the password \
                                     sqlh logs in with; remove it, or pass -x to turn substitution \
                                     off",
                                    source.name
                                )
                            }
                            Stop::Undefined(name, offset) => {
                                let line = line_at(offset);
                                format!(
                                    "{}:{line}: undefined variable $({name}); pass -v {name}=value, or -x to turn substitution off",
                                    source.name
                                )
                            }
                        },
                    )?
                }
                None => (raw.text, Vec::new()),
            };
            total += sql.len();
            if total > MAX_INPUT_BYTES {
                return Err(too_large());
            }
            batches.push(Batch {
                sql,
                source: index,
                line: raw.line,
                repeat: raw.repeat,
                line_map,
            });
        }
    }
    Ok(batches)
}

fn too_large() -> String {
    format!(
        "after $(var) substitution the SQL is over the {} MB limit; run it in parts",
        MAX_INPUT_BYTES / 1_000_000
    )
}

struct RawBatch {
    text: String,
    line: u32,
    repeat: u32,
    /// The line of the terminator that ended the batch (0 for the last batch, which has none).
    terminator_line: u32,
}

/// Splits on lines that hold only the terminator (plus an optional repeat count and
/// trailing `--` comment). Terminators inside strings, quoted names and block comments
/// don't count. Whitespace-only batches are dropped.
fn split(text: &str, terminator: &str) -> Vec<RawBatch> {
    let mut batches = Vec::new();
    let mut current = String::new();
    let mut start = 1u32;
    let mut lexer = Lexer::default();
    let mut push = |text: &mut String, line: u32, repeat: u32, terminator_line: u32| {
        if !text.trim().is_empty() {
            batches.push(RawBatch {
                text: std::mem::take(text),
                line,
                repeat,
                terminator_line,
            });
        }
        text.clear();
    };
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let number = index as u32 + 1;
        if lexer.is_clear()
            && let Some(repeat) = terminator_repeat(line, terminator)
        {
            push(&mut current, start, repeat, number);
            start = number + 1;
            continue;
        }
        lexer.scan(line);
        current.push_str(line);
    }
    push(&mut current, start, 1, 0);
    batches
}

/// `Some(repeat)` when the line is a batch terminator: `GO`, `go 5`, `GO -- done`.
fn terminator_repeat(line: &str, terminator: &str) -> Option<u32> {
    let line = line.trim();
    let head = line.get(..terminator.len())?;
    if !head.eq_ignore_ascii_case(terminator) {
        return None;
    }
    let mut rest = &line[terminator.len()..];
    if let Some(comment) = rest.find("--") {
        // A lone CR ends this comment as it does any other (DESIGN.md §7), and what follows it is
        // a statement, which must reach the server rather than go with the terminator. The `trim`
        // above took a CR at the line's end. The line then is no terminator, and the server reads
        // the `GO` in it.
        if rest[comment..].contains('\r') {
            return None;
        }
        rest = &rest[..comment];
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(1);
    }
    // The terminator must be a whole word: "GOTO label" is not "GO".
    if !line[terminator.len()..].starts_with(char::is_whitespace) {
        return None;
    }
    rest.parse::<u32>().ok().filter(|n| *n > 0)
}

/// Tracks whether a line ends inside a string, quoted name or block comment.
#[derive(Default)]
struct Lexer {
    comment_depth: u32,
    /// The byte that closes the open string or quoted name.
    close: Option<u8>,
}

impl Lexer {
    fn is_clear(&self) -> bool {
        self.comment_depth == 0 && self.close.is_none()
    }

    fn scan(&mut self, line: &str) {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            let next = bytes.get(i + 1).copied();
            if self.comment_depth > 0 {
                match (c, next) {
                    (b'*', Some(b'/')) => {
                        self.comment_depth -= 1;
                        i += 2;
                    }
                    (b'/', Some(b'*')) => {
                        self.comment_depth += 1;
                        i += 2;
                    }
                    _ => i += 1,
                }
                continue;
            }
            if let Some(close) = self.close {
                if c == close {
                    if next == Some(close) {
                        i += 2;
                        continue;
                    }
                    self.close = None;
                }
                i += 1;
                continue;
            }
            match (c, next) {
                // The servers end this comment at a lone CR too (`guard::Lexer`), and a line here
                // ends only at LF.
                (b'-', Some(b'-')) => match bytes[i..].iter().position(|&b| b == b'\r') {
                    Some(cr) => i += cr,
                    None => return,
                },
                (b'/', Some(b'*')) => {
                    self.comment_depth = 1;
                    i += 2;
                    continue;
                }
                (b'\'', _) => self.close = Some(b'\''),
                (b'"', _) => self.close = Some(b'"'),
                (b'[', _) => self.close = Some(b']'),
                // A literal ends where the guard ends it: `1e--` and `$--` are no comment to the
                // servers (DESIGN.md §7), so a `GO` in a string after one is in the string.
                _ => {
                    if let Some(end) = guard::plain_token_end(line, i) {
                        i = end;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }
}

/// Why `substitute` stopped.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// A name without a value, with its byte offset in the text.
    Undefined(String, usize),
    /// The password variable, with its byte offset in the text.
    Password(usize),
    /// The text would pass the room it was given.
    TooLarge,
}

/// The variable a password comes from (`config::resolve`). No reference reads it from the
/// environment (DESIGN.md §4.3, §8): a model that may run sqlh could print it with one `SELECT`.
const PASSWORD_VAR: &str = "SQLCMDPASSWORD";

/// Replaces `$(name)` with values from `-v`, then environment variables (as sqlcmd does), the
/// password's excepted. Names are case-insensitive. A `$(` that isn't a well-formed reference stays
/// literal.
/// Returns the text and its line map (see `Batch::line_map`).
///
/// `room` is how many bytes of SQL the run may still take (`MAX_INPUT_BYTES` less the earlier
/// batches). The text is checked against it before every piece is appended, so a reference
/// repeated a million times stops at the limit instead of being built first: peak allocation is
/// bounded by the room, not by what the input would expand to. The text never gets capacity
/// beyond the room, and the line map, made only when a value adds lines, takes at most 8 bytes
/// a line of the result (4 bytes of entry and room to double).
fn substitute(
    text: &str,
    vars: &[(String, String)],
    room: usize,
) -> Result<(String, Vec<u32>), Stop> {
    if !text.contains("$(") {
        return if text.len() > room {
            Err(Stop::TooLarge)
        } else {
            Ok((text.to_owned(), Vec::new()))
        };
    }
    let mut result = Builder {
        out: String::with_capacity(text.len().min(room)),
        room,
        original_line: 0,
        map: None,
    };
    let mut rest = text;
    let mut offset = 0;
    while let Some(pos) = rest.find("$(") {
        result.original(&rest[..pos])?;
        let after = &rest[pos + 2..];
        let len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after.len());
        let consumed = if len > 0 && after[len..].starts_with(')') {
            let name = &after[..len];
            let value = match vars
                .iter()
                .rev()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
            {
                Some((_, value)) => Cow::Borrowed(value.as_str()),
                None if name.eq_ignore_ascii_case(PASSWORD_VAR) => {
                    return Err(Stop::Password(offset + pos));
                }
                None => Cow::Owned(
                    std::env::var(name)
                        .map_err(|_| Stop::Undefined(name.to_owned(), offset + pos))?,
                ),
            };
            result.value(&value)?;
            pos + 2 + len + 1
        } else {
            result.original("$(")?;
            pos + 2
        };
        rest = &rest[consumed..];
        offset += consumed;
    }
    result.original(rest)?;
    Ok((result.out, result.map.unwrap_or_default()))
}

/// The text of a substitution so far, within the room it may take.
struct Builder {
    out: String,
    room: usize,
    /// The line of the input the output has reached (0-based).
    original_line: u32,
    /// For each line of the output, the input line it came from. Made when a value first adds
    /// a line: until then every line is its own, and nothing is stored.
    map: Option<Vec<u32>>,
}

impl Builder {
    /// Appends input text, recording which input line each new output line starts on.
    fn original(&mut self, segment: &str) -> Result<(), Stop> {
        self.reserve(segment.len())?;
        for _ in segment.matches('\n') {
            self.original_line += 1;
            if let Some(map) = &mut self.map {
                map.push(self.original_line);
            }
        }
        self.out.push_str(segment);
        Ok(())
    }

    /// Appends a value. The lines it adds all come from the line the reference is on.
    fn value(&mut self, value: &str) -> Result<(), Stop> {
        self.reserve(value.len())?;
        let lines = value.matches('\n').count();
        if lines > 0 {
            let line = self.original_line;
            let map = self.map.get_or_insert_with(|| (0..=line).collect());
            map.extend(std::iter::repeat_n(line, lines));
        }
        self.out.push_str(value);
        Ok(())
    }

    /// Makes sure `more` bytes fit in the room, and in the string: it grows by doubling, but
    /// never past the room.
    fn reserve(&mut self, more: usize) -> Result<(), Stop> {
        let needed = self.out.len().saturating_add(more);
        if needed > self.room {
            return Err(Stop::TooLarge);
        }
        if needed > self.out.capacity() {
            let target = needed
                .max(self.out.capacity().saturating_mul(2))
                .min(self.room);
            self.out.reserve_exact(target - self.out.len());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batches(text: &str) -> Vec<(String, u32, u32)> {
        split(text, "GO")
            .into_iter()
            .map(|b| (b.text, b.line, b.repeat))
            .collect()
    }

    // The servers end a line comment at a lone CR, so what follows one on the line opens a string.
    #[test]
    fn a_lone_cr_ends_a_line_comment_when_finding_terminators() {
        let sql = "SELECT 1 --c\r, 'a\nGO\nb' AS x";
        assert_eq!(batches(sql), vec![(sql.into(), 1, 1)]);
        // The byte just past the CR counts: a quote right there opens the string.
        let sql = "SELECT 1 --c\r'a\nGO\nb' AS x";
        assert_eq!(batches(sql), vec![(sql.into(), 1, 1)]);
        // On a terminator's own line too: what follows the CR is a statement, not a comment's end
        // to drop, so the line goes to the server, whose error says the `GO` is in the way.
        let sql = "SELECT 1 AS a\nGO --c\rSELECT 2 AS b\nSELECT 3 AS c\n";
        assert_eq!(batches(sql), vec![(sql.into(), 1, 1)]);
        let ended = "SELECT 1 --it's\r\nGO\nSELECT 2 --'\n";
        assert_eq!(
            batches(ended),
            vec![
                ("SELECT 1 --it's\r\n".into(), 1, 1),
                ("SELECT 2 --'\n".into(), 3, 1)
            ]
        );
    }

    // Each `--` here is what follows a literal, as the guard reads it and the servers do, so the
    // `GO` is in the quoted name. Where it is a comment after all, what it hides opens nothing.
    #[test]
    fn a_sign_a_literal_takes_is_no_comment_when_finding_terminators() {
        for literal in [
            "1e", "1E", "1.e", "1.5e", ".5e", "$", "$ ", "$   ", "\\", "\\ ", "£", "£ ", "€",
            "\u{FF04}",
        ] {
            let sql = format!("SELECT {literal}--1 AS [a\nGO\nb]");
            assert_eq!(batches(&sql), vec![(sql.clone(), 1, 1)], "{literal:?}");
            let sql = format!("SELECT {literal}--'\nGO\nb'");
            assert_eq!(batches(&sql), vec![(sql.clone(), 1, 1)], "{literal:?}");
        }
        for head in [
            "SELECT 1 --",
            "SELECT 1e---",
            "SELECT 1e+--",
            "SELECT 1e --",
            "SELECT 0x--",
            "SELECT $---",
            "SELECT $+--",
            "SELECT $ ---",
            "SELECT $ /*c*/ --",
            // Names, which a digit, an `e` or a `$` inside doesn't make literals.
            "SELECT a1e--",
            "SELECT a$--",
            "SELECT $action--",
            "SELECT @a1e--",
            "SELECT #a1e--",
            "SELECT \u{E9}1e--",
        ] {
            let first = format!("{head}'\n");
            let sql = format!("{first}GO\nSELECT 2");
            assert_eq!(
                batches(&sql),
                vec![(first, 1, 1), ("SELECT 2".into(), 3, 1)],
                "{head:?}"
            );
        }
        // Split before substitution (DESIGN.md §4.3), so a literal a value makes isn't seen. That
        // fails closed: the pieces reach the guard and the server, whose error says so.
        assert_eq!(batches("SELECT $(v)-- 1 AS [a\nGO\nb]").len(), 2);
        // A space beyond ASCII is one token's end, and takes nothing with it.
        let first = "SELECT 1\u{A0}--'\n".to_string();
        let sql = format!("{first}GO\nSELECT 2");
        assert_eq!(
            batches(&sql),
            vec![(first, 1, 1), ("SELECT 2".into(), 3, 1)]
        );
    }

    #[test]
    fn splits_on_go_lines() {
        let got = batches("SELECT 1\nGO\nSELECT 2\r\ngo 3\r\n\nSELECT 3");
        assert_eq!(
            got,
            vec![
                ("SELECT 1\n".into(), 1, 1),
                ("SELECT 2\r\n".into(), 3, 3),
                ("\nSELECT 3".into(), 5, 1),
            ]
        );
    }

    #[test]
    fn go_variants() {
        assert_eq!(terminator_repeat("GO", "GO"), Some(1));
        assert_eq!(terminator_repeat("  go  \r\n", "GO"), Some(1));
        assert_eq!(terminator_repeat("GO 5", "GO"), Some(5));
        assert_eq!(terminator_repeat("GO -- done", "GO"), Some(1));
        assert_eq!(terminator_repeat("GO -- done\r\n", "GO"), Some(1));
        assert_eq!(terminator_repeat("GO 2 -- done\r", "GO"), Some(2));
        assert_eq!(terminator_repeat("GO --c\rSELECT 2\n", "GO"), None);
        assert_eq!(terminator_repeat("GOTO x", "GO"), None);
        assert_eq!(terminator_repeat("GO x", "GO"), None);
        assert_eq!(terminator_repeat("GO 0", "GO"), None);
        assert_eq!(terminator_repeat("SELECT 1 GO", "GO"), None);
    }

    #[test]
    fn go_inside_strings_and_comments_is_text() {
        let text =
            "SELECT 'a\nGO\nb'\n/* x\nGO\n/* nested */\nGO\n*/\nSELECT [x\nGO\n]\nGO\nSELECT 2";
        let got = batches(text);
        assert_eq!(got.len(), 2);
        assert!(got[0].0.ends_with("]\n"));
        assert_eq!(got[1], ("SELECT 2".into(), 13, 1));
    }

    #[test]
    fn line_comment_quote_does_not_open_string() {
        let got = batches("SELECT 1 -- don't\nGO\nSELECT 2");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn blank_batches_are_dropped() {
        assert!(batches("\n  \nGO\nGO\n").is_empty());
    }

    fn source(name: &str, text: &str) -> Source {
        Source {
            name: name.into(),
            path: None,
            text: text.into(),
        }
    }

    #[test]
    fn substitutes_variables() {
        let vars = vec![("db".to_string(), "AppDb".to_string())];
        assert_eq!(
            substitute("USE $(DB); SELECT '$(' ", &vars, MAX_INPUT_BYTES).unwrap(),
            ("USE AppDb; SELECT '$(' ".to_owned(), Vec::new())
        );
        assert_eq!(
            substitute("$(nope_sqlh_test)", &vars, MAX_INPUT_BYTES).unwrap_err(),
            Stop::Undefined("nope_sqlh_test".into(), 0)
        );
    }

    #[test]
    fn undefined_variable_reports_line() {
        let sources = [source(
            "a.sql",
            "SELECT 1\nGO\nSELECT 2,\n$(missing_sqlh_var)",
        )];
        let err = prepare(&sources, "GO", Some(&[])).err().unwrap();
        assert!(
            err.starts_with("a.sql:4: undefined variable $(missing_sqlh_var)"),
            "{err}"
        );
    }

    // F10: substitution happens after splitting, so a value can't create a batch.
    #[test]
    fn a_variable_cannot_create_a_batch_separator() {
        let sources = [source("-Q", "SELECT 1;\n$(sep)\nSELECT 2;")];
        let vars = [("sep".to_string(), "GO".to_string())];
        let batches = prepare(&sources, "GO", Some(&vars)).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].sql, "SELECT 1;\nGO\nSELECT 2;");
    }

    // F10: server line numbers refer to the substituted text; map them back.
    #[test]
    fn multi_line_values_keep_line_numbers_right() {
        let sources = [source(
            "a.sql",
            "-- header\nGO\nSELECT\n$(cols)\nFROM dbo.T\nWHERE x = 1",
        )];
        let vars = [("cols".to_string(), "a,\nb,\nc".to_string())];
        let batch = &prepare(&sources, "GO", Some(&vars)).unwrap()[1];
        assert_eq!(batch.line, 3);
        assert_eq!(batch.sql, "SELECT\na,\nb,\nc\nFROM dbo.T\nWHERE x = 1");
        assert_eq!(batch.line_map, vec![0, 1, 1, 1, 2, 3]);
        // Server line 1 is SELECT (source line 3); lines 2-4 are the value, all on source
        // line 4; server line 6 is WHERE (source line 6).
        let mapped: Vec<u32> = (1..=6).map(|l| batch.source_line(l)).collect();
        assert_eq!(mapped, vec![3, 4, 4, 4, 5, 6]);
        // Without line breaks in values, lines map one to one and no map is stored.
        let plain = &prepare(&sources, "GO", Some(&[("cols".into(), "a".into())])).unwrap()[1];
        assert!(plain.line_map.is_empty());
        assert_eq!(plain.source_line(4), 6);
    }

    // F06: a batch repeats at most 1000 times, and substitution can't grow the SQL past the
    // input limit.
    #[test]
    fn repeats_and_expansion_are_bounded() {
        let ok = prepare(&[source("a.sql", "SELECT 1\nGO 1000\n")], "GO", None).unwrap();
        assert_eq!(ok[0].repeat, 1000);
        let err = prepare(&[source("a.sql", "SELECT 1\ngo 1001\n")], "GO", None)
            .err()
            .unwrap();
        assert_eq!(
            err,
            "a.sql:2: GO 1001 would run the batch 1001 times; the most is 1000"
        );
        let half = "x".repeat(MAX_INPUT_BYTES / 2 + 1);
        let vars = [("v".to_owned(), half)];
        let err = prepare(
            &[source("a.sql", "SELECT '$(v)', '$(v)'")],
            "GO",
            Some(&vars),
        )
        .err()
        .unwrap();
        assert!(err.contains("over the 10 MB limit"), "{err}");
    }

    // plan-history.md 2.7: the limit is checked as the text is built, not after. This input would
    // expand to 32 GB ("$(v)" a million times, 32 KB each); it is refused when it reaches the
    // limit, which is the only reason this test finishes at all, and the message is the
    // existing one.
    #[test]
    fn a_runaway_expansion_is_refused_at_the_limit() {
        let vars = [("v".to_owned(), "x".repeat(32 * 1024))];
        let text = "$(v)".repeat(1_000_000);
        assert_eq!(
            substitute(&text, &vars, MAX_INPUT_BYTES).unwrap_err(),
            Stop::TooLarge
        );
        let err = prepare(&[source("a.sql", &text)], "GO", Some(&vars))
            .err()
            .unwrap();
        assert_eq!(
            err,
            "after $(var) substitution the SQL is over the 10 MB limit; run it in parts"
        );
    }

    // plan-history.md 2.7: the room is exact. Original text before and after a reference, a value,
    // a `$(` kept literal and text without any reference all count, byte for byte.
    #[test]
    fn the_room_is_exact() {
        let vars = [
            ("v".to_owned(), "12345".to_owned()),
            ("e".to_owned(), String::new()),
        ];
        for (text, size) in [
            ("a$(v)b", 7),
            ("aaa$(v)", 8),
            ("$(v)aaa", 8),
            ("$(v)$(", 7),
            ("$(v)$(v)", 10),
            ("$(e)", 0),
            ("x$(e)y$(e)", 2),
            ("abcdef$(", 8),
            ("abcdef", 6),
        ] {
            let (sql, _) = substitute(text, &vars, size).unwrap();
            assert_eq!(sql.len(), size, "{text}");
            assert!(
                sql.capacity() <= size,
                "{text}: capacity {}",
                sql.capacity()
            );
            if size > 0 {
                assert_eq!(
                    substitute(text, &vars, size - 1).unwrap_err(),
                    Stop::TooLarge,
                    "{text}"
                );
            }
        }
    }

    // plan-history.md 2.7: the limit is shared by a run's batches, so each gets what the earlier
    // ones left.
    #[test]
    fn the_room_shrinks_across_batches() {
        // `SELECT '$(v)'` twice (14 and 13 bytes, 4 of them the reference), then a comment of
        // 3 bytes and `comment` more.
        let run = |value: usize, comment: usize| {
            let sources = [
                source("a.sql", "SELECT '$(v)'\nGO\nSELECT '$(v)'"),
                source("b.sql", &format!("-- {}", "x".repeat(comment))),
            ];
            prepare(&sources, "GO", Some(&[("v".to_owned(), "y".repeat(value))]))
        };
        let limit = |batches: Vec<Batch>| batches.iter().map(|b| b.sql.len()).sum::<usize>();
        // 10 + 4,000,000, 9 + 4,000,000 and 3 + 1,999,978 bytes: exactly the limit.
        assert_eq!(limit(run(4_000_000, 1_999_978).unwrap()), MAX_INPUT_BYTES);
        for (value, comment) in [
            // One byte over, found in the comment (text without a reference).
            (4_000_000, 1_999_979),
            // Two bytes over, found there too.
            (4_999_990, 0),
            // The second batch alone passes what the first left, and is refused as it is built.
            (5_000_000, 0),
        ] {
            let err = run(value, comment).err().unwrap();
            assert!(
                err.contains("over the 10 MB limit"),
                "{value} {comment}: {err}"
            );
        }
        assert_eq!(limit(run(4_999_989, 0).unwrap()), MAX_INPUT_BYTES);
    }

    /// `substitute` as it was before plan-history.md 2.7: no room, and the line map made eagerly.
    fn reference(
        text: &str,
        vars: &[(String, String)],
    ) -> Result<(String, Vec<u32>), (String, usize)> {
        if !text.contains("$(") {
            return Ok((text.to_owned(), Vec::new()));
        }
        let mut out = String::new();
        let mut map = vec![0u32];
        let mut original_line = 0u32;
        let mut multi_line = false;
        let mut rest = text;
        let mut offset = 0;
        let copy = |out: &mut String, map: &mut Vec<u32>, line: &mut u32, segment: &str| {
            for _ in segment.matches('\n') {
                *line += 1;
                map.push(*line);
            }
            out.push_str(segment);
        };
        while let Some(pos) = rest.find("$(") {
            copy(&mut out, &mut map, &mut original_line, &rest[..pos]);
            let after = &rest[pos + 2..];
            let len = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(after.len());
            let consumed = if len > 0 && after[len..].starts_with(')') {
                let name = &after[..len];
                let value = vars
                    .iter()
                    .rev()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.clone())
                    .or_else(|| std::env::var(name).ok())
                    .ok_or_else(|| (name.to_owned(), offset + pos))?;
                for _ in value.matches('\n') {
                    map.push(original_line);
                    multi_line = true;
                }
                out.push_str(&value);
                pos + 2 + len + 1
            } else {
                out.push_str("$(");
                pos + 2
            };
            rest = &rest[consumed..];
            offset += consumed;
        }
        copy(&mut out, &mut map, &mut original_line, rest);
        Ok((out, if multi_line { map } else { Vec::new() }))
    }

    // plan-history.md 2.7: building within a room, and making the line map only when a value adds
    // lines, change nothing about the result. Against the old algorithm, for any room: the
    // same text and line map when it fits, TooLarge when it doesn't, and for an undefined name
    // the same error unless the room ran out before the name was reached.
    #[test]
    fn substitution_within_a_room_matches_the_unbounded_algorithm() {
        const PIECES: &[&str] = &[
            "SELECT",
            " ",
            "\n",
            "\r\n",
            "$(a)",
            "$(b)",
            "$(c)",
            "$(none_sqlh_test)",
            "$(",
            ")",
            "é",
            "😀",
            "GO",
            "'",
        ];
        let vars = [
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "x\ny\nz".to_string()),
            ("C".to_string(), String::new()),
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 12) as usize;
            let text: String = (0..len)
                .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
                .collect();
            let wanted = reference(&text, &vars);
            for room in [0, 1, 5, 12, 30, 1000, MAX_INPUT_BYTES] {
                let got = substitute(&text, &vars, room);
                match (&wanted, &got) {
                    (Ok((sql, _)), Ok(_)) if sql.len() <= room => {
                        assert_eq!(wanted.as_ref().ok(), got.as_ref().ok(), "{text:?} {room}");
                        assert!(
                            got.as_ref().unwrap().0.capacity() <= room,
                            "{text:?} {room}"
                        );
                    }
                    (Ok((sql, _)), Err(Stop::TooLarge)) => {
                        assert!(sql.len() > room, "{text:?} {room}");
                    }
                    (Err((name, offset)), Err(Stop::Undefined(n, o))) => {
                        assert_eq!((name, offset), (n, o), "{text:?} {room}");
                    }
                    (Err(_), Err(Stop::TooLarge)) => {}
                    _ => panic!("{text:?} in {room}: wanted {wanted:?}, got {got:?}"),
                }
            }
        }
    }

    #[test]
    fn rejects_odd_length_utf16() {
        assert!(
            decode(vec![0xFF, 0xFE, 0x53, 0x00, 0xFF], "f.sql")
                .unwrap_err()
                .contains("odd number of bytes")
        );
        assert!(
            decode(vec![0xFE, 0xFF, 0x00], "f.sql")
                .unwrap_err()
                .contains("odd number of bytes")
        );
        assert_eq!(decode(vec![0xFF, 0xFE], "f.sql").unwrap(), "");
    }

    #[test]
    fn splitting_arbitrary_input_never_panics() {
        const PIECES: &[&str] = &[
            "GO", "go 3", "\n", "\r\n", "'", "\"", "[", "]", "/*", "*/", "--", "é", "😀", "$(",
            "$(x)", ")", " ", "SELECT",
        ];
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let vars = [("x".to_string(), "1\n2".to_string())];
        for _ in 0..20_000 {
            let len = (next() % 14) as usize;
            let text: String = (0..len)
                .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
                .collect();
            let sources = [source("-Q", &text)];
            if let Ok(batches) = prepare(&sources, "GO", Some(&vars)) {
                for b in &batches {
                    let _ = b.source_line(1);
                    let _ = b.source_line(1_000);
                }
            }
        }
    }

    #[test]
    fn wide_text_without_a_usable_bom_is_no_utf8() {
        for bytes in [
            &b"S\0E\0L\0"[..],           // UTF-16 LE
            &b"\0S\0E\0L"[..],           // UTF-16 BE
            &b"S\0\0\0E\0\0\0"[..],      // UTF-32 LE
            &b"\0\0\0S\0\0\0E"[..],      // UTF-32 BE
            &b"\xFF\xFE\0\0S\0\0\0"[..], // UTF-32 LE, read as UTF-16 LE until now
            &b"\0\0\xFE\xFF\0\0\0S"[..], // UTF-32 BE
            &b"\n\0S\0E\0"[..],          // whatever the first character is
        ] {
            let err = decode(bytes.to_vec(), "f.sql").unwrap_err();
            assert!(err.contains("without a BOM"), "{bytes:?}: {err}");
            assert!(err.starts_with("f.sql: "), "{err}");
        }
        // A NUL elsewhere is still text, which the guard and the server read as whitespace.
        for text in [
            "SELECT\0 1",
            "S\0ELECT 1",
            "SELECT 1\0",
            "\0SELECT",
            "S\0",
            "\0",
        ] {
            assert_eq!(decode(text.as_bytes().to_vec(), "f.sql").unwrap(), text);
        }
    }

    #[test]
    fn the_password_variable_is_never_read_from_the_environment() {
        for sql in [
            "SELECT '$(SQLCMDPASSWORD)'",
            "SELECT '$(sqlcmdpassword)'",
            "SELECT '$(SqlCmdPassword)'",
        ] {
            let sources = [Source {
                name: "-Q".into(),
                path: None,
                text: format!("SELECT 1\n{sql}"),
            }];
            let err = prepare(&sources, "GO", Some(&[])).err().unwrap();
            assert_eq!(
                err,
                "-Q:2: $(SQLCMDPASSWORD) isn't substituted, since rows, refusals and the server's \
                 errors would print the password sqlh logs in with; remove it, or pass -x to turn \
                 substitution off"
            );
        }
        // Given on purpose it is any variable's value, and -x leaves the text alone.
        let sources = [Source {
            name: "-Q".into(),
            path: None,
            text: "SELECT '$(SQLCMDPASSWORD)'".into(),
        }];
        let vars = [("sqlcmdpassword".to_owned(), "x".to_owned())];
        let given = prepare(&sources, "GO", Some(&vars)).unwrap();
        assert_eq!(given[0].sql, "SELECT 'x'");
        let off = prepare(&sources, "GO", None).unwrap();
        assert_eq!(off[0].sql, "SELECT '$(SQLCMDPASSWORD)'");
    }

    #[test]
    fn decodes_boms() {
        assert_eq!(
            decode(b"\xEF\xBB\xBFSELECT".to_vec(), "x").unwrap(),
            "SELECT"
        );
        assert_eq!(decode(b"\xFF\xFES\x001\x00".to_vec(), "x").unwrap(), "S1");
        assert_eq!(decode(b"\xFE\xFF\x00S\x001".to_vec(), "x").unwrap(), "S1");
        assert!(
            decode(b"SELECT \xE9".to_vec(), "x")
                .unwrap_err()
                .contains("byte 7")
        );
    }
}
