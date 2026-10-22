//! String functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/StringFunctions.scala.
//! The server delegates most of these to the JVM and ICU4J: regexes are
//! `java.util.regex` (see [`jregex`]), `Format` is `java.util.Formatter`
//! (see [`jformat`]), case mapping and normalisation are ICU (ported with
//! ICU4X), and offsets are counted in code points over UTF-16 strings.

pub mod jformat;
pub mod jregex;

use super::cast;
use super::forms::{Args, Registry};
use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::Backend;
use crate::value::{Type, Value};
use icu_locid::LanguageIdentifier;
use jformat::{FmtErr, Spec, Tm};
use jregex::JRegex;

pub fn register<B: Backend>(r: &mut Registry<B>) {
    // Only the legacy un-underscored spellings exist in API v4; the
    // snake_case variants (`starts_with`, `lower_case`, `split_str`, ...) are
    // `introducedOn(APIVersion.Unstable)` in Parser.scala and the v4 server
    // answers "No form/function found" for them.
    r.add("concat", &["concat"], &["separator"], concat);
    r.add("casefold", &["casefold"], &["normalizer"], casefold);
    r.add("regexescape", &["regexescape"], &[], |c, e, a, p| regex_escape("regexescape", c, e, a, p));
    r.add("startswith", &["startswith", "search"], &[], |c, e, a, p| str_pred("startswith", c, e, a, p));
    r.add("endswith", &["endswith", "search"], &[], |c, e, a, p| str_pred("endswith", c, e, a, p));
    r.add("containsstr", &["containsstr", "search"], &[], |c, e, a, p| str_pred("containsstr", c, e, a, p));
    r.add("containsstrregex", &["containsstrregex", "pattern"], &[], |c, e, a, p| {
        contains_str_regex("containsstrregex", c, e, a, p)
    });
    r.add("findstr", &["findstr", "find"], &["start"], |c, e, a, p| find_str("findstr", c, e, a, p));
    r.add("findstrregex", &["findstrregex", "pattern"], &["start", "num_results"], |c, e, a, p| {
        find_str_regex("findstrregex", c, e, a, p)
    });
    r.add("length", &["length"], &[], |c, e, a, p| unary("length", c, e, a, p));
    r.add("lowercase", &["lowercase"], &[], |c, e, a, p| unary("lowercase", c, e, a, p));
    r.add("ltrim", &["ltrim"], &[], |c, e, a, p| unary("ltrim", c, e, a, p));
    r.add("ngram", &["ngram"], &["min", "max"], ngram);
    r.add("repeat", &["repeat"], &["number"], repeat);
    r.add("replacestr", &["replacestr", "find", "replace"], &[], |c, e, a, p| replace_str("replacestr", c, e, a, p));
    r.add("replacestrregex", &["replacestrregex", "pattern", "replace"], &["first"], |c, e, a, p| {
        replace_str_regex("replacestrregex", c, e, a, p)
    });
    r.add("rtrim", &["rtrim"], &[], |c, e, a, p| unary("rtrim", c, e, a, p));
    r.add("space", &["space"], &[], space);
    r.add("substring", &["substring"], &["start", "length"], |c, e, a, p| sub_string("substring", c, e, a, p));
    r.add("titlecase", &["titlecase"], &[], |c, e, a, p| unary("titlecase", c, e, a, p));
    r.add("trim", &["trim"], &[], |c, e, a, p| unary("trim", c, e, a, p));
    r.add("uppercase", &["uppercase"], &[], |c, e, a, p| unary("uppercase", c, e, a, p));
    r.add("format", &["format", "values"], &[], format);
}

// ---- argument helpers ----

fn merge_errs(rs: Vec<Option<crate::error::Errors>>) -> crate::error::Errors {
    rs.into_iter().flatten().flatten().collect()
}

/// Arguments are evaluated and cast independently and their errors are
/// reported together, like the server's `parT` over native call args.
pub(crate) fn join2<A, B>(a: R<A>, b: R<B>) -> R<(A, B)> {
    match (a, b) {
        (Ok(a), Ok(b)) => Ok((a, b)),
        (a, b) => Err(merge_errs(vec![a.err(), b.err()])),
    }
}

pub(crate) fn join3<A, B, C>(a: R<A>, b: R<B>, c: R<C>) -> R<(A, B, C)> {
    match (a, b, c) {
        (Ok(a), Ok(b), Ok(c)) => Ok((a, b, c)),
        (a, b, c) => Err(merge_errs(vec![a.err(), b.err(), c.err()])),
    }
}

pub(crate) fn join4<A, B, C, D>(a: R<A>, b: R<B>, c: R<C>, d: R<D>) -> R<(A, B, C, D)> {
    match (a, b, c, d) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => Ok((a, b, c, d)),
        (a, b, c, d) => Err(merge_errs(vec![a.err(), b.err(), c.err(), d.err()])),
    }
}

