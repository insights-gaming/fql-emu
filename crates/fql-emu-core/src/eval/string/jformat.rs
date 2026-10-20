//! Ports of the JDK number formatting the server relies on:
//! `Double.toString` and the `java.util.Formatter` conversions used by
//! `Format()` (`%s %b %d %o %x %e %f %g` and the `%t` date/time
//! suffixes), in the server's default `en_US` locale.
//!
//! Floating point conversions follow `sun.misc.FormattedFloatingDecimal`:
//! they round the *shortest* decimal representation half-up (so
//! `%.1f` of `0.15` is `0.2`, unlike C's `printf`).

/// Why a specifier could not be formatted.
#[derive(Debug, PartialEq)]
pub enum FmtErr {
    /// An `IllegalFormatException` on the server, reported as
    /// "Invalid conversion format '<spec>'.".
    Conversion,
    /// Valid on the server but not emulated.
    Unsupported(String),
}

pub type FRes<T> = Result<T, FmtErr>;

// ---- Double.toString ----

/// Shortest round-tripping decimal digits of a finite `v > 0`, as
/// `0.d1d2d3... * 10^exp` (the `FloatingDecimal` convention).
fn decimal_digits(v: f64) -> (Vec<u8>, i32) {
    if v == 0.0 {
        return (vec![b'0'], 0);
    }
    let s = format!("{:e}", v);
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: Vec<u8> = mant.bytes().filter(|b| b.is_ascii_digit()).collect();
    (digits, exp + 1)
}

/// `java.lang.Double.toString`.
pub fn java_double_to_string(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let sign = if d.is_sign_negative() { "-" } else { "" };
    if d == 0.0 {
        return format!("{}0.0", sign);
    }
    let a = d.abs();
    let (digits, exp) = decimal_digits(a);
    let ds: String = digits.iter().map(|&b| b as char).collect();
    if (1e-3..1e7).contains(&a) {
        if exp > 0 {
            let e = exp as usize;
            if ds.len() <= e {
                format!("{}{}{}.0", sign, ds, "0".repeat(e - ds.len()))
            } else {
                format!("{}{}.{}", sign, &ds[..e], &ds[e..])
            }
        } else {
            format!("{}0.{}{}", sign, "0".repeat((-exp) as usize), ds)
        }
    } else {
        let frac = if ds.len() > 1 { &ds[1..] } else { "0" };
        format!("{}{}.{}E{}", sign, &ds[..1], frac, exp - 1)
    }
}

// ---- format specifiers ----

#[derive(Clone, Copy, Default, Debug)]
pub struct Flags {
    pub left: bool,
    pub alt: bool,
    pub plus: bool,
    pub space: bool,
    pub zero: bool,
    pub group: bool,
    pub paren: bool,
}

/// The parts of a `%...` specifier Java's `Formatter` validates.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub flags: Flags,
    pub width: Option<usize>,
    pub precision: Option<usize>,
    pub upper: bool,
}

const MAX_WIDTH: u64 = 1 << 20;

fn parse_num(s: Option<&str>) -> FRes<Option<usize>> {
    match s {
        None => Ok(None),
        Some(s) => match s.parse::<u64>() {
            Ok(n) if n > i32::MAX as u64 => Err(FmtErr::Conversion),
            Ok(n) if n > MAX_WIDTH => Err(FmtErr::Unsupported("format widths/precisions over 1048576".into())),
            Ok(n) => Ok(Some(n as usize)),
            Err(_) => Err(FmtErr::Conversion),
        },
    }
}

impl Spec {
    pub fn parse(flags: &str, width: Option<&str>, precision: Option<&str>, upper: bool) -> FRes<Spec> {
        let mut f = Flags::default();
        for c in flags.chars() {
            let slot = match c {
                '-' => &mut f.left,
                '#' => &mut f.alt,
                '+' => &mut f.plus,
                ' ' => &mut f.space,
                '0' => &mut f.zero,
                ',' => &mut f.group,
                '(' => &mut f.paren,
                // `%<s` refers to the previous argument, of which there is
                // none in a single-value `String.format`.
                _ => return Err(FmtErr::Conversion),
            };
            if *slot {
                return Err(FmtErr::Conversion); // DuplicateFormatFlagsException
            }
            *slot = true;
        }
        Ok(Spec { flags: f, width: parse_num(width)?, precision: parse_num(precision)?, upper })
    }

