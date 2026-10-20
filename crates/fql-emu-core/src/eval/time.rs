//! Time, date and conversion functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/TimeFunctions.scala and
//! ConversionFunctions.scala. `Now()` is the transaction timestamp and is
//! constant for the whole query.
//!
//! The server works with `java.time.Instant` / `LocalDate`, whose range
//! (years ±10⁹) is far wider than what `value::Timestamp` can render
//! (chrono's ±262 143 years). Results inside the Java range but outside
//! the renderable one fail with `not emulated`; results outside the Java
//! range fail with the server's bounds error.

use super::cast;
use super::forms::{Args, Registry};
use super::string::jformat::java_double_to_string;
use super::string::{join2, join3};
use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::value::{Date, Timestamp, Type, Value};
use chrono::{Datelike, Timelike};

pub fn register(r: &mut Registry) {
    r.add("time", &["time"], &[], time);
    r.add("now", &["now"], &[], now);
    r.add("epoch", &["epoch", "unit"], &[], epoch);
    r.add("date", &["date"], &[], date);
    r.add("to_micros", &["to_micros"], &[], |c, e, a, p| to_unit("to_micros", c, e, a, p));
    r.add("to_millis", &["to_millis"], &[], |c, e, a, p| to_unit("to_millis", c, e, a, p));
    r.add("to_seconds", &["to_seconds"], &[], |c, e, a, p| to_unit("to_seconds", c, e, a, p));
    r.add("second", &["second"], &[], |c, e, a, p| component("second", c, e, a, p));
    r.add("minute", &["minute"], &[], |c, e, a, p| component("minute", c, e, a, p));
    r.add("hour", &["hour"], &[], |c, e, a, p| component("hour", c, e, a, p));
    r.add("day_of_month", &["day_of_month"], &[], |c, e, a, p| component("day_of_month", c, e, a, p));
    r.add("day_of_week", &["day_of_week"], &[], |c, e, a, p| component("day_of_week", c, e, a, p));
    r.add("day_of_year", &["day_of_year"], &[], |c, e, a, p| component("day_of_year", c, e, a, p));
    r.add("month", &["month"], &[], |c, e, a, p| component("month", c, e, a, p));
    r.add("year", &["year"], &[], |c, e, a, p| component("year", c, e, a, p));
    r.add("time_add", &["time_add", "offset", "unit"], &[], |c, e, a, p| time_math("time_add", c, e, a, p));
    r.add("time_subtract", &["time_subtract", "offset", "unit"], &[], |c, e, a, p| {
        time_math("time_subtract", c, e, a, p)
    });
    r.add("time_diff", &["time_diff", "other", "unit"], &[], time_diff);
    r.add("to_string", &["to_string"], &[], to_string);
    r.add("to_number", &["to_number"], &[], to_number);
    r.add("to_double", &["to_double"], &[], to_double);
    r.add("to_integer", &["to_integer"], &[], to_integer);
    r.add("to_time", &["to_time"], &[], to_time);
    r.add("to_date", &["to_date"], &[], to_date);
}

// ---- java.time ranges ----

const NANOS_PER_SEC: i128 = 1_000_000_000;
/// `Instant.MIN` / `Instant.MAX` epoch seconds.
const INSTANT_MIN_SECS: i128 = -31_557_014_167_219_200;
const INSTANT_MAX_SECS: i128 = 31_556_889_864_403_199;
/// `LocalDate.MIN` / `LocalDate.MAX` epoch days.
const DATE_MIN_DAY: i128 = -365_243_219_162;
const DATE_MAX_DAY: i128 = 365_241_780_471;

fn time_bounds(pos: &Position) -> FaunaError {
    FaunaError::bounds(
        "dates and times",
        "between -1000000000-01-01T00:00:00Z and +1000000000-12-31T23:59:59.999999999Z",
        pos,
    )
}

