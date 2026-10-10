//! Input text to batches: decoding, GO splitting, $(var) substitution, line mapping.

pub struct Source {
    /// How the source is named in locations: a file path, `stdin` or `-Q`.
    pub name: String,
    pub text: String,
}

pub struct Batch {
    pub sql: String,
    /// Index into the sources slice.
    pub source: usize,
    /// 1-based line in the source where the batch text starts. Server line N maps to `line + N - 1`.
    pub line: u32,
    /// `GO 3` runs a batch three times.
    pub repeat: u32,
}

/// Decodes input bytes: UTF-8 (BOM optional) or UTF-16 with a BOM. Anything else is an
/// error rather than a lossy guess that would silently corrupt string literals.
pub fn decode(bytes: Vec<u8>, name: &str) -> Result<String, String> {
    let utf16 = |body: &[u8], from: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = body.as_chunks::<2>().0.iter().map(|c| from(*c)).collect();
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
            let sql = match vars {
                Some(vars) => substitute(&raw.text, vars).map_err(|(name, offset)| {
                    let line = raw.line + raw.text[..offset].matches('\n').count() as u32;
                    format!(
                        "{}:{line}: undefined variable $({name}); pass -v {name}=value, or -x to turn substitution off",
                        source.name
                    )
                })?,
                None => raw.text,
            };
            batches.push(Batch {
                sql,
                source: index,
                line: raw.line,
                repeat: raw.repeat,
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
/// On an undefined name, returns it with its byte offset.
fn substitute(text: &str, vars: &[(String, String)]) -> Result<String, (String, usize)> {
    if !text.contains("$(") {
        return Ok(text.to_owned());
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut offset = 0;
    while let Some(pos) = rest.find("$(") {
        out.push_str(&rest[..pos]);
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
            out.push_str(&value);
            pos + 2 + len + 1
        } else {
            out.push_str("$(");
            pos + 2
        };
        rest = &rest[consumed..];
        offset += consumed;
    }
    out.push_str(rest);
    Ok(out)
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

    #[test]
    fn substitutes_variables() {
        let vars = vec![("db".to_string(), "AppDb".to_string())];
        assert_eq!(
            substitute("USE $(DB); SELECT '$(' ", &vars).unwrap(),
            "USE AppDb; SELECT '$(' "
        );
        assert_eq!(
            substitute("$(nope_sqlh_test)", &vars).unwrap_err(),
            ("nope_sqlh_test".into(), 0)
        );
    }

    #[test]
    fn undefined_variable_reports_line() {
        let sources = [Source {
            name: "a.sql".into(),
            text: "SELECT 1\nGO\nSELECT 2,\n$(missing_sqlh_var)".into(),
        }];
        let err = prepare(&sources, "GO", Some(&[])).err().unwrap();
        assert!(
            err.starts_with("a.sql:4: undefined variable $(missing_sqlh_var)"),
            "{err}"
        );
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
