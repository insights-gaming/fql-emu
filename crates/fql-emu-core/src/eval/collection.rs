//! Array / page / set iteration functions: `Map`, `Foreach`, `Filter`,
//! `Reduce`, `Take`, `Drop`, `Prepend`, `Append`, `Count`, `Sum`, `Mean`,
//! `Max`, `Min`, `Any`, `All`, `IsEmpty`, `IsNonEmpty`, `Reverse`,
//! `First`, `Last`.
//!
//! Spec: ext/model/src/main/scala/ast/functions/ArrayFunctions.scala,
//! ReduceFunctions.scala, AbstractFoldFunction.scala, ReadFunctions.scala
//! (`First`/`Last`), SetFunctions.scala (`Reverse`) and the forms in
//! ast/Parser.scala. API version 4 semantics:
//!
//! - `Map`/`Foreach` take an Array or Page only (a Set is an error);
//!   `Filter` on a Set stays lazy; `Reverse` of a Set stays lazy.
//! - The folds (`Reduce`, `Count`, `Sum`, `Mean`, `Max`, `Min`, `Any`,
//!   `All`) materialise Sets, and over a Page return a Page whose `data`
//!   is the single folded value (cursors kept).
//! - `Take`/`Drop` are the v4 `TakeFunction`/`DropFunction`: Array or Page
//!   only. On a Page the server moves the cursors using the page's
//!   *unmapped* values, which `Value::Page` does not carry, so the cases
//!   that need them fail with `not emulated`.
//! - Lambdas for `Map`/`Foreach`/`Filter`/`Reduce` must be inline
//!   `{"lambda", "expr"}` forms (`QueryParser.parseLambda`); arity errors
//!   are reported at `[fn, "lambda"]` and body errors under `[fn, "expr"]`.

use super::cast;
use super::forms::{with_registry, Args, Registry};
use super::{eval, json_type, parse_lambda_expr, Ctx, Env, MAX_STACK_DEPTH};
use crate::error::{one, Errors, FaunaError, Position, R};
use crate::ordering;
use crate::value::{Lambda, Pattern, SetExpr, Type, Value};
use serde_json::Value as Json;
use std::cmp::Ordering;
use std::collections::HashSet;

pub fn register(r: &mut Registry) {
    r.add("map", &["map", "collection"], &[], map);
    r.add("foreach", &["foreach", "collection"], &[], foreach);
    r.add("filter", &["filter", "collection"], &[], filter);
    r.add("reduce", &["reduce", "initial", "collection"], &[], reduce);
    r.add("prepend", &["prepend", "collection"], &[], |c, e, a, p| prepend_append("prepend", c, e, a, p));
    r.add("append", &["append", "collection"], &[], |c, e, a, p| prepend_append("append", c, e, a, p));
    r.add("take", &["take", "collection"], &[], take);
    r.add("drop", &["drop", "collection"], &[], drop);
    r.add("is_empty", &["is_empty"], &[], |c, e, a, p| is_empty("is_empty", c, e, a, p));
    r.add("is_nonempty", &["is_nonempty"], &[], |c, e, a, p| is_empty("is_nonempty", c, e, a, p));
    r.add("count", &["count"], &[], |c, e, a, p| fold_simple(Fold::Count, c, e, a, p));
    r.add("sum", &["sum"], &[], |c, e, a, p| fold_simple(Fold::Sum, c, e, a, p));
    r.add("mean", &["mean"], &[], |c, e, a, p| fold_simple(Fold::Mean, c, e, a, p));
    r.add("max", &["max"], &[], |c, e, a, p| fold_simple(Fold::Max, c, e, a, p));
    r.add("min", &["min"], &[], |c, e, a, p| fold_simple(Fold::Min, c, e, a, p));
    r.add("any", &["any"], &[], |c, e, a, p| fold_simple(Fold::Any, c, e, a, p));
    r.add("all", &["all"], &[], |c, e, a, p| fold_simple(Fold::All, c, e, a, p));
    r.add("reverse", &["reverse"], &[], reverse);
    r.add("first", &["first"], &[], |c, e, a, p| first_last("first", c, e, a, p));
    r.add("last", &["last"], &[], |c, e, a, p| first_last("last", c, e, a, p));
}

// ---- lambdas ----

