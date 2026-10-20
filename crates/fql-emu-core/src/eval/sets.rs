//! Sets: `Match`, `Documents`, `Singleton`, `Union`, `Intersection`,
//! `Difference`, `Distinct`, `Range`, `Join`, set `Filter`/`Reverse`, and
//! the brute-force set engine behind `Paginate` and the fold functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/SetFunctions.scala,
//! ext/model/src/main/scala/set/*.scala (set algebra),
//! ext/model/src/main/scala/doc/Index.scala (term extraction: `TermConfig`,
//! `legacyDataExtractor`), ext/repo/src/main/scala/doc/Indexer.scala
//! (cartesian product of term/value fields, null filtering),
//! ext/storage/src/main/scala/index/IndexTuple.scala (ordering) and
//! ext/model/src/main/scala/ast/functions/ReadAdaptor.scala (`paginate`,
//! `PaginateArray`), ext/model/src/main/scala/ast/Parser.scala
//! (`CursorParser`).
//!
//! There are no index data structures. `Match` scans every document in
//! the index's source collections, evaluates the index's terms/values
//! for each one, filters on the terms and sorts by (values..., ref).
//! That is the whole point: this is for small fixtures.
//!
//! The engine mirrors the server's `EventSet.snapshot(from, to, ascending)`
//! contract: every set kind produces its elements between two bounds in
//! the requested direction, and composite sets (`Union`, `Range`,
//! `Reverse`, `Join`, ...) push the bounds down to their sources exactly
//! like the Scala implementations do. `Paginate` then applies the
//! server's drop/over-fetch/cursor logic on top. Elements are
//! `(values, doc)` tuples (the server's `SetEvent`): values carry their
//! per-field `reverse` flag, and the doc is the source document's ref.

use super::cast;
use super::cast::PathElem;
use super::forms::{Args, Registry};
use super::{apply_lambda, eval, parse_lambda_expr, Ctx, Env};
use crate::error::{one, Errors, FaunaError, Position, R};
use crate::ordering;
use crate::store::{Database, Document, IndexDef, SourceCollection, TermPath};
use crate::value::{JoinTarget, Lambda, Ref, SetExpr, Type, Value};
use serde_json::Value as Json;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

pub fn register(r: &mut Registry) {
    r.add("match_index", &["match", "index"], &[], match_index);
    r.add("match_terms", &["match", "terms"], &[], match_terms);
    r.add("match", &["match"], &[], match_all);
    r.add("documents", &["documents"], &[], documents);
    r.add("singleton", &["singleton"], &[], singleton);
    r.add("events", &["events"], &[], events);
    r.add("union", &["union"], &[], |c, e, a, p| algebra(Algebra::Union, c, e, a, p));
    r.add("intersection", &["intersection"], &[], |c, e, a, p| algebra(Algebra::Intersection, c, e, a, p));
    r.add("difference", &["difference"], &[], |c, e, a, p| algebra(Algebra::Difference, c, e, a, p));
    r.add("distinct", &["distinct"], &[], distinct);
    r.add("range", &["range", "from", "to"], &[], range);
    r.add("join", &["join", "with"], &[], join);
}

/// One materialised element of a set together with its sort/cursor key.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The entry's cursor: the index values followed by the source
    /// document's ref (`IndexTuple.cursor`). For `Documents()` and
    /// value-less indexes this is just `[ref]`. Paginate cursors are made
    /// of exactly this, so `key.last()` is always the document ref.
    pub key: Vec<Value>,
    /// What the set yields (`IndexTuple.coveredValues`): the ref when the
    /// index has no values, the value itself when it has exactly one, and
    /// an array of the values when it has several. A ref is only part of
    /// it when the index lists `{"field": ["ref"]}` as a value.
    pub value: Value,
}

/// Materialise a set in iteration order: the values it yields.
pub fn materialize(ctx: &mut Ctx, set: &SetExpr, pos: &Position) -> R<Vec<Value>> {
    Ok(materialize_entries(ctx, set, pos)?.into_iter().map(|e| e.value).collect())
}

/// One index entry for one document, after multi-value (array) expansion:
/// the server emits one entry per combination of term/value elements.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexEntry {
    pub terms: Vec<Value>,
    pub values: Vec<Value>,
}

/// Compute the index entries a document contributes to `index`
/// (`Index.scala`: `TermConfig.extractor`, `legacyDataExtractor`,
/// bindings via `SourceConfig.fields`). Used by `Match` and by the
/// unique-constraint check in `write.rs`. A document that is not in the
/// index's sources contributes no entries.
///
/// Semantics (all from the server):
/// - a field path is read like `SelectAsIndex(path, doc)`: arrays met
///   along the path are traversed, and the result is flattened, so array
///   fields (and arrays of objects) contribute one entry per element;
///   objects (and other non-scalars) index as `null`;
/// - a binding is the source's `fields[name]` lambda applied to the
///   document; a scalar result is one element, an array result one
///   element per item (non-scalar items become `null`), anything else
///   (including an error, except the emulator's own `not emulated`) none;
///   bindings that are not pure fail with `not emulated` (the server
///   refuses to create such an index);
/// - a field with no elements indexes as `null`;
/// - entries are the cartesian product of the term fields times the
///   cartesian product of the value fields, minus the term combinations
///   that are all `null` and minus the value combinations that are all
///   `null` (so a document missing every term or every value field is not
///   in the index), deduplicated;
/// - `["ts"]` as a *term* is the transaction-time placeholder, which the
///   server stores as `null` but does not count as null for the filter
///   above; as a *value* it is the document's `ts`.
pub fn index_entries(
    ctx: &mut Ctx,
    index: &IndexDef,
    collection: &str,
    id: &str,
    doc: &Document,
    pos: &Position,
) -> R<Vec<IndexEntry>> {
    let source = match source_fields(index, collection) {
        Some(s) => s.clone(),
        None => return Ok(vec![]),
    };
    for (name, lam) in &source {
        if lambda_effect(lam) != Effect::Pure {
            return one(FaunaError::not_emulated(
                &format!("index binding '{}' that reads or writes (the server rejects it at CreateIndex)", name),
                pos,
            ));
        }
    }
    let rendered = ctx.db.render_document(collection, id, doc);
    let mut bindings: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut extract = |ctx: &mut Ctx, path: &TermPath, is_term: bool| -> R<Vec<Extracted>> {
        let raw: Vec<Extracted> = match path {
            TermPath::Field(p) => extract_field(&rendered, collection, p, is_term),
            TermPath::Binding(name) => {
                if !bindings.contains_key(name) {
                    let vals = match source.get(name) {
                        Some(lam) => eval_binding(ctx, lam, &rendered, pos)?,
                        None => vec![],
                    };
                    bindings.insert(name.clone(), vals);
                }
                bindings[name].iter().map(|v| Extracted { value: v.clone(), placeholder: false }).collect()
            }
        };
        let mut out: Vec<Extracted> = Vec::with_capacity(raw.len().max(1));
        for e in raw {
            if !out.contains(&e) {
                out.push(e);
            }
        }
        if out.is_empty() {
            out.push(Extracted { value: Value::Null, placeholder: false });
        }
        Ok(out)
    };
    let mut term_axes = Vec::with_capacity(index.terms.len());
    for t in &index.terms {
        term_axes.push(extract(ctx, &t.path, true)?);
    }
    let mut value_axes = Vec::with_capacity(index.values.len());
    for v in &index.values {
        value_axes.push(extract(ctx, &v.path, false)?);
    }
    let term_combos = combos(&term_axes);
    let value_combos = combos(&value_axes);
    let mut out: Vec<IndexEntry> = Vec::new();
    for t in &term_combos {
        for v in &value_combos {
            let e = IndexEntry {
                terms: t.iter().map(|x| x.value.clone()).collect(),
                values: v.iter().map(|x| x.value.clone()).collect(),
            };
            if !out.contains(&e) {
                out.push(e);
            }
        }
    }
    Ok(out)
}

/// Materialise a set as ordered entries (with cursor keys), the way the
/// server iterates it for `Count`, `Select`, the fold functions, ... :
/// a full ascending snapshot.
pub fn materialize_entries(ctx: &mut Ctx, set: &SetExpr, pos: &Position) -> R<Vec<Entry>> {
    let elems = snapshot(ctx, set, &SEvent::min(), &SEvent::max(), true, pos)?;
    Ok(elems.iter().map(|e| Entry { key: e.cursor(), value: e.covered() }).collect())
}

// ---------------------------------------------------------------------
// Lambda effects (`Expression.maxEffect`)
// ---------------------------------------------------------------------

/// `fauna.model.runtime.Effect`, ordered by strength. The server computes
/// a lambda's maximum effect statically and rejects impure lambdas in some
/// contexts (`Join` targets must be pure, set `Filter`s may not write,
/// index bindings must be pure).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    Pure,
    Observation,
    Read,
    Write,
}

/// The static maximum effect of a lambda's body.
pub fn lambda_effect(l: &Lambda) -> Effect {
    expr_effect(&l.body)
}