    fn check(&self, bad: bool) -> FRes<()> {
        if bad {
            Err(FmtErr::Conversion)
        } else {
            Ok(())
        }
    }

    /// `checkGeneral` (`%s`, `%b`).
    fn check_general(&self, is_bool: bool) -> FRes<()> {
        let f = self.flags;
        self.check(is_bool && f.alt)?;
        self.check(self.width.is_none() && f.left)?;
        self.check(f.plus || f.space || f.zero || f.group || f.paren)
    }

    /// `checkNumeric`.
    fn check_numeric(&self) -> FRes<()> {
        let f = self.flags;
        self.check(self.width.is_none() && (f.left || f.zero))?;
        self.check((f.plus && f.space) || (f.left && f.zero))
    }

    /// `appendJustified`. Widths count UTF-16 units, like Java.
    fn justify(&self, s: String) -> String {
        let Some(w) = self.width else { return s };
        let len = s.encode_utf16().count();
        if len >= w {
            return s;
        }
        let pad = " ".repeat(w - len);
        if self.flags.left {
            s + &pad
        } else {
            pad + &s
        }
    }

    fn leading_sign(&self, sb: &mut String, neg: bool) {
        if !neg {
            if self.flags.plus {
                sb.push('+');
            } else if self.flags.space {
                sb.push(' ');
            }
        } else if self.flags.paren {
            sb.push('(');
        } else {
            sb.push('-');
        }
    }

    fn trailing_sign(&self, sb: &mut String, neg: bool) {
        if neg && self.flags.paren {
            sb.push(')');
        }
    }

    fn adjust_width(&self, width: Option<i64>, neg: bool) -> Option<i64> {
        match width {
            Some(w) if neg && self.flags.paren => Some(w - 1),
            w => w,
        }
    }

    /// `localizedMagnitude` for an ASCII digit string with an optional
    /// `.`: grouping and zero padding (the padding counts what is already
    /// in `sb`, i.e. the sign).
    fn magnitude(&self, sb: &mut String, value: &str, offset: usize, width: Option<i64>) {
        let begin = sb.len();
        let dot = value.find('.').unwrap_or(value.len());
        let mut group = self.flags.group;
        for (j, c) in value.bytes().enumerate().skip(offset) {
            if j == dot {
                sb.push('.');
                group = false;
                continue;
            }
            sb.push(c as char);
            if group && j + 1 != dot && (dot - j) % 3 == 1 {
                sb.push(',');
            }
        }
        if let Some(w) = width {
            if self.flags.zero {
                while (sb.len() as i64) < w {
                    sb.insert(begin, '0');
                }
            }
        }
    }
}

/// `%s` of an already converted string.
pub fn fmt_string(spec: &Spec, s: &str) -> FRes<String> {
    spec.check_general(false)?;
    // `#` needs a java.util.Formattable argument.
    spec.check(spec.flags.alt)?;
    let mut s = s.to_string();
    if let Some(p) = spec.precision {
        let mut units = 0;
        let mut end = s.len();
        for (i, c) in s.char_indices() {
            if units + c.len_utf16() > p {
                end = i;
                break;
            }
            units += c.len_utf16();
        }
        s.truncate(end);
    }
    if spec.upper {
        s = s.to_uppercase();
    }
    Ok(spec.justify(s))
}

/// `%b`: `true`/`false`; `null` is `false`, anything else `true`.
pub fn fmt_bool(spec: &Spec, b: bool) -> FRes<String> {
    spec.check_general(true)?;
    let mut s = b.to_string();
    if let Some(p) = spec.precision {
        s.truncate(p.min(s.len()));
    }
    if spec.upper {
        s = s.to_uppercase();
    }
    Ok(spec.justify(s))
}

