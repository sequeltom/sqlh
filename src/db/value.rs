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
            let mut s = String::from("[");
            match &v.data {
                VectorData::Float32(items) => {
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            s.push(',');
                        }
                        s.push_str(&real(*item));
                    }
                }
                VectorData::Float16(items) => {
                    let mut scratch = String::new();
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            s.push(',');
                        }
                        push_half(*item, &mut scratch, &mut s);
                    }
                }
            }
            s.push(']');
            Value::Text(s)
        }
    }
}

/// A float16 element (vector feature version 2, decision 71), which the driver widens to f32
/// exactly, written as the shortest decimal that reads back as the same float16: the
/// shortest round-trip form at the element's own precision (DESIGN.md §5.1). Widened, `0.1`
/// would print as `0.099975586`.
///
/// The magnitude is written and the sign put before it, in `{:?}`'s form (exponent notation
/// below 1e-4). It is done in integers, exactly: a float16 that isn't whole is `V / 2^26` for a
/// whole `V` below 2^37, and the range of decimals that read back as it is known exactly, so no
/// decimal is formatted to a precision or parsed (formatting to a precision would bring
/// Rust's exact float formatter into the exe, 25 KB). For each length from 1 significant digit
/// the nearest decimal of that length is tried, and when it falls below the value, so is the
/// next one up: at a power of two the float16 below is closer than the one above, so the range
/// is narrower below, and the nearest decimal can miss it while the next one up is in it. Five
/// digits tell every float16 apart. `scratch` is reused across a vector's elements.
fn push_half(v: f32, scratch: &mut String, out: &mut String) {
    const ONE: u128 = 1 << 26;
    let magnitude = f64::from(v.abs());
    if !magnitude.is_finite() {
        return push_float(out, f64::from(v));
    }
    if v.is_sign_negative() && magnitude != 0.0 {
        out.push('-');
    }
    // A whole number is written whole, as `real` writes one: every float16 from 2,048 up is
    // one, and the shortest form of 65504 would be 65500. A value below float16's smallest
    // isn't one the driver gives, and has no unit here.
    if magnitude == magnitude.trunc() || magnitude < 2f64.powi(-24) {
        return push_float(out, magnitude);
    }
    // Exact: below 2,048, and a multiple of 2^-24.
    let value = (magnitude * ONE as f64) as u128;
    // float16's binary exponent (its subnormals share -14's unit), and half its unit, in 2^-26:
    // the reach of the decimals that read back as the value, above it and below it.
    let exponent = (((magnitude.to_bits() >> 52) & 0x7ff) as i32 - 1023).max(-14);
    let half_up = 1u128 << (exponent + 15);
    let power_of_two = magnitude.to_bits() & ((1 << 52) - 1) == 0;
    let half_down = if power_of_two && exponent > -14 {
        half_up / 2
    } else {
        half_up
    };
    let even = (value >> (exponent + 16)) & 1 == 0;
    // The decimal exponent of the leading digit: from -8 (2^-24 is 5.96e-8) to 3.
    let leading = (-8..=3)
        .rev()
        .find(|&d: &i32| {
            if d >= 0 {
                10u128.pow(d.unsigned_abs()) * ONE <= value
            } else {
                ONE <= value * 10u128.pow(d.unsigned_abs())
            }
        })
        .unwrap_or(-8);
    for digits in 1..=5 {
        // The candidate is `n * up / down`: `n` digits, `k` places after the point.
        let k = digits - 1 - leading;
        let (up, down) = if k >= 0 {
            (10u128.pow(k.unsigned_abs()), 1)
        } else {
            (1, 10u128.pow(k.unsigned_abs()))
        };
        let reads_back = |n: u128| {
            let (candidate, target) = (n * down * ONE, value * up);
            let (distance, reach) = if candidate >= target {
                (candidate - target, half_up * up)
            } else {
                (target - candidate, half_down * up)
            };
            distance < reach || (distance == reach && even)
        };
        let nearest = (value * up + down * ONE / 2) / (down * ONE);
        let found = if reads_back(nearest) {
            Some(nearest)
        } else if nearest * down * ONE < value * up && reads_back(nearest + 1) {
            Some(nearest + 1)
        } else {
            None
        };
        if let Some(n) = found {
            return push_decimal(n, k, scratch, out);
        }
    }
    push_float(out, magnitude);
}

