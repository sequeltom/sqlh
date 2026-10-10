//! Input text to batches: decoding, GO splitting, $(var) substitution, line mapping.
//!
//! Batches are split first and substituted after (DESIGN.md §4.3), so a variable can't
//! create a batch separator: a value of `GO` stays inside its batch.

use std::path::PathBuf;

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
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8(bytes).map_err(|e| not_utf8(name, e.utf8_error().valid_up_to())),
    }
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
    for (index, source) in sources.iter().enumerate() {
        for raw in split(&source.text, terminator) {
            let (sql, line_map) = match vars {
                Some(vars) => substitute(&raw.text, vars).map_err(|(name, offset)| {
                    let line = raw.line + raw.text[..offset].matches('\n').count() as u32;
                    format!(
                        "{}:{line}: undefined variable $({name}); pass -v {name}=value, or -x to turn substitution off",
                        source.name
                    )
                })?,
                None => (raw.text, Vec::new()),
            };
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

struct RawBatch {
    text: String,
    line: u32,
    repeat: u32,
}

/// Splits on lines that hold only the terminator (plus an optional repeat count and
/// trailing `--` comment). Terminators inside strings, quoted names and block comments
/// don't count. Whitespace-only batches are dropped.
fn split(text: &str, terminator: &str) -> Vec<RawBatch> {
    let mut batches = Vec::new();
    let mut current = String::new();
    let mut start = 1u32;
    let mut lexer = Lexer::default();
    let mut push = |text: &mut String, line: u32, repeat: u32| {
        if !text.trim().is_empty() {
            batches.push(RawBatch {
                text: std::mem::take(text),
                line,
                repeat,
            });
        }
        text.clear();
    };
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let number = index as u32 + 1;
        if lexer.is_clear()
            && let Some(repeat) = terminator_repeat(line, terminator)
        {
            push(&mut current, start, repeat);
            start = number + 1;
            continue;
        }
        lexer.scan(line);
        current.push_str(line);
    }
    push(&mut current, start, 1);
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
                (b'-', Some(b'-')) => return,
                (b'/', Some(b'*')) => {
                    self.comment_depth = 1;
                    i += 2;
                    continue;
                }
                (b'\'', _) => self.close = Some(b'\''),
                (b'"', _) => self.close = Some(b'"'),
                (b'[', _) => self.close = Some(b']'),
                _ => {}
            }
            i += 1;
        }
    }
}

/// Replaces `$(name)` with values from `-v`, then environment variables (as sqlcmd does).
/// Names are case-insensitive. A `$(` that isn't a well-formed reference stays literal.
/// Returns the text and its line map (see `Batch::line_map`). On an undefined name,
/// returns the name with its byte offset.
fn substitute(
    text: &str,
    vars: &[(String, String)],
) -> Result<(String, Vec<u32>), (String, usize)> {
    if !text.contains("$(") {
        return Ok((text.to_owned(), Vec::new()));
    }
    let mut out = String::with_capacity(text.len());
    let mut map = vec![0u32];
    let mut original_line = 0u32;
    let mut multi_line = false;
    let mut rest = text;
    let mut offset = 0;
    while let Some(pos) = rest.find("$(") {
        copy_original(&mut out, &mut map, &mut original_line, &rest[..pos]);
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
            // Lines a value adds all come from the line the reference is on.
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
    copy_original(&mut out, &mut map, &mut original_line, rest);
    Ok((out, if multi_line { map } else { Vec::new() }))
}

/// Appends original text, recording which original line each new output line starts on.
fn copy_original(out: &mut String, map: &mut Vec<u32>, original_line: &mut u32, segment: &str) {
    for _ in segment.matches('\n') {
        *original_line += 1;
        map.push(*original_line);
    }
    out.push_str(segment);
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
            substitute("USE $(DB); SELECT '$(' ", &vars).unwrap(),
            ("USE AppDb; SELECT '$(' ".to_owned(), Vec::new())
        );
        assert_eq!(
            substitute("$(nope_sqlh_test)", &vars).unwrap_err(),
            ("nope_sqlh_test".into(), 0)
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
