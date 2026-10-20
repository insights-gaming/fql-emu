//! `java.util.regex` on top of the `regex` crate.
//!
//! The server compiles patterns with `java.util.regex.Pattern` (flags 0)
//! and iterates matches with `Matcher.find`. This module translates a Java
//! pattern into an equivalent Rust pattern, making every Java default
//! explicit (ASCII `\d \w \s`, `.` excluding all Java line terminators,
//! ASCII-only case-insensitivity), and reimplements the Java iteration,
//! replacement and split algorithms, which differ from the Rust ones for
//! empty matches.
//!
//! Constructs Java accepts but this translation cannot express exactly
//! (lookaround, backreferences, possessive/atomic groups, `\G`, comments
//! mode, ...) are reported as [`Error::Unsupported`], so the caller can fail
//! with `not emulated` instead of guessing. Patterns Java rejects are
//! [`Error::Invalid`].

use regex::{Captures, Regex, RegexBuilder};

#[derive(Debug, PartialEq)]
pub enum Error {
    /// `PatternSyntaxException` on the server.
    Invalid,
    /// Valid Java, but not emulated; the string names the construct.
    Unsupported(String),
}

type Res<T> = Result<T, Error>;

const JAVA_LINE_TERMINATORS: [char; 5] = ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'];

/// A compiled Java pattern.
pub struct JRegex {
    pub re: Regex,
    /// Uses `$` without MULTILINE, or `\Z`: in Java these also match before
    /// a final line terminator.
    eol_anchor: bool,
    /// Uses `^` in MULTILINE mode.
    ml_caret: bool,
    /// Uses `$` in MULTILINE mode.
    ml_dollar: bool,
}

