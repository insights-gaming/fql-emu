//! Basic forms: `Let`, `Var`, `If`, `Do`, `Lambda`, `Call`, `Select`,
//! `Equals`, `Contains*`, `Merge`, `Abort`, ref constructors, `NewId`,
//! conversions and type checks.
//!
//! Spec: ext/model/src/main/scala/ast/Parser.scala (forms) and
//! ext/model/src/main/scala/ast/functions/BasicFunctions.scala,
//! ObjectFunctions.scala, ConversionFunctions.scala,
//! TypeCheckFunctions.scala.

use super::cast::{self, PathElem};
use super::forms::{Args, Registry};
use super::{apply_lambda, call_function, eval, eval_lambda_arg, json_type, Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::value::{Object, Ref, Type, Value};
use serde_json::Value as Json;

pub fn register(r: &mut Registry) {
    r.stub("at", &["at", "expr"], &[]);
    r.add("var", &["var"], &[], var);
    r.add("let", &["let", "in"], &[], let_);
    // NB: `{"lambda", "expr"}` is deliberately *not* a form. The server only
    // parses it as the argument of specific functions (`QueryParser.lambdas`);
    // at the top level or in a `Let` it is "No form/function found".
    r.add("if", &["if", "then", "else"], &[], if_);
    r.add("do", &["do"], &[], do_);
    r.add("object", &["object"], &[], object);
    r.add("quote", &["quote"], &[], quote);
    r.add("call", &["call"], &["arguments"], call);
    r.add("abort", &["abort"], &[], abort);
    r.add("query", &["query"], &[], query);
    r.add("equals", &["equals"], &[], equals);
    r.add("contains_path", &["contains_path", "in"], &[], contains_path);
    r.add("contains", &["contains", "in"], &[], contains_path_legacy);
    r.add("contains_field", &["contains_field", "in"], &[], contains_field);
    r.add("contains_value", &["contains_value", "in"], &[], contains_value);
    r.add("select", &["select", "from"], &["default", "all"], select);
    r.add("select_all", &["select_all", "from"], &["default"], select_all);
    r.add("select_as_index", &["select_as_index", "from"], &["default"], select_as_index);
    r.add("merge", &["merge", "with"], &["lambda"], merge);
    r.add("to_object", &["to_object"], &[], to_object);
    r.add("to_array", &["to_array"], &[], to_array);

    // Ref constructors.
    r.add("ref", &["ref", "id"], &["scope"], ref_);
    r.add("database", &["database"], &["scope"], database);
    r.add("index", &["index"], &["scope"], index);
    r.add("collection", &["collection"], &["scope"], collection);
    r.add("class", &["class"], &["scope"], collection_legacy);
    r.add("function", &["function"], &["scope"], function);
    r.add("role", &["role"], &["scope"], role);
    r.stub("access_provider", &["access_provider"], &["scope"]);
    r.add("new_id", &["new_id"], &[], new_id);
    r.add("next_id", &["next_id"], &[], new_id);
    r.add("databases", &["databases"], &[], |c, e, a, p| native("databases", "databases", c, e, a, p));
    r.add("indexes", &["indexes"], &[], |c, e, a, p| native("indexes", "indexes", c, e, a, p));
    r.add("collections", &["collections"], &[], |c, e, a, p| native("collections", "collections", c, e, a, p));
    r.add("classes", &["classes"], &[], |c, e, a, p| native("classes", "collections", c, e, a, p));
    r.add("keys", &["keys"], &[], |c, e, a, p| native("keys", "keys", c, e, a, p));
    r.add("tokens", &["tokens"], &[], |c, e, a, p| native("tokens", "tokens", c, e, a, p));
    r.add("credentials", &["credentials"], &[], |c, e, a, p| native("credentials", "credentials", c, e, a, p));
    r.add("functions", &["functions"], &[], |c, e, a, p| native("functions", "functions", c, e, a, p));
    r.add("roles", &["roles"], &[], |c, e, a, p| native("roles", "roles", c, e, a, p));
    r.add("access_providers", &["access_providers"], &[], |c, e, a, p| {
        native("access_providers", "access_providers", c, e, a, p)
    });

    // Type checks (TypeCheckFunctions.scala).
    r.add("is_number", &["is_number"], &[], |c, e, a, p| is_type("is_number", c, e, a, p));
    r.add("is_double", &["is_double"], &[], |c, e, a, p| is_type("is_double", c, e, a, p));
    r.add("is_integer", &["is_integer"], &[], |c, e, a, p| is_type("is_integer", c, e, a, p));
    r.add("is_boolean", &["is_boolean"], &[], |c, e, a, p| is_type("is_boolean", c, e, a, p));
    r.add("is_null", &["is_null"], &[], |c, e, a, p| is_type("is_null", c, e, a, p));
    r.add("is_bytes", &["is_bytes"], &[], |c, e, a, p| is_type("is_bytes", c, e, a, p));
    r.add("is_timestamp", &["is_timestamp"], &[], |c, e, a, p| is_type("is_timestamp", c, e, a, p));
    r.add("is_date", &["is_date"], &[], |c, e, a, p| is_type("is_date", c, e, a, p));
    r.add("is_uuid", &["is_uuid"], &[], |c, e, a, p| is_type("is_uuid", c, e, a, p));
    r.add("is_string", &["is_string"], &[], |c, e, a, p| is_type("is_string", c, e, a, p));
    r.add("is_array", &["is_array"], &[], |c, e, a, p| is_type("is_array", c, e, a, p));
    r.add("is_object", &["is_object"], &[], |c, e, a, p| is_type("is_object", c, e, a, p));
    r.add("is_ref", &["is_ref"], &[], |c, e, a, p| is_type("is_ref", c, e, a, p));
    r.add("is_set", &["is_set"], &[], |c, e, a, p| is_type("is_set", c, e, a, p));
    r.add("is_doc", &["is_doc"], &[], |c, e, a, p| is_type("is_doc", c, e, a, p));
    r.add("is_lambda", &["is_lambda"], &[], |c, e, a, p| is_type("is_lambda", c, e, a, p));
    r.add("is_collection", &["is_collection"], &[], |c, e, a, p| is_type("is_collection", c, e, a, p));
    r.add("is_database", &["is_database"], &[], |c, e, a, p| is_type("is_database", c, e, a, p));
    r.add("is_index", &["is_index"], &[], |c, e, a, p| is_type("is_index", c, e, a, p));
    r.add("is_function", &["is_function"], &[], |c, e, a, p| is_type("is_function", c, e, a, p));
    r.add("is_key", &["is_key"], &[], |c, e, a, p| is_type("is_key", c, e, a, p));
    r.add("is_token", &["is_token"], &[], |c, e, a, p| is_type("is_token", c, e, a, p));
    r.add("is_credentials", &["is_credentials"], &[], |c, e, a, p| is_type("is_credentials", c, e, a, p));
    r.add("is_role", &["is_role"], &[], |c, e, a, p| is_type("is_role", c, e, a, p));
}

fn var(_ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    match args.req("var") {
        Json::String(name) => match env.lookup(name) {
            Some(v) => Ok(v.clone()),
            None => one(FaunaError::unbound_variable(name, pos)),
        },
        other => one(FaunaError::invalid_expr_type(&[Type::String], json_type(other), &pos.key("var"))),
    }
}

/// `Let` bindings are sequential: each binding sees the previous ones.
/// The driver sends `let` as an array of single-key objects (v4) or an
/// object (older drivers).
fn let_(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let bindings = args.req("let");
    let let_pos = pos.key("let");
    let mut env = env.clone();
    match bindings {
        Json::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                match item {
                    Json::Object(o) if o.len() == 1 => {
                        let (k, v) = o.iter().next().unwrap();
                        let val = eval(ctx, &env, v, &let_pos.idx(i).key(k))?;
                        env = env.bind(k, val);
                    }
                    other => {
                        return one(FaunaError::invalid_expr_type(&[Type::Object], json_type(other), &let_pos.idx(i)))
                    }
                }
            }
        }
        Json::Object(o) => {
            for (k, v) in o {
                let val = eval(ctx, &env, v, &let_pos.key(k))?;
                env = env.bind(k, val);
            }
        }
        other => return one(FaunaError::invalid_expr_type(&[Type::Array], json_type(other), &let_pos)),
    }
    eval(ctx, &env, args.req("in"), &pos.key("in"))
}

