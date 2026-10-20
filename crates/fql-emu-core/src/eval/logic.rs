//! Boolean and comparison functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/BooleanFunctions.scala and
//! ComparisonFunctions.scala. `And`/`Or` short-circuit in v3+ (they are
//! forms, not functions, in Parser.scala lines 594-620).

use super::cast;
use super::forms::{Args, Registry};
use super::{eval, Ctx, Env};
use crate::error::{Position, R};
use crate::ordering::compare;
use crate::value::Value;
use serde_json::Value as Json;
use std::cmp::Ordering;

pub fn register(r: &mut Registry) {
    r.add("and", &["and"], &[], and);
    r.add("or", &["or"], &[], or);
    r.add("not", &["not"], &[], not);
    r.add("lt", &["lt"], &[], |c, e, a, p| cmp("lt", c, e, a, p));
    r.add("lte", &["lte"], &[], |c, e, a, p| cmp("lte", c, e, a, p));
    r.add("gt", &["gt"], &[], |c, e, a, p| cmp("gt", c, e, a, p));
    r.add("gte", &["gte"], &[], |c, e, a, p| cmp("gte", c, e, a, p));
}

/// Short-circuit evaluation of the operands of `And`/`Or`.
fn short_circuit(key: &str, stop_on: bool, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let p = pos.key(key);
    match args.req(key) {
        Json::Array(items) => {
            if items.is_empty() {
                return Err(vec![crate::error::FaunaError::empty_array_argument(&p)]);
            }
            let mut result = !stop_on;
            for (i, it) in items.iter().enumerate() {
                let v = eval(ctx, env, it, &p.idx(i))?;
                let b = cast::boolean(&v, &p.idx(i))?;
                if b == stop_on {
                    result = stop_on;
                    break;
                }
            }
            Ok(Value::Bool(result))
        }
        other => {
            let v = eval(ctx, env, other, &p)?;
            let b = cast::boolean(&v, &p)?;
            Ok(Value::Bool(b))
        }
    }
}

fn and(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    short_circuit("and", false, ctx, env, args, pos)
}

fn or(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    short_circuit("or", true, ctx, env, args, pos)
}

fn not(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "not", pos)?;
    Ok(Value::Bool(!cast::boolean(&v, &pos.key("not"))?))
}

fn cmp(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let es = cast::one_or_more(&v, &pos.key(key))?;
    let ok = es.windows(2).all(|w| {
        let o = compare(&w[0].0, &w[1].0);
        match key {
            "lt" => o == Ordering::Less,
            "lte" => o != Ordering::Greater,
            "gt" => o == Ordering::Greater,
            _ => o != Ordering::Less,
        }
    });
    Ok(Value::Bool(ok))
}