/// `%d`, `%o`, `%x` of a Long.
pub fn fmt_long(spec: &Spec, conv: char, v: i64) -> FRes<String> {
    let f = spec.flags;
    spec.check_numeric()?;
    spec.check(spec.precision.is_some())?;
    match conv {
        'd' => spec.check(f.alt)?,
        _ => spec.check(f.group)?,
    }
    let width = spec.width.map(|w| w as i64);
    let mut sb = String::new();
    match conv {
        'd' => {
            let neg = v < 0;
            let s = v.to_string();
            spec.leading_sign(&mut sb, neg);
            spec.magnitude(&mut sb, &s, if neg { 1 } else { 0 }, spec.adjust_width(width, neg));
            spec.trailing_sign(&mut sb, neg);
        }
        _ => {
            spec.check(f.paren || f.space || f.plus)?;
            let (mut s, prefix) = match conv {
                'o' => (format!("{:o}", v as u64), "0"),
                _ => (format!("{:x}", v as u64), if spec.upper { "0X" } else { "0x" }),
            };
            let len = s.len() + if f.alt { prefix.len() } else { 0 };
            if f.alt {
                sb.push_str(prefix);
            }
            if f.zero {
                let w = spec.width.unwrap_or(0);
                for _ in len..w {
                    sb.push('0');
                }
            }
            if spec.upper {
                s = s.to_uppercase();
            }
            sb.push_str(&s);
        }
    }
    Ok(spec.justify(sb))
}

/// `FormattedFloatingDecimal.applyPrecision`: round `digits` half-up to
/// `prec` significant digits; returns the (possibly carried) exponent.
fn apply_precision(dec_exp: i32, digits: &mut [u8], prec: i32) -> i32 {
    let n = digits.len() as i32;
    if prec >= n || prec < 0 {
        return dec_exp;
    }
    if prec == 0 {
        if digits[0] >= b'5' {
            digits[0] = b'1';
            digits[1..].fill(b'0');
            return dec_exp + 1;
        }
        digits.fill(b'0');
        return dec_exp;
    }
    let p = prec as usize;
    if digits[p] >= b'5' {
        let mut i = p - 1;
        let mut q = digits[i];
        if q == b'9' {
            while q == b'9' && i > 0 {
                i -= 1;
                q = digits[i];
            }
            if q == b'9' {
                digits[0] = b'1';
                digits[1..].fill(b'0');
                return dec_exp + 1;
            }
        }
        digits[i] = q + 1;
        digits[i + 1..].fill(b'0');
    } else {
        digits[p..].fill(b'0');
    }
    dec_exp
}

fn s(digits: &[u8]) -> &str {
    std::str::from_utf8(digits).unwrap()
}

/// `FormattedFloatingDecimal.fillDecimal`.
fn fill_decimal(precision: i32, digits: &[u8], exp: i32) -> String {
    let n = digits.len() as i32;
    if exp > 0 {
        if n < exp {
            format!("{}{}", s(digits), "0".repeat((exp - n) as usize))
        } else {
            let t = (n - exp).min(precision);
            let int = s(&digits[..exp as usize]);
            if t > 0 {
                format!("{}.{}", int, s(&digits[exp as usize..(exp + t) as usize]))
            } else {
                int.to_string()
            }
        }
    } else {
        let zeros = (-exp).min(precision).max(0);
        let t = n.min(precision + exp).max(0);
        if zeros > 0 {
            format!("0.{}{}", "0".repeat(zeros as usize), s(&digits[..t as usize]))
        } else if t > 0 {
            format!("0.{}", s(&digits[..t as usize]))
        } else {
            "0".to_string()
        }
    }
}

/// `FormattedFloatingDecimal.fillScientific`: mantissa and exponent
/// (`+05`, `-123`).
fn fill_scientific(precision: i32, digits: &[u8], exp: i32) -> (String, String) {
    let n = digits.len() as i32;
    let t = (n - 1).min(precision).max(0);
    let mant = if t > 0 {
        format!("{}.{}", digits[0] as char, s(&digits[1..(1 + t) as usize]))
    } else {
        (digits[0] as char).to_string()
    };
    let (sign, e) = if exp <= 0 { ('-', -exp + 1) } else { ('+', exp - 1) };
    (mant, format!("{}{:02}", sign, e))
}

/// `Formatter.addZeros`.
fn add_zeros(mant: &mut String, prec: i32) {
    let (dot, need_dot) = match mant.find('.') {
        Some(i) => (i, false),
        None => (mant.len(), true),
    };
    let out_prec = mant.len() as i32 - dot as i32 - if need_dot { 0 } else { 1 };
    if out_prec >= prec {
        return;
    }
    if need_dot {
        mant.push('.');
    }
    for _ in out_prec..prec {
        mant.push('0');
    }
}