fn if_(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let cond = args.eval(ctx, env, "if", pos)?;
    let b = cast::boolean(&cond, &pos.key("if"))?;
    if b {
        args.eval(ctx, env, "then", pos)
    } else {
        args.eval(ctx, env, "else", pos)
    }
}

fn do_(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let p = pos.key("do");
    match args.req("do") {
        Json::Array(items) => {
            if items.is_empty() {
                return one(FaunaError::empty_expr(&p));
            }
            let mut last = Value::Null;
            for (i, it) in items.iter().enumerate() {
                last = eval(ctx, env, it, &p.idx(i))?;
            }
            Ok(last)
        }
        other => eval(ctx, env, other, &p),
    }
}

/// `{"object": {k: expr}}`: a literal object whose values are evaluated.
fn object(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let p = pos.key("object");
    match args.req("object") {
        Json::Object(o) => {
            let mut out = Object::with_capacity(o.len());
            let mut errs = Vec::new();
            for (k, v) in o {
                match eval(ctx, env, v, &p.key(k)) {
                    Ok(val) => {
                        out.insert(k.clone(), val);
                    }
                    Err(e) => errs.extend(e),
                }
            }
            if errs.is_empty() {
                Ok(Value::Object(out))
            } else {
                Err(errs)
            }
        }
        other => one(FaunaError::invalid_expr_type(&[Type::Object], json_type(other), &p)),
    }
}