fn str_arg<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, key: &str, pos: &Position) -> R<String> {
    let v = args.eval(ctx, env, key, pos)?;
    cast::string(&v, &pos.key(key)).map(str::to_owned)
}

fn opt_long<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, key: &str, pos: &Position) -> R<Option<i64>> {
    match args.eval_opt(ctx, env, key, pos)? {
        Some(v) => cast::long(&v, &pos.key(key)).map(Some),
        None => Ok(None),
    }
}

/// `Casts.Regex`.
fn regex_arg<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, key: &str, pos: &Position) -> R<JRegex> {
    let p = str_arg(ctx, env, args, key, pos)?;
    compile_regex(&p, &pos.key(key))
}

fn compile_regex(pattern: &str, pos: &Position) -> R<JRegex> {
    match jregex::compile(pattern) {
        Ok(r) => Ok(r),
        Err(jregex::Error::Invalid) => one(FaunaError::invalid_regex(pattern, pos)),
        Err(jregex::Error::Unsupported(what)) => one(FaunaError::not_emulated(&what, pos)),
    }
}

fn check_subject(re: &JRegex, s: &str, pos: &Position) -> R<()> {
    re.check_subject(s).or_else(|what| one(FaunaError::not_emulated(&what, pos)))
}

// ---- UTF-16 / code point arithmetic (java.lang.String) ----

fn utf16_len(s: &str) -> i64 {
    s.encode_utf16().count() as i64
}

fn cp_len(s: &str) -> i64 {
    s.chars().count() as i64
}

/// Byte offset of code point `n` (`offsetByCodePoints(s, 0, n)`), or
/// `None` if `s` has fewer code points.
fn cp_offset(s: &str, n: i64) -> Option<usize> {
    if n == cp_len(s) {
        return Some(s.len());
    }
    s.char_indices().nth(usize::try_from(n).ok()?).map(|(i, _)| i)
}

/// `codePointCount(0, i)` where `i` is a UTF-16 index; an index inside a
/// surrogate pair counts the dangling high surrogate.
fn cp_count_utf16(s: &str, utf16_index: i64) -> i64 {
    let mut units = 0;
    let mut cps = 0;
    for c in s.chars() {
        if units >= utf16_index {
            break;
        }
        units += c.len_utf16() as i64;
        cps += 1;
    }
    cps
}

fn utf16_index(s: &str, byte: usize) -> i64 {
    utf16_len(&s[..byte])
}

fn crash<T>(what: &str, pos: &Position) -> R<T> {
    one(FaunaError::not_emulated(&format!("{} (an internal error on the server)", what), pos))
}

// ---- functions ----

fn concat<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "concat", pos)?;
    let sep = match args.eval_opt(ctx, env, "separator", pos)? {
        Some(s) => cast::string(&s, &pos.key("separator"))?.to_string(),
        None => String::new(),
    };
    let mut parts = Vec::new();
    let mut errs = Vec::new();
    for (e, p) in cast::elems(&v, &pos.key("concat")) {
        match cast::string(&e, &p) {
            Ok(s) => parts.push(s.to_string()),
            Err(x) => errs.extend(x),
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(Value::String(parts.join(&sep)))
}

/// `Casts.ZeroOrMore(Casts.Scalar)`.
fn scalars(v: &Value, pos: &Position) -> R<Vec<Value>> {
    let mut out = Vec::new();
    let mut errs = Vec::new();
    for (e, p) in cast::elems(v, pos) {
        match e {
            Value::Array(_) | Value::Object(_) | Value::Page { .. } | Value::Lambda(_) | Value::Set(_) => {
                errs.push(FaunaError::invalid_argument(&[Type::Scalar], e.rtype(), &p))
            }
            e => out.push(e),
        }
    }
    if errs.is_empty() {
        Ok(out)
    } else {
        Err(errs)
    }
}

#[derive(Clone, Copy)]
enum Normalizer {
    NfkcCaseFold,
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

fn nfkc(s: &str) -> String {
    icu_normalizer::ComposingNormalizer::new_nfkc().normalize(s)
}

/// ICU `Normalizer2.getNFKCCasefoldInstance()`: NFKC, full case folding
/// and removal of default ignorables, to a fixed point.
fn nfkc_casefold(s: &str) -> String {
    let di = icu_properties::sets::default_ignorable_code_point();
    let cm = icu_casemap::CaseMapper::new();
    let mut cur = s.to_string();
    for _ in 0..4 {
        let folded = cm.fold_string(&nfkc(&cur));
        let next: String = nfkc(&folded).chars().filter(|c| !di.contains(*c)).collect();
        if next == cur {
            break;
        }
        cur = next;
    }
    cur
}

fn normalize(n: Normalizer, s: &str) -> String {
    match n {
        Normalizer::NfkcCaseFold => nfkc_casefold(s),
        Normalizer::Nfc => icu_normalizer::ComposingNormalizer::new_nfc().normalize(s),
        Normalizer::Nfkc => nfkc(s),
        Normalizer::Nfd => icu_normalizer::DecomposingNormalizer::new_nfd().normalize(s),
        Normalizer::Nfkd => icu_normalizer::DecomposingNormalizer::new_nfkd().normalize(s),
    }
}

fn casefold<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let terms = args.eval(ctx, env, "casefold", pos).and_then(|v| scalars(&v, &pos.key("casefold")));
    let norm = match args.eval_opt(ctx, env, "normalizer", pos) {
        Ok(None) => Ok(Normalizer::NfkcCaseFold),
        Ok(Some(v)) => match &v {
            Value::String(s) => match s.as_str() {
                "NFKCCaseFold" => Ok(Normalizer::NfkcCaseFold),
                "NFC" => Ok(Normalizer::Nfc),
                "NFD" => Ok(Normalizer::Nfd),
                "NFKC" => Ok(Normalizer::Nfkc),
                "NFKD" => Ok(Normalizer::Nfkd),
                _ => one(FaunaError::invalid_normalizer(Type::String, &pos.key("normalizer"))),
            },
            other => one(FaunaError::invalid_normalizer(other.rtype(), &pos.key("normalizer"))),
        },
        Err(e) => Err(e),
    };
    let (terms, norm) = join2(terms, norm)?;
    let mut out: Vec<Value> = terms
        .into_iter()
        .map(|t| match t {
            Value::String(s) => Value::String(normalize(norm, &s)),
            t => t,
        })
        .collect();
    if out.len() == 1 {
        Ok(out.pop().unwrap())
    } else {
        Ok(Value::Array(out))
    }
}

fn regex_escape<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let s = str_arg(ctx, env, args, key, pos)?;
    Ok(Value::String(jregex::quote(&s)))
}