/// `%e`, `%f`, `%g` of a Double.
pub fn fmt_double(spec: &Spec, conv: char, value: f64) -> FRes<String> {
    let f = spec.flags;
    spec.check_numeric()?;
    match conv {
        'e' => spec.check(f.group)?,
        'g' => spec.check(f.alt)?,
        _ => {}
    }
    let mut sb = String::new();
    let neg = value < 0.0 || (value == 0.0 && value.is_sign_negative());
    if value.is_nan() {
        sb.push_str(if spec.upper { "NAN" } else { "NaN" });
        return Ok(spec.justify(sb));
    }
    let v = value.abs();
    spec.leading_sign(&mut sb, neg);
    if v.is_infinite() {
        sb.push_str(if spec.upper { "INFINITY" } else { "Infinity" });
    } else {
        let width = spec.width.map(|w| w as i64);
        let (mut digits, dec_exp) = decimal_digits(v);
        match conv {
            'e' => {
                let prec = spec.precision.map(|p| p as i32).unwrap_or(6);
                let exp = apply_precision(dec_exp, &mut digits, prec + 1);
                let (mut mant, e) = fill_scientific(prec, &digits, exp);
                add_zeros(&mut mant, prec);
                if f.alt && prec == 0 {
                    mant.push('.');
                }
                let e = if v == 0.0 { "+00".to_string() } else { e };
                let w = width.map(|w| w - e.len() as i64 - 1);
                spec.magnitude(&mut sb, &mant, 0, spec.adjust_width(w, neg));
                sb.push(if spec.upper { 'E' } else { 'e' });
                sb.push_str(&e);
            }
            'f' => {
                let prec = spec.precision.map(|p| p as i32).unwrap_or(6);
                let exp = apply_precision(dec_exp, &mut digits, dec_exp + prec);
                let mut mant = fill_decimal(prec, &digits, exp);
                add_zeros(&mut mant, prec);
                if f.alt && prec == 0 {
                    mant.push('.');
                }
                spec.magnitude(&mut sb, &mant, 0, spec.adjust_width(width, neg));
            }
            _ => {
                let p = match spec.precision {
                    None => 6,
                    Some(0) => 1,
                    Some(p) => p as i32,
                };
                let (mut mant, e, frac) = if v == 0.0 {
                    ("0".to_string(), None, p - 1)
                } else {
                    let exp = apply_precision(dec_exp, &mut digits, p);
                    if exp - 1 < -4 || exp > p {
                        let (m, e) = fill_scientific(p - 1, &digits, exp);
                        (m, Some(e), p - 1)
                    } else {
                        (fill_decimal(p - exp, &digits, exp), None, p - exp)
                    }
                };
                add_zeros(&mut mant, frac);
                let w = match &e {
                    Some(e) => width.map(|w| w - e.len() as i64 - 1),
                    None => width,
                };
                spec.magnitude(&mut sb, &mant, 0, spec.adjust_width(w, neg));
                if let Some(e) = e {
                    sb.push(if spec.upper { 'E' } else { 'e' });
                    sb.push_str(&e);
                }
            }
        }
    }
    spec.trailing_sign(&mut sb, neg);
    Ok(spec.justify(sb))
}

// ---- %t ----

/// Calendar fields of a `LocalDate`, or of an `OffsetDateTime` at UTC
/// when `time` is set.
pub struct Tm {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    /// ISO day of week, Monday = 1.
    pub weekday: u32,
    pub yday: u32,
    pub time: Option<TimeFields>,
}

pub struct TimeFields {
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nano: u32,
    pub epoch_secs: i64,
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];
const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