fn is_inline_lambda(j: &Json) -> bool {
    match j {
        Json::Object(o) => {
            o.contains_key("lambda")
                && o.contains_key("expr")
                && o.keys().all(|k| k == "lambda" || k == "expr" || k == "api_version")
        }
        _ => false,
    }
}

/// The function effects from the server's `QFunction.effect` values.
/// Forms not listed are pure.
fn form_effect(name: &str) -> Effect {
    match name {
        "get" | "exists" | "paginate" | "first" | "last" | "key_from_secret" | "current_identity" | "identity"
        | "has_current_identity" | "has_identity" | "identify" | "current_token" | "has_current_token"
        | "contains_value" | "count" | "sum" | "mean" | "max" | "min" | "any" | "all" | "reduce" => Effect::Read,
        "create" | "update" | "replace" | "delete" | "insert" | "remove" | "move_database" | "new_id" | "next_id"
        | "login" | "logout" | "issue_access_jwt" | "create_collection" | "create_class" | "create_database"
        | "create_index" | "create_key" | "create_function" | "create_role" | "create_access_provider" | "call" => {
            Effect::Write
        }
        _ => Effect::Pure,
    }
}

/// `Expression.maxEffect` of an unevaluated expression: the maximum of the
/// form's own effect and its arguments' effects. Lambda literals are pure
/// values, except where the server adds the lambda body's effect (the
/// lambda of `Map`/`Foreach`/`Filter`/`Reduce`/`Merge`/`Join`).
pub fn expr_effect(expr: &Json) -> Effect {
    match expr {
        Json::Array(items) => items.iter().map(expr_effect).max().unwrap_or(Effect::Pure),
        Json::Object(o) => {
            if o.len() == 1 && o.keys().next().map(|k| k.starts_with('@')).unwrap_or(false) {
                return Effect::Pure;
            }
            if is_inline_lambda(expr) {
                return Effect::Pure;
            }
            let keys: HashSet<&str> = o.keys().map(|k| k.as_str()).collect();
            let name = super::forms::with_registry(|r| r.lookup(&keys).map(|f| f.name));
            match name {
                Some("quote") => return Effect::Pure,
                Some("object") => {
                    return match o.get("object") {
                        Some(Json::Object(fields)) => fields.values().map(expr_effect).max().unwrap_or(Effect::Pure),
                        Some(other) => expr_effect(other),
                        None => Effect::Pure,
                    }
                }
                Some("let") => {
                    let bindings = match o.get("let") {
                        Some(Json::Array(items)) => items
                            .iter()
                            .map(|b| match b {
                                Json::Object(bo) => bo.values().map(expr_effect).max().unwrap_or(Effect::Pure),
                                other => expr_effect(other),
                            })
                            .max()
                            .unwrap_or(Effect::Pure),
                        Some(Json::Object(bo)) => bo.values().map(expr_effect).max().unwrap_or(Effect::Pure),
                        Some(other) => expr_effect(other),
                        None => Effect::Pure,
                    };
                    return bindings.max(o.get("in").map(expr_effect).unwrap_or(Effect::Pure));
                }
                _ => {}
            }
            let mut eff = name.map(form_effect).unwrap_or(Effect::Pure);
            for (k, v) in o {
                let arg = if is_inline_lambda(v) {
                    let body_counts = matches!(
                        (name, k.as_str()),
                        (Some("map"), "map")
                            | (Some("foreach"), "foreach")
                            | (Some("filter"), "filter")
                            | (Some("reduce"), "reduce")
                            | (Some("merge"), "lambda")
                            | (Some("join"), "with")
                    );
                    if body_counts {
                        v.get("expr").map(expr_effect).unwrap_or(Effect::Pure)
                    } else {
                        Effect::Pure
                    }
                } else {
                    expr_effect(v)
                };
                eff = eff.max(arg);
            }
            eff
        }
        _ => Effect::Pure,
    }
}

// ---------------------------------------------------------------------
// Ref resolution (`UnresolvedRefL`)
// ---------------------------------------------------------------------

/// The server resolves ref constructors (`Collection("x")`, `Index("x")`,
/// `Ref(Collection("x"), id)`, ...) when they are evaluated and a cast
/// that expects an identifier reports an unresolvable one as `invalid
/// ref`. The emulator's refs are plain values, so functions check the
/// store where the server's cast would.
pub fn unresolved_ref(db: &Database, r: &Ref, pos: &Position) -> Option<FaunaError> {
    if r.database.is_some() {
        return None;
    }
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
    if r.is_database_ref() {
        // The emulator has no child databases.
        return missing("database", &r.id);
    }
    if let Some(coll) = r.document_collection() {
        return if db.collections.contains_key(coll) { None } else { missing("collection", coll) };
    }
    match r.collection.as_deref() {
        Some(p) if p.is_native() && p.id == "keys" => missing("key", &r.id),
        Some(p) if p.is_native() && p.id == "access_providers" => missing("access_provider", &r.id),
        _ => None,
    }
}

fn check_ref(db: &Database, r: &Ref, pos: &Position) -> R<()> {
    match unresolved_ref(db, r, pos) {
        Some(e) => one(e),
        None => Ok(()),
    }
}

/// True when `v` contains an unresolvable ref anywhere (the server cannot
/// encode such a value as an index term).
fn contains_unresolved(db: &Database, v: &Value) -> bool {
    match v {
        Value::Ref(r) => unresolved_ref(db, r, &Position::root()).is_some(),
        Value::Array(items) => items.iter().any(|i| contains_unresolved(db, i)),
        Value::Object(o) => o.values().any(|i| contains_unresolved(db, i)),
        _ => false,
    }
}

/// The native collection refs (`Collections()`, `Indexes()`, ...). The
/// server treats them as collection refs whose documents are the schema
/// documents.
fn is_native_collection(r: &Ref) -> bool {
    r.is_native()
        && r.database.is_none()
        && matches!(
            r.id.as_str(),
            "databases"
                | "collections"
                | "indexes"
                | "keys"
                | "tokens"
                | "credentials"
                | "functions"
                | "roles"
                | "access_providers"
        )
}

// ---------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------

/// `Casts.Identifier` for functions that need a set: a set, or a native
/// collection ref (its schema documents). `Ok(None)` for any other
/// resolvable ref (a document set in `Paginate`).
pub fn identifier(ctx: &Ctx, v: &Value, pos: &Position) -> R<Option<SetExpr>> {
    match v {
        Value::Set(s) => Ok(Some(s.clone())),
        Value::Ref(r) if is_native_collection(r) => Ok(Some(SetExpr::Documents(r.clone()))),
        Value::Ref(r) => {
            check_ref(ctx.db, r, pos)?;
            Ok(None)
        }
        other => one(FaunaError::invalid_argument(&[Type::Ref, Type::Set], other.rtype(), pos)),
    }
}

/// `Casts.Set`.
fn cast_set(ctx: &Ctx, v: &Value, pos: &Position) -> R<SetExpr> {
    match identifier(ctx, v, pos)? {
        Some(s) => Ok(s),
        None => one(FaunaError::invalid_argument(&[Type::Set], v.rtype(), pos)),
    }
}

enum Iterable {
    Array(Vec<Value>),
    Page(Vec<Value>, Option<Box<Value>>, Option<Box<Value>>),
    Set(SetExpr),
}

/// `Casts.Iterable`: an array, a page, or a set (incl. native collection
/// refs). Any other value is "Array, Set, or Page expected".
fn cast_iterable(ctx: &Ctx, v: &Value, pos: &Position) -> R<Iterable> {
    match v {
        Value::Array(a) => Ok(Iterable::Array(a.clone())),
        Value::Page { data, before, after } => Ok(Iterable::Page(data.clone(), before.clone(), after.clone())),
        Value::Set(s) => Ok(Iterable::Set(s.clone())),
        Value::Ref(r) if is_native_collection(r) => Ok(Iterable::Set(SetExpr::Documents(r.clone()))),
        Value::Ref(r) => {
            check_ref(ctx.db, r, pos)?;
            one(FaunaError::invalid_argument(&[Type::AbstractIterable], v.rtype(), pos))
        }
        other => one(FaunaError::invalid_argument(&[Type::AbstractIterable], other.rtype(), pos)),
    }
}

fn both<A, B>(a: R<A>, b: R<B>) -> R<(A, B)> {
    match (a, b) {
        (Ok(a), Ok(b)) => Ok((a, b)),
        (a, b) => {
            let mut errs: Errors = Vec::new();
            if let Err(e) = a {
                errs.extend(e);
            }
            if let Err(e) = b {
                errs.extend(e);
            }
            Err(errs)
        }
    }
}

// ---------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------

enum IndexIdent {
    Name(String),
    Ref(Ref),
}

/// `Casts.StringOrIndexRef`.
fn cast_index_ident(ctx: &Ctx, v: &Value, pos: &Position) -> R<IndexIdent> {
    match v {
        Value::String(s) => Ok(IndexIdent::Name(s.clone())),
        Value::Ref(r) => {
            check_ref(ctx.db, r, pos)?;
            if r.is_index_ref() {
                Ok(IndexIdent::Ref(r.clone()))
            } else {
                one(FaunaError::invalid_argument(&[Type::Identifier], v.rtype(), pos))
            }
        }
        other => one(FaunaError::invalid_argument(&[Type::Identifier], other.rtype(), pos)),
    }
}

