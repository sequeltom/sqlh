//! Large values, read in pieces (PLAN.md 2.7, DESIGN.md §11). The columns that can hold
//! megabytes or gigabytes are the partially length-prefixed ones (PLP): `varchar(max)`,
//! `nvarchar(max)`, `varbinary(max)`, `xml`, `json` and CLR types. The driver's row readers
//! (`next_row`, `next_row_into` with a `RowWriter`) allocate the whole declared length of such a
//! value before they hand it over, so a writer that cuts at `--max-cell` still cuts after the
//! peak. The column-at-a-time cursor (`next_row_cursor`, `read_row_column`,
//! `read_active_plp_chunk`) gives the payload in chunks instead. A `Cutter` takes those chunks,
//! keeps the first `cap` characters (bytes for binary, halved: two hex digits each) and only
//! counts the rest, so a gigabyte value costs a few buffers.
//!
//! The counts are exact: text is decoded all the way (a character is a Unicode scalar value,
//! as everywhere else, and a malformed sequence is one U+FFFD), because a UTF-16 surrogate pair,
//! a UTF-8 sequence or a double-byte character can straddle the cut and the chunk boundaries.
//! The decoder is the driver's own, so the characters are the ones `SqlString::to_utf8_string`
//! gives for the whole value.

use std::borrow::Cow;

use mssql_tds::datatypes::sql_string::{EncodingType, ResolvedDecoder};
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::encoding_rs::CoderResult;
use mssql_tds::query::metadata::{ColumnMetadata, PlpEncoding};
use mssql_tds::token::tokens::SqlCollation;

use super::Value;

/// Bytes asked of the driver at a time.
pub const CHUNK: usize = 32 * 1024;

/// Room to decode a chunk to UTF-8. Decoding loops when it is too small, so this only decides
/// how often: a chunk of UTF-16 takes at most 48 KiB, and a chunk of a single-byte code page
/// takes at most three bytes a byte, which is rare.
pub const TEXT: usize = 64 * 1024;

/// How a column's character data is encoded, and so decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `nvarchar(max)`: UTF-16LE.
    Utf16,
    /// `xml`: UTF-16LE, and the server may start it with a byte order mark, which isn't text.
    Xml,
    /// `json`: UTF-8.
    Utf8,
    /// `varchar(max)`: the column's collation names a code page, or UTF-8.
    Collation(SqlCollation),
}

/// How one column of a result set is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    /// The driver decodes the value in one piece. All the other types: the largest are 8,000
    /// bytes, except `text`, `ntext` and `image`, which the driver reads whole but refuses
    /// past 100 MB.
    Whole,
    /// Streamed, decoded and cut.
    Text(Source),
    /// Streamed and cut: `varbinary(max)` and CLR types.
    Binary,
}

impl Cell {
    fn of(column: &ColumnMetadata) -> Option<Cell> {
        Cell::from_parts(
            column.plp_encoding(),
            column.data_type == TdsDataType::Xml,
            column.get_collation(),
        )
    }

    /// How a column is read, given what its metadata says: its PLP encoding (none: not a PLP
    /// type), whether it is `xml`, and its collation. `None` for a column that can't be
    /// streamed: a `varchar(max)` that doesn't say what its collation is (no server sends that).
    fn from_parts(
        encoding: Option<PlpEncoding>,
        xml: bool,
        collation: Option<SqlCollation>,
    ) -> Option<Cell> {
        Some(match encoding {
            None => Cell::Whole,
            Some(PlpEncoding::Utf16Text) if xml => Cell::Text(Source::Xml),
            Some(PlpEncoding::Utf16Text) => Cell::Text(Source::Utf16),
            Some(PlpEncoding::Utf8Text) => Cell::Text(Source::Utf8),
            Some(PlpEncoding::SingleByteText) => Cell::Text(Source::Collation(collation?)),
            Some(PlpEncoding::Binary) => Cell::Binary,
        })
    }
}

/// How each column of a result set is read, if any of them can be streamed. `None` for a result
/// set the driver can read row by row without risk: no column of a PLP type, or a column that
/// can't be streamed (Always Encrypted ciphertext, a `varchar(max)` without a collation), which
/// stays on the old path.
pub fn plan(columns: &[ColumnMetadata]) -> Option<Vec<Cell>> {
    let cells: Option<Vec<Cell>> = columns.iter().map(Cell::of).collect();
    choose(cells?, columns.iter().any(ColumnMetadata::is_encrypted))
}