fn str_pred<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (value, search) = join2(str_arg(ctx, env, args, key, pos), str_arg(ctx, env, args, "search", pos))?;
    Ok(Value::Bool(match key {
        "starts_with" | "startswith" => value.starts_with(&search),
        "ends_with" | "endswith" => value.ends_with(&search),
        _ => value.contains(&search),
    }))
}

fn contains_str_regex<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (value, re) = join2(str_arg(ctx, env, args, key, pos), regex_arg(ctx, env, args, "pattern", pos))?;
    check_subject(&re, &value, &pos.key("pattern"))?;
    Ok(Value::Bool(re.re.is_match(&value)))
}

fn find_str<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (value, find, start) = join3(
        str_arg(ctx, env, args, key, pos),
        str_arg(ctx, env, args, "find", pos),
        opt_long(ctx, env, args, "start", pos),
    )?;
    let offset = match start {
        Some(s) if s > 0 && s < utf16_len(&value) => match cp_offset(&value, s) {
            Some(o) => o,
            None => return crash("a `start` beyond the last code point", &pos.key("start")),
        },
        _ => 0,
    };
    Ok(Value::Long(match value[offset..].find(&find) {
        Some(i) => cp_len(&value[..offset + i]),
        None => -1,
    }))
}

fn find_str_regex<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (value, re, start, num) = join4(
        str_arg(ctx, env, args, key, pos),
        regex_arg(ctx, env, args, "pattern", pos),
        opt_long(ctx, env, args, "start", pos),
        opt_long(ctx, env, args, "num_results", pos),
    )?;
    let count = num.unwrap_or(1024).min(1024) as i32;
    let (s, cpoff) = match start {
        Some(st) if st > 0 && st < utf16_len(&value) => match cp_offset(&value, st) {
            Some(o) => (&value[o..], st as i32 as i64),
            None => return crash("a `start` beyond the last code point", &pos.key("start")),
        },
        _ => (value.as_str(), 0),
    };
    check_subject(&re, s, &pos.key("pattern"))?;
    let mut out = Vec::new();
    for (ms, me) in re.find_all(s).into_iter().take(count.max(0) as usize) {
        let start16 = utf16_index(s, ms);
        let end16 = utf16_index(s, me);
        let end16 = if ms != me { end16 - 1 } else { end16 };
        out.push(Value::obj(vec![
            ("start", Value::Long(cpoff + cp_count_utf16(s, start16))),
            ("end", Value::Long(cpoff + cp_count_utf16(s, end16))),
            ("data", Value::String(s[ms..me].to_string())),
        ]));
    }
    Ok(Value::Array(out))
}

/// `Character.isWhitespace`, as used by `String.strip`.
fn java_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t'..='\r'
            | '\u{1C}'..='\u{1F}'
            | ' '
            | '\u{1680}'
            | '\u{2000}'..='\u{2006}'
            | '\u{2008}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

fn und() -> LanguageIdentifier {
    LanguageIdentifier::UND
}

