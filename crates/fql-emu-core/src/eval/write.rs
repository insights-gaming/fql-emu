//! Write functions: `Create`, `Update`, `Replace`, `Delete`,
//! `CreateCollection`, `CreateIndex`, `CreateFunction`, `CreateRole`, and
//! the deliberately unsupported ones (`Insert`, `Remove`, `CreateKey`,
//! `CreateDatabase`, `Login`, ...).
//!
//! Spec: ext/model/src/main/scala/ast/functions/WriteFunctions.scala and
//! WriteAdaptor.scala, ext/storage/src/main/scala/doc/Data.scala (`patch`)
//! and Validator.scala, ext/model/src/main/scala/doc/Document.scala.
//!
//! A user document stores only `data`, `constraints` and `ttl` (the
//! validator's filter mask); other top-level params are silently dropped
//! and `credentials` is split off (the emulator does not keep it: login is
//! not emulated). Writes apply `Data.patch`: objects merge recursively, a
//! `null` deletes a field, and nulls inside written arrays/objects are
//! dropped. `Update` patches the current fields, `Replace` patches an empty
//! document. Unique indexes are checked by brute force against every other
//! document in the index's source collections.

use super::cast;
use super::collection::unresolved_ref;
use super::forms::{Args, Registry};
use super::sets;
use super::{Ctx, Env};
use crate::error::{one, FaunaError, PosElem, Position, ValidationFailure, R};
use crate::store::{ir_type_name, Document, IndexDef, SourceCollection};
use crate::value::{Object, Ref, Timestamp, Type, Value};

pub fn register(r: &mut Registry) {
    r.add("create", &["create"], &["params"], create);
    r.add("update", &["update"], &["params"], |c, e, a, p| update_or_replace(true, c, e, a, p));
    r.add("replace", &["replace"], &["params"], |c, e, a, p| update_or_replace(false, c, e, a, p));
    r.add("delete", &["delete"], &[], delete);
    r.add("create_collection", &["create_collection"], &[], create_collection);
    r.add("create_class", &["create_class"], &[], create_collection);
    r.add("create_index", &["create_index"], &[], create_index);
    r.add("create_function", &["create_function"], &[], create_function);
    r.add("create_role", &["create_role"], &[], create_role);
    r.stub("create_database", &["create_database"], &[]);
    r.stub("create_key", &["create_key"], &[]);
    r.stub("create_access_provider", &["create_access_provider"], &[]);
    r.stub("insert", &["insert", "ts", "action"], &["params"]);
    r.stub("remove", &["remove", "ts", "action"], &[]);
    r.stub("move_database", &["move_database", "to"], &[]);
}

fn create_collection(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    ctx.wrote = true;
    let key = if args.get("create_collection").is_some() { "create_collection" } else { "create_class" };
    let v = args.eval(ctx, env, key, pos)?;
    let params = cast::object(&v, &pos.key(key))?;
    let ts = ctx.ts;
    ctx.db.create_collection(params, ts, &pos.key(key))
}

fn create_index(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    ctx.wrote = true;
    let v = args.eval(ctx, env, "create_index", pos)?;
    let params = cast::object(&v, &pos.key("create_index"))?;
    let ts = ctx.ts;
    ctx.db.create_index(params, ts, &pos.key("create_index"))
}

fn create_function(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    ctx.wrote = true;
    let v = args.eval(ctx, env, "create_function", pos)?;
    let params = cast::object(&v, &pos.key("create_function"))?;
    let ts = ctx.ts;
    ctx.db.create_function(params, ts, &pos.key("create_function"))
}

fn create_role(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    ctx.wrote = true;
    let v = args.eval(ctx, env, "create_role", pos)?;
    let params = cast::object(&v, &pos.key("create_role"))?;
    let ts = ctx.ts;
    ctx.db.create_role(params, ts, &pos.key("create_role"))
}

// ---- argument handling ----