/// `Pattern.quote`.
pub fn quote(s: &str) -> String {
    if !s.contains("\\E") {
        return format!("\\Q{}\\E", s);
    }
    let mut out = String::from("\\Q");
    let mut rest = s;
    while let Some(i) = rest.find("\\E") {
        out.push_str(&rest[..i]);
        out.push_str("\\E\\\\E\\Q");
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out.push_str("\\E");
    out
}

/// Compile a Java pattern.
pub fn compile(pattern: &str) -> Res<JRegex> {
    let mut t = Translator {
        chars: pattern.chars().collect(),
        pos: 0,
        out: String::new(),
        flags: Flags::default(),
        stack: Vec::new(),
        can_repeat: false,
        after_anchor: false,
        eol_anchor: false,
        ml_caret: false,
        ml_dollar: false,
    };
    t.run()?;
    let re = RegexBuilder::new(&t.out).size_limit(1 << 22).build().map_err(|e| match e {
        regex::Error::CompiledTooBig(_) => Error::Unsupported("very large regular expressions".into()),
        _ => Error::Invalid,
    })?;
    Ok(JRegex { re, eol_anchor: t.eol_anchor, ml_caret: t.ml_caret, ml_dollar: t.ml_dollar })
}

impl JRegex {
    /// The translation is exact except for anchors next to line
    /// terminators other than a plain `\n`. Returns the construct that
    /// would behave differently on `s`, if any.
    pub fn check_subject(&self, s: &str) -> Result<(), String> {
        let ends_with_term = s.ends_with(&JAVA_LINE_TERMINATORS[..]);
        let other_terms = s.contains(&['\r', '\u{85}', '\u{2028}', '\u{2029}'][..]);
        if self.eol_anchor && ends_with_term {
            return Err("Java `$`/`\\Z` semantics before a trailing line terminator".into());
        }
        if self.ml_caret && (s.is_empty() || s.ends_with('\n') || other_terms) {
            return Err("Java MULTILINE `^` semantics at the end of input or with non-`\\n` line terminators".into());
        }
        if self.ml_dollar && other_terms {
            return Err("Java MULTILINE `$` semantics with non-`\\n` line terminators".into());
        }
        Ok(())
    }

    /// Matches as successive `Matcher.find()` calls produce them: after an
    /// empty match the next search starts one character later, otherwise
    /// at the end of the previous match (where an empty match is allowed).
    pub fn find_all(&self, s: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= s.len() {
            let Some(m) = self.re.find_at(s, at) else { break };
            out.push((m.start(), m.end()));
            at = if m.start() == m.end() { next_boundary(s, m.end()) } else { m.end() };
        }
        out
    }

    fn captures_all<'s>(&self, s: &'s str, first_only: bool) -> Vec<Captures<'s>> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= s.len() {
            let Some(c) = self.re.captures_at(s, at) else { break };
            let m = c.get(0).unwrap();
            at = if m.start() == m.end() { next_boundary(s, m.end()) } else { m.end() };
            out.push(c);
            if first_only {
                break;
            }
        }
        out
    }

    /// `Matcher.replaceAll` / `replaceFirst`. `Err` carries a description
    /// of a replacement the server fails on with an internal error.
    pub fn replace(&self, s: &str, replacement: &str, first_only: bool) -> Result<String, String> {
        let parts = self.parse_replacement(replacement)?;
        let mut out = String::new();
        let mut last = 0;
        for c in self.captures_all(s, first_only) {
            let m = c.get(0).unwrap();
            out.push_str(&s[last..m.start()]);
            for p in &parts {
                match p {
                    Repl::Lit(l) => out.push_str(l),
                    Repl::Group(g) => {
                        if let Some(x) = c.get(*g) {
                            out.push_str(x.as_str())
                        }
                    }
                    Repl::Named(n) => {
                        if let Some(x) = c.name(n) {
                            out.push_str(x.as_str())
                        }
                    }
                }
            }
            last = m.end();
        }
        out.push_str(&s[last..]);
        Ok(out)
    }

    /// `Matcher.appendReplacement` syntax: `$n`, `${name}`, `\x`.
    fn parse_replacement(&self, r: &str) -> Result<Vec<Repl>, String> {
        let group_count = self.re.captures_len() - 1;
        let crash = |why: &str| Err(format!("replacement strings that fail on the server ({})", why));
        let cs: Vec<char> = r.chars().collect();
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut i = 0;
        while i < cs.len() {
            match cs[i] {
                '\\' => {
                    i += 1;
                    match cs.get(i) {
                        Some(c) => lit.push(*c),
                        None => return crash("character to be escaped is missing"),
                    }
                    i += 1;
                }
                '$' => {
                    i += 1;
                    match cs.get(i) {
                        None => return crash("group index is missing"),
                        Some('{') => {
                            i += 1;
                            let start = i;
                            while i < cs.len() && cs[i].is_ascii_alphanumeric() {
                                i += 1;
                            }
                            let name: String = cs[start..i].iter().collect();
                            if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) || cs.get(i) != Some(&'}') {
                                return crash("illegal named group reference");
                            }
                            i += 1;
                            if !self.re.capture_names().any(|n| n == Some(name.as_str())) {
                                return crash("no group with that name");
                            }
                            parts.push(Repl::Lit(std::mem::take(&mut lit)));
                            parts.push(Repl::Named(name));
                        }
                        Some(c) if c.is_ascii_digit() => {
                            let mut g = c.to_digit(10).unwrap() as usize;
                            if g > group_count {
                                return crash("no such group");
                            }
                            i += 1;
                            while let Some(d) = cs.get(i).and_then(|c| c.to_digit(10)) {
                                let ng = g * 10 + d as usize;
                                if ng > group_count {
                                    break;
                                }
                                g = ng;
                                i += 1;
                            }
                            parts.push(Repl::Lit(std::mem::take(&mut lit)));
                            parts.push(Repl::Group(g));
                        }
                        Some(_) => return crash("illegal group reference"),
                    }
                }
                c => {
                    lit.push(c);
                    i += 1;
                }
            }
        }
        parts.push(Repl::Lit(lit));
        Ok(parts)
    }

    /// `Pattern.split(input, limit)`.
    pub fn split(&self, s: &str, limit: i32) -> Vec<String> {
        let match_limited = limit > 0;
        let mut list: Vec<String> = Vec::new();
        let mut index = 0;
        for (start, end) in self.find_all(s) {
            if !match_limited || (list.len() as i64) < limit as i64 - 1 {
                if index == 0 && start == 0 && start == end {
                    // No empty leading substring for a zero-width match at 0.
                    continue;
                }
                list.push(s[index..start].to_string());
                index = end;
            } else if list.len() as i64 == limit as i64 - 1 {
                list.push(s[index..].to_string());
                index = end;
            }
        }
        if index == 0 {
            return vec![s.to_string()];
        }
        if !match_limited || (list.len() as i64) < limit as i64 {
            list.push(s[index..].to_string());
        }
        if limit == 0 {
            while list.last().map(|l| l.is_empty()).unwrap_or(false) {
                list.pop();
            }
        }
        list
    }
}