fn unary<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let s = str_arg(ctx, env, args, key, pos)?;
    let cm = icu_casemap::CaseMapper::new();
    Ok(match key {
        "length" => Value::Long(cp_len(&s)),
        "lower_case" | "lowercase" => Value::String(cm.lowercase_to_string(&s, &und())),
        "upper_case" | "uppercase" => Value::String(cm.uppercase_to_string(&s, &und())),
        "title_case" | "titlecase" => Value::String(title_case(&s)),
        "l_trim" | "ltrim" => Value::String(s.trim_start_matches(java_whitespace).to_string()),
        "r_trim" | "rtrim" => Value::String(s.trim_end_matches(java_whitespace).to_string()),
        _ => Value::String(s.trim_matches(java_whitespace).to_string()),
    })
}

/// Word class for a simplified UAX #29 word segmentation, enough to find
/// the words ICU's word `BreakIterator` would titlecase.
#[derive(Clone, Copy, PartialEq)]
enum Wb {
    Letter,
    Numeric,
    Katakana,
    ExtendNumLet,
    MidLetter,
    MidNum,
    MidNumLet,
    Extend,
    Space,
    Other,
}

fn word_class(c: char) -> Wb {
    let cp = c as u32;
    let gc = icu_properties::maps::general_category().get(c);
    use icu_properties::GeneralCategory as G;
    match c {
        '\'' | '.' | '\u{2018}' | '\u{2019}' | '\u{2024}' | '\u{FE52}' | '\u{FF07}' | '\u{FF0E}' => Wb::MidNumLet,
        ':' | '\u{B7}' | '\u{387}' | '\u{55F}' | '\u{5F4}' | '\u{2027}' | '\u{FE13}' | '\u{FE55}' | '\u{FF1A}' => {
            Wb::MidLetter
        }
        ',' | ';' | '\u{37E}' | '\u{589}' | '\u{60C}' | '\u{60D}' | '\u{66C}' | '\u{7F8}' | '\u{2044}' | '\u{FE10}'
        | '\u{FE14}' | '\u{FE50}' | '\u{FE54}' | '\u{FF0C}' | '\u{FF1B}' => Wb::MidNum,
        ' ' | '\u{1680}' | '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200A}' | '\u{205F}' | '\u{3000}' => Wb::Space,
        '\u{30A0}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9F}' => Wb::Katakana,
        _ => match gc {
            G::NonspacingMark | G::SpacingMark | G::EnclosingMark | G::Format => Wb::Extend,
            G::ConnectorPunctuation => Wb::ExtendNumLet,
            G::DecimalNumber => Wb::Numeric,
            _ if c.is_alphabetic() => {
                // Ideographs, kana and the scripts segmented by dictionary
                // are not ALetter.
                let not_aletter = matches!(cp,
                    0x2E80..=0x2FFF | 0x3040..=0x309F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
                    | 0x20000..=0x3FFFF | 0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF | 0x1950..=0x19DF
                    | 0xAA60..=0xAADF);
                if not_aletter {
                    Wb::Other
                } else {
                    Wb::Letter
                }
            }
            _ => Wb::Other,
        },
    }
}

fn word_segments(s: &str) -> Vec<&str> {
    let cs: Vec<(usize, char, Wb)> = s.char_indices().map(|(i, c)| (i, c, word_class(c))).collect();
    // Effective class: Extend chars take the class of what they attach to.
    let mut eff: Vec<Wb> = Vec::with_capacity(cs.len());
    for (i, &(_, _, w)) in cs.iter().enumerate() {
        eff.push(if w == Wb::Extend && i > 0 { eff[i - 1] } else { w });
    }
    let is_ah = |w: Wb| w == Wb::Letter;
    // Next index whose char is not Extend.
    let next_real = |mut j: usize| {
        while j < cs.len() && cs[j].2 == Wb::Extend {
            j += 1;
        }
        j
    };
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 1;
    while i < cs.len() {
        let cur = cs[i].2;
        let prev = eff[i - 1];
        let joins = if cur == Wb::Extend {
            !matches!(cs[i - 1].1, '\r' | '\n')
        } else {
            let after = next_real(i + 1);
            let next = cs.get(after).map(|x| x.2);
            let prev_prev = (0..i - 1).rev().find(|&k| cs[k].2 != Wb::Extend).map(|k| eff[k]);
            match (prev, cur) {
                (a, b) if is_ah(a) && is_ah(b) => true,
                (a, Wb::MidLetter | Wb::MidNumLet) if is_ah(a) => next.map(is_ah).unwrap_or(false),
                (Wb::MidLetter | Wb::MidNumLet, b) if is_ah(b) => prev_prev.map(is_ah).unwrap_or(false),
                (Wb::Numeric, Wb::Numeric) => true,
                (a, Wb::Numeric) if is_ah(a) => true,
                (Wb::Numeric, b) if is_ah(b) => true,
                (Wb::Numeric, Wb::MidNum | Wb::MidNumLet) => next == Some(Wb::Numeric),
                (Wb::MidNum | Wb::MidNumLet, Wb::Numeric) => prev_prev == Some(Wb::Numeric),
                (Wb::Katakana, Wb::Katakana) => true,
                (Wb::Letter | Wb::Numeric | Wb::Katakana | Wb::ExtendNumLet, Wb::ExtendNumLet) => true,
                (Wb::ExtendNumLet, Wb::Letter | Wb::Numeric | Wb::Katakana) => true,
                (Wb::Space, Wb::Space) => true,
                _ => false,
            }
        };
        if !joins {
            out.push(&s[cs[start].0..cs[i].0]);
            start = i;
        }
        i += 1;
    }
    if !cs.is_empty() {
        out.push(&s[cs[start].0..]);
    }
    out
}