/// Evaluate the optional `params` argument (`Casts.Object`); absent means
/// an empty object.
fn eval_params(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Result<Object, FaunaError>> {
    Ok(match args.eval_opt(ctx, env, "params", pos)? {
        None => Ok(Object::new()),
        Some(Value::Object(o)) => Ok(o),
        Some(other) => Err(FaunaError::invalid_argument(&[Type::Object], other.rtype(), &pos.key("params"))),
    })
}

fn pos_path(pos: &Position) -> Vec<String> {
    pos.0
        .iter()
        .map(|e| match e {
            PosElem::Index(i) => i.to_string(),
            PosElem::Key(k) => k.clone(),
        })
        .collect()
}

/// `RefValidator.validate`: refs the server would have left unresolved
/// (schema refs or document refs into missing collections) anywhere in
/// the params fail validation, with the field path rooted at the form
/// position.
fn validate_refs(ctx: &Ctx, params: &Object, pos: &Position) -> R<()> {
    fn walk(ctx: &Ctx, v: &Value, path: &mut Vec<String>, out: &mut Vec<ValidationFailure>) {
        match v {
            Value::Ref(_) if unresolved_ref(ctx, v, &Position::root()).is_some() => {
                out.push(ValidationFailure {
                    field: path.clone(),
                    code: "invalid reference".into(),
                    description: "Cannot read reference.".into(),
                });
            }
            Value::Object(o) => {
                for (k, x) in o {
                    path.push(k.clone());
                    walk(ctx, x, path, out);
                    path.pop();
                }
            }
            Value::Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    path.push(i.to_string());
                    walk(ctx, x, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut failures = Vec::new();
    let mut path = pos_path(pos);
    walk(ctx, &Value::Object(params.clone()), &mut path, &mut failures);
    if failures.is_empty() {
        Ok(())
    } else {
        one(FaunaError::validation_failed(failures, pos))
    }
}

// ---- data patching (Data.patch) ----

/// `patch0`: the value as stored. Nulls are dropped everywhere; a page is
/// stored as its object form.
fn stored(v: &Value) -> Option<Value> {
    match v {
        Value::Null => None,
        Value::Array(items) => Some(Value::Array(items.iter().filter_map(stored).collect())),
        Value::Object(o) => Some(Value::Object(
            o.iter().filter_map(|(k, x)| stored(x).map(|s| (k.clone(), s))).collect(),
        )),
        Value::Page { data, before, after } => {
            let mut o = Object::new();
            if let Some(b) = before {
                o.insert("before".into(), (**b).clone());
            }
            if let Some(a) = after {
                o.insert("after".into(), (**a).clone());
            }
            o.insert("data".into(), Value::Array(data.clone()));
            stored(&Value::Object(o))
        }
        other => Some(other.clone()),
    }
}

fn contains_set(v: &Value) -> bool {
    match v {
        Value::Set(_) => true,
        Value::Array(a) => a.iter().any(contains_set),
        Value::Object(o) => o.values().any(contains_set),
        Value::Page { data, before, after } => {
            data.iter().any(contains_set)
                || before.as_deref().is_some_and(contains_set)
                || after.as_deref().is_some_and(contains_set)
        }
        _ => false,
    }
}

/// `Data.patch`: objects merge recursively, anything else replaces, and
/// `null` removes.
pub fn patch(prev: &Object, diff: &Object) -> Object {
    let mut out = prev.clone();
    for (k, r) in diff {
        let merged = match (out.get(k), r) {
            (Some(Value::Object(l)), Value::Object(rr)) => Some(Value::Object(patch(l, rr))),
            (_, r) => stored(r),
        };
        match merged {
            Some(v) => {
                out.insert(k.clone(), v);
            }
            None => {
                out.shift_remove(k);
            }
        }
    }
    out
}

fn invalid_type(field: &str, expected: &str, actual: &Value) -> ValidationFailure {
    ValidationFailure {
        field: vec![field.into()],
        code: "invalid type".into(),
        description: format!("Invalid type {}, expected type {}.", ir_type_name(actual), expected),
    }
}

// ---- user documents ----

/// The fields of a user document after a write: `Document.VersionValidator`
/// (`data`, `constraints`, `ttl`) applied to `prev` patched with `params`.
/// Validation errors are reported at `vpos`.
fn document_fields(ctx: &Ctx, coll: &str, prev: &Object, params: &Object, vpos: &Position) -> R<Object> {
    if params.values().any(contains_set) {
        return one(FaunaError::not_emulated("storing a set value in a document", vpos));
    }
    match params.get("credentials") {
        None | Some(Value::Null) | Some(Value::Object(_)) => {}
        Some(_) => return one(FaunaError::not_emulated("non-object `credentials`", vpos)),
    }
    let mut diff = Object::new();
    for key in ["data", "constraints", "ttl"] {
        if let Some(v) = params.get(key) {
            diff.insert(key.to_string(), v.clone());
        }
    }
    let mut fields = patch(prev, &diff);
    // Keep only the validator's fields, in a stable order.
    let mut failures = Vec::new();
    for key in ["data", "constraints"] {
        if let Some(v) = fields.get(key) {
            if !matches!(v, Value::Object(_)) {
                failures.push(invalid_type(key, "Map", v));
            }
        }
    }
    if let Some(v) = fields.get("ttl") {
        if !matches!(v, Value::Time(_)) {
            failures.push(invalid_type("ttl", "Timestamp", v));
        }
    }
    if !failures.is_empty() {
        return one(FaunaError::validation_failed(failures, vpos));
    }
    fields.retain(|k, _| matches!(k.as_str(), "data" | "constraints" | "ttl"));
    // `UserDocumentWriteConfig.adjustTTL`: the collection TTL applies when
    // the document has none.
    if !fields.contains_key("ttl") {
        if let Some(days) = ctx.db.collections.get(coll).and_then(|c| c.ttl_days) {
            let ttl = Timestamp::of_micros(ctx.ts.micros().saturating_add(days.saturating_mul(86_400_000_000)));
            fields.insert("ttl".into(), Value::Time(ttl));
        }
    }
    Ok(fields)
}

fn index_covers(idx: &IndexDef, coll: &str) -> bool {
    idx.source.iter().any(|s| match &s.collection {
        SourceCollection::Collection(c) => c == coll,
        SourceCollection::Wildcard => true,
    })
}

/// Unique constraints (`IndexStore.add` with `UniqueValues`): every index
/// entry the write adds must not equal (terms and values) an entry of
/// another document in the index's sources.
fn check_unique(
    ctx: &mut Ctx,
    coll: &str,
    id: &str,
    old: Option<&Document>,
    new: &Document,
    pos: &Position,
) -> R<()> {
    let unique: Vec<IndexDef> =
        ctx.db.indexes.values().filter(|i| i.unique && index_covers(i, coll)).cloned().collect();
    for idx in unique {
        let new_entries = sets::index_entries(ctx, &idx, coll, id, new, pos)?;
        let old_entries = match old {
            Some(d) => sets::index_entries(ctx, &idx, coll, id, d, pos)?,
            None => vec![],
        };
        let added: Vec<sets::IndexEntry> = new_entries.into_iter().filter(|e| !old_entries.contains(e)).collect();
        if added.is_empty() {
            continue;
        }
        let colls: Vec<String> = if idx.source.iter().any(|s| s.collection == SourceCollection::Wildcard) {
            ctx.db.collections.keys().cloned().collect()
        } else {
            let mut cs: Vec<String> = idx
                .source
                .iter()
                .filter_map(|s| match &s.collection {
                    SourceCollection::Collection(c) => Some(c.clone()),
                    SourceCollection::Wildcard => None,
                })
                .collect();
            cs.dedup();
            cs
        };
        for c in colls {
            let docs: Vec<(String, Document)> =
                ctx.db.documents_in(&c).map(|(i, d)| (i.clone(), d.clone())).collect();
            for (oid, od) in docs {
                if c == coll && oid == id {
                    continue;
                }
                let entries = sets::index_entries(ctx, &idx, &c, &oid, &od, pos)?;
                if entries.iter().any(|e| added.contains(e)) {
                    return one(FaunaError::instance_not_unique(pos));
                }
            }
        }
    }
    Ok(())
}

/// Validate, constraint-check and store a user document; returns the
/// rendered document. `vpos` is where validation and uniqueness errors
/// are reported.
fn write_document(
    ctx: &mut Ctx,
    coll: &str,
    id: &str,
    prev: Option<&Document>,
    base: &Object,
    params: &Object,
    vpos: &Position,
) -> R<Value> {
    let fields = document_fields(ctx, coll, base, params, vpos)?;
    let doc = Document { ts: ctx.ts, fields };
    check_unique(ctx, coll, id, prev, &doc, vpos)?;
    let rendered = ctx.db.render_document(coll, id, &doc);
    ctx.db.put_document(coll, id, doc, vpos)?;
    Ok(rendered)
}

// ---- Create ----

/// `NativeCollectionID.fromName` names.
const NATIVE_COLLECTIONS: &[&str] = &[
    "databases",
    "collections",
    "classes",
    "indexes",
    "keys",
    "tokens",
    "credentials",
    "functions",
    "roles",
    "access_providers",
];

fn native_create_not_emulated(name: &str, pos: &Position) -> R<Value> {
    one(FaunaError::not_emulated(&format!("`Create` in the native `{}` collection", name), pos))
}

fn create(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    ctx.wrote = true;
    let target = args.eval(ctx, env, "create", pos)?;
    let params = eval_params(ctx, env, args, pos)?;
    let cpos = pos.key("create");

    // Casts: `create` is StringOrRef, `params` is Object; both are checked.
    let mut errs = Vec::new();
    match &target {
        Value::String(_) => {}
        Value::Ref(_) => {
            if let Some(e) = unresolved_ref(ctx, &target, &cpos) {
                errs.push(e);
            }
        }
        other => errs.push(FaunaError::invalid_argument(&[Type::Identifier], other.rtype(), &cpos)),
    }
    let params = match params {
        Ok(p) => p,
        Err(e) => {
            errs.push(e);
            Object::new()
        }
    };
    if !errs.is_empty() {
        return Err(errs);
    }
    validate_refs(ctx, &params, pos)?;

    let ts = ctx.ts;
    let (coll, explicit_id) = match &target {
        Value::String(name) => {
            if ctx.db.collections.contains_key(name) {
                (name.clone(), None)
            } else if NATIVE_COLLECTIONS.contains(&name.as_str()) {
                // The server fails internally resolving a native name here.
                return native_create_not_emulated(name, &cpos);
            } else {
                return one(FaunaError::unresolved_ref("collection", name, &cpos));
            }
        }
        Value::Ref(r) if r.is_collection_ref() => (r.id.clone(), None),
        Value::Ref(r) if r.is_native() => {
            return match r.id.as_str() {
                "collections" => ctx.db.create_collection(&params, ts, &cpos),
                "indexes" => ctx.db.create_index(&params, ts, &cpos),
                "functions" => ctx.db.create_function(&params, ts, &cpos),
                "roles" => ctx.db.create_role(&params, ts, &cpos),
                other => native_create_not_emulated(other, &cpos),
            };
        }
        Value::Ref(r) => match r.document_collection() {
            Some(c) => (c.to_string(), Some(r.id.clone())),
            None => return one(FaunaError::not_emulated(&format!("`Create` with ref {}", r.display()), &cpos)),
        },
        _ => unreachable!("cast checked above"),
    };

    let id = match explicit_id {
        Some(id) => {
            if ctx.db.exists_document(&coll, &id) {
                return one(FaunaError::document_already_exists(&cpos));
            }
            id
        }
        None => ctx.db.new_id(),
    };
    write_document(ctx, &coll, &id, None, &Object::new(), &params, &cpos)
}

// ---- Update / Replace ----

fn update_or_replace(partial: bool, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    ctx.wrote = true;
    let verb = if partial { "update" } else { "replace" };
    let vp = pos.key(verb);
    let target = args.eval(ctx, env, verb, pos)?;
    let params = eval_params(ctx, env, args, pos)?;

    let mut errs = Vec::new();
    let r = match &target {
        Value::Ref(r) => {
            if let Some(e) = unresolved_ref(ctx, &target, &vp) {
                errs.push(e);
            }
            Some(r.clone())
        }
        other => {
            errs.push(FaunaError::invalid_argument(&[Type::Ref], other.rtype(), &vp));
            None
        }
    };
    let params = match params {
        Ok(p) => p,
        Err(e) => {
            errs.push(e);
            Object::new()
        }
    };
    if !errs.is_empty() {
        return Err(errs);
    }
    let r = r.unwrap();
    validate_refs(ctx, &params, pos)?;

    if let Some(coll) = r.document_collection() {
        let coll = coll.to_string();
        let prev = match ctx.db.get_document(&coll, &r.id) {
            Some(d) => d.clone(),
            None => return one(FaunaError::document_not_found(&vp)),
        };
        let base = if partial { prev.fields.clone() } else { Object::new() };
        return write_document(ctx, &coll, &r.id, Some(&prev), &base, &params, pos);
    }
    update_schema(partial, ctx, &r, &params, pos)
}

fn schema_not_emulated(what: &str, pos: &Position) -> R<Value> {
    one(FaunaError::not_emulated(what, pos))
}

/// A `name` param on a schema update must keep the current name: renames
/// would have to rewrite every stored ref, which the emulator does not do.
fn check_same_name(params: &Object, current: &str, pos: &Position) -> R<()> {
    match params.get("name") {
        None => Ok(()),
        Some(Value::String(s)) if s == current => Ok(()),
        Some(_) => one(FaunaError::not_emulated("renaming schema documents", pos)),
    }
}

fn only_keys(params: &Object, allowed: &[&str], what: &str, pos: &Position) -> R<()> {
    match params.keys().find(|k| !allowed.contains(&k.as_str())) {
        None => Ok(()),
        Some(k) => one(FaunaError::not_emulated(&format!("updating `{}` of {}", k, what), pos)),
    }
}

/// Patch an optional schema `data` field; it must stay an object.
fn patch_data(prev: Option<&Value>, params: &Object, pos: &Position) -> R<Option<Value>> {
    let mut base = Object::new();
    if let Some(p) = prev {
        base.insert("data".into(), p.clone());
    }
    let mut diff = Object::new();
    if let Some(d) = params.get("data") {
        diff.insert("data".into(), d.clone());
    }
    let out = patch(&base, &diff);
    match out.get("data") {
        None => Ok(None),
        Some(v @ Value::Object(_)) => Ok(Some(v.clone())),
        Some(other) => one(FaunaError::validation_failed(vec![invalid_type("data", "Map", other)], pos)),
    }
}

fn opt_long_field(params: &Object, key: &str, current: Option<i64>, pos: &Position) -> R<Option<i64>> {
    match params.get(key) {
        None => Ok(current),
        Some(Value::Long(l)) => Ok(Some(*l)),
        Some(Value::Null) => {
            one(FaunaError::not_emulated(&format!("clearing `{}` with null", key), pos))
        }
        Some(other) => one(FaunaError::validation_failed(vec![invalid_type(key, "Long", other)], pos)),
    }
}

/// `Update`/`Replace` of collection, index, function and role documents.
/// Errors are reported at `pos` (the form), like the server's write path.
fn update_schema(partial: bool, ctx: &mut Ctx, r: &Ref, params: &Object, pos: &Position) -> R<Value> {
    let ts = ctx.ts;
    if params.values().any(contains_set) {
        return one(FaunaError::not_emulated("storing a set value in a schema document", pos));
    }
    if r.is_collection_ref() {
        only_keys(params, &["name", "data", "history_days", "ttl_days"], "a collection", pos)?;
        let cur = ctx.db.collections[&r.id].clone();
        check_same_name(params, &cur.name, pos)?;
        // `prevForReplace` keeps `name` and `history_days`.
        let data = patch_data(if partial { cur.data.as_ref() } else { None }, params, pos)?;
        let history_days = opt_long_field(params, "history_days", cur.history_days, pos)?;
        let ttl_days = opt_long_field(params, "ttl_days", if partial { cur.ttl_days } else { None }, pos)?;
        let def = crate::store::CollectionDef { name: cur.name.clone(), ts, history_days, ttl_days, data };
        let rendered = ctx.db.render_collection(&def);
        ctx.db.collections.insert(cur.name, def);
        return Ok(rendered);
    }
    if r.is_index_ref() {
        if !partial {
            return schema_not_emulated("`Replace` of an index", pos);
        }
        only_keys(params, &["name", "data"], "an index", pos)?;
        let mut def = ctx.db.indexes[&r.id].clone();
        check_same_name(params, &def.name, pos)?;
        def.data = patch_data(def.data.as_ref(), params, pos)?;
        def.ts = ts;
        let rendered = ctx.db.render_index(&def);
        ctx.db.indexes.insert(def.name.clone(), def);
        return Ok(rendered);
    }
    if r.is_function_ref() {
        only_keys(params, &["name", "data", "body", "role"], "a function", pos)?;
        let mut def = ctx.db.functions[&r.id].clone();
        check_same_name(params, &def.name, pos)?;
        match params.get("body") {
            Some(Value::Lambda(l)) => def.body = l.clone(),
            Some(_) => return schema_not_emulated("a non-lambda function `body`", pos),
            None if !partial => {
                return one(FaunaError::validation_failed(
                    vec![ValidationFailure {
                        field: vec!["body".into()],
                        code: "value required".into(),
                        description: "Required value not found.".into(),
                    }],
                    pos,
                ))
            }
            None => {}
        }
        def.data = patch_data(if partial { def.data.as_ref() } else { None }, params, pos)?;
        def.role = match params.get("role") {
            Some(Value::Null) => None,
            Some(v) => stored(v),
            None if partial => def.role,
            None => None,
        };
        def.ts = ts;
        let rendered = ctx.db.render_function(&def);
        ctx.db.functions.insert(def.name.clone(), def);
        return Ok(rendered);
    }
    if r.is_role_ref() {
        let mut def = ctx.db.roles[&r.id].clone();
        check_same_name(params, &def.name, pos)?;
        let mut diff = params.clone();
        diff.shift_remove("name");
        let base = if partial { def.fields.clone() } else { Object::new() };
        def.fields = patch(&base, &diff);
        def.ts = ts;
        let rendered = ctx.db.render_role(&def);
        ctx.db.roles.insert(def.name.clone(), def);
        return Ok(rendered);
    }
    let verb = if partial { "Update" } else { "Replace" };
    schema_not_emulated(&format!("`{}` of {}", verb, r.display()), pos)
}

// ---- Delete ----

fn delete(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    ctx.wrote = true;
    let dp = pos.key("delete");
    let target = args.eval(ctx, env, "delete", pos)?;
    let r = cast::ref_(&target, &dp)?.clone();
    if let Some(e) = unresolved_ref(ctx, &target, &dp) {
        return one(e);
    }
    if let Some(coll) = r.document_collection() {
        let coll = coll.to_string();
        // The server returns the deleted (previous) version, with its ts.
        return match ctx.db.remove_document(&coll, &r.id) {
            None => one(FaunaError::document_not_found(pos)),
            Some(d) => Ok(ctx.db.render_document(&coll, &r.id, &d)),
        };
    }
    if r.is_collection_ref() {
        let def = ctx.db.collections.shift_remove(&r.id).expect("resolved above");
        let rendered = ctx.db.render_collection(&def);
        let ids: Vec<String> = ctx.db.documents_in(&r.id).map(|(i, _)| i.clone()).collect();
        for id in ids {
            ctx.db.remove_document(&r.id, &id);
        }
        ctx.db.documents.remove(&r.id);
        // `indexesWithSoleSource`: indexes over only this collection go too.
        ctx.db.indexes.retain(|_, idx| {
            let mut sources = idx.source.iter().map(|s| &s.collection);
            let sole = sources.all(|c| *c == SourceCollection::Collection(r.id.clone()));
            !sole || idx.source.is_empty()
        });
        return Ok(rendered);
    }
    if r.is_index_ref() {
        let def = ctx.db.indexes.shift_remove(&r.id).expect("resolved above");
        return Ok(ctx.db.render_index(&def));
    }
    if r.is_function_ref() {
        let def = ctx.db.functions.shift_remove(&r.id).expect("resolved above");
        return Ok(ctx.db.render_function(&def));
    }
    if r.is_role_ref() {
        let def = ctx.db.roles.shift_remove(&r.id).expect("resolved above");
        return Ok(ctx.db.render_role(&def));
    }
    one(FaunaError::not_emulated(&format!("`Delete` of {}", r.display()), &dp))
}

#[cfg(test)]
mod tests {
    use crate::Emulator;
    use serde_json::{json, Value as Json};

    fn q(e: &mut Emulator, j: Json) -> Json {
        let r = e.query(&j);
        assert!(r.status == 200 || r.status == 201, "{}", r.body);
        r.body["resource"].clone()
    }

    fn user(id: &str) -> Json {
        json!({"ref": {"collection": "users"}, "id": id})
    }

    fn emu() -> Emulator {
        let mut e = Emulator::new();
        e.set_now_micros(1_000);
        e.load_schema(&json!({"collections": [{"name": "users"}]})).unwrap();
        e
    }

    #[test]
    fn writes_bump_ts_and_delete_returns_previous_version() {
        let mut e = emu();
        let created = q(&mut e, json!({"create": user("1"), "params": {"object": {"data": {"object": {"a": 1}}}}}));
        assert_eq!(created["ts"], json!(1_000));

        e.set_now_micros(2_000);
        let updated = q(&mut e, json!({"update": user("1"), "params": {"object": {"data": {"object": {"b": 2}}}}}));
        assert_eq!(updated["ts"], json!(2_000));
        assert_eq!(updated["data"], json!({"a": 1, "b": 2}));
        assert_eq!(q(&mut e, json!({"select": ["ts"], "from": {"get": user("1")}})), json!(2_000));

        e.set_now_micros(3_000);
        let deleted = q(&mut e, json!({"delete": user("1")}));
        assert_eq!(deleted["ts"], json!(2_000));
        assert_eq!(deleted["data"], json!({"a": 1, "b": 2}));
        assert_eq!(q(&mut e, json!({"exists": user("1")})), json!(false));
    }

    #[test]
    fn aborted_transactions_leave_no_writes() {
        let mut e = emu();
        q(&mut e, json!({"create": user("1"), "params": {"object": {"data": {"object": {"a": 1}}}}}));
        let r = e.query(&json!({"do": [
            {"create": user("2")},
            {"update": user("1"), "params": {"object": {"data": {"object": {"a": 2}}}}},
            {"delete": user("1")},
            {"abort": "stop"}
        ]}));
        assert_eq!(r.status, 400);
        assert_eq!(q(&mut e, json!({"exists": user("2")})), json!(false));
        assert_eq!(q(&mut e, json!({"select": ["data", "a"], "from": {"get": user("1")}})), json!(1));
        assert_eq!(e.db.doc_count, 1);
    }

    #[test]
    fn minted_ids_are_distinct_and_counted() {
        let mut e = emu();
        let a = q(&mut e, json!({"create": {"collection": "users"}}));
        let b = q(&mut e, json!({"create": {"collection": "users"}}));
        assert_ne!(a["ref"], b["ref"]);
        assert_eq!(e.db.doc_count, 2);
        q(&mut e, json!({"delete": {"collection": "users"}}));
        assert_eq!(e.db.doc_count, 0);
    }

    #[test]
    fn document_cap_applies_to_create() {
        let mut e = emu();
        e.set_max_documents(1);
        q(&mut e, json!({"create": user("1")}));
        let r = e.query(&json!({"create": user("2")}));
        assert_eq!(r.body["errors"][0]["code"], "emulator limit exceeded");
        // Replacing an existing document does not count against the cap.
        q(&mut e, json!({"replace": user("1"), "params": {"object": {"data": {"object": {"a": 1}}}}}));
    }
}