fn unrenderable(pos: &Position) -> FaunaError {
    FaunaError::not_emulated("times and dates beyond the year ±262143", pos)
}

/// A total-nanosecond instant: bounds error outside `Instant`'s range,
/// `not emulated` outside the renderable range.
fn instant_of_nanos(n: i128, pos: &Position) -> R<Timestamp> {
    let secs = n.div_euclid(NANOS_PER_SEC);
    if !(INSTANT_MIN_SECS..=INSTANT_MAX_SECS).contains(&secs) {
        return one(time_bounds(pos));
    }
    let t = Timestamp::of_nanos(n);
    if chrono::DateTime::from_timestamp(t.secs, t.nanos).is_none() {
        return one(unrenderable(pos));
    }
    Ok(t)
}

fn renderable(t: Timestamp, pos: &Position) -> R<Timestamp> {
    if chrono::DateTime::from_timestamp(t.secs, t.nanos).is_none() {
        return one(unrenderable(pos));
    }
    Ok(t)
}

const UNIX_EPOCH_DAYS_FROM_CE: i128 = 719_163;

fn date_of_epoch_day(d: i128, pos: &Position) -> R<Date> {
    if !(DATE_MIN_DAY..=DATE_MAX_DAY).contains(&d) {
        return one(time_bounds(pos));
    }
    i32::try_from(d + UNIX_EPOCH_DAYS_FROM_CE)
        .ok()
        .and_then(chrono::NaiveDate::from_num_days_from_ce_opt)
        .ok_or_else(|| vec![unrenderable(pos)])
}

fn epoch_day(d: &Date) -> i128 {
    d.num_days_from_ce() as i128 - UNIX_EPOCH_DAYS_FROM_CE
}

/// Days from 1970-01-01 of a proleptic Gregorian date (any i64 year).
fn days_from_civil(y: i64, m: u32, d: u32) -> i128 {
    let y = y as i128 - if m <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i128;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i128 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// ---- java.time parsing ----

struct Cursor<'a> {
    cs: &'a [char],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<char> {
        self.cs.get(self.i).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat_ci(&mut self, c: char) -> bool {
        match self.peek() {
            Some(x) if x.eq_ignore_ascii_case(&c) => {
                self.i += 1;
                true
            }
            _ => false,
        }
    }

    fn digits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            let d = self.peek()?.to_digit(10)?;
            self.i += 1;
            v = v * 10 + d;
        }
        Some(v)
    }

    fn done(&self) -> bool {
        self.i == self.cs.len()
    }
}

/// `ISO_LOCAL_DATE` (strict): `yyyy-MM-dd`; years beyond four digits need
/// a sign.
/// `lenient` is `ResolverStyle.SMART` (what `Timestamp.parse` uses): a day
/// past the end of the month is clamped (`2019-02-29` -> `02-28`), while a
/// day above 31 is still an error. `LocalDate.parse` (`Date()`) is STRICT.
fn parse_local_date(c: &mut Cursor, lenient: bool) -> Option<(i64, u32, u32)> {
    let sign = match c.peek() {
        Some('+') => Some(1),
        Some('-') => Some(-1),
        _ => None,
    };
    if sign.is_some() {
        c.i += 1;
    }
    let start = c.i;
    let mut year: i64 = 0;
    while c.i - start < 10 {
        match c.peek().and_then(|d| d.to_digit(10)) {
            Some(d) => {
                year = year * 10 + d as i64;
                c.i += 1;
            }
            None => break,
        }
    }
    let n = c.i - start;
    match sign {
        None if n != 4 => return None,
        Some(1) if n <= 4 => return None,
        Some(-1) if n < 4 || year == 0 => return None,
        _ => {}
    }
    let year = year * sign.unwrap_or(1);
    if !(-999_999_999..=999_999_999).contains(&year) {
        return None;
    }
    if !c.eat('-') {
        return None;
    }
    let month = c.digits(2)?;
    if !c.eat('-') {
        return None;
    }
    let day = c.digits(2)?;
    if !(1..=12).contains(&month) || day < 1 || day > 31 {
        return None;
    }
    let max = days_in_month(year, month);
    if day > max {
        if lenient {
            return Some((year, month, max));
        }
        return None;
    }
    Some((year, month, day))
}