/// `UCharacter.toTitleCase(s, BreakIterator.getWordInstance)`.
fn title_case(s: &str) -> String {
    let tm = icu_casemap::TitlecaseMapper::new();
    let opts = icu_casemap::titlecase::TitlecaseOptions::default();
    word_segments(s).into_iter().map(|w| tm.titlecase_segment_to_string(w, &und(), opts)).collect()
}

/// Lucene's `NGramTokenizer` (all characters are token characters).
fn ngrams(s: &str, min: usize, max: usize) -> Vec<Value> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    for start in 0..cs.len() {
        for size in min..=max {
            if start + size > cs.len() {
                break;
            }
            out.push(Value::String(cs[start..start + size].iter().collect()));
        }
    }
    out
}

fn ngram<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (terms, min, max) = join3(
        args.eval(ctx, env, "ngram", pos).and_then(|v| scalars(&v, &pos.key("ngram"))),
        opt_long(ctx, env, args, "min", pos),
        opt_long(ctx, env, args, "max", pos),
    )?;
    const DEFAULT_MIN: i64 = 1;
    const DEFAULT_MAX: i64 = 2;
    const MAX_DIFF: i64 = 1;
    let mut errs = Vec::new();
    let mut bounds = |a: &str, p: String| errs.push(FaunaError::bounds(a, &p, pos));
    match (min, max) {
        (Some(min), Some(max)) => {
            if min > max || min <= 0 {
                bounds("min ngram size", "> 0 and <= max".into());
            }
            if max <= 0 {
                bounds("max ngram size", "> 0".into());
            }
            if max.wrapping_sub(min) > MAX_DIFF {
                bounds("max ngram size", format!("<= min + {}", MAX_DIFF));
            }
        }
        (Some(min), None) => {
            if min > DEFAULT_MAX || min <= 0 {
                bounds("min ngram size", "> 0 and <= max".into());
            }
            if DEFAULT_MAX.wrapping_sub(min) > MAX_DIFF {
                bounds("min ngram size", format!(">= max - {}", MAX_DIFF));
            }
        }
        (None, Some(max)) => {
            if max <= 0 || max <= DEFAULT_MIN {
                bounds("max ngram size", "> 0 and >= min".into());
            }
            if max.wrapping_sub(DEFAULT_MIN) > MAX_DIFF {
                bounds("max ngram size", format!("<= min + {}", MAX_DIFF));
            }
        }
        (None, None) => {}
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let lo = min.unwrap_or(DEFAULT_MIN) as i32;
    let hi = max.unwrap_or(DEFAULT_MAX) as i32;
    if lo < 1 || lo > hi {
        return crash("ngram sizes that overflow an Int", pos);
    }
    let mut out = Vec::new();
    for t in terms {
        match t {
            Value::String(s) => out.extend(ngrams(&s, lo as usize, hi as usize)),
            t => out.push(t),
        }
    }
    Ok(Value::Array(out))
}

fn repeat<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (s, n) = join2(str_arg(ctx, env, args, "repeat", pos), opt_long(ctx, env, args, "number", pos))?;
    let n = n.unwrap_or(2).min(1024) as i32;
    Ok(Value::String(if n > 0 { s.repeat(n as usize) } else { String::new() }))
}

fn space<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "space", pos)?;
    let n = cast::long(&v, &pos.key("space"))?;
    Ok(Value::String(if n > 0 { " ".repeat(n.min(1024) as usize) } else { String::new() }))
}

fn replace_str<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (value, find, replace) = join3(
        str_arg(ctx, env, args, key, pos),
        str_arg(ctx, env, args, "find", pos),
        str_arg(ctx, env, args, "replace", pos),
    )?;
    Ok(Value::String(value.replace(&find, &replace)))
}