fn quote(_ctx: &mut Ctx, _env: &Env, args: &Args, pos: &Position) -> R<Value> {
    crate::wire::decode_literal(args.req("quote"), &pos.key("quote"))
}

fn call(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let f = args.eval(ctx, env, "call", pos)?;
    let a = match args.get("arguments") {
        Some(j) => eval(ctx, env, j, &pos.key("arguments"))?,
        None => Value::Array(vec![]),
    };
    call_function(ctx, &f, a, pos)
}

fn abort(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "abort", pos)?;
    let msg = cast::string(&v, &pos.key("abort"))?;
    one(FaunaError::transaction_abort(msg, pos))
}

fn query(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let l = eval_lambda_arg(ctx, env, args.req("query"), &pos.key("query"))?;
    Ok(Value::Lambda(l))
}

fn equals(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "equals", pos)?;
    let es = cast::one_or_more(&v, &pos.key("equals"))?;
    let first = &es[0].0;
    Ok(Value::Bool(es[1..].iter().all(|(e, _)| first.fauna_equals(e))))
}

// ---- paths ----

/// Walk a path into a container. `None` when the path is absent.
/// A page behaves like `{before?, after?, data: [...]}`.
pub fn value_at_path<'v>(container: &'v Value, path: &[PathElem]) -> Option<&'v Value> {
    let mut cur = container;
    let mut i = 0;
    while i < path.len() {
        cur = match (cur, &path[i]) {
            (Value::Object(o), PathElem::Key(k)) => o.get(k)?,
            (Value::Array(a), PathElem::Index(ix)) => a.get(usize::try_from(*ix).ok()?)?,
            (Value::Page { data, before, after }, PathElem::Key(k)) => match k.as_str() {
                "before" => before.as_deref()?,
                "after" => after.as_deref()?,
                "data" => {
                    // Step into the page data; the next element must be an index.
                    i += 1;
                    match path.get(i) {
                        None => return None, // callers special-case selecting `data` itself
                        Some(PathElem::Index(ix)) => data.get(usize::try_from(*ix).ok()?)?,
                        Some(PathElem::Key(_)) => return None,
                    }
                }
                _ => return None,
            },
            _ => return None,
        };
        i += 1;
    }
    Some(cur)
}