/// `Casts.ZeroOrMore(Casts.MatchTerm)`: an array is a list of terms, any
/// other value a single term.
fn cast_match_terms(ctx: &Ctx, v: &Value, pos: &Position) -> R<Vec<Value>> {
    let mut out = Vec::new();
    let mut errs = Vec::new();
    for (t, p) in cast::elems(v, pos) {
        if contains_unresolved(ctx.db, &t) {
            errs.push(FaunaError::invalid_argument(&[Type::Any], t.rtype(), &p));
        } else {
            out.push(t);
        }
    }
    if errs.is_empty() {
        Ok(out)
    } else {
        Err(errs)
    }
}

/// `MatchFunction`: an index given by name that does not exist is an
/// `invalid ref` at `pos ++ ["index"]`, whichever key held the name.
fn make_match(ctx: &Ctx, ident: IndexIdent, terms: Vec<Value>, pos: &Position) -> R<Value> {
    let index = match ident {
        IndexIdent::Name(name) => {
            if !ctx.db.indexes.contains_key(&name) {
                return one(FaunaError::unresolved_ref("index", &name, &pos.key("index")));
            }
            Ref::index(&name)
        }
        IndexIdent::Ref(r) => Ref::index(&r.id),
    };
    Ok(Value::Set(SetExpr::Match { index, terms }))
}

/// `{"match": terms, "index": index}` (the driver's `Match(index, terms)`).
fn match_index(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let terms = args.eval(ctx, env, "match", pos);
    let index = args.eval(ctx, env, "index", pos);
    let (terms, index) = both(terms, index)?;
    let terms = cast_match_terms(ctx, &terms, &pos.key("match"));
    let ident = cast_index_ident(ctx, &index, &pos.key("index"));
    let (terms, ident) = both(terms, ident)?;
    make_match(ctx, ident, terms, pos)
}

/// `{"match": index, "terms": terms}` (the `@set` shape).
fn match_terms(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let index = args.eval(ctx, env, "match", pos);
    let terms = args.eval(ctx, env, "terms", pos);
    let (index, terms) = both(index, terms)?;
    let ident = cast_index_ident(ctx, &index, &pos.key("match"));
    let terms = cast_match_terms(ctx, &terms, &pos.key("terms"));
    let (ident, terms) = both(ident, terms)?;
    make_match(ctx, ident, terms, pos)
}

/// `{"match": index}`: no terms.
fn match_all(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let index = args.eval(ctx, env, "match", pos)?;
    let ident = cast_index_ident(ctx, &index, &pos.key("match"))?;
    make_match(ctx, ident, vec![], pos)
}

/// `Documents(collection)` (`Casts.ClassRef`).
fn documents(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "documents", pos)?;
    let p = pos.key("documents");
    match &v {
        Value::Ref(r) if is_native_collection(r) => Ok(Value::Set(SetExpr::Documents(r.clone()))),
        Value::Ref(r) => {
            check_ref(ctx.db, r, &p)?;
            if r.is_collection_ref() {
                Ok(Value::Set(SetExpr::Documents(r.clone())))
            } else {
                one(FaunaError::invalid_argument(&[Type::CollectionRef], v.rtype(), &p))
            }
        }
        other => one(FaunaError::invalid_argument(&[Type::CollectionRef], other.rtype(), &p)),
    }
}

/// `Singleton(ref)` (`Casts.Ref`).
fn singleton(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "singleton", pos)?;
    let p = pos.key("singleton");
    match &v {
        Value::Ref(r) => {
            check_ref(ctx.db, r, &p)?;
            Ok(Value::Set(SetExpr::Singleton(r.clone())))
        }
        other => one(FaunaError::invalid_argument(&[Type::Ref], other.rtype(), &p)),
    }
}

/// `Events(set_or_ref)`: history is not emulated.
fn events(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "events", pos)?;
    identifier(ctx, &v, &pos.key("events"))?;
    one(FaunaError::not_emulated("`Events` (document and set history)", pos))
}

#[derive(Clone, Copy)]
enum Algebra {
    Union,
    Intersection,
    Difference,
}

impl Algebra {
    fn key(self) -> &'static str {
        match self {
            Algebra::Union => "union",
            Algebra::Intersection => "intersection",
            Algebra::Difference => "difference",
        }
    }
}

/// `Union`/`Intersection`/`Difference` (`AssociativeSetFunction`):
/// `Casts.OneOrMore(Casts.SetOrArray)`; all sets (with compatible shapes)
/// make a set, all arrays are combined eagerly, a mix is an error.
fn algebra(op: Algebra, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let key = op.key();
    let v = args.eval(ctx, env, key, pos)?;
    let p = pos.key(key);
    let items = cast::one_or_more(&v, &p)?;
    let mut errs = Vec::new();
    let mut sets = Vec::new();
    let mut arrays = Vec::new();
    for (item, ip) in &items {
        // `Casts.SetOrArray`: an iterable, but not a page.
        match cast_iterable(ctx, item, ip) {
            Ok(Iterable::Array(a)) => arrays.push(a),
            Ok(Iterable::Set(s)) => sets.push(s),
            Ok(Iterable::Page(..)) => {
                errs.push(FaunaError::invalid_argument(&[Type::Set, Type::Array], Type::Page, ip))
            }
            Err(e) => errs.extend(e),
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    if arrays.is_empty() {
        let mut shapes = Vec::with_capacity(sets.len());
        for s in &sets {
            shapes.push(shape_of(ctx, s, &p)?);
        }
        let mut errs = Vec::new();
        for w in shapes.windows(2) {
            if !w[0].compatible(&w[1]) {
                errs.push(FaunaError::sets_not_compatible(&p));
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(Value::Set(match op {
            Algebra::Union => SetExpr::Union(sets),
            Algebra::Intersection => SetExpr::Intersection(sets),
            Algebra::Difference => SetExpr::Difference(sets),
        }))
    } else if sets.is_empty() {
        Ok(Value::Array(match op {
            Algebra::Union => array_union(arrays),
            Algebra::Intersection => array_intersection(arrays),
            Algebra::Difference => array_difference(arrays),
        }))
    } else {
        one(FaunaError::mixed_arguments(&[Type::Set, Type::Array], &p))
    }
}

/// Literal equality as the server's case classes define it (used by the
/// array forms): structural, `1 != 1.0`, objects compare field by field
/// in order.
fn literal_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| literal_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|((k1, v1), (k2, v2))| k1 == k2 && literal_eq(v1, v2))
        }
        _ => a == b,
    }
}

struct Counts(Vec<(Value, usize)>);

impl Counts {
    fn of(items: &[Value]) -> Counts {
        let mut c = Counts(Vec::new());
        for i in items {
            *c.get_mut(i) += 1;
        }
        c
    }

    fn get_mut(&mut self, v: &Value) -> &mut usize {
        match self.0.iter().position(|(k, _)| literal_eq(k, v)) {
            Some(i) => &mut self.0[i].1,
            None => {
                self.0.push((v.clone(), 0));
                &mut self.0.last_mut().unwrap().1
            }
        }
    }
}

/// `UnionFunction.mkArray`: each value appears max(count) times, in
/// first-seen order.
fn array_union(arrays: Vec<Vec<Value>>) -> Vec<Value> {
    let mut it = arrays.into_iter();
    let mut acc = it.next().unwrap_or_default();
    for right in it {
        let mut occ = Counts::of(&acc);
        for (v, c) in Counts::of(&right).0 {
            let o = occ.get_mut(&v);
            *o = (*o).max(c);
        }
        let mut out = Vec::new();
        for v in acc.iter().chain(right.iter()) {
            let o = occ.get_mut(v);
            for _ in 0..*o {
                out.push(v.clone());
            }
            *o = 0;
        }
        acc = out;
    }
    acc
}

/// `List.intersect`, folded left.
fn array_intersection(arrays: Vec<Vec<Value>>) -> Vec<Value> {
    let mut it = arrays.into_iter();
    let mut acc = it.next().unwrap_or_default();
    for right in it {
        let mut occ = Counts::of(&right);
        acc.retain(|v| {
            let o = occ.get_mut(v);
            if *o > 0 {
                *o -= 1;
                true
            } else {
                false
            }
        });
    }
    acc
}

/// `List.diff`, folded left.
fn array_difference(arrays: Vec<Vec<Value>>) -> Vec<Value> {
    let mut it = arrays.into_iter();
    let mut acc = it.next().unwrap_or_default();
    for right in it {
        let mut occ = Counts::of(&right);
        acc.retain(|v| {
            let o = occ.get_mut(v);
            if *o == 0 {
                true
            } else {
                *o -= 1;
                false
            }
        });
    }
    acc
}

fn array_distinct(items: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    for v in items {
        if !out.iter().any(|o| literal_eq(o, &v)) {
            out.push(v);
        }
    }
    out
}