/// `QueryParser.parseLambda`: the argument must be an inline
/// `{"lambda": .., "expr": ..}` form. Anything else is a parse error
/// (a `Var` holding a lambda is not accepted by these forms).
fn inline_lambda(env: &Env, expr: &Json, pos: &Position) -> R<Lambda> {
    match expr {
        Json::Object(o) => {
            if o.len() == 2 && o.contains_key("lambda") && o.contains_key("expr") {
                parse_lambda_expr(env, expr, pos)
            } else {
                let keys: Vec<&str> = o.keys().map(|k| k.as_str()).collect();
                one(FaunaError::invalid_form(&keys, pos))
            }
        }
        other => one(FaunaError::invalid_expr_type(&[Type::Lambda], json_type(other), pos)),
    }
}

/// `EvalContext.genLambdaArgMap`: positional binding. A non-array
/// argument binds to a one-element pattern; arity errors are collected.
fn bind(env: &mut Env, pat: &Pattern, arg: &Value, pos: &Position, errs: &mut Errors) {
    match (pat, arg) {
        (Pattern::Ignore, _) => {}
        (Pattern::Name(n), v) => *env = env.bind(n, v.clone()),
        (Pattern::Array(ps), Value::Array(items)) => {
            for (i, (p, v)) in ps.iter().zip(items).enumerate() {
                bind(env, p, v, &pos.idx(i), errs);
            }
            if ps.len() != items.len() {
                errs.push(FaunaError::invalid_lambda_arity(ps.len(), items.len(), pos));
            }
        }
        (Pattern::Array(ps), v) if ps.len() == 1 => bind(env, &ps[0], v, &pos.idx(0), errs),
        (Pattern::Array(ps), _) => errs.push(FaunaError::invalid_lambda_arity(ps.len(), 1, pos)),
    }
}

/// `EvalContext.evalLambdaApply(lambda, arg, pos at fn)` for an inline
/// lambda parsed at `fn_pos`: bindings at `fn_pos ++ ["lambda"]`, body at
/// `fn_pos ++ ["expr"]`.
fn apply(ctx: &mut Ctx, lam: &Lambda, arg: &Value, fn_pos: &Position) -> R<Value> {
    if ctx.depth >= MAX_STACK_DEPTH {
        return one(FaunaError::stack_overflow(fn_pos));
    }
    let mut env = lam.env.clone();
    let mut errs = Vec::new();
    bind(&mut env, &lam.params, arg, &fn_pos.key("lambda"), &mut errs);
    if !errs.is_empty() {
        return Err(errs);
    }
    ctx.depth += 1;
    let r = eval(ctx, &env, &lam.body, &fn_pos.key("expr"));
    ctx.depth -= 1;
    r
}

// ---- casts ----

/// Classified `Casts.Iterable` result.
enum Iter {
    Array(Vec<Value>),
    Page { data: Vec<Value>, before: Option<Box<Value>>, after: Option<Box<Value>> },
    Set(SetExpr),
}

/// If `v` is a ref the server would have produced as an `UnresolvedRefL`
/// (a schema ref or document ref whose collection does not exist), the
/// matching `invalid ref` error.
pub fn unresolved_ref(ctx: &Ctx, v: &Value, pos: &Position) -> Option<FaunaError> {
    let r = match v {
        Value::Ref(r) => r,
        _ => return None,
    };
    let db = &*ctx.db;
    let missing = |kind: &str, name: &str| Some(FaunaError::unresolved_ref(kind, name, pos));
    if r.is_collection_ref() {
        return if db.collections.contains_key(&r.id) { None } else { missing("collection", &r.id) };
    }
    if r.is_index_ref() {
        return if db.indexes.contains_key(&r.id) { None } else { missing("index", &r.id) };
    }
    if r.is_function_ref() {
        return if db.functions.contains_key(&r.id) { None } else { missing("function", &r.id) };
    }
    if r.is_role_ref() {
        return if db.roles.contains_key(&r.id) { None } else { missing("role", &r.id) };
    }
    if let Some(coll) = r.document_collection() {
        return if db.collections.contains_key(coll) { None } else { missing("collection", coll) };
    }
    None
}

/// `Casts.Iterable`: Array, Page, or Set (native collection refs such as
/// `Collections()` are schema sets, which the emulator does not iterate).
fn iterable(ctx: &Ctx, v: Value, pos: &Position) -> R<Iter> {
    match v {
        Value::Array(a) => Ok(Iter::Array(a)),
        Value::Page { data, before, after } => Ok(Iter::Page { data, before, after }),
        Value::Set(s) => Ok(Iter::Set(s)),
        Value::Ref(r) if r.is_native() => {
            one(FaunaError::not_emulated("iterating native collections (schema sets)", pos))
        }
        other => {
            if let Some(e) = unresolved_ref(ctx, &other, pos) {
                return one(e);
            }
            one(FaunaError::invalid_argument(&[Type::AbstractIterable], other.rtype(), pos))
        }
    }
}