/// `LocalDate.parse`.
pub fn parse_java_date(s: &str) -> Option<(i64, u32, u32)> {
    let cs: Vec<char> = s.chars().collect();
    let mut c = Cursor { cs: &cs, i: 0 };
    let d = parse_local_date(&mut c, false)?;
    c.done().then_some(d)
}

/// `+HH:MM[:ss]` (or `Z`), then `+HHMM` (or `+0000`), as in
/// `fauna.lang.Timestamp`'s formatter. Returns offset seconds.
fn parse_offset(c: &mut Cursor) -> Option<i64> {
    let save = c.i;
    if c.eat_ci('Z') {
        return Some(0);
    }
    let sign = match c.peek() {
        Some('+') => 1,
        Some('-') => -1,
        _ => return None,
    };
    c.i += 1;
    let hh = c.digits(2)? as i64;
    let colon = c.eat(':');
    let mm = c.digits(2)? as i64;
    let mut ss = 0;
    if colon && c.eat(':') {
        ss = c.digits(2)? as i64;
    }
    if !colon && c.peek() == Some(':') {
        c.i = save;
        return None;
    }
    if hh > 18 || mm > 59 || ss > 59 {
        return None;
    }
    let total = hh * 3600 + mm * 60 + ss;
    if total > 18 * 3600 {
        return None;
    }
    Some(sign * total)
}

