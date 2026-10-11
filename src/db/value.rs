//! Wire values to `Value`, rendered exactly (DESIGN.md §5.1): no float detours for
//! decimals or money, ISO 8601 dates with the type's own precision.

use std::fmt::Write;

use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_vector::VectorData;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    /// Float rendered in shortest round-trip form.
    Num(String),
    /// Exact literal that needs no escaping: decimal, money, dates, GUIDs.
    Lit(String),
    /// Text that needs escaping and may be truncated.
    Text(String),
    Bin(Vec<u8>),
    /// Text the read cut (`BatchSink::cell_limit`): the characters kept, and how many more
    /// followed, which were counted and dropped as they arrived. A sink writes the kept
    /// characters and the cut marker, exactly as for a `Text` it cut itself.
    TextCut(String, usize),
    /// Binary the read cut: the bytes kept, and how many more followed.
    BinCut(Vec<u8>, usize),
}

/// Days from 0001-01-01 and from 1900-01-01 to the Unix epoch.
const DAYS_0001_TO_1970: i64 = 719_162;
const DAYS_1900_TO_1970: i64 = 25_567;
/// mssql-tds normalizes time values to 100 ns ticks whatever the scale, despite the field
/// name `time_nanoseconds`.
const TICKS_PER_SECOND: u64 = 10_000_000;
const TICKS_PER_DAY: i64 = 86_400 * TICKS_PER_SECOND as i64;

pub fn convert(value: ColumnValues) -> Value {
    match value {
        ColumnValues::Null => Value::Null,
        ColumnValues::TinyInt(v) => Value::Int(v.into()),
        ColumnValues::SmallInt(v) => Value::Int(v.into()),
        ColumnValues::Int(v) => Value::Int(v.into()),
        ColumnValues::BigInt(v) => Value::Int(v),
        ColumnValues::Bit(v) => Value::Int(v.into()),
        ColumnValues::Real(v) => Value::Num(real(v)),
        ColumnValues::Float(v) => Value::Num(float(v)),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => Value::Lit(d.to_string()),
        ColumnValues::SmallMoney(m) => Value::Lit(scaled(m.int_val.into(), 4)),
        ColumnValues::Money(m) => {
            let raw = (m.lsb_part as u32 as i64) | ((m.msb_part as i64) << 32);
            Value::Lit(scaled(raw, 4))
        }
        ColumnValues::String(s) => Value::Text(s.to_utf8_string()),
        ColumnValues::Xml(x) => Value::Text(utf16le(&x.bytes)),
        ColumnValues::Json(j) => Value::Text(String::from_utf8_lossy(&j.bytes).into_owned()),
        ColumnValues::Bytes(b) => Value::Bin(b),
        ColumnValues::Uuid(u) => Value::Lit(u.to_string().to_ascii_uppercase()),
        ColumnValues::Date(d) => Value::Lit(date(d.get_days() as i64 - DAYS_0001_TO_1970)),
        ColumnValues::Time(t) => Value::Lit(time(t.time_nanoseconds, t.scale)),
        ColumnValues::DateTime2(dt) => Value::Lit(format!(
            "{}T{}",
            date(dt.days as i64 - DAYS_0001_TO_1970),
            time(dt.time.time_nanoseconds, dt.time.scale)
        )),
        ColumnValues::DateTimeOffset(o) => {
            // The wire carries UTC plus an offset; show local time with the offset, like SSMS.
            let utc = (o.datetime2.days as i64 - DAYS_0001_TO_1970) * TICKS_PER_DAY
                + o.datetime2.time.time_nanoseconds as i64;
            let local = utc + o.offset as i64 * 60 * TICKS_PER_SECOND as i64;
            let sign = if o.offset < 0 { '-' } else { '+' };
            let minutes = o.offset.unsigned_abs();
            Value::Lit(format!(
                "{}T{}{sign}{:02}:{:02}",
                date(local.div_euclid(TICKS_PER_DAY)),
                time(
                    local.rem_euclid(TICKS_PER_DAY) as u64,
                    o.datetime2.time.scale
                ),
                minutes / 60,
                minutes % 60
            ))
        }
        ColumnValues::DateTime(dt) => {
            // 1/300 s ticks, rounded to SQL Server's .000/.003/.007 milliseconds.
            let millis = (dt.time as u64 * 10 + 1) / 3;
            let mut s = date(dt.days as i64 - DAYS_1900_TO_1970);
            let secs = millis / 1000;
            let _ = write!(
                s,
                "T{:02}:{:02}:{:02}.{:03}",
                secs / 3600,
                secs / 60 % 60,
                secs % 60,
                millis % 1000
            );
            Value::Lit(s)
        }
        ColumnValues::SmallDateTime(dt) => Value::Lit(format!(
            "{}T{:02}:{:02}:00",
            date(dt.days as i64 - DAYS_1900_TO_1970),
            dt.time / 60,
            dt.time % 60
        )),
        ColumnValues::Vector(v) => {
            let items = match &v.data {
                VectorData::Float32(items) | VectorData::Float16(items) => items,
            };
            let mut s = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&real(*item));
            }
            s.push(']');
            Value::Text(s)
        }
    }
}