fn page(data: Vec<Value>, before: Option<Box<Value>>, after: Option<Box<Value>>) -> Value {
    Value::Page { data, before, after }
}

// ---- map / foreach / filter / reduce ----

fn map(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let lp = pos.key("map");
    let lam = inline_lambda(env, args.req("map"), &lp)?;
    let coll = args.eval(ctx, env, "collection", pos)?;
    let cp = pos.key("collection");
    match iterable(ctx, coll, &cp)? {
        Iter::Array(items) => Ok(Value::Array(map_values(ctx, &lam, &items, &lp)?)),
        Iter::Page { data, before, after } => Ok(page(map_values(ctx, &lam, &data, &lp)?, before, after)),
        Iter::Set(_) => one(FaunaError::invalid_argument(&[Type::AbstractArray], Type::Set, &cp)),
    }
}

fn map_values(ctx: &mut Ctx, lam: &Lambda, items: &[Value], lp: &Position) -> R<Vec<Value>> {
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        out.push(apply(ctx, lam, it, lp)?);
    }
    Ok(out)
}

fn foreach(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let lp = pos.key("foreach");
    let lam = inline_lambda(env, args.req("foreach"), &lp)?;
    let coll = args.eval(ctx, env, "collection", pos)?;
    let cp = pos.key("collection");
    match iterable(ctx, coll, &cp)? {
        Iter::Array(items) => {
            map_values(ctx, &lam, &items, &lp)?;
            Ok(Value::Array(items))
        }
        Iter::Page { data, before, after } => {
            map_values(ctx, &lam, &data, &lp)?;
            Ok(page(data, before, after))
        }
        Iter::Set(_) => one(FaunaError::invalid_argument(&[Type::AbstractArray], Type::Set, &cp)),
    }
}

/// `AbstractMapFunction.filterValues`: the lambda results are cast with
/// `ZeroOrMore(Boolean)` at the form position, so a non-boolean result is
/// reported at `pos ++ [i]`.
fn filter_values(ctx: &mut Ctx, lam: &Lambda, items: Vec<Value>, lp: &Position, pos: &Position) -> R<Vec<Value>> {
    let mut keep = Vec::with_capacity(items.len());
    for it in &items {
        keep.push(apply(ctx, lam, it, lp)?);
    }
    let mut errs = Vec::new();
    let mut out = Vec::new();
    for (i, (it, k)) in items.into_iter().zip(keep).enumerate() {
        match k {
            Value::Bool(true) => out.push(it),
            Value::Bool(false) => {}
            other => errs.push(FaunaError::invalid_argument(&[Type::Boolean], other.rtype(), &pos.idx(i))),
        }
    }
    if errs.is_empty() {
        Ok(out)
    } else {
        Err(errs)
    }
}

/// Write forms (and `Call`, whose UDF may write) that make a lambda
/// unusable as a set filter (`lambda.callMaxEffect == Effect.Write`).
/// Matched by registered form name, so `replace_str`'s `replace` key does
/// not count.
const WRITE_FORMS: &[&str] = &[
    "create",
    "update",
    "replace",
    "delete",
    "insert",
    "remove",
    "create_collection",
    "create_class",
    "create_index",
    "create_function",
    "create_role",
    "create_database",
    "create_key",
    "create_access_provider",
    "move_database",
    "call",
];

fn has_write_effect(j: &Json) -> bool {
    match j {
        Json::Array(items) => items.iter().any(has_write_effect),
        Json::Object(o) => {
            if o.contains_key("quote") || o.keys().any(|k| k.starts_with('@')) {
                return false;
            }
            if let (Some(Json::Object(fields)), 1) = (o.get("object"), o.len()) {
                return fields.values().any(has_write_effect);
            }
            let keys: HashSet<&str> = o.keys().map(|k| k.as_str()).collect();
            let form = with_registry(|r| r.lookup(&keys).map(|f| f.name));
            form.is_some_and(|name| WRITE_FORMS.contains(&name)) || o.values().any(has_write_effect)
        }
        _ => false,
    }
}

fn filter(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let lp = pos.key("filter");
    let lam = inline_lambda(env, args.req("filter"), &lp)?;
    let coll = args.eval(ctx, env, "collection", pos)?;
    match iterable(ctx, coll, &pos.key("collection"))? {
        Iter::Array(items) => Ok(Value::Array(filter_values(ctx, &lam, items, &lp, pos)?)),
        Iter::Page { data, before, after } => Ok(page(filter_values(ctx, &lam, data, &lp, pos)?, before, after)),
        Iter::Set(source) => {
            if has_write_effect(&lam.body) {
                return one(FaunaError::invalid_lambda_effect(false, &lp));
            }
            Ok(Value::Set(SetExpr::Filter { lambda: lam, source: Box::new(source) }))
        }
    }
}