/// `fauna.lang.Timestamp.parse`: `ISO_LOCAL_DATE_TIME` followed by a
/// mandatory offset. Returns total nanoseconds since the epoch.
pub fn parse_java_instant(s: &str) -> Option<i128> {
    let cs: Vec<char> = s.chars().collect();
    let mut c = Cursor { cs: &cs, i: 0 };
    let (y, mo, d) = parse_local_date(&mut c, true)?;
    if !c.eat_ci('T') {
        return None;
    }
    let hh = c.digits(2)?;
    if !c.eat(':') {
        return None;
    }
    let mm = c.digits(2)?;
    let mut ss = 0;
    let mut nanos: u32 = 0;
    if c.eat(':') {
        ss = c.digits(2)?;
        if c.eat('.') {
            let mut n = 0;
            let mut scale = 100_000_000;
            while n < 9 {
                match c.peek().and_then(|d| d.to_digit(10)) {
                    Some(dg) => {
                        nanos += dg * scale;
                        scale /= 10;
                        n += 1;
                        c.i += 1;
                    }
                    None => break,
                }
            }
        }
    }
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let offset = parse_offset(&mut c)?;
    if !c.done() {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + (hh * 3600 + mm * 60 + ss) as i128 - offset as i128;
    Some(secs * NANOS_PER_SEC + nanos as i128)
}

// ---- java.time printing ----

/// The year as `LocalDate.toString` / `Instant.toString` print it.
fn java_year(y: i64) -> String {
    if y.abs() < 1000 {
        if y < 0 {
            format!("-{:04}", -y)
        } else {
            format!("{:04}", y)
        }
    } else if y > 9999 {
        format!("+{}", y)
    } else {
        y.to_string()
    }
}

/// `LocalDate.toString`.
pub fn date_to_string(d: &Date) -> String {
    format!("{}-{:02}-{:02}", java_year(d.year() as i64), d.month(), d.day())
}

/// `Instant.toString` (`ISO_INSTANT`): fractions in groups of three.
pub fn instant_to_string(t: &Timestamp) -> String {
    let dt = t.to_naive();
    let mut s = format!(
        "{}T{:02}:{:02}:{:02}",
        date_to_string(&dt.date()),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    if t.nanos > 0 {
        let f = format!("{:09}", t.nanos);
        let keep = if t.nanos.is_multiple_of(1_000_000) {
            3
        } else if t.nanos.is_multiple_of(1000) {
            6
        } else {
            9
        };
        s.push('.');
        s.push_str(&f[..keep]);
    }
    s.push('Z');
    s
}

/// `ToStringFunction` for the literals it accepts.
pub fn to_string_literal(v: &Value) -> Option<String> {
    Some(match v {
        Value::Long(l) => l.to_string(),
        Value::Double(d) => java_double_to_string(*d),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        Value::Time(t) => instant_to_string(t),
        Value::Date(d) => date_to_string(d),
        _ => return None,
    })
}

// ---- java.text.NumberFormat ----

enum JNum {
    Long(i64),
    Double(f64),
}

/// `NumberFormat.getInstance` (en_US `#,##0.###`) `.parse(s)`: parses
/// the longest numeric prefix. `Ok(None)` is a `ParseException`.
fn parse_java_number(s: &str) -> Result<Option<JNum>, String> {
    let cs: Vec<char> = s.chars().collect();
    let starts = |i: usize, t: &str| cs[i.min(cs.len())..].iter().copied().take(t.chars().count()).eq(t.chars());
    if starts(0, "NaN") {
        return Ok(Some(JNum::Double(f64::NAN)));
    }
    let negative = cs.first() == Some(&'-');
    let mut pos = if negative { 1 } else { 0 };
    if cs.get(pos) == Some(&'∞') {
        return Ok(Some(JNum::Double(if negative { f64::NEG_INFINITY } else { f64::INFINITY })));
    }
    let gc = icu_properties::maps::general_category();
    // DigitList: significant digits and the decimal point position.
    let mut digits = String::new();
    let mut decimal_at: i64 = 0;
    let mut digit_count: i64 = 0;
    let mut saw_digit = false;
    let mut saw_decimal = false;
    let mut backup: Option<usize> = None;
    let mut exponent: i64 = 0;
    while pos < cs.len() {
        let ch = cs[pos];
        if let Some(d) = ch.to_digit(10).filter(|_| ch.is_ascii_digit()) {
            saw_digit = true;
            backup = None;
            if d == 0 && digits.is_empty() {
                if saw_decimal {
                    decimal_at -= 1;
                }
            } else {
                digit_count += 1;
                digits.push(ch);
            }
        } else if gc.get(ch) == icu_properties::GeneralCategory::DecimalNumber {
            return Err("non-ASCII digits in ToNumber/ToDouble/ToInteger".into());
        } else if ch == ',' {
            if saw_decimal {
                break;
            }
            backup = Some(pos);
        } else if ch == '.' {
            if saw_decimal {
                break;
            }
            decimal_at = digit_count;
            saw_decimal = true;
        } else if ch == 'E' {
            // Exponent: optional '-', then at least one digit.
            let mut p = pos + 1;
            let neg_exp = cs.get(p) == Some(&'-');
            if neg_exp {
                p += 1;
            }
            let start = p;
            let mut e: i64 = 0;
            while let Some(d) = cs.get(p).filter(|c| c.is_ascii_digit()).and_then(|c| c.to_digit(10)) {
                e = e.saturating_mul(10).saturating_add(d as i64);
                p += 1;
            }
            if p > start {
                exponent = if neg_exp { -e } else { e };
                pos = p;
            }
            break;
        } else {
            break;
        }
        pos += 1;
    }
    if let Some(b) = backup {
        pos = b;
    }
    let _ = pos;
    if !saw_digit && digit_count == 0 {
        return Ok(None);
    }
    if !saw_decimal {
        decimal_at = digit_count;
    }
    decimal_at = decimal_at.saturating_add(exponent);
    let trimmed = digits.trim_end_matches('0');
    let count = trimmed.len() as i64;
    let fits_long = if count == 0 {
        !negative
    } else if decimal_at < count || decimal_at > 19 {
        false
    } else if decimal_at < 19 {
        true
    } else {
        // 19 digits: compare against 9223372036854775808.
        let padded = format!("{}{}", trimmed, "0".repeat((decimal_at - count) as usize));
        match padded.as_str().cmp("9223372036854775808") {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Equal => negative,
            std::cmp::Ordering::Greater => false,
        }
    };
    if fits_long {
        if count == 0 {
            return Ok(Some(JNum::Long(0)));
        }
        let padded = format!("{}{}", trimmed, "0".repeat((decimal_at - count) as usize));
        let mag: i128 = padded.parse().unwrap();
        let v = if negative { -mag } else { mag };
        return Ok(Some(JNum::Long(v as i64)));
    }
    let d: f64 = if count == 0 { 0.0 } else { format!("0.{}E{}", trimmed, decimal_at).parse().unwrap() };
    Ok(Some(JNum::Double(if negative { -d } else { d })))
}

/// `String.trim`: strips chars <= U+0020.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

// ---- casts ----

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Day,
    HalfDay,
    Hour,
    Minute,
    Second,
    Milli,
    Micro,
    Nano,
}

impl Unit {
    fn nanos(self) -> i128 {
        match self {
            Unit::Day => 86_400 * NANOS_PER_SEC,
            Unit::HalfDay => 43_200 * NANOS_PER_SEC,
            Unit::Hour => 3_600 * NANOS_PER_SEC,
            Unit::Minute => 60 * NANOS_PER_SEC,
            Unit::Second => NANOS_PER_SEC,
            Unit::Milli => 1_000_000,
            Unit::Micro => 1_000,
            Unit::Nano => 1,
        }
    }

    /// `ChronoUnit.isDateBased`.
    fn date_based(self) -> bool {
        self == Unit::Day
    }
}

/// `Casts.MidstUnit`. Keeps the original string for error messages.
fn unit(v: &Value, pos: &Position) -> R<(Unit, String)> {
    let u = match v {
        Value::String(s) => match s.trim() {
            "day" | "days" => Some(Unit::Day),
            "half day" | "half days" => Some(Unit::HalfDay),
            "hour" | "hours" => Some(Unit::Hour),
            "minute" | "minutes" => Some(Unit::Minute),
            "second" | "seconds" => Some(Unit::Second),
            "millisecond" | "milliseconds" => Some(Unit::Milli),
            "microsecond" | "microseconds" => Some(Unit::Micro),
            "nanosecond" | "nanoseconds" => Some(Unit::Nano),
            _ => None,
        }
        .map(|u| (u, s.clone())),
        _ => None,
    };
    u.ok_or_else(|| vec![FaunaError::invalid_argument(&[Type::TimeUnit], v.rtype(), pos)])
}

enum Temporal {
    Time(Timestamp),
    Date(Date),
}

/// `Casts.Temporal`.
fn temporal(v: &Value, pos: &Position) -> R<Temporal> {
    match v {
        Value::Time(t) => Ok(Temporal::Time(*t)),
        Value::Date(d) => Ok(Temporal::Date(*d)),
        other => one(FaunaError::invalid_argument(&[Type::Temporal], other.rtype(), pos)),
    }
}

fn arg<T>(
    ctx: &mut Ctx,
    env: &Env,
    args: &Args,
    key: &str,
    pos: &Position,
    cast: impl FnOnce(&Value, &Position) -> R<T>,
) -> R<T> {
    let v = args.eval(ctx, env, key, pos)?;
    cast(&v, &pos.key(key))
}

// ---- functions ----

/// `TimeFunction`: errors are reported at the form, not the argument.
fn parse_time(ctx: &Ctx, s: &str, pos: &Position) -> R<Value> {
    if s == "now" {
        return Ok(Value::Time(ctx.ts));
    }
    match parse_java_instant(java_trim(s)) {
        Some(n) => instant_of_nanos(n, pos).map(Value::Time),
        None => one(FaunaError::invalid_time_argument(s, pos)),
    }
}

/// `DateFunction`: errors are reported at the form, not the argument.
fn parse_date(s: &str, pos: &Position) -> R<Value> {
    match parse_java_date(java_trim(s)) {
        Some((y, m, d)) => date_of_epoch_day(days_from_civil(y, m, d), pos).map(Value::Date),
        None => one(FaunaError::invalid_date_argument(s, pos)),
    }
}

fn time(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "time", pos)?;
    let s = cast::string(&v, &pos.key("time"))?;
    parse_time(ctx, s, pos)
}