fn float(v: f64) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        format!("{v:?}")
    }
}

fn real(v: f32) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e7 {
        format!("{}", v as i64)
    } else {
        format!("{v:?}")
    }
}

/// An integer scaled by 10^scale, as an exact decimal string.
fn scaled(raw: i64, scale: u32) -> String {
    let unit = 10u64.pow(scale);
    let magnitude = raw.unsigned_abs();
    let sign = if raw < 0 { "-" } else { "" };
    format!(
        "{sign}{}.{:0width$}",
        magnitude / unit,
        magnitude % unit,
        width = scale as usize
    )
}

fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let text = String::from_utf16_lossy(&units);
    match text.strip_prefix('\u{FEFF}') {
        Some(rest) => rest.to_owned(),
        None => text,
    }
}

/// `YYYY-MM-DD` for a day count from 1970-01-01 (Howard Hinnant's civil_from_days).
fn date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `HH:MM:SS[.fffffff]` with `scale` fractional digits.
fn time(ticks: u64, scale: u8) -> String {
    let secs = ticks / TICKS_PER_SECOND;
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    let scale = scale.min(7) as u32;
    if scale > 0 {
        let fraction = ticks % TICKS_PER_SECOND / 10u64.pow(7 - scale);
        let _ = write!(s, ".{fraction:0width$}", width = scale as usize);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(-DAYS_0001_TO_1970), "0001-01-01");
        assert_eq!(date(-DAYS_1900_TO_1970), "1900-01-01");
        assert_eq!(date(19_727), "2024-01-05");
        assert_eq!(date(2_932_896), "9999-12-31");
        assert_eq!(date(11_016), "2000-02-29");
    }

    #[test]
    fn times() {
        assert_eq!(time(0, 0), "00:00:00");
        assert_eq!(time(331_201_234_567, 7), "09:12:00.1234567");
        assert_eq!(time(331_201_234_567, 3), "09:12:00.123");
        assert_eq!(time(863_999_999_999, 7), "23:59:59.9999999");
    }

    #[test]
    fn datetimeoffset_shows_local_time() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlDateTimeOffset, SqlTime};
        // '2024-01-05T23:30:00-05:30' arrives as UTC 2024-01-06T05:00:00 plus offset -330.
        let value = ColumnValues::DateTimeOffset(SqlDateTimeOffset {
            datetime2: SqlDateTime2 {
                days: (19_728 + DAYS_0001_TO_1970) as u32,
                time: SqlTime {
                    time_nanoseconds: 5 * 3600 * TICKS_PER_SECOND,
                    scale: 0,
                },
            },
            offset: -330,
        });
        assert_eq!(
            convert(value),
            Value::Lit("2024-01-05T23:30:00-05:30".into())
        );
    }

    #[test]
    fn money_and_floats() {
        assert_eq!(scaled(125_000, 4), "12.5000");
        assert_eq!(scaled(-1, 4), "-0.0001");
        assert_eq!(scaled(i64::MIN, 4), "-922337203685477.5808");
        assert_eq!(float(1.0), "1");
        assert_eq!(float(0.1), "0.1");
        assert_eq!(float(-2.5e-7), "-2.5e-7");
        assert_eq!(float(1e20), "1e20");
        assert_eq!(real(0.1), "0.1");
        assert_eq!(real(3.0), "3");
    }

    #[test]
    fn xml_bom_is_dropped() {
        assert_eq!(
            utf16le(&[0xFF, 0xFE, b'<', 0, b'a', 0, b'/', 0, b'>', 0]),
            "<a/>"
        );
    }
}
