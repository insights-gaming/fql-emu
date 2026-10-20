//! Read functions: `Get`, `Exists`, `Paginate`, identity, keys.
//!
//! Spec: ext/model/src/main/scala/ast/functions/ReadFunctions.scala,
//! PaginateFunction.scala, ReadAdaptor.scala (`paginate`, default page
//! size 64, max 100000), AuthFunctions.scala.

use super::cast;
use super::forms::{Args, Registry};
use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::value::{Type, Value};

pub fn register(r: &mut Registry) {
    r.add("get", &["get"], &["ts"], get);
    r.add("exists", &["exists"], &["ts"], exists);
    r.stub("paginate", &["paginate"], &["after", "before", "cursor", "ts", "size", "events", "sources"]);
    r.stub("key_from_secret", &["key_from_secret"], &[]);
    r.add("current_identity", &["current_identity"], &[], current_identity);
    r.add("identity", &["identity"], &[], current_identity);
    r.add("has_current_identity", &["has_current_identity"], &[], has_current_identity);
    r.add("has_identity", &["has_identity"], &[], has_current_identity);
    r.stub("current_token", &["current_token"], &[]);
    r.stub("has_current_token", &["has_current_token"], &[]);
    r.stub("identify", &["identify", "password"], &[]);
    r.stub("login", &["login", "params"], &[]);
    r.stub("logout", &["logout"], &[]);
    r.stub("issue_access_jwt", &["issue_access_jwt"], &["expire_in"]);
}

fn ts_not_emulated(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<()> {
    if let Some(v) = args.eval_opt(ctx, env, "ts", pos)? {
        let t = cast::timestamp(&v, &pos.key("ts"))?;
        if t != ctx.ts {
            return one(FaunaError::not_emulated("temporal reads (`ts` other than Now())", &pos.key("ts")));
        }
    }
    Ok(())
}

/// Classify a ref for error messages: which native kind it refers to.
fn native_kind(r: &crate::value::Ref) -> Option<&'static str> {
    if r.is_collection_ref() {
        Some("Collection")
    } else if r.is_index_ref() {
        Some("Index")
    } else if r.is_function_ref() {
        Some("Function")
    } else if r.is_role_ref() {
        Some("Role")
    } else if r.is_database_ref() {
        Some("Database")
    } else {
        None
    }
}

fn get(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "get", pos)?;
    ts_not_emulated(ctx, env, args, pos)?;
    // `Casts.Identifier` fails at `["get"]` (unresolved schema refs become
    // `invalid ref` there); `GetFunction` itself reports not-found at the
    // form position.
    let p = pos.key("get");
    match &v {
        Value::Ref(r) => {
            if let Some(e) = super::collection::unresolved_ref(ctx, &v, &p) {
                return one(e);
            }
            match ctx.db.get_by_ref(r) {
                Some(doc) => Ok(doc),
                None => match native_kind(r) {
                    Some(kind) => one(FaunaError::native_not_found(kind, pos)),
                    None => one(FaunaError::document_not_found(pos)),
                },
            }
        }
        Value::Set(s) => {
            // Get(set) returns the first element's document.
            let elems = super::sets::materialize(ctx, s, &p)?;
            match elems.first() {
                Some(Value::Ref(r)) => match ctx.db.get_by_ref(r) {
                    Some(doc) => Ok(doc),
                    None => one(FaunaError::document_not_found(pos)),
                },
                Some(Value::Array(items)) => match items.last() {
                    Some(Value::Ref(r)) => match ctx.db.get_by_ref(r) {
                        Some(doc) => Ok(doc),
                        None => one(FaunaError::document_not_found(pos)),
                    },
                    _ => one(FaunaError::set_not_found(pos)),
                },
                _ => one(FaunaError::set_not_found(pos)),
            }
        }
        other => one(FaunaError::invalid_argument(&[Type::Ref, Type::Set], other.rtype(), &p)),
    }
}

fn exists(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "exists", pos)?;
    ts_not_emulated(ctx, env, args, pos)?;
    let p = pos.key("exists");
    match &v {
        Value::Ref(r) => Ok(Value::Bool(ctx.db.exists_by_ref(r))),
        Value::Set(s) => Ok(Value::Bool(!super::sets::materialize(ctx, s, &p)?.is_empty())),
        other => one(FaunaError::invalid_argument(&[Type::Ref, Type::Set], other.rtype(), &p)),
    }
}

fn current_identity(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let key = if args.get("current_identity").is_some() { "current_identity" } else { "identity" };
    let v = args.eval(ctx, env, key, pos)?;
    cast::null(&v, &pos.key(key))?;
    match ctx.identity() {
        Some(r) => Ok(Value::Ref(r.clone())),
        None => one(FaunaError::missing_identity(pos)),
    }
}

fn has_current_identity(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let key = if args.get("has_current_identity").is_some() { "has_current_identity" } else { "has_identity" };
    let v = args.eval(ctx, env, key, pos)?;
    cast::null(&v, &pos.key(key))?;
    Ok(Value::Bool(ctx.identity().is_some()))
}