enum Repl {
    Lit(String),
    Group(usize),
    Named(String),
}

fn next_boundary(s: &str, i: usize) -> usize {
    match s[i..].chars().next() {
        Some(c) => i + c.len_utf8(),
        None => i + 1,
    }
}

#[derive(Clone, Copy, Default)]
struct Flags {
    /// CASE_INSENSITIVE
    i: bool,
    /// MULTILINE
    m: bool,
    /// DOTALL
    s: bool,
    /// UNICODE_CASE
    u: bool,
}

struct Translator {
    chars: Vec<char>,
    pos: usize,
    out: String,
    flags: Flags,
    stack: Vec<Flags>,
    /// The previous token is something a quantifier may follow.
    can_repeat: bool,
    /// The previous token is an anchor/boundary.
    after_anchor: bool,
    eol_anchor: bool,
    ml_caret: bool,
    ml_dollar: bool,
}

fn unsupported<T>(what: &str) -> Res<T> {
    Err(Error::Unsupported(format!("the Java regex construct {}", what)))
}

/// A literal character in Rust regex syntax, unambiguous both inside and
/// outside classes.
fn lit(c: char) -> String {
    if c.is_ascii_alphanumeric() {
        c.to_string()
    } else {
        format!("\\x{{{:X}}}", c as u32)
    }
}