fn now(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "now", pos)?;
    cast::null(&v, &pos.key("now"))?;
    Ok(Value::Time(ctx.ts))
}

fn date(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "date", pos)?;
    let s = cast::string(&v, &pos.key("date"))?;
    parse_date(s, pos)
}

fn epoch(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (n, (u, _)) = join2(arg(ctx, env, args, "epoch", pos, cast::long), arg(ctx, env, args, "unit", pos, unit))?;
    instant_of_nanos(n as i128 * u.nanos(), pos).map(Value::Time)
}

fn to_unit(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let t = cast::timestamp(&v, &pos.key(key))?;
    Ok(Value::Long(match key {
        "to_micros" => t.micros(),
        "to_millis" => t.millis(),
        _ => t.secs,
    }))
}

fn component(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let t = renderable(cast::timestamp(&v, &pos.key(key))?, &pos.key(key))?;
    let dt = t.to_naive();
    Ok(Value::Long(match key {
        "second" => dt.second() as i64,
        "minute" => dt.minute() as i64,
        "hour" => dt.hour() as i64,
        "day_of_month" => dt.day() as i64,
        "day_of_week" => dt.weekday().number_from_monday() as i64,
        "day_of_year" => dt.ordinal() as i64,
        "month" => dt.month() as i64,
        _ => dt.year() as i64,
    }))
}