fn reduce(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let lp = pos.key("reduce");
    let lam = inline_lambda(env, args.req("reduce"), &lp)?;
    let initial = args.eval(ctx, env, "initial", pos)?;
    let coll = args.eval(ctx, env, "collection", pos)?;
    let step = |ctx: &mut Ctx, items: Vec<Value>| -> R<Value> {
        let mut acc = initial.clone();
        for it in items {
            acc = apply(ctx, &lam, &Value::Array(vec![acc, it]), &lp)?;
        }
        Ok(acc)
    };
    match iterable(ctx, coll, &pos.key("collection"))? {
        Iter::Array(items) => step(ctx, items),
        Iter::Page { data, before, after } => Ok(page(vec![step(ctx, data)?], before, after)),
        Iter::Set(s) => {
            let items = super::sets::materialize(ctx, &s, &lp)?;
            step(ctx, items)
        }
    }
}

// ---- simple folds ----

#[derive(Clone, Copy, PartialEq)]
enum Fold {
    Count,
    Sum,
    Mean,
    Max,
    Min,
    Any,
    All,
}

impl Fold {
    fn name(self) -> &'static str {
        match self {
            Fold::Count => "count",
            Fold::Sum => "sum",
            Fold::Mean => "mean",
            Fold::Max => "max",
            Fold::Min => "min",
            Fold::Any => "any",
            Fold::All => "all",
        }
    }
}

/// `AbstractFoldFunction.fold` for the folds without a lambda.
fn fold_simple(f: Fold, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let name = f.name();
    let p = pos.key(name);
    let mut v = args.eval(ctx, env, name, pos)?;
    // `AbstractCBORFunction`: `Max([iterable])` folds the inner iterable.
    if matches!(f, Fold::Max | Fold::Min) {
        while let Value::Array(items) = &v {
            match items.as_slice() {
                [inner @ (Value::Array(_) | Value::Page { .. } | Value::Set(_))] => v = inner.clone(),
                _ => break,
            }
        }
    }
    match iterable(ctx, v, &p)? {
        Iter::Array(items) => fold_values(f, ctx, &items, &p),
        Iter::Page { data, before, after } => Ok(page(vec![fold_values(f, ctx, &data, &p)?], before, after)),
        Iter::Set(s) => {
            let items = super::sets::materialize(ctx, &s, &p)?;
            fold_values(f, ctx, &items, &p)
        }
    }
}

fn fold_values(f: Fold, ctx: &Ctx, items: &[Value], p: &Position) -> R<Value> {
    match f {
        Fold::Count => Ok(Value::Long(items.len() as i64)),
        Fold::Sum | Fold::Mean => {
            let mut acc = cast::Num::Long(0);
            for it in items {
                acc = match (acc, cast::number(it, p)?) {
                    (cast::Num::Long(a), cast::Num::Long(b)) => cast::Num::Long(a.wrapping_add(b)),
                    (a, b) => cast::Num::Double(a.as_f64() + b.as_f64()),
                };
            }
            if f == Fold::Sum {
                Ok(acc.to_value())
            } else if items.is_empty() {
                one(FaunaError::empty_array_argument(p))
            } else {
                Ok(Value::Double(acc.as_f64() / items.len() as f64))
            }
        }
        Fold::Any | Fold::All => {
            // Every element is type-checked; the value does not short-circuit.
            let mut acc = f == Fold::All;
            for it in items {
                let b = cast::boolean(it, p)?;
                acc = if f == Fold::All { acc && b } else { acc || b };
            }
            Ok(Value::Bool(acc))
        }
        Fold::Max | Fold::Min => {
            let errs: Vec<FaunaError> = items.iter().filter_map(|it| unresolved_ref(ctx, it, p)).collect();
            if !errs.is_empty() {
                return Err(errs);
            }
            let mut acc: Option<&Value> = None;
            for it in items {
                acc = Some(match acc {
                    None => it,
                    Some(a) => {
                        let cmp = ordering::compare(a, it);
                        let keep = if f == Fold::Max { cmp == Ordering::Greater } else { cmp == Ordering::Less };
                        if keep {
                            a
                        } else {
                            it
                        }
                    }
                });
            }
            match acc {
                Some(v) => Ok(v.clone()),
                None => one(FaunaError::empty_array_argument(p)),
            }
        }
    }
}