fn print_tm(sb: &mut String, tm: &Tm, c: char) -> FRes<()> {
    let year_of_era = if tm.year >= 1 { tm.year } else { 1 - tm.year };
    let t = |sb: &mut String, f: &dyn Fn(&TimeFields) -> String| -> FRes<()> {
        match &tm.time {
            Some(tf) => {
                sb.push_str(&f(tf));
                Ok(())
            }
            None => Err(FmtErr::Conversion),
        }
    };
    let h12 = |h: u32| if h.is_multiple_of(12) { 12 } else { h % 12 };
    match c {
        'H' => t(sb, &|tf| format!("{:02}", tf.hour))?,
        'I' => t(sb, &|tf| format!("{:02}", h12(tf.hour)))?,
        'k' => t(sb, &|tf| tf.hour.to_string())?,
        'l' => t(sb, &|tf| h12(tf.hour).to_string())?,
        'M' => t(sb, &|tf| format!("{:02}", tf.minute))?,
        'S' => t(sb, &|tf| format!("{:02}", tf.second))?,
        'L' => t(sb, &|tf| format!("{:03}", tf.nano / 1_000_000))?,
        'N' => t(sb, &|tf| format!("{:09}", tf.nano))?,
        'p' => t(sb, &|tf| (if tf.hour < 12 { "am" } else { "pm" }).to_string())?,
        'z' => t(sb, &|_| "+0000".to_string())?,
        'Z' => t(sb, &|_| "Z".to_string())?,
        's' => t(sb, &|tf| tf.epoch_secs.to_string())?,
        'Q' => t(sb, &|tf| (tf.epoch_secs * 1000 + (tf.nano / 1_000_000) as i64).to_string())?,
        'B' => sb.push_str(MONTHS[tm.month as usize - 1]),
        'b' | 'h' => sb.push_str(&MONTHS[tm.month as usize - 1][..3]),
        'A' => sb.push_str(DAYS[tm.weekday as usize - 1]),
        'a' => sb.push_str(&DAYS[tm.weekday as usize - 1][..3]),
        'C' => sb.push_str(&format!("{:02}", year_of_era / 100)),
        'y' => sb.push_str(&format!("{:02}", year_of_era % 100)),
        'Y' => sb.push_str(&format!("{:04}", year_of_era)),
        'j' => sb.push_str(&format!("{:03}", tm.yday)),
        'm' => sb.push_str(&format!("{:02}", tm.month)),
        'd' => sb.push_str(&format!("{:02}", tm.day)),
        'e' => sb.push_str(&tm.day.to_string()),
        'R' => {
            print_tm(sb, tm, 'H')?;
            sb.push(':');
            print_tm(sb, tm, 'M')?;
        }
        'T' => {
            print_tm(sb, tm, 'H')?;
            sb.push(':');
            print_tm(sb, tm, 'M')?;
            sb.push(':');
            print_tm(sb, tm, 'S')?;
        }
        'r' => {
            print_tm(sb, tm, 'I')?;
            sb.push(':');
            print_tm(sb, tm, 'M')?;
            sb.push(':');
            print_tm(sb, tm, 'S')?;
            sb.push(' ');
            let mut ampm = String::new();
            print_tm(&mut ampm, tm, 'p')?;
            sb.push_str(&ampm.to_uppercase());
        }
        'D' => {
            print_tm(sb, tm, 'm')?;
            sb.push('/');
            print_tm(sb, tm, 'd')?;
            sb.push('/');
            print_tm(sb, tm, 'y')?;
        }
        'F' => {
            print_tm(sb, tm, 'Y')?;
            sb.push('-');
            print_tm(sb, tm, 'm')?;
            sb.push('-');
            print_tm(sb, tm, 'd')?;
        }
        'c' => {
            for (i, c) in ['a', 'b', 'd', 'T', 'Z', 'Y'].into_iter().enumerate() {
                if i > 0 {
                    sb.push(' ');
                }
                print_tm(sb, tm, c)?;
            }
        }
        _ => return Err(FmtErr::Conversion),
    }
    Ok(())
}