/// `ReadAdaptor.select`: `all` collects every match when wildcards are
/// involved; without wildcards it wraps the single result in an array.
fn do_select(container: &Value, path: &[PathElem], all: bool, default: Option<Value>, pos: &Position) -> R<Value> {
    // Fast path for `Select(["data"], page)`.
    if let (Value::Page { data, .. }, [PathElem::Key(k)]) = (container, path) {
        if k == "data" {
            return Ok(Value::Array(data.clone()));
        }
    }
    if let (Value::Page { data, .. }, [PathElem::Key(k), rest @ ..]) = (container, path) {
        if k == "data" && !rest.is_empty() {
            return do_select(&Value::Array(data.clone()), rest, all, default, pos);
        }
    }
    if !all {
        match value_at_path(container, path).cloned().or(default) {
            Some(v) => Ok(v),
            None => one(FaunaError::value_not_found(path, pos)),
        }
    } else {
        // `valueAtPath0(all = true)`: arrays along the path are traversed
        // and the matches concatenated (the index `select_as_index` rule).
        Ok(match super::sets::select_all(container, path) {
            Some(Value::Array(a)) => Value::Array(a),
            Some(v) => Value::Array(vec![v]),
            None => Value::Array(vec![]),
        })
    }
}

fn select(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let path_v = args.eval(ctx, env, "select", pos)?;
    let path = cast::path(&path_v, &pos.key("select"))?;
    let from = args.eval(ctx, env, "from", pos)?;
    let all = match args.eval_opt(ctx, env, "all", pos)? {
        Some(v) => cast::boolean(&v, &pos.key("all"))?,
        None => false,
    };
    let default = args.eval_opt(ctx, env, "default", pos)?;
    do_select(&from, &path, all, default, &pos.key("from"))
}

fn select_all(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let path_v = args.eval(ctx, env, "select_all", pos)?;
    let path = cast::path(&path_v, &pos.key("select_all"))?;
    let from = args.eval(ctx, env, "from", pos)?;
    let default = args.eval_opt(ctx, env, "default", pos)?;
    do_select(&from, &path, true, default, &pos.key("from"))
}

fn select_as_index(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let path_v = args.eval(ctx, env, "select_as_index", pos)?;
    let path = cast::path(&path_v, &pos.key("select_as_index"))?;
    let from = args.eval(ctx, env, "from", pos)?;
    let default = args.eval_opt(ctx, env, "default", pos)?;
    do_select(&from, &path, true, default, &pos.key("from"))
}

fn contains_path(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let path_v = args.eval(ctx, env, "contains_path", pos)?;
    let path = cast::path(&path_v, &pos.key("contains_path"))?;
    let container = args.eval(ctx, env, "in", pos)?;
    Ok(Value::Bool(value_at_path(&container, &path).is_some()))
}

fn contains_path_legacy(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let path_v = args.eval(ctx, env, "contains", pos)?;
    let path = cast::path(&path_v, &pos.key("contains"))?;
    let container = args.eval(ctx, env, "in", pos)?;
    Ok(Value::Bool(value_at_path(&container, &path).is_some()))
}

fn contains_field(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let f = args.eval(ctx, env, "contains_field", pos)?;
    let field = cast::string(&f, &pos.key("contains_field"))?;
    let container = args.eval(ctx, env, "in", pos)?;
    if let Value::Array(_) = container {
        return one(FaunaError::invalid_argument(&[Type::Object], Type::Array, &pos.key("in")));
    }
    Ok(Value::Bool(value_at_path(&container, &[PathElem::Key(field.to_string())]).is_some()))
}

fn contains_value(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let value = args.eval(ctx, env, "contains_value", pos)?;
    let container = args.eval(ctx, env, "in", pos)?;
    match &container {
        Value::Object(o) => Ok(Value::Bool(o.values().any(|v| v.fauna_equals(&value)))),
        Value::Array(a) | Value::Page { data: a, .. } => Ok(Value::Bool(a.iter().any(|v| v.fauna_equals(&value)))),
        Value::Ref(r) => {
            // A ref "contains" its id string and its collection ref.
            let coll = r.collection.as_deref().map(|c| Value::Ref(c.clone()));
            Ok(Value::Bool(
                Value::String(r.id.clone()).fauna_equals(&value) || coll.map(|c| c.fauna_equals(&value)).unwrap_or(false),
            ))
        }
        Value::Set(s) => {
            let elems = super::sets::materialize(ctx, s, &pos.key("in"))?;
            Ok(Value::Bool(elems.iter().any(|v| v.fauna_equals(&value))))
        }
        other => one(FaunaError::invalid_argument(
            &[Type::Object, Type::Array, Type::Page, Type::Set, Type::Ref],
            if other.is_scalar() { Type::Scalar } else { other.rtype() },
            &pos.key("in"),
        )),
    }
}