fn replace_str_regex<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let first = match args.eval_opt(ctx, env, "first", pos) {
        Ok(Some(v)) => cast::boolean(&v, &pos.key("first")).map(Some),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    let (value, re, replace, first) = join4(
        str_arg(ctx, env, args, key, pos),
        regex_arg(ctx, env, args, "pattern", pos),
        str_arg(ctx, env, args, "replace", pos),
        first,
    )?;
    check_subject(&re, &value, &pos.key("pattern"))?;
    match re.replace(&value, &replace, first.unwrap_or(false)) {
        Ok(s) => Ok(Value::String(s)),
        Err(what) => crash(&what, &pos.key("replace")),
    }
}

fn sub_string<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (value, start, length) = join3(
        str_arg(ctx, env, args, key, pos),
        opt_long(ctx, env, args, "start", pos),
        opt_long(ctx, env, args, "length", pos),
    )?;
    let cpl = cp_len(&value);
    // Int arithmetic like the Scala (`toInt` truncates).
    let spos: i64 = match start {
        Some(s) if s >= 0 => s as i32 as i64,
        Some(s) => (cpl.wrapping_add(s)) as i32 as i64,
        None => 0,
    };
    let epos: i64 = match length {
        Some(0) => spos,
        Some(l) if l > 0 => spos.wrapping_add(l).min(cpl) as i32 as i64,
        Some(_) => -1,
        None => cpl,
    };
    if spos > cpl || spos < 0 {
        return one(FaunaError::bounds("start", "less than or equal to the string length", pos));
    }
    if epos < 0 && epos < spos {
        return one(FaunaError::bounds("length", "positive", pos));
    }
    if epos < spos {
        // `start + length` overflowed; the server throws.
        return crash("a `length` that overflows", &pos.key("length"));
    }
    let (a, b) = (cp_offset(&value, spos).unwrap(), cp_offset(&value, epos).unwrap());
    Ok(Value::String(value[a..b].to_string()))
}
// NB: `SplitStr`/`SplitStrRegex` do not exist in API v4 (Unstable only).


// ---- Format ----

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Long,
    Double,
    Str,
    Bool,
    /// `%t` / `%T` with an optional suffix.
    DateTime(Option<char>),
    /// `%@`: rendered with the default (2.1) API version.
    Debug,
    /// `%^`: rendered with the request's API version.
    FixedDebug,
}

struct FmtSpec {
    /// 1-based explicit index, or 0 for "next".
    index: i64,
    kind: Kind,
    flags: String,
    width: Option<String>,
    precision: Option<String>,
    conv: char,
    upper: bool,
    /// `%` + everything after the explicit index: what Java's
    /// `String.format` is called with, and what errors quote.
    text: String,
}

enum Piece {
    Raw(String),
    Spec(FmtSpec),
}

/// `FormatFunction.parse`: specifiers match
/// `%(\d+\$)?(([-#+ 0,(\<]*)?(\d+)?(\.\d+)?([tT])?([a-zA-Z%@\^])?)`.
fn parse_format(fmt: &str, pos: &Position) -> R<Vec<Piece>> {
    let cs: Vec<char> = fmt.chars().collect();
    let digits = |from: usize| -> usize {
        let mut j = from;
        while j < cs.len() && cs[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    let text = |a: usize, b: usize| -> String { cs[a..b].iter().collect() };
    let mut out = Vec::new();
    let mut raw = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] != '%' {
            raw.push(cs[i]);
            i += 1;
            continue;
        }
        if !raw.is_empty() {
            out.push(Piece::Raw(std::mem::take(&mut raw)));
        }
        let begin = i;
        let mut j = i + 1;
        let mut index = 0;
        let d = digits(j);
        if d > j && cs.get(d) == Some(&'$') {
            index = match text(j, d).parse::<i32>() {
                Ok(n) => n as i64,
                Err(_) => return crash("format indexes over Int.MaxValue", pos),
            };
            j = d + 1;
        }
        let spec_start = j;
        while j < cs.len() && "-#+ 0,(<".contains(cs[j]) {
            j += 1;
        }
        let flags = text(spec_start, j);
        let w_end = digits(j);
        let width = if w_end > j { Some(text(j, w_end)) } else { None };
        j = w_end;
        let mut precision = None;
        if cs.get(j) == Some(&'.') && cs.get(j + 1).map(|c| c.is_ascii_digit()).unwrap_or(false) {
            let p_end = digits(j + 1);
            precision = Some(text(j + 1, p_end));
            j = p_end;
        }
        let dt = match cs.get(j) {
            Some(c @ ('t' | 'T')) => {
                j += 1;
                Some(*c)
            }
            _ => None,
        };
        let conv = match cs.get(j) {
            Some(c) if c.is_ascii_alphabetic() || matches!(c, '%' | '@' | '^') => {
                j += 1;
                Some(*c)
            }
            _ => None,
        };
        let whole = text(begin, j);
        let spec_text = format!("%{}", text(spec_start, j));
        i = j;
        let (kind, upper) = if let Some(t) = dt {
            (Kind::DateTime(conv), t == 'T')
        } else {
            match conv {
                Some(c @ ('f' | 'e' | 'E' | 'g' | 'G')) => (Kind::Double, c.is_ascii_uppercase()),
                Some(c @ ('d' | 'x' | 'X' | 'o')) => (Kind::Long, c.is_ascii_uppercase()),
                Some(c @ ('s' | 'S')) => (Kind::Str, c == 'S'),
                Some(c @ ('b' | 'B')) => (Kind::Bool, c == 'B'),
                Some('%') => {
                    out.push(Piece::Raw("%".into()));
                    continue;
                }
                Some('n') => {
                    out.push(Piece::Raw("\n".into()));
                    continue;
                }
                Some('@') => (Kind::Debug, false),
                Some('^') => (Kind::FixedDebug, false),
                _ => return one(FaunaError::invalid_conversion_format(&whole, pos)),
            }
        };
        out.push(Piece::Spec(FmtSpec {
            index,
            kind,
            flags,
            width,
            precision,
            conv: conv.map(|c| c.to_ascii_lowercase()).unwrap_or(' '),
            upper,
            text: spec_text,
        }));
    }
    if !raw.is_empty() {
        out.push(Piece::Raw(raw));
    }
    Ok(out)
}

