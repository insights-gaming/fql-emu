//! The FQL v4 interpreter.
//!
//! Queries arrive as the JSON the `faunadb` driver would have POSTed. We
//! interpret that JSON directly (there is no separate AST): scalars and
//! arrays are literals, objects are either `@`-escapes, registered forms
//! (`{"let": ..., "in": ...}`), or errors.

pub mod basic;
pub mod cast;
pub mod collection;
pub mod forms;
pub mod logic;
pub mod numeric;
pub mod read;
pub mod sets;
pub mod string;
pub mod time;
pub mod write;

pub use crate::env::Env;
use crate::error::{one, FaunaError, Position, R};
use crate::store::Database;
use crate::value::{Lambda, Pattern, Ref, Timestamp, Type, Value};
use crate::wire;
use serde_json::Value as Json;

/// `EvalContext.MaxStackDepth`.
pub const MAX_STACK_DEPTH: usize = 200;

/// Per-transaction evaluation context. `db` is the working copy that is
/// committed only if the whole query succeeds.
pub struct Ctx<'a> {
    pub db: &'a mut Database,
    /// Transaction time: `Now()` and every `ts` written in this query.
    pub ts: Timestamp,
    pub depth: usize,
}

impl<'a> Ctx<'a> {
    pub fn new(db: &'a mut Database, ts: Timestamp) -> Ctx<'a> {
        Ctx { db, ts, depth: 0 }
    }

    pub fn identity(&self) -> Option<&Ref> {
        self.db.identity.as_ref()
    }
}

/// Evaluate an expression.
pub fn eval(ctx: &mut Ctx, env: &Env, expr: &Json, pos: &Position) -> R<Value> {
    match expr {
        Json::Null => Ok(Value::Null),
        Json::Bool(b) => Ok(Value::Bool(*b)),
        Json::Number(n) => wire::number(n, pos),
        Json::String(s) => Ok(Value::String(s.clone())),
        Json::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut errs = Vec::new();
            for (i, it) in items.iter().enumerate() {
                match eval(ctx, env, it, &pos.idx(i)) {
                    Ok(v) => out.push(v),
                    Err(e) => errs.extend(e),
                }
            }
            if errs.is_empty() {
                Ok(Value::Array(out))
            } else {
                Err(errs)
            }
        }
        Json::Object(o) => {
            if let Some(r) = forms::dispatch(ctx, env, o, pos) {
                return r;
            }
            if let Some(r) = wire::decode_escape(o, pos) {
                return r;
            }
            let keys: Vec<&str> = o.keys().map(|k| k.as_str()).collect();
            one(FaunaError::invalid_form(&keys, pos))
        }
    }
}

/// Bind a lambda pattern against an argument value.
fn bind_pattern(env: &Env, pat: &Pattern, arg: &Value, pos: &Position) -> R<Env> {
    match pat {
        Pattern::Ignore => Ok(env.clone()),
        Pattern::Name(n) => Ok(env.bind(n, arg.clone())),
        Pattern::Array(ps) => {
            let items = match arg {
                Value::Array(a) => a,
                Value::Page { data, .. } => data,
                other => return one(FaunaError::invalid_argument(&[Type::Array], other.rtype(), pos)),
            };
            if items.len() != ps.len() {
                return one(FaunaError::invalid_lambda_arity(ps.len(), items.len(), pos));
            }
            let mut e = env.clone();
            for (p, v) in ps.iter().zip(items) {
                e = bind_pattern(&e, p, v, pos)?;
            }
            Ok(e)
        }
    }
}

/// Apply a lambda to a single argument value (`ec.evalLambdaApply`).
/// Multi-parameter lambdas take an array.
pub fn apply_lambda(ctx: &mut Ctx, lam: &Lambda, arg: Value, pos: &Position) -> R<Value> {
    if ctx.depth >= MAX_STACK_DEPTH {
        return one(FaunaError::stack_overflow(pos));
    }
    let env = bind_pattern(&lam.env, &lam.params, &arg, pos)?;
    ctx.depth += 1;
    let r = eval(ctx, &env, &lam.body, pos);
    ctx.depth -= 1;
    r
}

