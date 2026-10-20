//! String functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/StringFunctions.scala.
//! Regex functions use Java regex syntax on the server; the `regex`
//! crate covers the common subset. Unsupported syntax should surface as
//! `invalid argument` with the server's "Search pattern /x/ is not a valid
//! regular expression." message.

use super::cast;
use super::forms::{Args, Registry};
use super::{Ctx, Env};
use crate::error::{Position, R};
use crate::value::Value;

pub fn register(r: &mut Registry) {
    r.add("concat", &["concat"], &["separator"], concat);
    r.stub("casefold", &["casefold"], &["normalizer"]);
    r.stub("regex_escape", &["regex_escape"], &[]);
    r.stub("regexescape", &["regexescape"], &[]);
    r.stub("starts_with", &["starts_with", "search"], &[]);
    r.stub("startswith", &["startswith", "search"], &[]);
    r.stub("ends_with", &["ends_with", "search"], &[]);
    r.stub("endswith", &["endswith", "search"], &[]);
    r.stub("contains_str", &["contains_str", "search"], &[]);
    r.stub("containsstr", &["containsstr", "search"], &[]);
    r.stub("contains_str_regex", &["contains_str_regex", "pattern"], &[]);
    r.stub("containsstrregex", &["containsstrregex", "pattern"], &[]);
    r.stub("find_str", &["find_str", "find"], &["start"]);
    r.stub("findstr", &["findstr", "find"], &["start"]);
    r.stub("find_str_regex", &["find_str_regex", "pattern"], &["start", "num_results"]);
    r.stub("findstrregex", &["findstrregex", "pattern"], &["start", "num_results"]);
    r.stub("length", &["length"], &[]);
    r.stub("lower_case", &["lower_case"], &[]);
    r.stub("lowercase", &["lowercase"], &[]);
    r.stub("l_trim", &["l_trim"], &[]);
    r.stub("ltrim", &["ltrim"], &[]);
    r.stub("ngram", &["ngram"], &["min", "max"]);
    r.stub("repeat", &["repeat"], &["number"]);
    r.stub("replace_str", &["replace_str", "find", "replace"], &[]);
    r.stub("replacestr", &["replacestr", "find", "replace"], &[]);
    r.stub("replace_str_regex", &["replace_str_regex", "pattern", "replace"], &["first"]);
    r.stub("replacestrregex", &["replacestrregex", "pattern", "replace"], &["first"]);
    r.stub("r_trim", &["r_trim"], &[]);
    r.stub("rtrim", &["rtrim"], &[]);
    r.stub("space", &["space"], &[]);
    r.stub("sub_string", &["sub_string"], &["start", "length"]);
    r.stub("substring", &["substring"], &["start", "length"]);
    r.stub("title_case", &["title_case"], &[]);
    r.stub("titlecase", &["titlecase"], &[]);
    r.stub("trim", &["trim"], &[]);
    r.stub("upper_case", &["upper_case"], &[]);
    r.stub("uppercase", &["uppercase"], &[]);
    r.stub("format", &["format", "values"], &[]);
    r.stub("split_str", &["split_str", "token"], &["count"]);
    r.stub("split_str_regex", &["split_str_regex", "pattern"], &["count"]);
}

fn concat(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
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
