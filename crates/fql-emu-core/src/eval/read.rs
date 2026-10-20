//! Read functions: `Get`, `Exists`, `Paginate`, identity, keys.
//!
//! Spec: ext/model/src/main/scala/ast/functions/ReadFunctions.scala,
//! PaginateFunction.scala, ReadAdaptor.scala (`paginate`, default page
//! size 64, max 100000), AuthFunctions.scala.

use super::cast;
use super::forms::{Args, Registry};
use super::{Ctx, Env};
use super::sets::PageCursor;
use crate::error::{one, Errors, FaunaError, Position, R};
use crate::value::{SetExpr, Type, Value};

pub fn register(r: &mut Registry) {
    r.add("get", &["get"], &["ts"], get);
    r.add("exists", &["exists"], &["ts"], exists);
    r.add("paginate", &["paginate"], &["after", "before", "cursor", "ts", "size", "events", "sources"], paginate);
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
    let p = pos.key("get");
    match &v {
        Value::Ref(r) => {
            if let Some(coll) = r.document_collection() {
                if !ctx.db.collections.contains_key(coll) {
                    return one(FaunaError::unresolved_ref("collection", coll, &p));
                }
            }
            match ctx.db.get_by_ref(r) {
                Some(doc) => Ok(doc),
                None => match native_kind(r) {
                    Some(kind) => one(FaunaError::native_not_found(kind, &p)),
                    None => one(FaunaError::document_not_found(&p)),
                },
            }
        }
        Value::Set(s) => {
            // Get(set) returns the document of the set's first entry
            // (`EventSet.snapshotHead`), whatever values the set covers.
            let entries = super::sets::materialize_entries(ctx, s, &p)?;
            match entries.first().and_then(|e| e.key.last()) {
                Some(Value::Ref(r)) => match ctx.db.get_by_ref(r) {
                    Some(doc) => Ok(doc),
                    None => one(FaunaError::document_not_found(&p)),
                },
                _ => one(FaunaError::set_not_found(&p)),
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

/// `Paginate(set_or_ref, after | before | cursor, ts, size, events,
/// sources)` (PaginateFunction.scala). The three cursor keys are separate
/// forms on the server, so at most one may be given. Argument errors are
/// collected like the server's parallel argument evaluation.
fn paginate(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let cursor_keys: Vec<&str> = ["after", "before", "cursor"].into_iter().filter(|k| args.get(k).is_some()).collect();
    if cursor_keys.len() > 1 {
        let keys: Vec<&str> = args.obj.keys().map(|k| k.as_str()).collect();
        return one(FaunaError::invalid_form(&keys, pos));
    }
    let mut errs: Errors = Vec::new();
    let mut take = |r: R<Option<Value>>| match r {
        Ok(v) => v,
        Err(e) => {
            errs.extend(e);
            None
        }
    };
    let target = take(args.eval(ctx, env, "paginate", pos).map(Some));
    let cursor = match cursor_keys.first() {
        Some(k) => take(args.eval_opt(ctx, env, k, pos)),
        None => None,
    };
    let ts = take(args.eval_opt(ctx, env, "ts", pos));
    let size = take(args.eval_opt(ctx, env, "size", pos));
    let events = take(args.eval_opt(ctx, env, "events", pos));
    let sources = take(args.eval_opt(ctx, env, "sources", pos));
    if !errs.is_empty() {
        return Err(errs);
    }

    let p = pos.key("paginate");
    let mut errs: Errors = Vec::new();
    let set = match &target {
        Some(v) => match super::sets::identifier(ctx, v, &p) {
            Ok(Some(s)) => Some(s),
            // Any other ref paginates the document itself (`DocSet`).
            Ok(None) => match v {
                Value::Ref(r) => Some(SetExpr::Singleton(r.clone())),
                _ => None,
            },
            Err(e) => {
                errs.extend(e);
                None
            }
        },
        None => None,
    };
    if let Some(t) = &ts {
        // `Casts.TransactionTimestamp`: a Time, Now(), or a Long.
        match cast::timestamp(t, &pos.key("ts")) {
            Ok(t) if t == ctx.ts => {}
            Ok(_) => errs.push(FaunaError::not_emulated("temporal reads (`ts` other than Now())", &pos.key("ts"))),
            Err(e) => errs.extend(e),
        }
    }
    let size = match &size {
        Some(v) => match cast::long(v, &pos.key("size")) {
            Ok(l) => Some(l),
            Err(e) => {
                errs.extend(e);
                None
            }
        },
        None => None,
    };
    for (key, v) in [("events", &events), ("sources", &sources)] {
        if let Some(v) = v {
            match cast::boolean(v, &pos.key(key)) {
                Ok(true) if key == "events" => errs
                    .push(FaunaError::not_emulated("`Paginate` with `events: true` (set history)", &pos.key(key))),
                Ok(true) => errs.push(FaunaError::not_emulated("`Paginate` with `sources: true`", &pos.key(key))),
                Ok(false) => {}
                Err(e) => errs.extend(e),
            }
        }
    }
    let set = match set {
        Some(s) if errs.is_empty() => s,
        _ => return Err(errs),
    };

    let cursor = match (cursor_keys.first().copied(), cursor) {
        (Some("after"), Some(v)) => PageCursor::After(v, pos.key("after")),
        (Some("before"), Some(v)) => PageCursor::Before(v, pos.key("before")),
        (Some("cursor"), Some(v)) => {
            // `CursorParser.parseCursorObj`: `null`, `{after}` or `{before}`.
            let cp = pos.key("cursor");
            match v {
                Value::Null => PageCursor::None,
                Value::Object(o) if o.len() == 1 && o.contains_key("after") => {
                    PageCursor::After(o["after"].clone(), cp.key("after"))
                }
                Value::Object(o) if o.len() == 1 && o.contains_key("before") => {
                    PageCursor::Before(o["before"].clone(), cp.key("before"))
                }
                _ => return one(FaunaError::invalid_cursor_object(&cp)),
            }
        }
        _ => PageCursor::None,
    };
    super::sets::paginate(ctx, &set, cursor, size, &p)
}