/// `%t<c>` / `%T<c>`.
pub fn fmt_temporal(spec: &Spec, c: char, tm: &Tm) -> FRes<String> {
    let f = spec.flags;
    spec.check(spec.precision.is_some())?;
    spec.check(f.alt || f.plus || f.space || f.zero || f.group || f.paren)?;
    spec.check(spec.width.is_none() && f.left)?;
    let mut sb = String::new();
    print_tm(&mut sb, tm, c)?;
    if spec.upper {
        sb = sb.to_uppercase();
    }
    Ok(spec.justify(sb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(flags: &str, w: Option<&str>, p: Option<&str>) -> Spec {
        Spec::parse(flags, w, p, false).unwrap()
    }

    #[test]
    fn double_to_string() {
        assert_eq!(java_double_to_string(1.0), "1.0");
        assert_eq!(java_double_to_string(-0.0), "-0.0");
        assert_eq!(java_double_to_string(2.71), "2.71");
        assert_eq!(java_double_to_string(1e7), "1.0E7");
        assert_eq!(java_double_to_string(1234567.0), "1234567.0");
        assert_eq!(java_double_to_string(0.001), "0.001");
        assert_eq!(java_double_to_string(0.0001), "1.0E-4");
        assert_eq!(java_double_to_string(1.5e-10), "1.5E-10");
        assert_eq!(java_double_to_string(100.0), "100.0");
        assert_eq!(java_double_to_string(f64::NAN), "NaN");
    }

    #[test]
    fn floats_round_half_up_on_shortest_digits() {
        let d = |p: &str, v: f64| fmt_double(&spec("", None, Some(p)), 'f', v).unwrap();
        assert_eq!(d("1", 0.15), "0.2");
        assert_eq!(d("2", 1.005), "1.01");
        assert_eq!(d("2", 1.23456), "1.23");
        assert_eq!(d("2", 0.009), "0.01");
        assert_eq!(d("2", 0.0009), "0.00");
        assert_eq!(d("0", 2.5), "3");
        assert_eq!(fmt_double(&spec("", None, None), 'f', 1e20).unwrap(), "100000000000000000000.000000");
        assert_eq!(fmt_double(&spec("", None, None), 'e', 2.5).unwrap(), "2.500000e+00");
        assert_eq!(fmt_double(&spec("", None, None), 'g', 2.5).unwrap(), "2.50000");
        assert_eq!(fmt_double(&spec("", None, None), 'g', 0.0001234).unwrap(), "0.000123400");
        assert_eq!(fmt_double(&spec("", None, None), 'g', 0.0).unwrap(), "0.00000");
        assert_eq!(fmt_double(&spec("", None, None), 'e', 0.0).unwrap(), "0.000000e+00");
        assert_eq!(fmt_double(&spec("", None, Some("1")), 'e', 9.99).unwrap(), "1.0e+01");
        assert_eq!(fmt_double(&spec(",", None, Some("2")), 'f', 1234567.891).unwrap(), "1,234,567.89");
        assert_eq!(fmt_double(&spec("0", Some("10"), Some("2")), 'f', -3.5).unwrap(), "-000003.50");
        assert_eq!(fmt_double(&spec("(", None, None), 'f', -1.23456).unwrap(), "(1.234560)");
        assert_eq!(fmt_double(&spec("", None, None), 'f', -0.0).unwrap(), "-0.000000");
    }

    #[test]
    fn longs() {
        let l = |f: &str, w: Option<&str>, c: char, v: i64| fmt_long(&spec(f, w, None), c, v);
        assert_eq!(l("", None, 'x', -1).unwrap(), "ffffffffffffffff");
        assert_eq!(l("#", None, 'o', 8).unwrap(), "010");
        assert_eq!(l("0", Some("5"), 'd', -42).unwrap(), "-0042");
        assert_eq!(l(",", None, 'd', 1234567).unwrap(), "1,234,567");
        assert_eq!(l("0,", Some("10"), 'd', 1234567).unwrap(), "01,234,567");
        assert_eq!(l("(", Some("6"), 'd', -10).unwrap(), "  (10)");
        assert_eq!(l("#", None, 'd', 1), Err(FmtErr::Conversion));
        assert_eq!(l("+", None, 'x', 1), Err(FmtErr::Conversion));
        assert_eq!(l("-", None, 'd', 1), Err(FmtErr::Conversion));
        assert_eq!(fmt_long(&spec("", None, Some("2")), 'd', 1), Err(FmtErr::Conversion));
        assert!(Spec::parse("--", None, None, false).is_err());
    }

    #[test]
    fn strings() {
        assert_eq!(fmt_string(&spec("", Some("5"), Some("2")), "abc").unwrap(), "   ab");
        assert_eq!(fmt_string(&spec(" ", None, None), "abc"), Err(FmtErr::Conversion));
        assert_eq!(fmt_bool(&spec("#", None, None), true), Err(FmtErr::Conversion));
    }
}