/// `Distinct(iterable)`: arrays and pages are deduplicated eagerly (pages
/// keep their cursors), sets lazily.
fn distinct(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "distinct", pos)?;
    match cast_iterable(ctx, &v, &pos.key("distinct"))? {
        Iterable::Array(a) => Ok(Value::Array(array_distinct(a))),
        Iterable::Page(data, before, after) => Ok(Value::Page { data: array_distinct(data), before, after }),
        Iterable::Set(s) => Ok(Value::Set(SetExpr::Distinct(Box::new(s)))),
    }
}

/// `Range(set, from, to)`: the bounds are cursors (see `parse_cursor`),
/// inclusive at both ends.
fn range(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let set = args.eval(ctx, env, "range", pos);
    let from = args.eval(ctx, env, "from", pos);
    let to = args.eval(ctx, env, "to", pos);
    let ((set, from), to) = both(both(set, from), to)?;
    let set = cast_set(ctx, &set, &pos.key("range"))?;
    let check = |v: &Value, p: &Position| match v {
        Value::Ref(r) => check_ref(ctx.db, r, p),
        _ => Ok(()),
    };
    both(check(&from, &pos.key("from")), check(&to, &pos.key("to")))?;
    Ok(Value::Set(SetExpr::Range { set: Box::new(set), from: Box::new(from), to: Box::new(to) }))
}

/// `Join(source, with)`: `with` is a pure lambda returning a set for each
/// source element, or an index matched against each element.
fn join(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let wp = pos.key("with");
    let source = args.eval(ctx, env, "join", pos);
    let with_j = args.req("with");
    let with = if is_inline_lambda(with_j) {
        parse_lambda_expr(env, with_j, &wp).map(Value::Lambda)
    } else {
        eval(ctx, env, with_j, &wp)
    };
    let (source, with) = both(source, with)?;
    let source = cast_set(ctx, &source, &pos.key("join"))?;
    let target = match &with {
        Value::Lambda(l) => {
            if lambda_effect(l) != Effect::Pure {
                return one(FaunaError::invalid_lambda_effect(true, &wp));
            }
            JoinTarget::Lambda(l.clone())
        }
        Value::Ref(r) => {
            check_ref(ctx.db, r, &wp)?;
            if r.is_index_ref() {
                JoinTarget::Index(Ref::index(&r.id))
            } else {
                return one(FaunaError::invalid_argument(&[Type::Lambda, Type::IndexRef], with.rtype(), &wp));
            }
        }
        other => return one(FaunaError::invalid_argument(&[Type::Lambda, Type::IndexRef], other.rtype(), &wp)),
    };
    Ok(Value::Set(SetExpr::Join { source: Box::new(source), with: Box::new(target) }))
}

// ---------------------------------------------------------------------
// Index term extraction
// ---------------------------------------------------------------------

/// The source config that applies to documents of `collection`: an
/// explicitly listed collection wins over the `_` wildcard; the last
/// matching config wins (`Index.parseBinders`).
fn source_fields<'i>(index: &'i IndexDef, collection: &str) -> Option<&'i indexmap::IndexMap<String, Lambda>> {
    let explicit = index
        .source
        .iter()
        .rev()
        .find(|s| matches!(&s.collection, SourceCollection::Collection(c) if c == collection));
    explicit
        .or_else(|| index.source.iter().rev().find(|s| s.collection == SourceCollection::Wildcard))
        .map(|s| &s.fields)
}

/// One extracted term/value element.
#[derive(Clone, Debug, PartialEq)]
struct Extracted {
    value: Value,
    /// The transaction-time placeholder (`["ts"]` as a term): stored as
    /// `null` but not null for the all-null filter.
    placeholder: bool,
}

/// `ScalarL` in the server: what an index can store as a term.
fn is_index_scalar(v: &Value) -> bool {
    matches!(
        v,
        Value::Null
            | Value::Bool(_)
            | Value::Long(_)
            | Value::Double(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::Time(_)
            | Value::Date(_)
            | Value::Ref(_)
    )
}

/// `legacyDataExtractor` with `mva = true`:
/// `SelectAsIndex(path, version)` then flatten (non-scalars become null).
fn extract_field(doc: &Value, collection: &str, path: &[PathElem], is_term: bool) -> Vec<Extracted> {
    let plain = |v: Value| Extracted { value: v, placeholder: false };
    let found = match path {
        [PathElem::Key(k)] if k == "ts" => {
            if is_term {
                return vec![Extracted { value: Value::Null, placeholder: true }];
            }
            doc.as_object().and_then(|o| o.get("ts")).cloned()
        }
        [PathElem::Key(k), rest @ ..] if k == "class" => {
            select_all(&Value::Ref(Ref::collection(collection)), rest)
        }
        [PathElem::Key(k), _, ..] if k == "ts" => None,
        _ => select_all(doc, path),
    };
    let items = match found {
        None => vec![],
        Some(Value::Array(a)) => a,
        Some(v) => vec![v],
    };
    let mut out = Vec::new();
    flatten_into(items, &mut out);
    out.into_iter().map(plain).collect()
}

fn flatten_into(items: Vec<Value>, out: &mut Vec<Value>) {
    for v in items {
        match v {
            Value::Array(inner) => flatten_into(inner, out),
            v if is_index_scalar(&v) => out.push(v),
            _ => out.push(Value::Null),
        }
    }
}

/// `ReadAdaptor.valueAtPath0` with `all = true`: arrays met along the
/// path (or at its end) are traversed and their results concatenated.
pub fn select_all(v: &Value, path: &[PathElem]) -> Option<Value> {
    match v {
        Value::Ref(r) => super::basic::ref_at_path(r, path),
        Value::Array(items) => match path {
            [PathElem::Index(i), rest @ ..] => {
                let item = items.get(usize::try_from(*i).ok()?)?;
                select_all(item, rest)
            }
            _ => {
                let mut out = Vec::new();
                for item in items {
                    match select_all(item, path) {
                        None => {}
                        Some(Value::Array(inner)) => out.extend(inner),
                        Some(other) => out.push(other),
                    }
                }
                Some(Value::Array(out))
            }
        },
        Value::Object(o) => match path {
            [] => Some(v.clone()),
            [PathElem::Key(k), rest @ ..] => select_all(o.get(k)?, rest),
            _ => None,
        },
        _ => {
            if path.is_empty() {
                Some(v.clone())
            } else {
                None
            }
        }
    }
}

/// `Index.mkBinders`: apply a binding lambda to the document. Errors mean
/// "no value" on the server, except that the emulator never hides its own
/// `not emulated` failures.
fn eval_binding(ctx: &mut Ctx, lam: &Lambda, doc: &Value, pos: &Position) -> R<Vec<Value>> {
    match apply_lambda(ctx, lam, doc.clone(), pos) {
        Ok(Value::Array(items)) => {
            Ok(items.into_iter().map(|i| if is_index_scalar(&i) { i } else { Value::Null }).collect())
        }
        Ok(v) if is_index_scalar(&v) => Ok(vec![v]),
        Ok(_) => Ok(vec![]),
        Err(errs) => {
            if errs.iter().any(|e| e.code == "not emulated" || e.code == "emulator limit exceeded") {
                Err(errs)
            } else {
                Ok(vec![])
            }
        }
    }
}

/// Cartesian product of the axes, minus the all-null combinations
/// (`BasicIndexer.combos` with `!isNull`).
fn combos(axes: &[Vec<Extracted>]) -> Vec<Vec<Extracted>> {
    let mut out: Vec<Vec<Extracted>> = vec![vec![]];
    for axis in axes {
        let mut next = Vec::with_capacity(out.len() * axis.len());
        for prefix in &out {
            for e in axis {
                let mut c = prefix.clone();
                c.push(e.clone());
                next.push(c);
            }
        }
        out = next;
    }
    out.retain(|c| c.is_empty() || !c.iter().all(|e| e.value == Value::Null && !e.placeholder));
    out
}

/// Index key equality: the server looks entries up by the encoded terms,
/// so `1` does not match `1.0` and strings match byte for byte.
fn term_key_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| term_key_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map(|w| term_key_eq(v, w)).unwrap_or(false))
        }
        _ => a == b,
    }
}

// ---------------------------------------------------------------------
// Set elements and their order
// ---------------------------------------------------------------------

/// Sort key standing in for the server's `DocID` (subID, then collection
/// ID). Document ids are numeric; schema documents (collections, indexes,
/// ...) get sequential ids in creation order on the server, which the
/// store's insertion order reproduces.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DocKey {
    sub: i128,
    coll: (u8, usize),
    tie: String,
}

fn native_rank(name: &str) -> usize {
    match name {
        "databases" => 0,
        "collections" => 1,
        "indexes" => 2,
        "keys" => 3,
        "tokens" => 4,
        "credentials" => 5,
        "functions" => 6,
        "roles" => 7,
        "access_providers" => 8,
        _ => 9,
    }
}