// ---- prepend / append / take / drop ----

fn prepend_append(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let elems = args.eval(ctx, env, key, pos)?;
    let coll = args.eval(ctx, env, "collection", pos)?;
    let coll = cast::array(&coll, &pos.key("collection"))?;
    let elems: Vec<Value> = cast::elems(&elems, &pos.key(key)).into_iter().map(|(v, _)| v).collect();
    let out = if key == "prepend" {
        elems.into_iter().chain(coll.iter().cloned()).collect()
    } else {
        coll.iter().cloned().chain(elems).collect()
    };
    Ok(Value::Array(out))
}

/// The page-cursor cases of v4 `Take`/`Drop` that need the page's unmapped
/// values (which `Value::Page` does not keep).
fn page_cursor_not_emulated(fname: &str, pos: &Position) -> R<Value> {
    one(FaunaError::not_emulated(
        &format!("`{}` on a page when the page cursors must move (the emulator does not keep a page's unmapped values)", fname),
        pos,
    ))
}

/// Scala's `Long.toInt` truncation.
fn to_int(n: i64) -> i64 {
    n as i32 as i64
}

/// Scala `List.take` / `List.drop` with a possibly negative count.
fn split_at(len: usize, n: i64) -> usize {
    n.clamp(0, len as i64) as usize
}

fn take(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let nv = args.eval(ctx, env, "take", pos)?;
    let coll = args.eval(ctx, env, "collection", pos)?;
    let n = to_int(cast::long(&nv, &pos.key("take"))?);
    match coll {
        Value::Array(mut items) => {
            items.truncate(split_at(items.len(), n));
            Ok(Value::Array(items))
        }
        Value::Page { data, before, after } => {
            if n < 0 {
                Ok(page(vec![], before.clone(), before))
            } else if n as usize >= data.len() {
                Ok(page(data, before, after))
            } else {
                page_cursor_not_emulated("Take", pos)
            }
        }
        other => one(FaunaError::invalid_argument(&[Type::AbstractArray], other.rtype(), &pos.key("collection"))),
    }
}

fn drop(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let nv = args.eval(ctx, env, "drop", pos)?;
    let coll = args.eval(ctx, env, "collection", pos)?;
    let n = to_int(cast::long(&nv, &pos.key("drop"))?);
    match coll {
        Value::Array(items) => {
            let k = split_at(items.len(), n);
            Ok(Value::Array(items[k..].to_vec()))
        }
        Value::Page { data, before, after } => {
            if n < 0 {
                Ok(page(data, before, after))
            } else if n as usize >= data.len() {
                Ok(page(vec![], after.clone(), after))
            } else {
                page_cursor_not_emulated("Drop", pos)
            }
        }
        other => one(FaunaError::invalid_argument(&[Type::AbstractArray], other.rtype(), &pos.key("collection"))),
    }
}

// ---- is_empty / reverse / first / last ----

fn is_empty(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let empty = match iterable(ctx, v, &pos.key(key))? {
        Iter::Array(items) => items.is_empty(),
        Iter::Page { data, .. } => data.is_empty(),
        Iter::Set(s) => super::sets::materialize(ctx, &s, pos)?.is_empty(),
    };
    Ok(Value::Bool(if key == "is_empty" { empty } else { !empty }))
}

fn reverse(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "reverse", pos)?;
    match iterable(ctx, v, &pos.key("reverse"))? {
        Iter::Array(mut items) => {
            items.reverse();
            Ok(Value::Array(items))
        }
        Iter::Page { mut data, before, after } => {
            data.reverse();
            Ok(page(data, before, after))
        }
        Iter::Set(s) => Ok(Value::Set(SetExpr::Reverse(Box::new(s)))),
    }
}

/// `First`/`Last` (`Casts.SetOrArray`): a Page is rejected.
fn first_last(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let p = pos.key(key);
    let pick = |mut items: Vec<Value>| if key == "first" { items.swap_remove(0) } else { items.pop().unwrap() };
    match iterable(ctx, v, &p)? {
        Iter::Array(items) if items.is_empty() => one(FaunaError::empty_array_argument(&p)),
        Iter::Array(items) => Ok(pick(items)),
        Iter::Page { .. } => one(FaunaError::invalid_argument(&[Type::Set, Type::Array], Type::Page, &p)),
        Iter::Set(s) => {
            let items = super::sets::materialize(ctx, &s, pos)?;
            if items.is_empty() {
                one(FaunaError::empty_set_argument(&p))
            } else {
                Ok(pick(items))
            }
        }
    }
}