fn tm_of_date(d: &chrono::NaiveDate) -> Tm {
    use chrono::Datelike;
    Tm {
        year: d.year() as i64,
        month: d.month(),
        day: d.day(),
        weekday: d.weekday().number_from_monday(),
        yday: d.ordinal(),
        time: None,
    }
}

fn tm_of_time(t: &crate::value::Timestamp) -> Tm {
    use chrono::Timelike;
    let dt = t.to_naive();
    let mut tm = tm_of_date(&dt.date());
    tm.time = Some(jformat::TimeFields {
        hour: dt.hour(),
        minute: dt.minute(),
        second: dt.second(),
        nano: t.nanos,
        epoch_secs: t.secs,
    });
    tm
}

const DATE_SUFFIXES: &str = "bBhaACyYdemjDF";
const TIME_SUFFIXES: &str = "HIklMSLpzZsQRTrc";

/// Render a value as the server's JSON (for `%@` / `%^`).
fn render_json(v: &Value, legacy: bool, pos: &Position) -> R<String> {
    fn has_unrenderable(v: &Value) -> bool {
        match v {
            Value::Set(_) | Value::Lambda(_) => true,
            Value::Array(a) | Value::Page { data: a, .. } => a.iter().any(has_unrenderable),
            Value::Object(o) => o.values().any(has_unrenderable),
            _ => false,
        }
    }
    if legacy && has_unrenderable(v) {
        return one(FaunaError::not_emulated("`%@` of sets and lambdas (API 2.1 rendering)", pos));
    }
    /// `ref_body`: `j` is the body of an `@ref`, which 2.1 renders with
    /// `class` instead of `collection` (and native collections as
    /// `classes`, which is not emulated).
    fn write(j: &serde_json::Value, legacy: bool, ref_body: bool, out: &mut String) -> Result<(), ()> {
        use serde_json::Value as J;
        match j {
            J::Number(n) if n.is_f64() => out.push_str(&jformat::java_double_to_string(n.as_f64().unwrap())),
            J::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(x, legacy, false, out)?;
                }
                out.push(']');
            }
            J::Object(o) => {
                if legacy && ref_body && o.get("collection").is_none() && o.get("id") == Some(&J::from("collections")) {
                    return Err(());
                }
                out.push('{');
                for (i, (k, x)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let key = if legacy && ref_body && k == "collection" { "class" } else { k.as_str() };
                    out.push_str(&serde_json::to_string(key).unwrap());
                    out.push(':');
                    write(x, legacy, k == "@ref", out)?;
                }
                out.push('}');
            }
            other => out.push_str(&serde_json::to_string(other).unwrap()),
        }
        Ok(())
    }
    let mut out = String::new();
    match write(&crate::wire::encode(v), legacy, false, &mut out) {
        Ok(()) => Ok(out),
        Err(()) => one(FaunaError::not_emulated("`%@` of collection refs (API 2.1 rendering)", pos)),
    }
}