/// `Merge(left, right, lambda?)` (ObjectFunctions.scala). The default
/// merge lambda keeps the right value; returning `null` from a custom
/// lambda removes the key.
fn merge(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let left = args.eval(ctx, env, "merge", pos)?;
    let right = args.eval(ctx, env, "with", pos)?;
    let lam = match args.get("lambda") {
        Some(j) => Some(eval_lambda_arg(ctx, env, j, &pos.key("lambda"))?),
        None => None,
    };
    let mut errs = Vec::new();
    let left_obj = match &left {
        Value::Object(o) => o.clone(),
        other => {
            errs.push(FaunaError::invalid_argument(&[Type::Object], other.rtype(), &pos.key("merge")));
            Object::new()
        }
    };
    let mut rights: Vec<Object> = Vec::new();
    for (v, p) in cast::elems(&right, &pos.key("with")) {
        match v {
            Value::Object(o) => rights.push(o),
            other => errs.push(FaunaError::invalid_argument(&[Type::Object], other.rtype(), &p)),
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let mut out = left_obj;
    for r in rights {
        for (k, rv) in r {
            match &lam {
                None => {
                    out.insert(k, rv);
                }
                Some(l) => {
                    let lv = out.get(&k).cloned().unwrap_or(Value::Null);
                    let res = apply_lambda(ctx, l, Value::Array(vec![Value::String(k.clone()), lv, rv]), pos)?;
                    match res {
                        Value::Null => {
                            out.shift_remove(&k);
                        }
                        v => {
                            out.insert(k, v);
                        }
                    }
                }
            }
        }
    }
    Ok(Value::Object(out))
}

fn to_object(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_object", pos)?;
    let p = pos.key("to_object");
    match &v {
        Value::Array(a) | Value::Page { data: a, .. } => {
            let mut out = Object::new();
            for (i, item) in a.iter().enumerate() {
                match item {
                    Value::Array(pair) if pair.len() == 2 => match &pair[0] {
                        Value::String(k) => {
                            out.insert(k.clone(), pair[1].clone());
                        }
                        other => return one(FaunaError::invalid_argument(&[Type::Field], other.rtype(), &p.idx(i))),
                    },
                    other => return one(FaunaError::invalid_argument(&[Type::Field], other.rtype(), &p.idx(i))),
                }
            }
            Ok(Value::Object(out))
        }
        Value::Object(_) => Ok(v),
        other => one(FaunaError::invalid_cast(other.rtype(), Type::Object, &p)),
    }
}

fn to_array(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "to_array", pos)?;
    let p = pos.key("to_array");
    match v {
        Value::Object(o) => Ok(Value::Array(
            o.into_iter().map(|(k, v)| Value::Array(vec![Value::String(k), v])).collect(),
        )),
        Value::Array(_) => Ok(v),
        Value::Page { data, .. } => Ok(Value::Array(data)),
        Value::Set(s) => Ok(Value::Array(super::sets::materialize(ctx, &s, &p)?)),
        other => one(FaunaError::invalid_cast(other.rtype(), Type::Array, &p)),
    }
}

// ---- refs ----

fn scope_not_emulated(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<()> {
    if let Some(v) = args.eval_opt(ctx, env, "scope", pos)? {
        if v != Value::Null {
            return one(FaunaError::not_emulated("scoped refs (child databases)", &pos.key("scope")));
        }
    }
    Ok(())
}

fn named_ref(
    key: &str,
    ctor: fn(&str) -> Ref,
    ctx: &mut Ctx,
    env: &Env,
    args: &Args,
    pos: &Position,
) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let name = cast::string(&v, &pos.key(key))?;
    scope_not_emulated(ctx, env, args, pos)?;
    Ok(Value::Ref(ctor(name)))
}

fn database(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    named_ref("database", Ref::database, ctx, env, args, pos)
}

fn index(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    named_ref("index", Ref::index, ctx, env, args, pos)
}

fn collection(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    named_ref("collection", Ref::collection, ctx, env, args, pos)
}

fn collection_legacy(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    named_ref("class", Ref::collection, ctx, env, args, pos)
}

fn function(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    named_ref("function", Ref::function, ctx, env, args, pos)
}

fn role(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    named_ref("role", Ref::role, ctx, env, args, pos)
}

fn native(key: &str, name: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    if v != Value::Null {
        return one(FaunaError::not_emulated("scoped refs (child databases)", &pos.key(key)));
    }
    Ok(Value::Ref(Ref::native(name)))
}

/// `Ref(Collection("x"), "123")` (RefFunction in BasicFunctions.scala).
fn ref_(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let cls = args.eval(ctx, env, "ref", pos)?;
    let id = args.eval(ctx, env, "id", pos)?;
    scope_not_emulated(ctx, env, args, pos)?;
    let coll = match &cls {
        Value::Ref(r) if r.is_collection_ref() => r.clone(),
        Value::Ref(r) if r.is_native() => return one(FaunaError::invalid_schema_class_argument(&pos.key("ref"))),
        other => return one(FaunaError::invalid_argument(&[Type::CollectionRef], other.rtype(), &pos.key("ref"))),
    };
    let id_str = match &id {
        Value::Long(l) => l.to_string(),
        Value::String(s) => match s.parse::<u64>() {
            Ok(_) => s.clone(),
            Err(_) => return one(FaunaError::non_numeric_sub_id(s, &pos.key("id"))),
        },
        other => return one(FaunaError::invalid_argument(&[Type::String, Type::Integer], other.rtype(), &pos.key("id"))),
    };
    Ok(Value::Ref(Ref::child(coll, &id_str)))
}

fn new_id(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let key = if args.get("new_id").is_some() { "new_id" } else { "next_id" };
    let v = args.eval(ctx, env, key, pos)?;
    cast::null(&v, &pos.key(key))?;
    Ok(Value::String(ctx.db.new_id()))
}

// ---- type checks ----

fn is_type(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let b = match key {
        "is_number" => matches!(v, Value::Long(_) | Value::Double(_)),
        "is_double" => matches!(v, Value::Double(_)),
        "is_integer" => matches!(v, Value::Long(_)),
        "is_boolean" => matches!(v, Value::Bool(_)),
        "is_null" => matches!(v, Value::Null),
        "is_bytes" => matches!(v, Value::Bytes(_)),
        "is_timestamp" => matches!(v, Value::Time(_)),
        "is_date" => matches!(v, Value::Date(_)),
        "is_uuid" => false,
        "is_string" => matches!(v, Value::String(_)),
        "is_array" => matches!(v, Value::Array(_)),
        "is_object" => matches!(v, Value::Object(_)),
        "is_ref" => matches!(v, Value::Ref(_)),
        "is_set" => matches!(v, Value::Set(_)),
        "is_doc" => match &v {
            Value::Ref(r) => r.is_document_ref() && ctx.db.exists_by_ref(r),
            Value::Object(o) => matches!(o.get("ref"), Some(Value::Ref(r)) if r.is_document_ref()) && o.contains_key("ts"),
            _ => false,
        },
        "is_lambda" => matches!(v, Value::Lambda(_)),
        "is_collection" => match &v {
            Value::Ref(r) => r.is_collection_ref(),
            Value::Object(o) => matches!(o.get("ref"), Some(Value::Ref(r)) if r.is_collection_ref()),
            _ => false,
        },
        "is_database" => match &v {
            Value::Ref(r) => r.is_database_ref(),
            Value::Object(o) => matches!(o.get("ref"), Some(Value::Ref(r)) if r.is_database_ref()),
            _ => false,
        },
        "is_index" => match &v {
            Value::Ref(r) => r.is_index_ref(),
            Value::Object(o) => matches!(o.get("ref"), Some(Value::Ref(r)) if r.is_index_ref()),
            _ => false,
        },
        "is_function" => match &v {
            Value::Ref(r) => r.is_function_ref(),
            Value::Object(o) => matches!(o.get("ref"), Some(Value::Ref(r)) if r.is_function_ref()),
            _ => false,
        },
        "is_role" => match &v {
            Value::Ref(r) => r.is_role_ref(),
            Value::Object(o) => matches!(o.get("ref"), Some(Value::Ref(r)) if r.is_role_ref()),
            _ => false,
        },
        "is_key" | "is_token" | "is_credentials" => false,
        _ => false,
    };
    Ok(Value::Bool(b))
}