fn doc_key(db: &Database, r: &Ref) -> DocKey {
    let tie = r.display();
    if let Some(coll) = r.document_collection() {
        let sub = r.id.parse::<i128>().unwrap_or(i128::MAX);
        let pos = db.collections.get_index_of(coll).unwrap_or(usize::MAX);
        return DocKey { sub, coll: (1, pos), tie };
    }
    match r.collection.as_deref() {
        None => DocKey { sub: -1, coll: (0, native_rank(&r.id)), tie },
        Some(parent) if parent.is_native() => {
            let pos = match parent.id.as_str() {
                "collections" => db.collections.get_index_of(&r.id),
                "indexes" => db.indexes.get_index_of(&r.id),
                "functions" => db.functions.get_index_of(&r.id),
                "roles" => db.roles.get_index_of(&r.id),
                _ => r.id.parse::<usize>().ok(),
            };
            DocKey { sub: pos.map(|p| p as i128).unwrap_or(i128::MAX), coll: (0, native_rank(&parent.id)), tie }
        }
        Some(_) => DocKey { sub: i128::MAX, coll: (2, 0), tie },
    }
}

/// An index term (`IndexTerm`): a value with its field's `reverse` flag.
/// Refs carry their `DocKey` so they sort like the server's `DocIDV`.
#[derive(Clone, Debug)]
struct Term {
    v: Value,
    rev: bool,
    key: Option<DocKey>,
}

impl Term {
    fn new(db: &Database, v: Value, rev: bool) -> Term {
        let key = match &v {
            Value::Ref(r) => Some(doc_key(db, r)),
            _ => None,
        };
        Term { v, rev, key }
    }

    /// `IndexTerm` equality (`Distinct`): structural, and the flag.
    fn same(&self, other: &Term) -> bool {
        self.rev == other.rev && self.v == other.v
    }
}

/// Where a bound or element sits relative to documents with equal values:
/// `DocID.MinValue`, a document, or `DocID.MaxValue`.
#[derive(Clone, Debug)]
enum DocPos {
    Min,
    Doc(Ref, DocKey),
    Max,
}

/// A set element or bound (the server's `SetEvent` / `IndexTuple`).
#[derive(Clone, Debug)]
struct SEvent {
    values: Vec<Term>,
    doc: DocPos,
}

impl SEvent {
    /// `Event.MinValue`.
    fn min() -> SEvent {
        SEvent { values: vec![], doc: DocPos::Min }
    }

    /// `Event.MaxValue`.
    fn max() -> SEvent {
        SEvent { values: vec![], doc: DocPos::Max }
    }

    fn doc_ref(&self) -> Option<&Ref> {
        match &self.doc {
            DocPos::Doc(r, _) => Some(r),
            _ => None,
        }
    }

    /// `IndexTuple.cursor`: the values, then the document ref.
    fn cursor(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self.values.iter().map(|t| t.v.clone()).collect();
        if let Some(r) = self.doc_ref() {
            out.push(Value::Ref(r.clone()));
        }
        out
    }

    /// `IndexTuple.coveredValues`: the values, or the document ref.
    fn covered_terms(&self) -> Vec<Term> {
        if self.values.is_empty() {
            vec![doc_term(&self.doc)]
        } else {
            self.values.clone()
        }
    }

    /// `Literal.fromIndexTerms(coveredValues)`: what the set yields.
    fn covered(&self) -> Value {
        let mut terms = self.covered_terms();
        if terms.len() == 1 {
            terms.pop().unwrap().v
        } else {
            Value::Array(terms.into_iter().map(|t| t.v).collect())
        }
    }
}

/// `IndexTerm(DocIDV(doc))`.
fn doc_term(doc: &DocPos) -> Term {
    match doc {
        DocPos::Doc(r, k) => Term { v: Value::Ref(r.clone()), rev: false, key: Some(k.clone()) },
        DocPos::Min => Term {
            v: Value::Ref(Ref::native("")),
            rev: false,
            key: Some(DocKey { sub: i128::MIN, coll: (0, 0), tie: String::new() }),
        },
        DocPos::Max => Term {
            v: Value::Ref(Ref::native("")),
            rev: false,
            key: Some(DocKey { sub: i128::MAX, coll: (u8::MAX, usize::MAX), tie: String::new() }),
        },
    }
}

fn cmp_plain(a: &Term, b: &Term) -> Ordering {
    match (&a.key, &b.key) {
        (Some(x), Some(y)) => x.cmp(y),
        _ => ordering::compare(&a.v, &b.v),
    }
}

/// A reversed term is CBOR-tagged (tag 6): it sorts after numbers, bytes,
/// strings, arrays and objects, and before other tagged values, booleans
/// and null.
fn reversed_vs_plain(plain: &Value) -> Ordering {
    match plain {
        Value::Long(_)
        | Value::Double(_)
        | Value::Bytes(_)
        | Value::String(_)
        | Value::Array(_)
        | Value::Page { .. }
        | Value::Object(_) => Ordering::Greater,
        _ => Ordering::Less,
    }
}

/// `IndexTerm.compare`: `reverse` flips the comparison.
fn cmp_term(a: &Term, b: &Term) -> Ordering {
    match (a.rev, b.rev) {
        (false, false) => cmp_plain(a, b),
        (true, true) => cmp_plain(b, a),
        (true, false) => reversed_vs_plain(&b.v),
        (false, true) => reversed_vs_plain(&a.v).reverse(),
    }
}