fn print_spec(f: &FmtSpec, v: &Value, vpos: &Position, fpos: &Position) -> R<String> {
    let conversion = |r: Result<String, FmtErr>| -> R<String> {
        match r {
            Ok(s) => Ok(s),
            Err(FmtErr::Conversion) => one(FaunaError::invalid_conversion_format(&f.text, fpos)),
            Err(FmtErr::Unsupported(what)) => one(FaunaError::not_emulated(&what, fpos)),
        }
    };
    let spec = || Spec::parse(&f.flags, f.width.as_deref(), f.precision.as_deref(), f.upper);
    match f.kind {
        Kind::Long => match v {
            Value::Long(l) => conversion(spec().and_then(|s| jformat::fmt_long(&s, f.conv, *l))),
            other => one(FaunaError::invalid_argument(&[Type::Integer], other.rtype(), vpos)),
        },
        Kind::Double => match v {
            Value::Double(d) => conversion(spec().and_then(|s| jformat::fmt_double(&s, f.conv, *d))),
            other => one(FaunaError::invalid_argument(&[Type::Number], other.rtype(), vpos)),
        },
        Kind::Str => match super::time::to_string_literal(v) {
            Some(s) => conversion(spec().and_then(|sp| jformat::fmt_string(&sp, &s))),
            None => one(FaunaError::invalid_cast(v.rtype(), Type::String, vpos)),
        },
        Kind::Bool => {
            let b = match v {
                Value::Bool(b) => *b,
                Value::Null => false,
                _ => true,
            };
            conversion(spec().and_then(|s| jformat::fmt_bool(&s, b)))
        }
        Kind::Debug => render_json(v, true, vpos),
        Kind::FixedDebug => render_json(v, false, vpos),
        Kind::DateTime(suffix) => {
            let date_suffix = |tm: &Tm, c: char| -> R<String> {
                if DATE_SUFFIXES.contains(c) {
                    conversion(spec().and_then(|s| jformat::fmt_temporal(&s, c, tm)))
                } else {
                    one(FaunaError::invalid_date_time_suffix(&f.text, vpos))
                }
            };
            match (v, suffix) {
                (Value::Date(d), None) => Ok(super::time::date_to_string(d)),
                (Value::Date(d), Some(c)) => date_suffix(&tm_of_date(d), c),
                (Value::Time(t), None) => Ok(super::time::instant_to_string(t)),
                (Value::Time(t), Some('N')) => Ok(format!("{:09}", t.nanos)),
                (Value::Time(t), Some(c)) if TIME_SUFFIXES.contains(c) => {
                    conversion(spec().and_then(|s| jformat::fmt_temporal(&s, c, &tm_of_time(t))))
                }
                (Value::Time(t), Some(c)) => {
                    let mut tm = tm_of_time(t);
                    tm.time = None;
                    date_suffix(&tm, c)
                }
                (other, _) => one(FaunaError::invalid_argument(&[Type::Date, Type::Time], other.rtype(), vpos)),
            }
        }
    }
}

fn format<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let (fmt, values) =
        join2(str_arg(ctx, env, args, "format", pos), args.eval(ctx, env, "values", pos))?;
    let values: Vec<Value> = cast::elems(&values, &pos.key("values")).into_iter().map(|(v, _)| v).collect();
    let fpos = pos.key("format");
    let pieces = parse_format(&fmt, &fpos)?;
    let mut out = String::new();
    let mut errs = Vec::new();
    let mut last = 0usize;
    for piece in &pieces {
        let printed = match piece {
            Piece::Raw(s) => Ok(s.clone()),
            Piece::Spec(f) => {
                let idx = if f.index == 0 {
                    let i = last;
                    last += 1;
                    if i >= values.len() {
                        Err(FaunaError::bounds("format specifiers count", "<= arguments size", &fpos))
                    } else {
                        Ok(i)
                    }
                } else {
                    let i = (f.index - 1) as usize;
                    if i >= values.len() {
                        Err(FaunaError::bounds("index", ">= 1 and <= arguments size", &fpos))
                    } else {
                        Ok(i)
                    }
                };
                match idx {
                    Ok(i) => print_spec(f, &values[i], &pos.key("values").idx(i), &fpos),
                    Err(e) => Err(vec![e]),
                }
            }
        };
        match printed {
            Ok(s) => out.push_str(&s),
            Err(e) => errs.extend(e),
        }
    }
    if errs.is_empty() {
        Ok(Value::String(out))
    } else {
        Err(errs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titlecase_words() {
        assert_eq!(title_case("bob f smITH  don't-stop 1ST o'neil"), "Bob F Smith  Don't-Stop 1st O'neil");
        assert_eq!(title_case("hello_world foo.bar 3.14x"), "Hello_world Foo.bar 3.14x");
        assert_eq!(title_case("ǆemal"), "ǅemal");
    }

    #[test]
    fn casefold_oddities() {
        assert_eq!(nfkc_casefold("Weißkopfseeadler"), "weisskopfseeadler");
        assert_eq!(nfkc_casefold("φιλόσοφος"), "φιλόσοφοσ");
        assert_eq!(nfkc_casefold("İstanbul"), "i̇stanbul");
        assert_eq!(nfkc_casefold("ﬁ\u{AD}x"), "fix");
    }

    #[test]
    fn java_strip() {
        assert!(java_whitespace('\u{1680}'));
        assert!(!java_whitespace('\u{A0}'));
        assert!(!java_whitespace('\u{85}'));
    }
}