fn time_math(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (base, offset, (u, original)) = join3(
        arg(ctx, env, args, key, pos, temporal),
        arg(ctx, env, args, "offset", pos, cast::long),
        arg(ctx, env, args, "unit", pos, unit),
    )?;
    let delta = if key == "time_add" { offset as i128 } else { -(offset as i128) };
    let opos = pos.key("offset");
    match base {
        Temporal::Time(t) => instant_of_nanos(t.total_nanos() + delta * u.nanos(), &opos).map(Value::Time),
        Temporal::Date(_) if !u.date_based() => one(FaunaError::invalid_time_unit(&original, &pos.key("unit"))),
        Temporal::Date(d) => date_of_epoch_day(epoch_day(&d) + delta, &opos).map(Value::Date),
    }
}

/// `ChronoUnit.between` for instants (`Instant.until`).
fn instant_until(a: Timestamp, b: Timestamp, u: Unit) -> Option<i64> {
    let secs_diff = b.secs as i128 - a.secs as i128;
    let nanos_diff = b.nanos as i128 - a.nanos as i128;
    let seconds_until = || {
        let mut s = secs_diff;
        if s > 0 && nanos_diff < 0 {
            s -= 1;
        } else if s < 0 && nanos_diff > 0 {
            s += 1;
        }
        s
    };
    let r: i128 = match u {
        Unit::Nano => secs_diff * NANOS_PER_SEC + nanos_diff,
        // Java integer division truncates towards zero, like Rust.
        Unit::Micro => secs_diff * 1_000_000 + nanos_diff / 1000,
        Unit::Milli => b.millis() as i128 - a.millis() as i128,
        Unit::Second => seconds_until(),
        Unit::Minute => seconds_until() / 60,
        Unit::Hour => seconds_until() / 3600,
        Unit::HalfDay => seconds_until() / 43_200,
        Unit::Day => seconds_until() / 86_400,
    };
    i64::try_from(r).ok()
}

