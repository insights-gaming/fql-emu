//! In-memory document store.
//!
//! Deliberately naive: documents live in `BTreeMap`s and every index
//! lookup is a scan over the source collections. There is a hard cap on
//! the number of documents so nobody mistakes this for a database. A
//! fork is a `Clone`.

use crate::error::{one, FaunaError, Position, R, ValidationFailure};
use crate::eval::cast::PathElem;
use crate::value::{Lambda, Object, Ref, Timestamp, Value};
use indexmap::IndexMap;
use std::collections::BTreeMap;

/// Default document cap. Fixtures bigger than this should use the real
/// container.
pub const DEFAULT_MAX_DOCUMENTS: usize = 5_000;

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_documents: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_documents: DEFAULT_MAX_DOCUMENTS }
    }
}

/// Transaction clock. `Fixed` makes `Now()` and document `ts` values
/// deterministic; `Auto` advances one microsecond per transaction so
/// timestamps stay monotonic without touching the wall clock.
#[derive(Clone, Debug)]
pub enum Clock {
    Fixed(Timestamp),
    Auto { last: Timestamp },
}

impl Clock {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Timestamp {
        match self {
            Clock::Fixed(t) => *t,
            Clock::Auto { last } => {
                *last = Timestamp::of_micros(last.micros() + 1);
                *last
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub ts: Timestamp,
    /// Everything except `ref` and `ts`: `data`, `ttl`, `credentials`, ...
    pub fields: Object,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CollectionDef {
    pub name: String,
    pub ts: Timestamp,
    pub history_days: Option<i64>,
    pub ttl_days: Option<i64>,
    pub data: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TermPath {
    Field(Vec<PathElem>),
    Binding(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TermCfg {
    pub path: TermPath,
    pub reverse: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceCollection {
    Collection(String),
    /// `"_"` wildcard: every collection.
    Wildcard,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexSource {
    pub collection: SourceCollection,
    pub fields: IndexMap<String, Lambda>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub ts: Timestamp,
    pub source: Vec<IndexSource>,
    pub terms: Vec<TermCfg>,
    pub values: Vec<TermCfg>,
    pub unique: bool,
    pub serialized: bool,
    pub data: Option<Value>,
    /// The raw `source` value as given, for rendering `Get(Index(..))`.
    pub source_value: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub name: String,
    pub ts: Timestamp,
    pub body: Lambda,
    pub role: Option<Value>,
    pub data: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RoleDef {
    pub name: String,
    pub ts: Timestamp,
    pub fields: Object,
}

/// A schema object created in the current transaction. The server does not
/// resolve such names until the transaction commits (`CreateCollection`
/// followed by `Collection("x")` in the same query is `invalid ref`), so
/// creations are parked here and applied by `commit_pending`.
#[derive(Clone, Debug)]
pub enum PendingSchema {
    /// Create or update (applied by name, so an update is a replacement).
    Collection(CollectionDef),
    Index(IndexDef),
    Function(FunctionDef),
    Role(RoleDef),
    DeleteCollection(String),
    DeleteIndex(String),
    DeleteFunction(String),
    DeleteRole(String),
}

#[derive(Clone, Debug)]
pub struct Database {
    /// Schema objects created in the current transaction, invisible to name
    /// resolution until `commit_pending`.
    pub pending: Vec<PendingSchema>,
    pub collections: IndexMap<String, CollectionDef>,
    pub indexes: IndexMap<String, IndexDef>,
    pub functions: IndexMap<String, FunctionDef>,
    pub roles: IndexMap<String, RoleDef>,
    /// collection name -> document id -> document
    pub documents: BTreeMap<String, BTreeMap<String, Document>>,
    pub doc_count: usize,
    pub next_id: u64,
    pub clock: Clock,
    pub identity: Option<Ref>,
    pub limits: Limits,
}

impl Default for Database {
    fn default() -> Self {
        Database::new()
    }
}

/// Reserved names that cannot be used for collections, indexes, etc.
pub const RESERVED_NAMES: &[&str] = &[
    "events",
    "sets",
    "self",
    "documents",
    "_",
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

/// Storage type name of a value (`fauna.storage.ir.IRType`), as used in
/// validation-failure descriptions ("Invalid type Long, expected type
/// Map."). Objects are `Map`, lambdas are `Query`.
pub fn ir_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Long(_) => "Long",
        Value::Double(_) => "Double",
        Value::String(_) => "String",
        Value::Bytes(_) => "Bytes",
        Value::Time(_) => "Time",
        Value::Date(_) => "Date",
        Value::Ref(_) => "Ref",
        Value::Lambda(_) => "Query",
        Value::Array(_) => "Array",
        Value::Object(_) | Value::Page { .. } | Value::Set(_) => "Map",
    }
}

impl Database {
    pub fn new() -> Database {
        Database {
            pending: Vec::new(),
            collections: IndexMap::new(),
            indexes: IndexMap::new(),
            functions: IndexMap::new(),
            roles: IndexMap::new(),
            documents: BTreeMap::new(),
            doc_count: 0,
            // Fauna ids are 18-digit snowflakes; start somewhere plausible.
            next_id: 300_000_000_000_000_000,
            clock: Clock::Auto { last: Timestamp::of_micros(1_600_000_000_000_000) },
            identity: None,
            limits: Limits::default(),
        }
    }

    pub fn fork(&self) -> Database {
        self.clone()
    }

    /// Apply the schema objects created in this transaction. Called on
    /// commit; a failed transaction drops the working copy, pending and all.
    pub fn commit_pending(&mut self) {
        for p in std::mem::take(&mut self.pending) {
            match p {
                PendingSchema::Collection(c) => {
                    self.collections.insert(c.name.clone(), c);
                }
                PendingSchema::Index(i) => {
                    self.indexes.insert(i.name.clone(), i);
                }
                PendingSchema::Function(f) => {
                    self.functions.insert(f.name.clone(), f);
                }
                PendingSchema::Role(r) => {
                    self.roles.insert(r.name.clone(), r);
                }
                PendingSchema::DeleteCollection(name) => {
                    self.collections.shift_remove(&name);
                    if let Some(docs) = self.documents.remove(&name) {
                        self.doc_count -= docs.len();
                    }
                    // `indexesWithSoleSource`: indexes over only this collection go too.
                    self.indexes.retain(|_, idx| {
                        let sole = idx.source.iter().all(|s| s.collection == SourceCollection::Collection(name.clone()));
                        !sole || idx.source.is_empty()
                    });
                }
                PendingSchema::DeleteIndex(name) => {
                    self.indexes.shift_remove(&name);
                }
                PendingSchema::DeleteFunction(name) => {
                    self.functions.shift_remove(&name);
                }
                PendingSchema::DeleteRole(name) => {
                    self.roles.shift_remove(&name);
                }
            }
        }
    }

    /// True if a schema object of this name was deleted earlier in the
    /// transaction (its name may be reused by a create in the same
    /// transaction, as on the server).
    fn pending_deleted(&self, kind: &str, name: &str) -> bool {
        self.pending.iter().any(|p| match (kind, p) {
            ("collection", PendingSchema::DeleteCollection(n)) => n == name,
            // Deleting a collection takes its sole-source indexes with it.
            ("index", PendingSchema::DeleteCollection(c)) => self.indexes.get(name).is_some_and(|idx| {
                !idx.source.is_empty()
                    && idx.source.iter().all(|s| s.collection == SourceCollection::Collection(c.clone()))
            }),
            ("index", PendingSchema::DeleteIndex(n)) => n == name,
            ("function", PendingSchema::DeleteFunction(n)) => n == name,
            ("role", PendingSchema::DeleteRole(n)) => n == name,
            _ => false,
        })
    }

    fn pending_has(&self, kind: &str, name: &str) -> bool {
        self.pending.iter().any(|p| match (kind, p) {
            ("collection", PendingSchema::Collection(c)) => c.name == name,
            ("index", PendingSchema::Index(i)) => i.name == name,
            ("function", PendingSchema::Function(f)) => f.name == name,
            ("role", PendingSchema::Role(r)) => r.name == name,
            _ => false,
        })
    }

    pub fn new_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        id.to_string()
    }

    // ---- documents ----

    pub fn get_document(&self, collection: &str, id: &str) -> Option<&Document> {
        self.documents.get(collection)?.get(id)
    }

    pub fn exists_document(&self, collection: &str, id: &str) -> bool {
        self.get_document(collection, id).is_some()
    }

    /// Insert or replace. Enforces the fixture cap on insert.
    pub fn put_document(&mut self, collection: &str, id: &str, doc: Document, pos: &Position) -> R<()> {
        let coll = self.documents.entry(collection.to_string()).or_default();
        if !coll.contains_key(id) {
            if self.doc_count >= self.limits.max_documents {
                return one(FaunaError::emulator_limit("document count", self.limits.max_documents, pos));
            }
            self.doc_count += 1;
        }
        coll.insert(id.to_string(), doc);
        Ok(())
    }

    /// Fixture hook: insert a document directly, bypassing `Create` (no
    /// validation, no unique checks, no id minting). `fields` is everything
    /// except `ref` and `ts` (`{"data": ...}`). The collection must exist.
    pub fn seed_document(&mut self, collection: &str, id: &str, fields: Object, ts: Timestamp) -> R<()> {
        let pos = Position::root();
        if !self.collections.contains_key(collection) {
            return one(FaunaError::unresolved_ref("collection", collection, &pos));
        }
        self.put_document(collection, id, Document { ts, fields }, &pos)
    }

    pub fn remove_document(&mut self, collection: &str, id: &str) -> Option<Document> {
        let removed = self.documents.get_mut(collection)?.remove(id);
        if removed.is_some() {
            self.doc_count -= 1;
        }
        removed
    }

    pub fn documents_in(&self, collection: &str) -> impl Iterator<Item = (&String, &Document)> {
        self.documents.get(collection).into_iter().flat_map(|m| m.iter())
    }

    /// Render a user document as the object `Get` returns:
    /// `{ref, ts, data, ...}`.
    pub fn render_document(&self, collection: &str, id: &str, doc: &Document) -> Value {
        let mut o = Object::with_capacity(doc.fields.len() + 2);
        o.insert("ref".into(), Value::Ref(Ref::document(collection, id)));
        o.insert("ts".into(), Value::Long(doc.ts.micros()));
        for (k, v) in &doc.fields {
            o.insert(k.clone(), v.clone());
        }
        Value::Object(o)
    }

    // ---- schema lookups ----

    pub fn render_collection(&self, c: &CollectionDef) -> Value {
        let mut o = Object::new();
        o.insert("ref".into(), Value::Ref(Ref::collection(&c.name)));
        o.insert("ts".into(), Value::Long(c.ts.micros()));
        o.insert("history_days".into(), c.history_days.map(Value::Long).unwrap_or(Value::Null));
        o.insert("name".into(), Value::String(c.name.clone()));
        if let Some(ttl) = c.ttl_days {
            o.insert("ttl_days".into(), Value::Long(ttl));
        }
        if let Some(d) = &c.data {
            o.insert("data".into(), d.clone());
        }
        Value::Object(o)
    }

    pub fn render_index(&self, i: &IndexDef) -> Value {
        let term_cfg = |t: &TermCfg| {
            let mut o = Object::new();
            match &t.path {
                TermPath::Field(p) => o.insert(
                    "field".into(),
                    Value::Array(p.iter().map(|e| e.to_value()).collect()),
                ),
                TermPath::Binding(b) => o.insert("binding".into(), Value::String(b.clone())),
            };
            if t.reverse {
                o.insert("reverse".into(), Value::Bool(true));
            }
            Value::Object(o)
        };
        let mut o = Object::new();
        o.insert("ref".into(), Value::Ref(Ref::index(&i.name)));
        o.insert("ts".into(), Value::Long(i.ts.micros()));
        o.insert("active".into(), Value::Bool(true));
        o.insert("serialized".into(), Value::Bool(i.serialized));
        o.insert("name".into(), Value::String(i.name.clone()));
        o.insert("source".into(), i.source_value.clone());
        if !i.terms.is_empty() {
            o.insert("terms".into(), Value::Array(i.terms.iter().map(term_cfg).collect()));
        }
        if !i.values.is_empty() {
            o.insert("values".into(), Value::Array(i.values.iter().map(term_cfg).collect()));
        }
        if i.unique {
            o.insert("unique".into(), Value::Bool(true));
        }
        let partitions = if i.unique || i.terms.is_empty() { 1 } else { 8 };
        o.insert("partitions".into(), Value::Long(partitions));
        if let Some(d) = &i.data {
            o.insert("data".into(), d.clone());
        }
        Value::Object(o)
    }

    pub fn render_function(&self, f: &FunctionDef) -> Value {
        let mut o = Object::new();
        o.insert("ref".into(), Value::Ref(Ref::function(&f.name)));
        o.insert("ts".into(), Value::Long(f.ts.micros()));
        o.insert("name".into(), Value::String(f.name.clone()));
        o.insert("body".into(), Value::Lambda(f.body.clone()));
        if let Some(r) = &f.role {
            o.insert("role".into(), r.clone());
        }
        if let Some(d) = &f.data {
            o.insert("data".into(), d.clone());
        }
        Value::Object(o)
    }

    pub fn render_role(&self, r: &RoleDef) -> Value {
        let mut o = Object::new();
        o.insert("ref".into(), Value::Ref(Ref::role(&r.name)));
        o.insert("ts".into(), Value::Long(r.ts.micros()));
        o.insert("name".into(), Value::String(r.name.clone()));
        for (k, v) in &r.fields {
            o.insert(k.clone(), v.clone());
        }
        Value::Object(o)
    }

    /// Resolve any ref to the object `Get` would return, or `None` if it
    /// does not exist. Native-collection refs themselves (e.g.
    /// `Collections()`) are not gettable.
    pub fn get_by_ref(&self, r: &Ref) -> Option<Value> {
        // A schema document deleted earlier in this transaction is gone for
        // `Get`/`Exists` even though its name still resolves (see `pending`).
        if r.is_collection_ref() {
            if self.pending_deleted("collection", &r.id) {
                return None;
            }
            return self.collections.get(&r.id).map(|c| self.render_collection(c));
        }
        if r.is_index_ref() {
            if self.pending_deleted("index", &r.id) {
                return None;
            }
            return self.indexes.get(&r.id).map(|i| self.render_index(i));
        }
        if r.is_function_ref() {
            if self.pending_deleted("function", &r.id) {
                return None;
            }
            return self.functions.get(&r.id).map(|f| self.render_function(f));
        }
        if r.is_role_ref() {
            if self.pending_deleted("role", &r.id) {
                return None;
            }
            return self.roles.get(&r.id).map(|x| self.render_role(x));
        }
        if let Some(coll) = r.document_collection() {
            if !self.collections.contains_key(coll) {
                return None;
            }
            return self.get_document(coll, &r.id).map(|d| self.render_document(coll, &r.id, d));
        }
        None
    }

    pub fn exists_by_ref(&self, r: &Ref) -> bool {
        if r.is_collection_ref() {
            return self.collections.contains_key(&r.id) && !self.pending_deleted("collection", &r.id);
        }
        if r.is_index_ref() {
            return self.indexes.contains_key(&r.id) && !self.pending_deleted("index", &r.id);
        }
        if r.is_function_ref() {
            return self.functions.contains_key(&r.id) && !self.pending_deleted("function", &r.id);
        }
        if r.is_role_ref() {
            return self.roles.contains_key(&r.id) && !self.pending_deleted("role", &r.id);
        }
        if let Some(coll) = r.document_collection() {
            return self.collections.contains_key(coll) && self.exists_document(coll, &r.id);
        }
        false
    }

    // ---- schema writes ----

    fn require_name(params: &Object, pos: &Position) -> R<String> {
        match params.get("name") {
            Some(Value::String(s)) if !s.is_empty() => {
                // The only reserved name on the v4 server is `_` (verified on the
                // container); `RESERVED_NAMES` below are what the *ref parser*
                // treats specially and are accepted as collection names.
                if s == "_" {
                    return one(FaunaError::validation_failed(
                        vec![ValidationFailure {
                            field: vec!["name".into()],
                            code: "reserved name".into(),
                            description: "Invalid name _. Reserved names: _.".into(),
                        }],
                        pos,
                    ));
                }
                Ok(s.clone())
            }
            Some(Value::String(_)) | None => one(FaunaError::validation_failed(
                vec![ValidationFailure {
                    field: vec!["name".into()],
                    code: "value required".into(),
                    description: "Required value not found.".into(),
                }],
                pos,
            )),
            Some(other) => one(FaunaError::validation_failed(
                vec![ValidationFailure {
                    field: vec!["name".into()],
                    code: "invalid type".into(),
                    description: format!("Invalid type {}, expected type Name.", ir_type_name(other)),
                }],
                pos,
            )),
        }
    }

    fn opt_long(params: &Object, key: &str, pos: &Position) -> R<Option<i64>> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Long(l)) => Ok(Some(*l)),
            Some(other) => one(FaunaError::validation_failed(
                vec![ValidationFailure {
                    field: vec![key.into()],
                    code: "invalid type".into(),
                    description: format!("Invalid type {}, expected type Long.", ir_type_name(other)),
                }],
                pos,
            )),
        }
    }

    /// `CreateCollection(params)`.
    pub fn create_collection(&mut self, params: &Object, ts: Timestamp, pos: &Position) -> R<Value> {
        let name = Self::require_name(params, pos)?;
        if (self.collections.contains_key(&name) && !self.pending_deleted("collection", &name))
            || self.pending_has("collection", &name)
        {
            return one(FaunaError::native_already_exists("Collection", pos));
        }
        let history_days = Self::opt_long(params, "history_days", pos)?;
        let ttl_days = Self::opt_long(params, "ttl_days", pos)?;
        let def = CollectionDef {
            name: name.clone(),
            ts,
            // `Document.DefaultRetainDays` is a server config value; the
            // fauna/faunadb container (our oracle) reports 0.
            history_days: Some(history_days.unwrap_or(0)),
            ttl_days,
            data: params.get("data").cloned(),
        };
        let rendered = self.render_collection(&def);
        self.pending.push(PendingSchema::Collection(def));
        Ok(rendered)
    }

    fn parse_term_cfgs(v: Option<&Value>, key: &str, pos: &Position) -> R<Vec<TermCfg>> {
        let fail = |desc: String| {
            FaunaError::validation_failed(
                vec![ValidationFailure { field: vec![key.into()], code: "invalid type".into(), description: desc }],
                pos,
            )
        };
        let items: Vec<&Value> = match v {
            None | Some(Value::Null) => return Ok(vec![]),
            Some(Value::Array(a)) => a.iter().collect(),
            Some(other) => return one(fail(format!("Invalid type {}, expected type Array.", ir_type_name(other)))),
        };
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let o = match it {
                Value::Object(o) => o,
                other => return one(fail(format!("Invalid type {}, expected type Map.", ir_type_name(other)))),
            };
            let reverse = matches!(o.get("reverse"), Some(Value::Bool(true)));
            if let Some(t) = o.get("transform") {
                // `casefold` etc. transforms (Index.scala `TermConfig.transform`)
                // are not implemented; fail at definition time rather than
                // silently indexing untransformed values.
                let name = t.as_str().unwrap_or("?").to_string();
                return one(FaunaError::not_emulated(&format!("index term/value transforms (`transform: {}`)", name), pos));
            }
            let path = if let Some(Value::String(b)) = o.get("binding") {
                TermPath::Binding(b.clone())
            } else {
                match o.get("field") {
                    Some(Value::Array(elems)) => {
                        let mut p = Vec::with_capacity(elems.len());
                        for e in elems {
                            match e {
                                Value::String(s) => p.push(PathElem::Key(s.clone())),
                                Value::Long(l) => p.push(PathElem::Index(*l)),
                                other => {
                                    return one(fail(format!(
                                        "Invalid type {}, expected type Long or String.",
                                        ir_type_name(other)
                                    )))
                                }
                            }
                        }
                        TermPath::Field(p)
                    }
                    Some(Value::String(s)) => TermPath::Field(vec![PathElem::Key(s.clone())]),
                    _ => {
                        return one(FaunaError::validation_failed(
                            vec![ValidationFailure {
                                field: vec![key.into()],
                                code: "value required".into(),
                                description: "Must provide one of the following: field, binding".into(),
                            }],
                            pos,
                        ))
                    }
                }
            };
            out.push(TermCfg { path, reverse });
        }
        Ok(out)
    }

    fn parse_sources(v: Option<&Value>, pos: &Position) -> R<Vec<IndexSource>> {
        let fail = |desc: String| {
            FaunaError::validation_failed(
                vec![ValidationFailure { field: vec!["source".into()], code: "invalid type".into(), description: desc }],
                pos,
            )
        };
        let items: Vec<&Value> = match v {
            None | Some(Value::Null) => {
                return one(FaunaError::validation_failed(
                    vec![ValidationFailure {
                        field: vec!["source".into()],
                        code: "value required".into(),
                        description: "Required value not found.".into(),
                    }],
                    pos,
                ))
            }
            Some(Value::Array(a)) => a.iter().collect(),
            Some(other) => vec![other],
        };
        let coll_of = |v: &Value| -> R<SourceCollection> {
            match v {
                Value::Ref(r) if r.is_collection_ref() => Ok(SourceCollection::Collection(r.id.clone())),
                Value::String(s) if s == "_" => Ok(SourceCollection::Wildcard),
                other => one(fail(format!("Invalid type {}, expected type Collection Ref.", ir_type_name(other)))),
            }
        };
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            match it {
                Value::Object(o) => {
                    let collection = match o.get("collection") {
                        Some(c) => coll_of(c)?,
                        None => {
                            return one(FaunaError::validation_failed(
                                vec![ValidationFailure {
                                    field: vec!["source".into(), "collection".into()],
                                    code: "value required".into(),
                                    description: "Required value not found.".into(),
                                }],
                                pos,
                            ))
                        }
                    };
                    let mut fields = IndexMap::new();
                    if let Some(Value::Object(fs)) = o.get("fields") {
                        for (k, v) in fs {
                            match v {
                                Value::Lambda(l) => {
                                    fields.insert(k.clone(), l.clone());
                                }
                                other => {
                                    return one(fail(format!(
                                        "Invalid type {}, expected type Lambda for binding '{}'.",
                                        ir_type_name(other),
                                        k
                                    )))
                                }
                            }
                        }
                    }
                    out.push(IndexSource { collection, fields });
                }
                other => out.push(IndexSource { collection: coll_of(other)?, fields: IndexMap::new() }),
            }
        }
        Ok(out)
    }

    /// `CreateIndex(params)`. Indexes are active immediately: the store
    /// has no build phase.
    pub fn create_index(&mut self, params: &Object, ts: Timestamp, pos: &Position) -> R<Value> {
        let name = Self::require_name(params, pos)?;
        if (self.indexes.contains_key(&name) && !self.pending_deleted("index", &name)) || self.pending_has("index", &name) {
            return one(FaunaError::native_already_exists("Index", pos));
        }
        let source = Self::parse_sources(params.get("source"), pos)?;
        for s in &source {
            if let SourceCollection::Collection(c) = &s.collection {
                if !self.collections.contains_key(c) {
                    return one(FaunaError::validation_failed(
                        vec![ValidationFailure {
                            field: {
                                let mut f: Vec<String> = pos.0.iter().map(|e| match e { crate::error::PosElem::Index(i) => i.to_string(), crate::error::PosElem::Key(k) => k.clone() }).collect();
                                f.push("source".into());
                                f
                            },
                            code: "invalid reference".into(),
                            description: "Cannot read reference.".into(),
                        }],
                        pos,
                    ));
                }
            }
        }
        let terms = Self::parse_term_cfgs(params.get("terms"), "terms", pos)?;
        let values = Self::parse_term_cfgs(params.get("values"), "values", pos)?;
        let unique = matches!(params.get("unique"), Some(Value::Bool(true)));
        let serialized = match params.get("serialized") {
            Some(Value::Bool(b)) => *b,
            _ => true,
        };
        let def = IndexDef {
            name: name.clone(),
            ts,
            source,
            terms,
            values,
            unique,
            serialized,
            data: params.get("data").cloned(),
            source_value: params.get("source").cloned().unwrap_or(Value::Null),
        };
        let rendered = self.render_index(&def);
        self.pending.push(PendingSchema::Index(def));
        Ok(rendered)
    }

    /// `CreateFunction(params)`.
    pub fn create_function(&mut self, params: &Object, ts: Timestamp, pos: &Position) -> R<Value> {
        let name = Self::require_name(params, pos)?;
        if (self.functions.contains_key(&name) && !self.pending_deleted("function", &name))
            || self.pending_has("function", &name)
        {
            return one(FaunaError::native_already_exists("Function", pos));
        }
        let body = match params.get("body") {
            Some(Value::Lambda(l)) => {
                // A stored body is parsed at the root on the server, so errors
                // inside it are positioned `["expr", ...]` when called.
                let mut l = l.clone();
                l.pos = Position::root();
                l
            }
            Some(other) => {
                return one(FaunaError::validation_failed(
                    vec![ValidationFailure {
                        field: vec!["body".into()],
                        code: "invalid type".into(),
                        description: format!("Invalid type {}, expected type Lambda.", ir_type_name(other)),
                    }],
                    pos,
                ))
            }
            None => {
                return one(FaunaError::validation_failed(
                    vec![ValidationFailure {
                        field: vec!["body".into()],
                        code: "value required".into(),
                        description: "Required value not found.".into(),
                    }],
                    pos,
                ))
            }
        };
        let def = FunctionDef {
            name: name.clone(),
            ts,
            body,
            role: params.get("role").cloned(),
            data: params.get("data").cloned(),
        };
        let rendered = self.render_function(&def);
        self.pending.push(PendingSchema::Function(def));
        Ok(rendered)
    }

    /// `CreateRole(params)`. Roles are stored but never enforced: the
    /// emulator runs every query with admin privileges.
    pub fn create_role(&mut self, params: &Object, ts: Timestamp, pos: &Position) -> R<Value> {
        let name = Self::require_name(params, pos)?;
        if (self.roles.contains_key(&name) && !self.pending_deleted("role", &name)) || self.pending_has("role", &name) {
            return one(FaunaError::native_already_exists("Role", pos));
        }
        let mut fields = Object::new();
        for (k, v) in params {
            if k != "name" {
                fields.insert(k.clone(), v.clone());
            }
        }
        let def = RoleDef { name: name.clone(), ts, fields };
        let rendered = self.render_role(&def);
        self.pending.push(PendingSchema::Role(def));
        Ok(rendered)
    }
}