fn swap_ascii_case(c: char) -> Option<char> {
    if c.is_ascii_lowercase() {
        Some(c.to_ascii_uppercase())
    } else if c.is_ascii_uppercase() {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

/// A Java escape that denotes a single character.
enum Esc {
    Char(char),
    /// A class, as a Rust class expression (`[...]` or `\p{..}`).
    Class(String),
    /// An anchor / boundary, as Rust syntax.
    Anchor(&'static str),
    /// `\Q`: start of a quoted section.
    Quote,
}

impl Translator {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn atom(&mut self, s: &str) {
        self.out.push_str(s);
        self.can_repeat = true;
        self.after_anchor = false;
    }

    fn literal_atom(&mut self, c: char) {
        let s = self.literal(c);
        self.atom(&s);
    }

    /// A literal outside a class, with Java case-insensitivity.
    fn literal(&self, c: char) -> String {
        if self.flags.i && self.flags.u {
            format!("(?i:{})", lit(c))
        } else if self.flags.i {
            match swap_ascii_case(c) {
                Some(o) => format!("[{}{}]", c, o),
                None => lit(c),
            }
        } else {
            lit(c)
        }
    }

    fn run(&mut self) -> Res<()> {
        while let Some(c) = self.next() {
            match c {
                '\\' => match self.escape(false)? {
                    Esc::Char(ch) => self.literal_atom(ch),
                    Esc::Class(cls) => self.atom(&cls),
                    Esc::Anchor(a) => {
                        self.out.push_str(a);
                        self.can_repeat = false;
                        self.after_anchor = true;
                    }
                    Esc::Quote => {
                        while self.pos < self.chars.len() {
                            if self.peek() == Some('\\') && self.peek_at(1) == Some('E') {
                                self.pos += 2;
                                break;
                            }
                            let ch = self.next().unwrap();
                            self.literal_atom(ch);
                        }
                    }
                },
                '[' => {
                    let cls = self.class()?;
                    let cls = if self.flags.i && self.flags.u { format!("(?i:{})", cls) } else { cls };
                    self.atom(&cls);
                }
                '(' => self.group_open()?,
                ')' => {
                    match self.stack.pop() {
                        Some(f) => self.flags = f,
                        None => return Err(Error::Invalid),
                    }
                    self.atom(")");
                }
                '|' => {
                    self.out.push('|');
                    self.can_repeat = false;
                    self.after_anchor = false;
                }
                '.' => {
                    if self.flags.s {
                        self.atom("(?s:.)")
                    } else {
                        self.atom("[^\\n\\r\\x{85}\\x{2028}\\x{2029}]")
                    }
                }
                '^' => {
                    if self.flags.m {
                        self.ml_caret = true;
                        self.out.push_str("(?m:^)");
                    } else {
                        self.out.push_str("\\A");
                    }
                    self.can_repeat = false;
                    self.after_anchor = true;
                }
                '$' => {
                    if self.flags.m {
                        self.ml_dollar = true;
                        self.out.push_str("(?m:$)");
                    } else {
                        self.eol_anchor = true;
                        self.out.push_str("\\z");
                    }
                    self.can_repeat = false;
                    self.after_anchor = true;
                }
                '*' | '+' | '?' => {
                    self.quantifier_target()?;
                    self.out.push(c);
                    self.quantifier_suffix()?;
                }
                '{' => {
                    self.quantifier_target()?;
                    let start = self.pos;
                    let mut seen_comma = false;
                    let mut digits_before = 0;
                    loop {
                        match self.next() {
                            Some(d) if d.is_ascii_digit() => {
                                if !seen_comma {
                                    digits_before += 1
                                }
                            }
                            Some(',') if !seen_comma && digits_before > 0 => seen_comma = true,
                            Some('}') if digits_before > 0 => break,
                            _ => return Err(Error::Invalid),
                        }
                    }
                    let body: String = self.chars[start..self.pos - 1].iter().collect();
                    self.out.push('{');
                    self.out.push_str(&body);
                    self.out.push('}');
                    self.quantifier_suffix()?;
                }
                c => self.literal_atom(c),
            }
        }
        if !self.stack.is_empty() {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn quantifier_target(&mut self) -> Res<()> {
        if self.after_anchor {
            return unsupported("of a quantified anchor");
        }
        if !self.can_repeat {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn quantifier_suffix(&mut self) -> Res<()> {
        match self.peek() {
            Some('?') => {
                self.pos += 1;
                self.out.push('?');
            }
            Some('+') => return unsupported("of possessive quantifiers"),
            _ => {}
        }
        self.can_repeat = false;
        Ok(())
    }

    fn group_open(&mut self) -> Res<()> {
        if self.peek() != Some('?') {
            self.stack.push(self.flags);
            self.out.push('(');
            self.can_repeat = false;
            self.after_anchor = false;
            return Ok(());
        }
        self.pos += 1;
        match self.peek() {
            Some(':') => {
                self.pos += 1;
                self.stack.push(self.flags);
                self.out.push_str("(?:");
            }
            Some('=') | Some('!') => return unsupported("of lookahead"),
            Some('>') => return unsupported("of atomic groups"),
            Some('<') => match self.peek_at(1) {
                Some('=') | Some('!') => return unsupported("of lookbehind"),
                Some(c) if c.is_ascii_alphabetic() => {
                    self.pos += 1;
                    let start = self.pos;
                    while self.peek().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
                        self.pos += 1;
                    }
                    if self.next() != Some('>') {
                        return Err(Error::Invalid);
                    }
                    let name: String = self.chars[start..self.pos - 1].iter().collect();
                    self.stack.push(self.flags);
                    self.out.push_str(&format!("(?P<{}>", name));
                }
                _ => return Err(Error::Invalid),
            },
            _ => {
                // Inline flags: (?idmsuxU-idmsuxU) or (?flags:X)
                let mut on = true;
                let mut f = self.flags;
                loop {
                    match self.next() {
                        Some('-') => on = false,
                        Some('i') => f.i = on,
                        Some('m') => f.m = on,
                        Some('s') => f.s = on,
                        Some('u') => f.u = on,
                        Some('x') if on => return unsupported("of COMMENTS mode (?x)"),
                        Some('d') if on => return unsupported("of UNIX_LINES mode (?d)"),
                        Some('U') if on => return unsupported("of UNICODE_CHARACTER_CLASS mode (?U)"),
                        Some('x') | Some('d') | Some('U') => {}
                        Some(')') => {
                            self.flags = f;
                            return Ok(());
                        }
                        Some(':') => {
                            self.stack.push(self.flags);
                            self.flags = f;
                            self.out.push_str("(?:");
                            break;
                        }
                        _ => return Err(Error::Invalid),
                    }
                }
            }
        }
        self.can_repeat = false;
        self.after_anchor = false;
        Ok(())
    }

    fn hex(&mut self, n: usize) -> Res<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let d = self.next().and_then(|c| c.to_digit(16)).ok_or(Error::Invalid)?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn code_point(v: u32) -> Res<char> {
        match char::from_u32(v) {
            Some(c) => Ok(c),
            None if (0xD800..0xE000).contains(&v) => unsupported("of a lone surrogate escape"),
            None => Err(Error::Invalid),
        }
    }

    /// Parse the escape after a `\`.
    fn escape(&mut self, in_class: bool) -> Res<Esc> {
        let c = self.next().ok_or(Error::Invalid)?;
        let class = |s: &str| Ok(Esc::Class(s.to_string()));
        match c {
            '0' => {
                // \0n, \0nn, \0mnn (m <= 3)
                let oct = |c: Option<char>| c.and_then(|c| c.to_digit(8));
                let a = oct(self.peek()).ok_or(Error::Invalid)?;
                self.pos += 1;
                let mut v = a;
                if let Some(b) = oct(self.peek()) {
                    self.pos += 1;
                    v = v * 8 + b;
                    if let Some(d) = oct(self.peek()) {
                        if a <= 3 {
                            self.pos += 1;
                            v = v * 8 + d;
                        }
                    }
                }
                Ok(Esc::Char(char::from_u32(v).unwrap()))
            }
            '1'..='9' => unsupported("of backreferences"),
            't' => Ok(Esc::Char('\t')),
            'n' => Ok(Esc::Char('\n')),
            'r' => Ok(Esc::Char('\r')),
            'f' => Ok(Esc::Char('\u{0C}')),
            'a' => Ok(Esc::Char('\u{07}')),
            'e' => Ok(Esc::Char('\u{1B}')),
            'c' => {
                let x = self.next().ok_or(Error::Invalid)?;
                Ok(Esc::Char(char::from_u32(x as u32 ^ 64).ok_or(Error::Invalid)?))
            }
            'x' => {
                if self.peek() == Some('{') {
                    self.pos += 1;
                    let mut v: u32 = 0;
                    let mut n = 0;
                    while let Some(d) = self.peek().and_then(|c| c.to_digit(16)) {
                        self.pos += 1;
                        n += 1;
                        v = v.saturating_mul(16).saturating_add(d);
                    }
                    if n == 0 || self.next() != Some('}') || v > 0x10FFFF {
                        return Err(Error::Invalid);
                    }
                    Ok(Esc::Char(Self::code_point(v)?))
                } else {
                    let v = self.hex(2)?;
                    Ok(Esc::Char(char::from_u32(v).unwrap()))
                }
            }
            'u' => {
                let hi = self.hex(4)?;
                if (0xD800..0xDC00).contains(&hi) && self.peek() == Some('\\') && self.peek_at(1) == Some('u') {
                    let save = self.pos;
                    self.pos += 2;
                    let lo = self.hex(4)?;
                    if (0xDC00..0xE000).contains(&lo) {
                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                        return Ok(Esc::Char(char::from_u32(cp).unwrap()));
                    }
                    self.pos = save;
                }
                Ok(Esc::Char(Self::code_point(hi)?))
            }
            'd' => class("[0-9]"),
            'D' => class("[^0-9]"),
            'w' => class("[a-zA-Z_0-9]"),
            'W' => class("[^a-zA-Z_0-9]"),
            's' => class("[\\t\\n\\x0B\\x0C\\r ]"),
            'S' => class("[^\\t\\n\\x0B\\x0C\\r ]"),
            'h' => class("[ \\t\\xA0\\x{1680}\\x{180E}\\x{2000}-\\x{200A}\\x{202F}\\x{205F}\\x{3000}]"),
            'H' => class("[^ \\t\\xA0\\x{1680}\\x{180E}\\x{2000}-\\x{200A}\\x{202F}\\x{205F}\\x{3000}]"),
            'v' => class("[\\n\\x0B\\x0C\\r\\x{85}\\x{2028}\\x{2029}]"),
            'V' => class("[^\\n\\x0B\\x0C\\r\\x{85}\\x{2028}\\x{2029}]"),
            'p' | 'P' => self.property(c == 'P').map(Esc::Class),
            'Q' => Ok(Esc::Quote),
            'b' | 'B' | 'A' | 'z' | 'Z' | 'G' if in_class => unsupported("of anchors inside character classes"),
            'b' => {
                if self.peek() == Some('{') {
                    return unsupported("of \\b{g}");
                }
                Ok(Esc::Anchor("\\b"))
            }
            'B' => Ok(Esc::Anchor("\\B")),
            'A' => Ok(Esc::Anchor("\\A")),
            'z' => Ok(Esc::Anchor("\\z")),
            'Z' => {
                self.eol_anchor = true;
                Ok(Esc::Anchor("\\z"))
            }
            'G' => unsupported("\\G"),
            'k' => unsupported("of named backreferences"),
            'R' => unsupported("\\R"),
            'X' => unsupported("\\X"),
            'N' => unsupported("\\N{...}"),
            c if c.is_ascii_alphabetic() => Err(Error::Invalid),
            c => Ok(Esc::Char(c)),
        }
    }

    /// `\p{...}` / `\P{...}` (Java property names).
    fn property(&mut self, negated: bool) -> Res<String> {
        let name: String = match self.next() {
            Some('{') => {
                let start = self.pos;
                while self.peek().map(|c| c != '}').unwrap_or(false) {
                    self.pos += 1;
                }
                if self.next() != Some('}') {
                    return Err(Error::Invalid);
                }
                self.chars[start..self.pos - 1].iter().collect()
            }
            Some(c) => c.to_string(),
            None => return Err(Error::Invalid),
        };
        let neg = |cls: &str| -> String {
            if negated {
                format!("[^{}]", &cls[1..cls.len() - 1])
            } else {
                cls.to_string()
            }
        };
        let posix = |n: &str| -> Option<&'static str> {
            Some(match n {
                "Lower" => "[a-z]",
                "Upper" => "[A-Z]",
                "ASCII" => "[\\x00-\\x7F]",
                "Alpha" => "[a-zA-Z]",
                "Digit" => "[0-9]",
                "Alnum" => "[a-zA-Z0-9]",
                "Punct" => "[!-/:-@\\x5B-`{-~]",
                "Graph" => "[!-~]",
                "Print" => "[ -~]",
                "Blank" => "[ \\t]",
                "Cntrl" => "[\\x00-\\x1F\\x7F]",
                "XDigit" => "[0-9a-fA-F]",
                "Space" => "[\\t\\n\\x0B\\x0C\\r ]",
                "all" => "[\\x00-\\x{10FFFF}]",
                "L1" => "[\\x00-\\xFF]",
                "LD" => "[\\p{L}\\p{Nd}]",
                _ => return None,
            })
        };
        const GC: [&str; 38] = [
            "L", "Lu", "Ll", "Lt", "Lm", "Lo", "LC", "M", "Mn", "Mc", "Me", "N", "Nd", "Nl", "No", "P", "Pc", "Pd",
            "Ps", "Pe", "Pi", "Pf", "Po", "S", "Sm", "Sc", "Sk", "So", "Z", "Zs", "Zl", "Zp", "C", "Cc", "Cf", "Cs",
            "Co", "Cn",
        ];
        let case_related = |n: &str| {
            matches!(n, "Lu" | "Ll" | "Lt" | "LC" | "Lower" | "Upper")
                || n.eq_ignore_ascii_case("lowercase")
                || n.eq_ignore_ascii_case("uppercase")
                || n.eq_ignore_ascii_case("titlecase")
        };
        let rust_prop = |n: &str| -> Res<String> {
            let p = format!("\\{}{{{}}}", if negated { 'P' } else { 'p' }, n);
            if Regex::new(&p).is_ok() {
                Ok(p)
            } else {
                unsupported(&format!("\\p{{{}}}", name))
            }
        };
        let (bare, prefixed) = match name.strip_prefix("Is") {
            Some(rest) => (rest, true),
            None => (name.as_str(), false),
        };
        if self.flags.i && case_related(bare) {
            return unsupported("of case-insensitive case properties");
        }
        if name.starts_with("In") || name.starts_with("blk=") || name.starts_with("block=") {
            return unsupported("of Unicode block properties");
        }
        if name.starts_with("java") {
            return unsupported("of java.lang.Character properties");
        }
        if let Some((key, value)) = name.split_once('=') {
            return match key {
                "sc" | "script" => rust_prop(&format!("Script={}", value)),
                "gc" | "general_category" => rust_prop(&format!("General_Category={}", value)),
                _ => Err(Error::Invalid),
            };
        }
        if GC.contains(&bare) {
            return Ok(format!("\\{}{{{}}}", if negated { 'P' } else { 'p' }, bare));
        }
        if let Some(cls) = posix(bare) {
            return Ok(neg(cls));
        }
        if prefixed {
            // Binary properties and scripts.
            return rust_prop(bare);
        }
        Err(Error::Invalid)
    }

    /// A single class member: a char, possibly the start of a range.
    fn class_char(&mut self) -> Res<Option<char>> {
        match self.peek() {
            Some('\\') => {
                let save = self.pos;
                self.pos += 1;
                match self.escape(true)? {
                    Esc::Char(c) => Ok(Some(c)),
                    _ => {
                        self.pos = save;
                        Ok(None)
                    }
                }
            }
            Some(c) => {
                self.pos += 1;
                Ok(Some(c))
            }
            None => Err(Error::Invalid),
        }
    }

    fn class_lit(&self, out: &mut String, c: char) {
        out.push_str(&lit(c));
        if self.flags.i && !self.flags.u {
            if let Some(o) = swap_ascii_case(c) {
                out.push(o);
            }
        }
    }

    /// Parse a class after its `[`, returning a Rust class.
    fn class(&mut self) -> Res<String> {
        let mut out = String::from("[");
        if self.peek() == Some('^') {
            self.pos += 1;
            out.push('^');
        }
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(Error::Invalid),
                Some(']') if !first => {
                    self.pos += 1;
                    break;
                }
                Some('[') => {
                    self.pos += 1;
                    let nested = self.class()?;
                    out.push_str(&nested);
                }
                Some('&') if self.peek_at(1) == Some('&') => {
                    self.pos += 2;
                    out.push_str("&&");
                }
                Some('\\') if self.peek_at(1) == Some('Q') => {
                    self.pos += 2;
                    while self.pos < self.chars.len() {
                        if self.peek() == Some('\\') && self.peek_at(1) == Some('E') {
                            self.pos += 2;
                            break;
                        }
                        let ch = self.next().unwrap();
                        self.class_lit(&mut out, ch);
                    }
                }
                Some('\\') => {
                    let save = self.pos;
                    self.pos += 1;
                    match self.escape(true)? {
                        Esc::Class(cls) => out.push_str(&cls),
                        Esc::Char(_) => {
                            self.pos = save;
                            self.class_member(&mut out)?;
                        }
                        Esc::Anchor(_) | Esc::Quote => return Err(Error::Invalid),
                    }
                }
                Some(_) => self.class_member(&mut out)?,
            }
            first = false;
        }
        out.push(']');
        Ok(out)
    }

    fn class_member(&mut self, out: &mut String) -> Res<()> {
        let lo = self.class_char()?.ok_or(Error::Invalid)?;
        let is_range =
            self.peek() == Some('-') && !matches!(self.peek_at(1), Some(']') | Some('[') | None);
        if !is_range {
            self.class_lit(out, lo);
            return Ok(());
        }
        self.pos += 1;
        let hi = self.class_char()?.ok_or(Error::Invalid)?;
        if hi < lo {
            return Err(Error::Invalid);
        }
        out.push_str(&lit(lo));
        out.push('-');
        out.push_str(&lit(hi));
        if self.flags.i && !self.flags.u {
            for c in ('A'..='Z').chain('a'..='z') {
                if c >= lo && c <= hi {
                    let o = swap_ascii_case(c).unwrap();
                    if o < lo || o > hi {
                        out.push(o);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(p: &str, s: &str) -> Vec<(usize, usize)> {
        compile(p).unwrap().find_all(s)
    }

    #[test]
    fn java_empty_match_iteration() {
        // Java yields the empty match right after a non-empty one.
        assert_eq!(find("a*", "baaab"), vec![(0, 0), (1, 4), (4, 4), (5, 5)]);
        assert_eq!(find("^|$|database", "x,database"), vec![(0, 0), (2, 10), (10, 10)]);
    }

    #[test]
    fn ascii_classes_and_dot() {
        assert_eq!(find("\\d", "١2"), vec![(2, 3)]);
        assert!(find("\\w", "é").is_empty());
        assert!(find(".", "\r").is_empty());
        assert_eq!(find("(?s).", "\r"), vec![(0, 1)]);
    }

    #[test]
    fn case_insensitive_is_ascii_only() {
        assert_eq!(find("(?i)k", "kK\u{212A}"), vec![(0, 1), (1, 2)]);
        assert!(find("(?i)é", "É").is_empty());
        assert_eq!(find("(?iu)é", "É"), vec![(0, 2)]);
        assert_eq!(find("(?i)[a-c]", "B"), vec![(0, 1)]);
        assert!(find("(?i)[^a]", "A").is_empty());
    }

    #[test]
    fn flags_are_scoped_to_groups() {
        assert_eq!(find("((?i)a)a", "AA Aa"), vec![(3, 5)]);
        assert_eq!(find("(?i:a)a", "AA Aa"), vec![(3, 5)]);
    }

    #[test]
    fn quote_round_trips() {
        for s in ["a.b", "\\E", "x\\Ey", "[", ""] {
            let q = quote(s);
            let r = compile(&q).unwrap();
            assert_eq!(r.find_all(s).first().copied(), Some((0, s.len())), "{}", q);
        }
    }

    #[test]
    fn invalid_and_unsupported() {
        assert_eq!(compile("{").err(), Some(Error::Invalid));
        assert_eq!(compile("a{").err(), Some(Error::Invalid));
        assert_eq!(compile("(").err(), Some(Error::Invalid));
        assert_eq!(compile("[a").err(), Some(Error::Invalid));
        assert_eq!(compile("*a").err(), Some(Error::Invalid));
        assert_eq!(compile("\\y").err(), Some(Error::Invalid));
        assert!(matches!(compile("(?=a)").err(), Some(Error::Unsupported(_))));
        assert!(matches!(compile("(a)\\1").err(), Some(Error::Unsupported(_))));
        assert!(matches!(compile("a*+").err(), Some(Error::Unsupported(_))));
        assert!(compile("a{2,3}b}").is_ok());
        assert!(compile("[]a]").is_ok());
        assert!(compile("\\p{Lu}\\p{IsLatin}\\p{Alpha}").is_ok());
    }

    #[test]
    fn replacement_syntax() {
        let r = compile("(a)(b)?").unwrap();
        assert_eq!(r.replace("xaby a", "[$1$2]", false).unwrap(), "x[ab]y [a]");
        assert_eq!(r.replace("aa", "\\$", true).unwrap(), "$a");
        assert!(r.replace("a", "$3", false).is_err());
        assert_eq!(r.replace("ab", "$21", false).unwrap(), "b1");
    }

    #[test]
    fn split_like_java() {
        let r = compile(".*").unwrap();
        assert_eq!(r.split("One 2\n3 Four", -1), vec!["", "", "\n", "", ""]);
        assert_eq!(r.split("One 2\n3 Four", 2), vec!["", "\n3 Four"]);
        let e = compile("").unwrap();
        assert_eq!(e.split("ab", -1), vec!["a", "b", ""]);
    }
}