fn time_diff(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (start, finish, (u, original)) = join3(
        arg(ctx, env, args, "time_diff", pos, temporal),
        arg(ctx, env, args, "other", pos, temporal),
        arg(ctx, env, args, "unit", pos, unit),
    )?;
    match (start, finish) {
        (Temporal::Time(a), Temporal::Time(b)) => match instant_until(a, b, u) {
            Some(n) => Ok(Value::Long(n)),
            None => one(FaunaError::not_emulated(
                "TimeDiff results that overflow a Long (an internal error on the server)",
                pos,
            )),
        },
        (Temporal::Date(a), Temporal::Date(b)) if u.date_based() => {
            Ok(Value::Long((epoch_day(&b) - epoch_day(&a)) as i64))
        }
        (Temporal::Date(_), Temporal::Date(_)) => one(FaunaError::invalid_time_unit(&original, &pos.key("unit"))),
        _ => one(FaunaError::incompatible_time_arguments(&pos.key("time_diff"))),
    }
}

fn to_string(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_string", pos)?;
    match to_string_literal(&v) {
        Some(s) => Ok(Value::String(s)),
        None => one(FaunaError::invalid_cast(v.rtype(), Type::String, pos)),
    }
}

fn parse_number_arg(s: &str, pos: &Position) -> R<Option<JNum>> {
    parse_java_number(java_trim(s)).or_else(|what| one(FaunaError::not_emulated(&what, pos)))
}

fn to_number(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_number", pos)?;
    match &v {
        Value::Long(_) | Value::Double(_) => Ok(v),
        Value::String(s) => match parse_number_arg(s, pos)? {
            Some(JNum::Long(l)) => Ok(Value::Long(l)),
            Some(JNum::Double(d)) => Ok(Value::Double(d)),
            None => one(FaunaError::invalid_cast(Type::String, Type::Number, pos)),
        },
        other => one(FaunaError::invalid_cast(other.rtype(), Type::Number, pos)),
    }
}

fn to_double(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_double", pos)?;
    let p = pos.key("to_double");
    match &v {
        Value::Double(_) => Ok(v),
        Value::Long(l) => Ok(Value::Double(*l as f64)),
        Value::String(s) => match parse_number_arg(s, &p)? {
            Some(JNum::Long(l)) => Ok(Value::Double(l as f64)),
            Some(JNum::Double(d)) => Ok(Value::Double(d)),
            None => one(FaunaError::invalid_cast(Type::String, Type::Double, &p)),
        },
        other => one(FaunaError::invalid_cast(other.rtype(), Type::Double, &p)),
    }
}

fn to_integer(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_integer", pos)?;
    let p = pos.key("to_integer");
    match &v {
        Value::Long(_) => Ok(v),
        // `Double.toLong` saturates and maps NaN to 0, like `as`.
        Value::Double(d) => Ok(Value::Long(*d as i64)),
        Value::String(s) => match parse_number_arg(s, &p)? {
            Some(JNum::Long(l)) => Ok(Value::Long(l)),
            Some(JNum::Double(d)) => Ok(Value::Long(d as i64)),
            None => one(FaunaError::invalid_cast(Type::String, Type::Integer, &p)),
        },
        other => one(FaunaError::invalid_cast(other.rtype(), Type::Integer, &p)),
    }
}

fn to_time(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_time", pos)?;
    match &v {
        // `Timestamp.ofMillis`.
        Value::Long(l) => instant_of_nanos(*l as i128 * 1_000_000, pos).map(Value::Time),
        Value::Time(_) => Ok(v),
        Value::Date(d) => instant_of_nanos(epoch_day(d) * 86_400 * NANOS_PER_SEC, pos).map(Value::Time),
        Value::String(s) => parse_time(ctx, s, pos),
        other => one(FaunaError::invalid_cast(other.rtype(), Type::Time, pos)),
    }
}