/// `n * 10^-places`, positive, as `{:?}` writes an f64: positional from 1e-4 up, else
/// `d.ddde-x`; trailing zeros dropped.
fn push_decimal(mut n: u128, mut places: i32, scratch: &mut String, out: &mut String) {
    while n != 0 && n.is_multiple_of(10) {
        n /= 10;
        places -= 1;
    }
    scratch.clear();
    let _ = write!(scratch, "{n}");
    let length = scratch.len() as i32;
    let exponent = length - 1 - places;
    if exponent < -4 {
        out.push_str(&scratch[..1]);
        if length > 1 {
            out.push('.');
            out.push_str(&scratch[1..]);
        }
        let _ = write!(out, "e{exponent}");
    } else if places <= 0 {
        out.push_str(scratch);
        out.extend(std::iter::repeat_n('0', places.unsigned_abs() as usize));
    } else if length <= places {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (places - length) as usize));
        out.push_str(scratch);
    } else {
        let point = (length - places) as usize;
        out.push_str(&scratch[..point]);
        out.push('.');
        out.push_str(&scratch[point..]);
    }
}

/// `float`'s form, written into `out`.
fn push_float(out: &mut String, v: f64) {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e16 {
        let _ = write!(out, "{}", v as i64);
    } else {
        let _ = write!(out, "{v:?}");
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

    /// `x` rounded to the nearest float16, ties to even, as an f64 (which holds every float16
    /// exactly); past float16's largest finite value it is infinite. The oracle for what a
    /// printed decimal reads back as, itself checked against `half` below.
    fn to_half(x: f64) -> f64 {
        if x == 0.0 || !x.is_finite() {
            return x;
        }
        let magnitude = x.abs();
        // float16's exponent: at least -14 (its subnormals share that unit), from the f64's bits.
        let exponent = (((magnitude.to_bits() >> 52) & 0x7ff) as i32 - 1023).max(-14);
        let unit = 2f64.powi(exponent - 10);
        let rounded = (magnitude / unit).round_ties_even() * unit;
        let rounded = if rounded > 65504.0 {
            f64::INFINITY
        } else {
            rounded
        };
        rounded.copysign(x)
    }

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

    // Decision 71: a float32 vector prints as before; a float16 one at its own precision.
    #[test]
    fn vectors_print_each_element_at_its_own_precision() {
        use mssql_tds::datatypes::sql_vector::SqlVector;
        let render = |vector: SqlVector| match convert(ColumnValues::Vector(vector)) {
            Value::Text(text) => text,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            render(SqlVector::try_from_f32(vec![0.1, 2.0, -3.5, 1e-8]).unwrap()),
            "[0.1,2,-3.5,1e-8]"
        );
        // What the driver hands over: each float16 widened to f32 exactly.
        let widened = |values: &[f64]| {
            values
                .iter()
                .map(|&v| half::f16::from_f64(v).to_f32())
                .collect::<Vec<f32>>()
        };
        let values = [
            0.1,
            2.0,
            -3.5,
            0.3333,
            65504.0,
            5.960464477539063e-8,
            -0.0,
            1e-3,
        ];
        assert_eq!(widened(&[0.1])[0], 0.099975586);
        assert_eq!(
            render(SqlVector::try_from_f16(widened(&values)).unwrap()),
            "[0.1,2,-3.5,0.3333,65504,6e-8,0,0.001]"
        );
    }

    // `half` is the reference only from f32: its `from_f64` rounds to f32 first, so it rounds
    // twice just past a float16 midpoint (it gives 0 for 2^-25's next f64 up). Every float16
    // midpoint is an f32, and so are its f32 neighbours, which round once.
    #[test]
    fn to_half_rounds_as_float16_does() {
        let reference = |x: f32| half::f16::from_f32(x).to_f64();
        let mut checked = 0;
        for bits in 0..=u16::MAX {
            let a = half::f16::from_bits(bits);
            if !a.is_finite() {
                continue;
            }
            let a = a.to_f64();
            assert_eq!(to_half(a), a, "{bits:#06x}");
            // Halfway to the next float16 up in magnitude, and a little either side of it: the
            // ties go to the even one.
            let next = half::f16::from_bits(bits.wrapping_add(1)).to_f64();
            if next.is_finite() && next.abs() > a.abs() {
                let mid = ((a + next) / 2.0) as f32;
                assert_eq!(f64::from(mid), (a + next) / 2.0, "{bits:#06x}");
                for x in [mid, mid.next_down(), mid.next_up()] {
                    assert_eq!(to_half(f64::from(x)), reference(x), "{x:e}");
                }
            }
            checked += 1;
        }
        assert_eq!(checked, 63_488);
        for x in [
            65504.0, 65519.99, 65520.0, 1e6, -65520.0, 1e-9, -1e-9, 2.98e-8, 2.99e-8,
        ] {
            assert_eq!(to_half(f64::from(x)), reference(x), "{x:e}");
        }
        // Past the f32 neighbours: the f64 just above 2^-25, half the smallest subnormal,
        // rounds up to it, not down to 0 as a tie would.
        let half_unit = 2f64.powi(-25);
        assert_eq!(to_half(half_unit), 0.0);
        assert_eq!(to_half(half_unit.next_up()), 2f64.powi(-24));
    }

    // Every finite float16: what is printed reads back as the same float16, and no decimal with
    // fewer significant digits would: both decimals of each shorter length either side of the
    // value, cut from its exact expansion, read back as another float16. A whole number is
    // printed whole instead. Reading back goes through `to_half`, checked against `half` above:
    // a decimal of 5 digits or fewer is never within an f64 rounding of a float16 midpoint it
    // isn't, so parsing it to f64 first can't move it across one.
    #[test]
    fn every_float16_prints_in_its_shortest_round_trip_form() {
        let mut scratch = String::new();
        let mut whole = 0;
        for bits in 0..=u16::MAX {
            let value = half::f16::from_bits(bits);
            if !value.is_finite() {
                continue;
            }
            let mut printed = String::new();
            push_half(value.to_f32(), &mut scratch, &mut printed);
            let parsed: f64 = printed.parse().unwrap();
            assert_eq!(to_half(parsed), value.to_f64(), "{bits:#06x}: {printed}");
            // In the form `real` and `float` use for any other number.
            assert_eq!(printed, float(parsed), "{bits:#06x}");
            let magnitude = value.to_f64().abs();
            if magnitude == 0.0 {
                assert_eq!(printed, "0");
                continue;
            }
            if magnitude == magnitude.trunc() {
                assert_eq!(
                    printed.trim_start_matches('-'),
                    format!("{}", magnitude as u32)
                );
                whole += 1;
                continue;
            }
            let significant = printed
                .trim_start_matches('-')
                .split('e')
                .next()
                .unwrap()
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>();
            let length = significant.trim_matches('0').len().max(1);
            // The exact expansion: a float16 has at most 25 significant digits.
            let exact = format!("{magnitude:.30e}");
            let (digits, exponent) = exact.split_once('e').unwrap();
            let digits = digits.replace('.', "");
            let exponent: i32 = exponent.parse().unwrap();
            for shorter in 1..length {
                let below: u64 = digits[..shorter].parse().unwrap();
                for candidate in [below, below + 1] {
                    let decimal: f64 = format!("{candidate}e{}", exponent - shorter as i32 + 1)
                        .parse()
                        .unwrap();
                    assert_ne!(
                        to_half(decimal),
                        magnitude,
                        "{bits:#06x}: {printed}, but {decimal:e} is shorter"
                    );
                }
            }
        }
        // 1 to 2,048 in steps of 1, then 1,024 more in each binade to 32,768, then 32,800 to
        // 65,504 in steps of 32, each signed.
        assert_eq!(whole, 2 * (2048 + 4 * 1024 + 1023));
    }

    #[test]
    fn xml_bom_is_dropped() {
        assert_eq!(
            utf16le(&[0xFF, 0xFE, b'<', 0, b'a', 0, b'/', 0, b'>', 0]),
            "<a/>"
        );
    }
}