fn choose(cells: Vec<Cell>, encrypted: bool) -> Option<Vec<Cell>> {
    let streams = cells.iter().any(|cell| *cell != Cell::Whole);
    (streams && !encrypted).then_some(cells)
}

/// Buffers for reading large values, kept by the session and made on first use: 96 KiB, once.
#[derive(Default)]
pub struct Scratch {
    chunk: Vec<u8>,
    text: Vec<u8>,
}

impl Scratch {
    /// The chunk to read into and the room to decode in.
    pub fn buffers(&mut self) -> (&mut [u8], &mut [u8]) {
        if self.chunk.is_empty() {
            self.chunk.resize(CHUNK, 0);
            self.text.resize(TEXT, 0);
        }
        (&mut self.chunk, &mut self.text)
    }
}

/// Takes a large value in chunks and keeps only the part that will be shown.
pub struct Cutter<'a> {
    body: Body<'a>,
}

enum Body<'a> {
    Text {
        decoder: ResolvedDecoder,
        room: &'a mut [u8],
        kept: Kept,
    },
    Binary {
        kept: Vec<u8>,
        /// How many bytes to keep: `usize::MAX` for all of them.
        keep: usize,
        more: usize,
    },
}

/// The text kept so far, and how much came after it.
struct Kept {
    text: String,
    /// Characters in `text`, counted only while a limit applies.
    chars: usize,
    /// The most characters to keep; 0 for all of them.
    cap: usize,
    /// Characters after the kept ones, which were counted and dropped.
    more: usize,
    /// The next text is the start of an `xml` value, which may begin with a byte order mark.
    bom: bool,
}

impl Kept {
    fn add(&mut self, mut text: &str) {
        if self.bom && !text.is_empty() {
            self.bom = false;
            text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        }
        if text.is_empty() {
            return;
        }
        if self.cap == 0 {
            self.text.push_str(text);
            return;
        }
        let room = self.cap - self.chars;
        if room == 0 {
            self.more += text.chars().count();
        } else if let Some((at, _)) = text.char_indices().nth(room) {
            let (head, tail) = text.split_at(at);
            self.text.push_str(head);
            self.chars = self.cap;
            self.more += tail.chars().count();
        } else {
            self.text.push_str(text);
            self.chars += text.chars().count();
        }
    }
}