/// `IndexTuple.compareValues` / `SetEvent.compareValues`: pairwise, then
/// the shorter list first.
fn cmp_values(a: &[Term], b: &[Term]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = cmp_term(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

fn cmp_doc(a: &DocPos, b: &DocPos) -> Ordering {
    let rank = |d: &DocPos| match d {
        DocPos::Min => 0,
        DocPos::Doc(..) => 1,
        DocPos::Max => 2,
    };
    match (a, b) {
        (DocPos::Doc(_, x), DocPos::Doc(_, y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// `IndexTuple.compare`: values, then document.
fn cmp_tuple(a: &SEvent, b: &SEvent) -> Ordering {
    cmp_values(&a.values, &b.values).then_with(|| cmp_doc(&a.doc, &b.doc))
}

/// `Event.SnapshotOrd` (at a fixed snapshot time): compares `terms`, i.e.
/// the values, or the document when there are none. Elements with equal
/// values but different documents are *equivalent*, which is what makes
/// `Union` & co. merge on values.
fn cmp_snapshot(a: &SEvent, b: &SEvent) -> Ordering {
    match (a.values.is_empty(), b.values.is_empty()) {
        (true, true) => cmp_doc(&a.doc, &b.doc),
        (false, false) => cmp_values(&a.values, &b.values),
        _ => cmp_values(&a.covered_terms(), &b.covered_terms()),
    }
}

fn sort_dir(elems: &mut [SEvent], asc: bool) {
    if asc {
        elems.sort_by(cmp_tuple);
    } else {
        elems.sort_by(|a, b| cmp_tuple(b, a));
    }
}

/// `EventSet.Shape`: the number of covered values plus one (the ref) and
/// their reverse flags. `zero` marks the server's `Shape.Zero` singleton
/// (lambda joins), which the cursor parser special-cases by identity.
#[derive(Clone, Debug)]
struct Shape {
    values: usize,
    reversed: Vec<bool>,
    zero: bool,
}

impl Shape {
    fn zero() -> Shape {
        Shape { values: 0, reversed: vec![], zero: true }
    }

    fn single() -> Shape {
        Shape { values: 1, reversed: vec![false], zero: false }
    }

    fn of_index(idx: &IndexDef) -> Shape {
        let mut reversed: Vec<bool> = idx.values.iter().map(|v| v.reverse).collect();
        reversed.push(false);
        Shape { values: idx.values.len() + 1, reversed, zero: false }
    }

    fn compatible(&self, other: &Shape) -> bool {
        self.reversed.iter().zip(&other.reversed).all(|(a, b)| a == b)
    }
}

fn index_def(ctx: &Ctx, r: &Ref, pos: &Position) -> R<IndexDef> {
    match ctx.db.indexes.get(&r.id) {
        Some(i) => Ok(i.clone()),
        None => one(FaunaError::unresolved_ref("index", &r.id, pos)),
    }
}

fn shape_of(ctx: &Ctx, set: &SetExpr, pos: &Position) -> R<Shape> {
    Ok(match set {
        SetExpr::Match { index, .. } => Shape::of_index(&index_def(ctx, index, pos)?),
        SetExpr::Documents(_) | SetExpr::Singleton(_) => Shape::single(),
        SetExpr::Union(sets) | SetExpr::Intersection(sets) | SetExpr::Difference(sets) => match sets.first() {
            Some(s) => shape_of(ctx, s, pos)?,
            None => Shape::zero(),
        },
        SetExpr::Distinct(s) | SetExpr::Filter { source: s, .. } => shape_of(ctx, s, pos)?,
        SetExpr::Range { set, .. } => shape_of(ctx, set, pos)?,
        SetExpr::Reverse(s) => {
            let inner = shape_of(ctx, s, pos)?;
            if inner.values == 1 {
                Shape { values: 2, reversed: vec![true, false], zero: false }
            } else {
                let n = inner.reversed.len().saturating_sub(1);
                let mut reversed: Vec<bool> = inner.reversed[..n].iter().map(|r| !r).collect();
                reversed.push(false);
                Shape { values: inner.values, reversed, zero: false }
            }
        }
        SetExpr::Join { with, .. } => match &**with {
            JoinTarget::Lambda(_) => Shape::zero(),
            JoinTarget::Index(r) => Shape::of_index(&index_def(ctx, r, pos)?),
        },
        SetExpr::Events(_) => return one(FaunaError::not_emulated("`Events` (document and set history)", pos)),
    })
}

// ---------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------

/// `SetPadding.pad`: extend a prefix bound with the extreme value for each
/// missing covered value (`IndexTerm.MinValue` is `Long.MinValue`,
/// `MaxValue` is `null`).
fn pad(e: &SEvent, flags: &[bool], floor: bool) -> SEvent {
    if flags.len() <= e.values.len() {
        return e.clone();
    }
    let mut values = e.values.clone();
    for &rev in &flags[e.values.len()..] {
        let v = if floor ^ rev { Value::Long(i64::MIN) } else { Value::Null };
        values.push(Term { v, rev, key: None });
    }
    SEvent { values, doc: e.doc.clone() }
}

/// An index-backed leaf (`IndexSet`, `DocumentsSet`, `SchemaSet`): the
/// storage range read between the padded bounds, inclusive.
fn leaf_snapshot(mut elems: Vec<SEvent>, flags: &[bool], from: &SEvent, to: &SEvent, asc: bool) -> Vec<SEvent> {
    let (max, min) = if asc { (to, from) } else { (from, to) };
    let max = pad(max, flags, false);
    let min = pad(min, flags, true);
    elems.retain(|e| cmp_tuple(&min, e) != Ordering::Greater && cmp_tuple(e, &max) != Ordering::Greater);
    sort_dir(&mut elems, asc);
    elems
}

/// All entries of `index` whose terms equal `terms`.
fn scan_match(ctx: &mut Ctx, idx: &IndexDef, terms: &[Value], pos: &Position) -> R<Vec<SEvent>> {
    let mut colls: Vec<String> = Vec::new();
    for s in &idx.source {
        match &s.collection {
            SourceCollection::Collection(c) => {
                if !colls.contains(c) {
                    colls.push(c.clone());
                }
            }
            SourceCollection::Wildcard => {
                for c in ctx.db.collections.keys() {
                    if !colls.contains(c) {
                        colls.push(c.clone());
                    }
                }
            }
        }
    }
    let flags: Vec<bool> = idx.values.iter().map(|v| v.reverse).collect();
    let mut out = Vec::new();
    for coll in colls {
        let docs: Vec<(String, Document)> =
            ctx.db.documents_in(&coll).map(|(id, d)| (id.clone(), d.clone())).collect();
        for (id, doc) in docs {
            let entries = index_entries(ctx, idx, &coll, &id, &doc, pos)?;
            let r = Ref::document(&coll, &id);
            let key = doc_key(ctx.db, &r);
            for e in entries {
                if e.terms.len() != terms.len() || !e.terms.iter().zip(terms).all(|(a, b)| term_key_eq(a, b)) {
                    continue;
                }
                let values =
                    e.values.into_iter().zip(&flags).map(|(v, &rev)| Term::new(ctx.db, v, rev)).collect();
                out.push(SEvent { values, doc: DocPos::Doc(r.clone(), key.clone()) });
            }
        }
    }
    Ok(out)
}

fn element(db: &Database, r: Ref) -> SEvent {
    let key = doc_key(db, &r);
    SEvent { values: vec![], doc: DocPos::Doc(r, key) }
}

/// The documents of `Documents(coll)`; native collections list the schema
/// documents (`SchemaSet`).
fn documents_elems(ctx: &Ctx, coll: &Ref, pos: &Position) -> R<Vec<SEvent>> {
    let db = &*ctx.db;
    if coll.is_native() {
        let refs: Vec<Ref> = match coll.id.as_str() {
            "collections" => db.collections.keys().map(|n| Ref::collection(n)).collect(),
            "indexes" => db.indexes.keys().map(|n| Ref::index(n)).collect(),
            "functions" => db.functions.keys().map(|n| Ref::function(n)).collect(),
            // `ServerReadOnlyPermissions` has no unrestricted collection
            // read, so a read-only key cannot list these.
            "roles" | "databases" | "keys" | "access_providers" if db.read_only => {
                return one(FaunaError::permission_denied(pos))
            }
            "roles" => db.roles.keys().map(|n| Ref::role(n)).collect(),
            // The emulator cannot create any of these.
            "databases" | "keys" | "tokens" | "access_providers" => vec![],
            other => {
                return one(FaunaError::not_emulated(&format!("the `{}` native collection as a set", other), pos))
            }
        };
        return Ok(refs.into_iter().map(|r| element(db, r)).collect());
    }
    if !coll.is_collection_ref() {
        return Ok(vec![]);
    }
    Ok(db.documents_in(&coll.id).map(|(id, _)| element(db, Ref::document(&coll.id, id))).collect())
}

/// The order in which `scala.collection.immutable.HashMap[Int, _]` (a
/// CHAMP trie over `Hashing.improve(key)`) iterates `keys`. The server's
/// set algebra picks the first stream in this order on ties.
fn scala_int_map_order(keys: &[usize]) -> Vec<usize> {
    fn improve(h: i32) -> u32 {
        let mut h = h.wrapping_add(!(h.wrapping_shl(9)));
        h ^= ((h as u32) >> 14) as i32;
        h = h.wrapping_add(h.wrapping_shl(4));
        (h ^ (((h as u32) >> 10) as i32)) as u32
    }
    fn walk(keys: Vec<(usize, u32)>, shift: u32, out: &mut Vec<usize>) {
        let mut groups: BTreeMap<u32, Vec<(usize, u32)>> = BTreeMap::new();
        for k in keys {
            groups.entry((k.1 >> shift) & 31).or_default().push(k);
        }
        for g in groups.values() {
            if g.len() == 1 {
                out.push(g[0].0);
            }
        }
        for g in groups.into_values() {
            if g.len() > 1 {
                if shift >= 30 {
                    out.extend(g.into_iter().map(|k| k.0));
                } else {
                    walk(g, shift + 5, out);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(keys.len());
    walk(keys.iter().map(|&k| (k, improve(k as i32))).collect(), 0, &mut out);
    out
}

/// A group of equivalent elements (`SnapshotOrd`) gathered by the merge,
/// as `(stream, elements in stream order)`.
type Group = Vec<(usize, Vec<SEvent>)>;

/// Merge sorted streams into groups of equivalent elements, in merged
/// order (`Page.mergeReduce` over `SnapshotOrdering`).
fn merge_groups(streams: Vec<Vec<SEvent>>, asc: bool) -> Vec<Group> {
    let mut all: Vec<(usize, SEvent)> =
        streams.into_iter().enumerate().flat_map(|(i, s)| s.into_iter().map(move |e| (i, e))).collect();
    all.sort_by(|a, b| if asc { cmp_snapshot(&a.1, &b.1) } else { cmp_snapshot(&b.1, &a.1) });
    let mut groups: Vec<Group> = Vec::new();
    let mut last: Option<SEvent> = None;
    for (i, e) in all {
        let same = last.as_ref().map(|l| cmp_snapshot(l, &e) == Ordering::Equal).unwrap_or(false);
        if !same {
            groups.push(Vec::new());
        }
        last = Some(e.clone());
        let g = groups.last_mut().unwrap();
        match g.iter_mut().find(|(s, _)| *s == i) {
            Some((_, es)) => es.push(e),
            None => g.push((i, vec![e])),
        }
    }
    groups
}

fn snapshot(ctx: &mut Ctx, set: &SetExpr, from: &SEvent, to: &SEvent, asc: bool, pos: &Position) -> R<Vec<SEvent>> {
    match set {
        SetExpr::Match { index, terms } => {
            let idx = index_def(ctx, index, pos)?;
            let flags: Vec<bool> = idx.values.iter().map(|v| v.reverse).collect();
            let elems = scan_match(ctx, &idx, terms, pos)?;
            Ok(leaf_snapshot(elems, &flags, from, to, asc))
        }
        SetExpr::Documents(coll) => {
            let elems = documents_elems(ctx, coll, pos)?;
            Ok(leaf_snapshot(elems, &[], from, to, asc))
        }
        // `DocEventSet.snapshot`: the document, if it exists and its id is
        // within the bounds' document ids.
        SetExpr::Singleton(r) => {
            let e = element(ctx.db, r.clone());
            let (min, max) = if asc { (from, to) } else { (to, from) };
            let inside =
                cmp_doc(&min.doc, &e.doc) != Ordering::Greater && cmp_doc(&e.doc, &max.doc) != Ordering::Greater;
            Ok(if inside && ctx.db.exists_by_ref(r) { vec![e] } else { vec![] })
        }
        SetExpr::Union(sets) => {
            let mut streams = Vec::with_capacity(sets.len());
            for s in sets {
                streams.push(snapshot(ctx, s, from, to, asc, pos)?);
            }
            // The stream with the most elements in each group wins (first
            // in hash-map order on ties); `MergeElemsIfPresent` collects a
            // group's elements newest-first.
            let mut out = Vec::new();
            for g in merge_groups(streams, asc) {
                let mut best: Option<&Vec<SEvent>> = None;
                for s in scala_int_map_order(&g.iter().map(|(s, _)| *s).collect::<Vec<_>>()) {
                    let es = &g.iter().find(|(i, _)| *i == s).unwrap().1;
                    if best.map(|b| b.len() < es.len()).unwrap_or(true) {
                        best = Some(es);
                    }
                }
                if let Some(es) = best {
                    out.extend(es.iter().rev().cloned());
                }
            }
            Ok(out)
        }
        SetExpr::Intersection(sets) => {
            // `Intersection.snapshot` restarts every source at the bound's
            // values (ignoring its document); `Paginate` drops what is
            // before the actual cursor.
            let start = if from.values.is_empty() {
                from.clone()
            } else {
                SEvent { values: from.values.clone(), doc: if asc { DocPos::Min } else { DocPos::Max } }
            };
            let mut streams = Vec::with_capacity(sets.len());
            for s in sets {
                streams.push(snapshot(ctx, s, &start, to, asc, pos)?);
            }
            let n = streams.len();
            let first = scala_int_map_order(&(0..n).collect::<Vec<_>>()).first().copied().unwrap_or(0);
            // One element per source per step while every source has an
            // equivalent head: the minimum count, taken from the first
            // stream in hash-map order.
            let mut out = Vec::new();
            for g in merge_groups(streams, asc) {
                if g.len() != n {
                    continue;
                }
                let m = g.iter().map(|(_, es)| es.len()).min().unwrap_or(0);
                let es = &g.iter().find(|(i, _)| *i == first).unwrap().1;
                out.extend(es.iter().take(m).cloned());
            }
            Ok(out)
        }
        SetExpr::Difference(sets) => {
            let mut streams = Vec::with_capacity(sets.len());
            for s in sets {
                streams.push(snapshot(ctx, s, from, to, asc, pos)?);
            }
            let mut out = Vec::new();
            for g in merge_groups(streams, asc) {
                if g.len() == 1 && g[0].0 == 0 {
                    out.extend(g[0].1.iter().rev().cloned());
                }
            }
            Ok(out)
        }
        SetExpr::Distinct(inner) => {
            // Consecutive elements with equal values (or documents, without
            // values) collapse to the first.
            let elems = snapshot(ctx, inner, from, to, asc, pos)?;
            let mut out: Vec<SEvent> = Vec::with_capacity(elems.len());
            for e in elems {
                let dup = out
                    .last()
                    .map(|p| {
                        let (a, b) = (p.covered_terms(), e.covered_terms());
                        a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| x.same(y))
                    })
                    .unwrap_or(false);
                if !dup {
                    out.push(e);
                }
            }
            Ok(out)
        }
        SetExpr::Filter { lambda, source } => {
            let elems = snapshot(ctx, source, from, to, asc, pos)?;
            let fp = pos.key("filter");
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                match apply_lambda(ctx, lambda, e.covered(), &fp)? {
                    Value::Bool(true) => out.push(e),
                    Value::Bool(false) => {}
                    other => return one(FaunaError::invalid_argument(&[Type::Boolean], other.rtype(), &fp)),
                }
            }
            Ok(out)
        }
        SetExpr::Reverse(inner) => {
            // `ReverseSet.snapshot`: read the source the other way round
            // between the re-shaped bounds, and flip every element's
            // reverse flags (value-less elements get their ref as a
            // reversed value).
            let inner_from = reshape_bound(ctx.db, from, !asc);
            let inner_to = reshape_bound(ctx.db, to, asc);
            let elems = snapshot(ctx, inner, &inner_from, &inner_to, !asc, pos)?;
            Ok(elems.into_iter().map(reshape_elem).collect())
        }
        SetExpr::Range { set: source, from: lo, to: hi } => {
            let shape = shape_of(ctx, source, pos)?;
            let lower = parse_cursor(ctx, &shape, lo, true, &pos.key("from"))?.1;
            let upper = parse_cursor(ctx, &shape, hi, false, &pos.key("to"))?.1;
            let (from0, to0) = if asc {
                (
                    if compare_cursors(from, &lower, true) == Ordering::Less { &lower } else { from },
                    if compare_cursors(to, &upper, false) == Ordering::Greater { &upper } else { to },
                )
            } else {
                (
                    if compare_cursors(from, &upper, false) == Ordering::Greater { &upper } else { from },
                    if compare_cursors(to, &lower, true) == Ordering::Less { &lower } else { to },
                )
            };
            let (from0, to0) = (from0.clone(), to0.clone());
            snapshot(ctx, source, &from0, &to0, asc, pos)
        }
        SetExpr::Join { source, with } => {
            // `AbstractJoin.snapshot`: every source element (whole set)
            // selects a target set; the targets are read between the
            // bounds and merged, one element per equivalence group.
            let (f, t) = if asc { (SEvent::min(), SEvent::max()) } else { (SEvent::max(), SEvent::min()) };
            let sources = snapshot(ctx, source, &f, &t, asc, pos)?;
            let wp = pos.key("with");
            let mut streams = Vec::with_capacity(sources.len());
            for e in sources {
                let target = match &**with {
                    JoinTarget::Lambda(l) => {
                        let v = apply_lambda(ctx, l, e.covered(), &wp)?;
                        cast_set(ctx, &v, &wp)?
                    }
                    JoinTarget::Index(r) => SetExpr::Match {
                        index: r.clone(),
                        terms: e.covered_terms().into_iter().map(|t| t.v).collect(),
                    },
                };
                streams.push(snapshot(ctx, &target, from, to, asc, pos)?);
            }
            Ok(merge_groups(streams, asc)
                .into_iter()
                .filter_map(|g| g.into_iter().last().and_then(|(_, mut es)| es.pop()))
                .collect())
        }
        SetExpr::Events(_) => one(FaunaError::not_emulated("`Events` (document and set history)", pos)),
    }
}

/// `ReverseSet.reshapeEvent`.
fn reshape_bound(db: &Database, e: &SEvent, floor: bool) -> SEvent {
    let base = if floor { SEvent::min() } else { SEvent::max() };
    if let [t] = e.values.as_slice() {
        match &t.v {
            Value::Ref(r) => return SEvent { values: vec![], doc: DocPos::Doc(r.clone(), doc_key(db, r)) },
            Value::Long(0) => return SEvent { values: vec![], doc: DocPos::Min },
            Value::Null => return SEvent { values: vec![], doc: DocPos::Max },
            _ => {}
        }
    }
    let values = e.values.iter().map(|t| Term { rev: !t.rev, ..t.clone() }).collect();
    match &e.doc {
        DocPos::Doc(..) => SEvent { values, doc: e.doc.clone() },
        _ => SEvent { values, doc: base.doc },
    }
}

/// `ReverseSet.reshapeElem`.
fn reshape_elem(e: SEvent) -> SEvent {
    if e.values.is_empty() {
        let mut t = doc_term(&e.doc);
        t.rev = true;
        SEvent { values: vec![t], doc: e.doc }
    } else {
        let values = e.values.into_iter().map(|t| Term { rev: !t.rev, ..t }).collect();
        SEvent { values, doc: e.doc }
    }
}

/// `RangeSet.compareCursors`: the document first, then the values; a
/// longer cursor is greater as a floor and smaller as a ceiling.
fn compare_cursors(a: &SEvent, b: &SEvent, floor: bool) -> Ordering {
    let o = cmp_doc(&a.doc, &b.doc);
    if o != Ordering::Equal {
        return o;
    }
    for (x, y) in a.values.iter().zip(&b.values) {
        let o = cmp_term(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    match a.values.len().cmp(&b.values.len()) {
        Ordering::Greater => {
            if floor {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        Ordering::Less => {
            if floor {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        Ordering::Equal => Ordering::Equal,
    }
}

// ---------------------------------------------------------------------
// Cursors and Paginate
// ---------------------------------------------------------------------

/// `CursorParser.parseArray`: a scalar or array cursor against a set's
/// shape. Up to `values` elements make a prefix cursor (the document
/// bound is the minimum for a `floor`, the maximum otherwise); one more
/// element is the document, which must be a ref to be one (anything that
/// sorts before refs means "before every document", anything after means
/// "after every document"). Returns the cursor as the page echoes it (an
/// array) and the bound.
fn parse_cursor(ctx: &Ctx, shape: &Shape, cursor: &Value, floor: bool, pos: &Position) -> R<(Value, SEvent)> {
    if let Value::Ref(r) = cursor {
        check_ref(ctx.db, r, pos)?;
    }
    let terms: Vec<Value> = match cursor {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    let echo = Value::Array(terms.clone());
    let k = shape.values as isize - 1;
    let split = if k <= 0 { 0 } else { (k as usize).min(terms.len()) };
    let (t, c) = terms.split_at(split);
    let mk = |vals: &[Value], flags: &[bool]| -> Vec<Term> {
        vals.iter()
            .zip(flags.iter().chain(std::iter::repeat(&false)))
            .map(|(v, &r)| Term::new(ctx.db, v.clone(), r))
            .collect()
    };
    let event = if c.is_empty() {
        let n = t.len().min(shape.reversed.len());
        SEvent { values: mk(&t[..n], &shape.reversed), doc: if floor { DocPos::Min } else { DocPos::Max } }
    } else {
        let (values, head) = if shape.zero {
            let (vals, last) = c.split_at(c.len() - 1);
            (mk(vals, &[]), &last[0])
        } else {
            let n = t.len().min(shape.reversed.len());
            (mk(&t[..n], &shape.reversed), &c[0])
        };
        let doc = match head {
            Value::Ref(r) => DocPos::Doc(r.clone(), doc_key(ctx.db, r)),
            other => {
                if ordering::compare(other, &Value::Ref(Ref::native(""))) == Ordering::Greater {
                    DocPos::Max
                } else {
                    DocPos::Min
                }
            }
        };
        SEvent { values, doc }
    };
    Ok((echo, event))
}

/// Where a `Paginate` starts.
pub enum PageCursor {
    /// No cursor: from the beginning, ascending.
    None,
    /// `after` (or `cursor: {after}`): ascending, inclusive.
    After(Value, Position),
    /// `before` (or `cursor: {before}`): descending, exclusive.
    Before(Value, Position),
}

/// `ReadAdaptor.paginate` / `PaginateArray`. A plain document ref is
/// paginated as `SetExpr::Singleton` (the server's `DocSet`). `size` is
/// the raw `size` argument; `pos` is the position of the set argument.
pub fn paginate(ctx: &mut Ctx, set: &SetExpr, cursor: PageCursor, size: Option<i64>, pos: &Position) -> R<Value> {
    // `DefaultPageSize`, `MaxPageSize`; `.toInt` truncates like `as i32`.
    let size: i32 = match size {
        None => 64,
        Some(s) => s.min(100_000) as i32,
    };
    let shape = shape_of(ctx, set, pos)?;
    let (asc, parsed) = match cursor {
        PageCursor::None => (true, None),
        PageCursor::After(v, p) => (true, Some(parse_cursor(ctx, &shape, &v, true, &p)?)),
        PageCursor::Before(v, p) => (false, Some(parse_cursor(ctx, &shape, &v, true, &p)?)),
    };
    let (orig, from) = match parsed {
        Some((echo, ev)) => (Some(echo), ev),
        None => (None, if asc { SEvent::min() } else { SEvent::max() }),
    };
    let to = if asc { SEvent::max() } else { SEvent::min() };
    let elems = snapshot(ctx, set, &from, &to, asc, pos)?;
    // Ascending pages include the cursor element, descending ones do not.
    let page: Vec<SEvent> =
        elems.into_iter().skip_while(|e| asc ^ (cmp_tuple(e, &from) != Ordering::Less)).collect();
    let has_next = size >= 0 && page.len() > size as usize;
    let next = if size == 0 {
        orig.clone()
    } else if has_next {
        let i = if asc { size as usize } else { size as usize - 1 };
        Some(Value::Array(page[i].cursor()))
    } else {
        None
    };
    let mut trimmed: Vec<SEvent> = page.into_iter().take(size.max(0) as usize).collect();
    if !asc {
        trimmed.reverse();
    }
    let (before, after) = if asc { (orig, next) } else { (next, orig) };
    Ok(Value::Page {
        data: trimmed.iter().map(|e| e.covered()).collect(),
        before: before.map(Box::new),
        after: after.map(Box::new),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scala_hash_map_order_is_stable() {
        let o = scala_int_map_order(&[0, 1]);
        assert_eq!(o.len(), 2);
        let mut sorted = o.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1]);
    }

    #[test]
    fn reversed_terms_sort_backwards() {
        let db = Database::new();
        let a = Term::new(&db, Value::Long(1), true);
        let b = Term::new(&db, Value::Long(2), true);
        assert_eq!(cmp_term(&a, &b), Ordering::Greater);
        let n = Term::new(&db, Value::Null, true);
        assert_eq!(cmp_term(&n, &a), Ordering::Less);
    }

    #[test]
    fn select_all_traverses_arrays() {
        let doc = Value::obj(vec![(
            "data",
            Value::obj(vec![(
                "foo",
                Value::Array(vec![
                    Value::obj(vec![("baz", Value::Long(1))]),
                    Value::obj(vec![("baz", Value::Long(2))]),
                ]),
            )]),
        )]);
        let path = vec![PathElem::Key("data".into()), PathElem::Key("foo".into()), PathElem::Key("baz".into())];
        assert_eq!(select_all(&doc, &path), Some(Value::Array(vec![Value::Long(1), Value::Long(2)])));
    }

    #[test]
    fn combos_drop_all_null() {
        let e = |v: Value| Extracted { value: v, placeholder: false };
        let axes = vec![vec![e(Value::Null), e(Value::Long(1))], vec![e(Value::Null)]];
        let c = combos(&axes);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0][0].value, Value::Long(1));
        assert_eq!(combos(&[]).len(), 1);
    }

    fn obj(entries: Vec<(&str, Value)>) -> crate::value::Object {
        entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn field(path: &[&str]) -> Value {
        Value::obj(vec![("field", Value::Array(path.iter().map(|p| Value::str(p)).collect()))])
    }

    #[test]
    fn index_entries_expand_arrays_and_drop_all_null_combinations() {
        let mut db = Database::new();
        let ts = crate::value::Timestamp::of_micros(7);
        let pos = Position::root();
        db.create_collection(&obj(vec![("name", Value::str("users"))]), ts, &pos).unwrap();
        db.commit_pending();
        db.create_index(
            &obj(vec![
                ("name", Value::str("idx")),
                ("source", Value::Ref(Ref::collection("users"))),
                ("terms", Value::Array(vec![field(&["data", "tags"]), field(&["ts"])])),
                ("values", Value::Array(vec![field(&["data", "n"]), field(&["ts"])])),
            ]),
            ts,
            &pos,
        )
        .unwrap();
        db.commit_pending();
        let idx = db.indexes["idx"].clone();
        let data = Value::obj(vec![
            ("tags", Value::Array(vec![Value::str("a"), Value::str("a"), Value::str("b")])),
            ("n", Value::Null),
        ]);
        let doc = Document { ts, fields: obj(vec![("data", data)]) };
        let mut ctx = Ctx::new(&mut db, ts);
        let entries = index_entries(&mut ctx, &idx, "users", "1", &doc, &pos).unwrap();
        // tags: a, b (deduplicated); ts as a term is stored as null; the
        // values are [null, 7] (not all null, so kept).
        let entry = |t: &str| IndexEntry {
            terms: vec![Value::str(t), Value::Null],
            values: vec![Value::Null, Value::Long(7)],
        };
        assert_eq!(entries, vec![entry("a"), entry("b")]);
        // Documents of other collections are not in the index.
        assert_eq!(index_entries(&mut ctx, &idx, "other", "1", &doc, &pos).unwrap(), vec![]);
    }

    #[test]
    fn lambda_effects() {
        let effect = |j: serde_json::Value| expr_effect(&j);
        assert_eq!(effect(serde_json::json!({"match": {"index": "x"}, "terms": 1})), Effect::Pure);
        assert_eq!(effect(serde_json::json!({"get": {"var": "x"}})), Effect::Read);
        assert_eq!(effect(serde_json::json!({"select": ["a"], "from": {"get": {"var": "x"}}})), Effect::Read);
        assert_eq!(effect(serde_json::json!({"create": {"collection": "c"}})), Effect::Write);
        assert_eq!(effect(serde_json::json!({"call": "f", "arguments": []})), Effect::Write);
        // A lambda value is pure, unless its body runs as part of the form.
        assert_eq!(effect(serde_json::json!({"lambda": "x", "expr": {"get": {"var": "x"}}})), Effect::Pure);
        assert_eq!(
            effect(serde_json::json!({"map": {"lambda": "x", "expr": {"get": {"var": "x"}}}, "collection": []})),
            Effect::Read
        );
        // Literal keys are not forms.
        assert_eq!(effect(serde_json::json!({"let": [{"get": 1}], "in": {"object": {"create": 2}}})), Effect::Pure);
        assert_eq!(effect(serde_json::json!({"select": ["a"], "from": {"object": {}}, "all": true})), Effect::Pure);
    }

    #[test]
    fn array_algebra() {
        let a = |xs: &[i64]| xs.iter().map(|x| Value::Long(*x)).collect::<Vec<_>>();
        assert_eq!(array_union(vec![a(&[3, 2, 2, 1]), a(&[6, 5, 9, 1])]), a(&[3, 2, 2, 1, 6, 5, 9]));
        assert_eq!(array_union(vec![a(&[2, 2]), a(&[2, 2, 2])]), a(&[2, 2, 2]));
        assert_eq!(array_intersection(vec![a(&[3, 2, 2, 1]), a(&[2, 2, 1, 0])]), a(&[2, 2, 1]));
        assert_eq!(array_difference(vec![a(&[3, 2, 2, 1, 3]), a(&[3])]), a(&[2, 2, 1, 3]));
        assert_eq!(array_distinct(a(&[4, 4, 2, 2, 3, 3, 1, 1])), a(&[4, 2, 3, 1]));
    }
}