fn to_date(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_date", pos)?;
    match &v {
        Value::Time(t) => Ok(Value::Date(t.to_naive().date())),
        Value::Date(_) => Ok(v),
        Value::String(s) => parse_date(s, pos),
        other => one(FaunaError::invalid_cast(other.rtype(), Type::Date, pos)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(s: &str) -> String {
        match parse_java_number(s).unwrap() {
            Some(JNum::Long(l)) => format!("L{}", l),
            Some(JNum::Double(d)) => format!("D{:?}", d),
            None => "err".into(),
        }
    }

    #[test]
    fn number_format_parse() {
        assert_eq!(num("12"), "L12");
        assert_eq!(num("-12"), "L-12");
        assert_eq!(num("1.0"), "L1");
        assert_eq!(num("1.5"), "D1.5");
        assert_eq!(num("-0"), "D-0.0");
        assert_eq!(num("0"), "L0");
        assert_eq!(num("1,234"), "L1234");
        assert_eq!(num("1,"), "L1");
        assert_eq!(num("12abc"), "L12");
        assert_eq!(num("1E3"), "L1000");
        assert_eq!(num("1.5E-3"), "D0.0015");
        assert_eq!(num("1e3"), "L1");
        assert_eq!(num(".5"), "D0.5");
        assert_eq!(num("abc"), "err");
        assert_eq!(num("+5"), "err");
        assert_eq!(num("-"), "err");
        assert_eq!(num("9223372036854775807"), "L9223372036854775807");
        assert_eq!(num("-9223372036854775808"), "L-9223372036854775808");
        assert_eq!(num("9223372036854775808"), "D9.223372036854776e18");
        assert_eq!(num("0.000123"), "D0.000123");
        assert_eq!(num("NaN"), "DNaN");
    }

    #[test]
    fn instant_parse_and_print() {
        let p = |s: &str| parse_java_instant(s).map(|n| instant_to_string(&Timestamp::of_nanos(n)));
        assert_eq!(p("2019-05-27T05:15:48+0100").as_deref(), Some("2019-05-27T04:15:48Z"));
        assert_eq!(p("2019-05-27T05:15:48+01:00").as_deref(), Some("2019-05-27T04:15:48Z"));
        assert_eq!(p("2019-05-27T05:15:48+0000").as_deref(), Some("2019-05-27T05:15:48Z"));
        assert_eq!(p("2019-05-27T05:15Z").as_deref(), Some("2019-05-27T05:15:00Z"));
        assert_eq!(p("1970-01-01T00:00:00.1Z").as_deref(), Some("1970-01-01T00:00:00.100Z"));
        assert_eq!(p("1970-01-01T00:00:00.0000001Z").as_deref(), Some("1970-01-01T00:00:00.000000100Z"));
        assert_eq!(p("2019-05-27T05:15:48"), None);
        // SMART resolver: an out-of-range day clamps to the month end.
        assert_eq!(p("2019-02-29T00:00:00Z").as_deref(), Some("2019-02-28T00:00:00Z"));
        assert_eq!(p("2020-02-29T00:00:00Z").as_deref(), Some("2020-02-29T00:00:00Z"));
        assert_eq!(p("2019-05-27 05:15:48Z"), None);
        assert_eq!(p("2019-05-27T24:00:00Z"), None);
        assert_eq!(p("+2019-05-27T00:00:00Z"), None);
        assert_eq!(p("+12019-05-27T00:00:00Z").as_deref(), Some("+12019-05-27T00:00:00Z"));
        assert_eq!(p("-0001-01-01T00:00:00Z").as_deref(), Some("-0001-01-01T00:00:00Z"));
        assert_eq!(parse_java_date("19700101"), None);
        assert_eq!(parse_java_date("1970-1-01"), None);
    }
}