/// Parse a `{"lambda": ..., "expr": ...}` expression into a closure over
/// `env` (`QueryParser.parseLambdaExpr`).
pub fn parse_lambda_expr(env: &Env, expr: &Json, pos: &Position) -> R<Lambda> {
    let o = match expr {
        Json::Object(o) => o,
        other => {
            let t = match other {
                Json::Null => Type::Null,
                Json::Bool(_) => Type::Boolean,
                Json::Number(_) => Type::Number,
                Json::String(_) => Type::String,
                Json::Array(_) => Type::Array,
                Json::Object(_) => Type::Object,
            };
            return one(FaunaError::invalid_expr_type(&[Type::Lambda], t, pos));
        }
    };
    if let Some(r) = wire::decode_escape(o, pos) {
        return match r? {
            Value::Lambda(l) => Ok(l),
            other => one(FaunaError::invalid_expr_type(&[Type::Lambda], other.rtype(), pos)),
        };
    }
    let (params, body) = match (o.get("lambda"), o.get("expr")) {
        (Some(p), Some(b)) if o.len() == 2 => (p, b),
        _ => {
            let keys: Vec<&str> = o.keys().map(|k| k.as_str()).collect();
            return one(FaunaError::invalid_form(&keys, pos));
        }
    };
    let pattern = parse_pattern(params, &pos.key("lambda"))?;
    Ok(Lambda { params: pattern, body: std::rc::Rc::new(body.clone()), env: env.clone() })
}

fn parse_pattern(v: &Json, pos: &Position) -> R<Pattern> {
    match v {
        Json::String(s) if s == "_" => Ok(Pattern::Ignore),
        Json::String(s) => Ok(Pattern::Name(s.clone())),
        Json::Array(items) => {
            let mut ps = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Json::String(s) if s == "_" => ps.push(Pattern::Ignore),
                    Json::String(s) => ps.push(Pattern::Name(s.clone())),
                    Json::Array(_) => ps.push(parse_pattern(it, &pos.idx(i))?),
                    other => {
                        let t = json_type(other);
                        return one(FaunaError::invalid_expr_type(&[Type::String], t, &pos.idx(i)));
                    }
                }
            }
            Ok(Pattern::Array(ps))
        }
        other => one(FaunaError::invalid_expr_type(&[Type::String, Type::Array], json_type(other), pos)),
    }
}

pub fn json_type(j: &Json) -> Type {
    match j {
        Json::Null => Type::Null,
        Json::Bool(_) => Type::Boolean,
        Json::Number(n) => {
            if n.is_f64() {
                Type::Double
            } else {
                Type::Integer
            }
        }
        Json::String(_) => Type::String,
        Json::Array(_) => Type::Array,
        Json::Object(_) => Type::Object,
    }
}

/// Evaluate a lambda-valued argument: either an inline
/// `{"lambda": ..}` form, or an expression that evaluates to a lambda
/// value (e.g. `Var("f")` or a `@query`).
pub fn eval_lambda_arg(ctx: &mut Ctx, env: &Env, expr: &Json, pos: &Position) -> R<Lambda> {
    if let Json::Object(o) = expr {
        if o.contains_key("lambda") && o.contains_key("expr") {
            return parse_lambda_expr(env, expr, pos);
        }
    }
    match eval(ctx, env, expr, pos)? {
        Value::Lambda(l) => Ok(l),
        other => one(FaunaError::invalid_argument(&[Type::Lambda], other.rtype(), pos)),
    }
}

/// Resolve a user function by name or ref.
pub fn resolve_function<'c>(ctx: &'c Ctx, v: &Value, pos: &Position) -> R<&'c crate::store::FunctionDef> {
    let name = match v {
        Value::String(s) => s.as_str(),
        Value::Ref(r) if r.is_function_ref() => r.id.as_str(),
        other => return one(FaunaError::invalid_argument(&[Type::UserFunctionRef], other.rtype(), pos)),
    };
    match ctx.db.functions.get(name) {
        Some(f) => Ok(f),
        None => one(FaunaError::unresolved_ref("function", name, pos)),
    }
}

/// `Call(fn, args)`: errors inside the body are wrapped in a `call error`.
pub fn call_function(ctx: &mut Ctx, f: &Value, args: Value, pos: &Position) -> R<Value> {
    let def = resolve_function(ctx, f, &pos.key("call"))?;
    let body = def.body.clone();
    match apply_lambda(ctx, &body, args, pos) {
        Ok(v) => Ok(v),
        Err(errs) => {
            if errs.iter().any(|e| e.code == "stack overflow") {
                Err(errs)
            } else {
                one(FaunaError::call_error(errs, pos))
            }
        }
    }
}