impl<'a> Cutter<'a> {
    /// A cutter for a cell, keeping `cap` characters (0: all of them). `room` is where text is
    /// decoded to, of at least four bytes. The error is a bug in words.
    pub fn new(cell: Cell, cap: usize, room: &'a mut [u8]) -> Result<Cutter<'a>, String> {
        let encoding = match cell {
            Cell::Whole => return Err("a value the driver reads whole was streamed".into()),
            Cell::Binary => {
                let keep = if cap == 0 { usize::MAX } else { cap / 2 };
                return Ok(Cutter {
                    body: Body::Binary {
                        kept: Vec::new(),
                        keep,
                        more: 0,
                    },
                });
            }
            Cell::Text(Source::Utf16 | Source::Xml) => EncodingType::Utf16,
            Cell::Text(Source::Utf8) => EncodingType::Utf8,
            Cell::Text(Source::Collation(collation)) if collation.utf8() => EncodingType::Utf8,
            Cell::Text(Source::Collation(collation)) => EncodingType::LcidBased(collation),
        };
        let decoder = encoding
            .resolved_encoding()
            .ok_or("a text value's encoding is not known")?
            .new_decoder_without_bom_handling();
        if room.len() < 4 {
            return Err("no room to decode text".into());
        }
        Ok(Cutter {
            body: Body::Text {
                decoder,
                room,
                kept: Kept {
                    text: String::new(),
                    chars: 0,
                    cap,
                    more: 0,
                    bom: cell == Cell::Text(Source::Xml),
                },
            },
        })
    }

    /// The next chunk of the value, `last` for the final one (which may be empty).
    pub fn push(&mut self, chunk: &[u8], last: bool) {
        match &mut self.body {
            Body::Binary { kept, keep, more } => {
                let take = chunk.len().min(*keep - kept.len());
                kept.extend_from_slice(&chunk[..take]);
                *more += chunk.len() - take;
            }
            Body::Text {
                decoder,
                room,
                kept,
            } => {
                let mut rest = chunk;
                loop {
                    let (result, read, written, _malformed) =
                        decoder.decode_to_utf8(rest, room, last);
                    rest = &rest[read..];
                    // The decoder writes UTF-8; the fallback is for a decoder that doesn't.
                    let text = std::str::from_utf8(&room[..written])
                        .map(Cow::Borrowed)
                        .unwrap_or_else(|_| String::from_utf8_lossy(&room[..written]));
                    kept.add(&text);
                    if matches!(result, CoderResult::InputEmpty) || (read == 0 && written == 0) {
                        break;
                    }
                }
            }
        }
    }

    /// The value, whole if nothing was cut.
    pub fn finish(self) -> Value {
        match self.body {
            Body::Text { kept, .. } if kept.more == 0 => Value::Text(kept.text),
            Body::Text { kept, .. } => Value::TextCut(kept.text, kept.more),
            Body::Binary { kept, more: 0, .. } => Value::Bin(kept),
            Body::Binary { kept, more, .. } => Value::BinCut(kept, more),
        }
    }

    /// Bytes this cutter holds, for tests that pin the memory bound: what it kept and the
    /// capacity of what it kept (the decode room is the caller's, and fixed).
    #[cfg(test)]
    fn held(&self) -> usize {
        match &self.body {
            Body::Text { kept, .. } => kept.text.capacity(),
            Body::Binary { kept, .. } => kept.capacity(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::datatypes::sql_string::ResolvedEncoding;
    use mssql_tds::encoding_rs;

    fn collation(lcid: u32, sort_id: u8) -> SqlCollation {
        SqlCollation {
            info: lcid,
            lcid_language_id: lcid as i32,
            col_flags: 0,
            sort_id,
        }
    }

    /// Runs a cutter over `bytes` in chunks of `chunk` bytes (0: all at once), the last chunk
    /// marked as the last. The room to decode in is tiny on purpose, so that decoding has to
    /// stop for room and go on, as it does for a chunk of text full of multi-byte characters.
    fn cut(cell: Cell, cap: usize, bytes: &[u8], chunk: usize) -> Value {
        let mut room = [0u8; 8];
        let mut cutter = Cutter::new(cell, cap, &mut room).unwrap();
        let mut chunks = bytes.chunks(if chunk == 0 {
            bytes.len().max(1)
        } else {
            chunk
        });
        let mut next = chunks.next();
        if next.is_none() {
            cutter.push(&[], true);
        }
        while let Some(piece) = next {
            next = chunks.next();
            cutter.push(piece, next.is_none());
        }
        cutter.finish()
    }

    /// What decoding the whole value gives (the old path: `SqlString::to_utf8_string`), cut
    /// after `cap` characters the way a sink cuts.
    fn whole(encoding: EncodingType, xml: bool, cap: usize, bytes: &[u8]) -> Value {
        let (text, _) = encoding
            .resolved_encoding()
            .unwrap()
            .decode_without_bom_handling(bytes);
        let mut text: &str = &text;
        if xml {
            text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        }
        let total = text.chars().count();
        if cap == 0 || total <= cap {
            Value::Text(text.to_owned())
        } else {
            Value::TextCut(text.chars().take(cap).collect(), total - cap)
        }
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    type Case = (&'static str, Cell, EncodingType, Vec<u8>);

    /// A `varchar(max)` column with this collation.
    fn varchar(collation: SqlCollation) -> Cell {
        Cell::Text(Source::Collation(collation))
    }

    // The cut is exact whatever the encoding, the chunking and the limit: a character is a
    // Unicode scalar value, so a surrogate pair is one, and a multi-byte character cut by a
    // chunk boundary is still one.
    #[test]
    fn text_is_cut_exactly_like_the_whole_value_in_any_chunks() {
        let sample = "héllo wörld 😀 日本語 한국어 中文 абв € NULL -- \t\n\u{2028}".repeat(4);
        let mut cases: Vec<Case> = vec![
            (
                "nvarchar",
                Cell::Text(Source::Utf16),
                EncodingType::Utf16,
                utf16(&sample),
            ),
            (
                "json",
                Cell::Text(Source::Utf8),
                EncodingType::Utf8,
                sample.clone().into_bytes(),
            ),
            (
                "utf8 collation",
                varchar(SqlCollation {
                    col_flags: 0x40,
                    ..collation(0x0409, 0)
                }),
                EncodingType::Utf8,
                sample.clone().into_bytes(),
            ),
        ];
        // Code pages. Characters a page lacks are encoded as `&#N;`, which is still bytes
        // valid in it. The check that the collation names the page is what makes the cases
        // mean what they say.
        for (name, lcid, page) in [
            ("1252", 0x0409, encoding_rs::WINDOWS_1252),
            ("1251", 0x0419, encoding_rs::WINDOWS_1251),
            ("932", 0x0411, encoding_rs::SHIFT_JIS),
            ("936", 0x0804, encoding_rs::GBK),
            ("949", 0x0412, encoding_rs::EUC_KR),
            ("950", 0x0404, encoding_rs::BIG5),
        ] {
            let collation = collation(lcid, 0);
            let encoding = EncodingType::LcidBased(collation);
            assert_eq!(
                encoding.resolved_encoding(),
                Some(ResolvedEncoding::from(page)),
                "{name}"
            );
            cases.push((
                name,
                varchar(collation),
                encoding,
                page.encode(&sample).0.into_owned(),
            ));
        }
        // The OEM pages are tables in the driver, not in encoding_rs.
        for (name, sort_id) in [("437", 30), ("850", 40)] {
            let collation = collation(0x0409, sort_id);
            cases.push((
                name,
                varchar(collation),
                EncodingType::LcidBased(collation),
                (0..=255).cycle().take(900).collect(),
            ));
        }
        for (name, cell, encoding, bytes) in &cases {
            let total = whole(*encoding, false, 0, bytes);
            let Value::Text(text) = &total else { panic!() };
            let chars = text.chars().count();
            for chunk in [0, 1, 2, 3, 5, 7, 13, 64, 255, bytes.len() + 3] {
                for cap in [0, 1, 2, 3, 7, 16, 100, chars - 1, chars, chars + 1] {
                    assert_eq!(
                        cut(*cell, cap, bytes, chunk),
                        whole(*encoding, false, cap, bytes),
                        "{name}: chunks of {chunk}, cap {cap}"
                    );
                }
            }
        }
    }

    // Malformed input too: a lone surrogate, a truncated sequence, a lead byte with nothing
    // after it. The decoder keeps its state across chunks, so each is one U+FFFD however the
    // chunks fall, and the last call (which may be empty) flushes what it was holding.
    #[test]
    fn malformed_bytes_decode_like_the_whole_value_in_any_chunks() {
        let encodings = [
            (Cell::Text(Source::Utf16), EncodingType::Utf16),
            (Cell::Text(Source::Utf8), EncodingType::Utf8),
            (
                varchar(collation(0x0411, 0)),
                EncodingType::LcidBased(collation(0x0411, 0)),
            ),
            (
                varchar(collation(0x0804, 0)),
                EncodingType::LcidBased(collation(0x0804, 0)),
            ),
            (
                varchar(collation(0x0409, 30)),
                EncodingType::LcidBased(collation(0x0409, 30)),
            ),
        ];
        // Bytes that start, continue and break sequences in these encodings.
        const POOL: &[u8] = &[
            0x00, 0x41, 0x7F, 0x80, 0x81, 0x9F, 0xA0, 0xB0, 0xC3, 0xD8, 0xDC, 0xE3, 0xF0, 0xFE,
            0xFF, 0x30,
        ];
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for (cell, encoding) in encodings {
            for _ in 0..1500 {
                let bytes: Vec<u8> = (0..next() % 40)
                    .map(|_| POOL[(next() % POOL.len() as u64) as usize])
                    .collect();
                let chunk = (next() % 9) as usize;
                let cap = (next() % 30) as usize;
                assert_eq!(
                    cut(cell, cap, &bytes, chunk),
                    whole(encoding, false, cap, &bytes),
                    "{encoding:?}: {bytes:02X?} in chunks of {chunk}, cap {cap}"
                );
            }
        }
    }

    // What the old path gave for the whole value, `SqlString::to_utf8_string`, is what a value
    // that isn't cut comes out as.
    #[test]
    fn an_uncut_value_is_what_the_driver_decodes() {
        use mssql_tds::datatypes::sql_string::SqlString;
        let sample = "héllo 😀 日本語";
        let cases = [
            (
                Cell::Text(Source::Utf16),
                EncodingType::Utf16,
                utf16(sample),
            ),
            (
                Cell::Text(Source::Utf8),
                EncodingType::Utf8,
                sample.as_bytes().to_vec(),
            ),
            (
                varchar(collation(0x0411, 0)),
                EncodingType::LcidBased(collation(0x0411, 0)),
                encoding_rs::SHIFT_JIS
                    .encode("日本語テキスト")
                    .0
                    .into_owned(),
            ),
        ];
        for (cell, encoding, bytes) in cases {
            for chunk in [0, 1, 5] {
                assert_eq!(
                    cut(cell, 0, &bytes, chunk),
                    Value::Text(SqlString::decode(&bytes, encoding)),
                    "{encoding:?}, chunks of {chunk}"
                );
            }
        }
    }

    // An `xml` value may start with a byte order mark, which the old path dropped; an
    // `nvarchar` value that starts with U+FEFF keeps it, as the old path did.
    #[test]
    fn only_xml_loses_a_leading_byte_order_mark() {
        let mut xml = vec![0xFF, 0xFE];
        xml.extend(utf16("<a/>"));
        for chunk in [0, 1, 2, 3] {
            assert_eq!(
                cut(Cell::Text(Source::Xml), 0, &xml, chunk),
                Value::Text("<a/>".into())
            );
            // The mark isn't a character: it isn't kept, and it isn't counted as cut.
            assert_eq!(
                cut(Cell::Text(Source::Xml), 2, &xml, chunk),
                Value::TextCut("<a".into(), 2)
            );
            assert_eq!(
                cut(Cell::Text(Source::Utf16), 0, &xml, chunk),
                Value::Text("\u{FEFF}<a/>".into())
            );
            assert_eq!(
                cut(Cell::Text(Source::Xml), 0, &[0xFF, 0xFE], chunk),
                Value::Text(String::new())
            );
        }
        // Not at the start: it stays.
        let mut inner = utf16("<a>");
        inner.extend([0xFF, 0xFE]);
        assert_eq!(
            cut(Cell::Text(Source::Xml), 0, &inner, 0),
            Value::Text("<a>\u{FEFF}".into())
        );
    }

    #[test]
    fn binary_keeps_half_the_limit_in_bytes() {
        let bytes: Vec<u8> = (0..=255).cycle().take(1000).collect();
        for chunk in [0, 1, 7, 999, 1000, 4096] {
            let at = |cap| cut(Cell::Binary, cap, &bytes, chunk);
            assert_eq!(at(0), Value::Bin(bytes.clone()));
            assert_eq!(at(2000), Value::Bin(bytes.clone()));
            assert_eq!(at(2001), Value::Bin(bytes.clone()));
            assert_eq!(at(1999), Value::BinCut(bytes[..999].to_vec(), 1));
            assert_eq!(at(10), Value::BinCut(bytes[..5].to_vec(), 995));
            assert_eq!(at(11), Value::BinCut(bytes[..5].to_vec(), 995));
            assert_eq!(at(1), Value::BinCut(Vec::new(), 1000));
        }
        assert_eq!(cut(Cell::Binary, 10, &[], 0), Value::Bin(Vec::new()));
        assert_eq!(cut(Cell::Binary, 1, &[], 0), Value::Bin(Vec::new()));
    }

    #[test]
    fn empty_values_are_empty_not_cut() {
        for source in [Source::Utf16, Source::Xml, Source::Utf8] {
            for cap in [0, 1, 500] {
                assert_eq!(
                    cut(Cell::Text(source), cap, &[], 0),
                    Value::Text(String::new())
                );
            }
        }
    }

    // A surrogate pair is one character even when the limit falls between its halves, and a
    // chunk of one byte splits every character.
    #[test]
    fn the_limit_counts_characters_not_code_units() {
        let bytes = utf16(&"😀".repeat(5));
        for chunk in [0, 1, 2, 3] {
            assert_eq!(
                cut(Cell::Text(Source::Utf16), 1, &bytes, chunk),
                Value::TextCut("😀".into(), 4)
            );
            assert_eq!(
                cut(Cell::Text(Source::Utf16), 5, &bytes, chunk),
                Value::Text("😀".repeat(5))
            );
        }
    }

    // PLAN.md 2.7: what a cutter holds depends on the limit, not on the value. A value of
    // 16 MB (and nothing stops it being 16 GB) goes through chunk by chunk with the same few
    // kilobytes held, and still counts every character.
    #[test]
    fn a_cutter_holds_the_limit_not_the_value() {
        let chunk = vec![b'x'; CHUNK];
        let mut room = vec![0u8; TEXT];
        let chunks = 512;
        let total = chunks * CHUNK;
        for cell in [varchar(collation(0x0409, 0)), Cell::Text(Source::Utf8)] {
            let mut cutter = Cutter::new(cell, 500, &mut room).unwrap();
            for i in 0..chunks {
                cutter.push(&chunk, i == chunks - 1);
                assert!(cutter.held() <= 4096, "{cell:?}: holds {}", cutter.held());
            }
            assert_eq!(
                cutter.finish(),
                Value::TextCut("x".repeat(500), total - 500)
            );
        }
        let mut cutter = Cutter::new(Cell::Binary, 500, &mut room).unwrap();
        for i in 0..chunks {
            cutter.push(&chunk, i == chunks - 1);
            assert!(cutter.held() <= 4096);
        }
        assert_eq!(cutter.finish(), Value::BinCut(vec![b'x'; 250], total - 250));
        // UTF-16 takes twice the bytes for the same characters.
        let mut cutter = Cutter::new(Cell::Text(Source::Utf16), 500, &mut room).unwrap();
        let ascii: Vec<u8> = [b'x', 0].repeat(CHUNK / 2);
        for i in 0..chunks {
            cutter.push(&ascii, i == chunks - 1);
            assert!(cutter.held() <= 4096);
        }
        assert_eq!(
            cutter.finish(),
            Value::TextCut("x".repeat(500), total / 2 - 500)
        );
    }

    #[test]
    fn a_cutter_that_cannot_work_says_why() {
        let mut room = [0u8; 8];
        let whole = Cutter::new(Cell::Whole, 0, &mut room).err().unwrap();
        assert!(whole.contains("reads whole"), "{whole}");
        let tiny = Cutter::new(Cell::Text(Source::Utf8), 0, &mut room[..3])
            .err()
            .unwrap();
        assert!(tiny.contains("no room"), "{tiny}");
        // Binary decodes nothing, so it needs no room.
        assert!(Cutter::new(Cell::Binary, 0, &mut room[..0]).is_ok());
    }

    // Which columns are streamed, from what the metadata says of them.
    #[test]
    fn the_plan_streams_the_plp_types_and_nothing_else() {
        let latin = Some(collation(0x0409, 0));
        let from = Cell::from_parts;
        assert_eq!(from(None, false, None), Some(Cell::Whole));
        assert_eq!(from(None, false, latin), Some(Cell::Whole));
        assert_eq!(
            from(Some(PlpEncoding::Utf16Text), false, None),
            Some(Cell::Text(Source::Utf16))
        );
        assert_eq!(
            from(Some(PlpEncoding::Utf16Text), true, None),
            Some(Cell::Text(Source::Xml))
        );
        assert_eq!(
            from(Some(PlpEncoding::Utf8Text), false, None),
            Some(Cell::Text(Source::Utf8))
        );
        assert_eq!(
            from(Some(PlpEncoding::SingleByteText), false, latin),
            Some(varchar(collation(0x0409, 0)))
        );
        assert_eq!(
            from(Some(PlpEncoding::Binary), false, None),
            Some(Cell::Binary)
        );
        // A varchar(max) that doesn't say how it is encoded can't be decoded: the result set
        // stays on the old path.
        assert_eq!(from(Some(PlpEncoding::SingleByteText), false, None), None);

        let int = Cell::Whole;
        assert_eq!(choose(vec![int, int], false), None);
        assert_eq!(
            choose(vec![int, Cell::Binary], false),
            Some(vec![int, Cell::Binary])
        );
        // Ciphertext can't be streamed by the driver.
        assert_eq!(choose(vec![int, Cell::Binary], true), None);
    }

    #[test]
    fn scratch_is_made_once_and_sized() {
        let mut scratch = Scratch::default();
        let (chunk, text) = scratch.buffers();
        assert_eq!((chunk.len(), text.len()), (CHUNK, TEXT));
        chunk[0] = 7;
        let (chunk, _) = scratch.buffers();
        assert_eq!(chunk[0], 7);
    }
}
