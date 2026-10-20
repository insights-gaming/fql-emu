//! Runtime value model for FQL v4.
//!
//! This mirrors `fauna.ast.Literal` / `fauna.storage.ir.IRValue` from the
//! open-sourced Fauna server (ext/model/src/main/scala/ast/Expression.scala).
//! Only the subset that can appear in a v4 query result is modelled.

use crate::env::Env;
use indexmap::IndexMap;
use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

/// Nanosecond-precision instant, like `fauna.lang.Timestamp`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp {
    pub secs: i64,
    pub nanos: u32,
}

impl Timestamp {
    pub const EPOCH: Timestamp = Timestamp { secs: 0, nanos: 0 };

    pub fn of_micros(micros: i64) -> Timestamp {
        let secs = micros.div_euclid(1_000_000);
        let rem = micros.rem_euclid(1_000_000) as u32;
        Timestamp { secs, nanos: rem * 1000 }
    }

    pub fn of_nanos(nanos: i128) -> Timestamp {
        let secs = nanos.div_euclid(1_000_000_000) as i64;
        let rem = nanos.rem_euclid(1_000_000_000) as u32;
        Timestamp { secs, nanos: rem }
    }

    pub fn micros(&self) -> i64 {
        self.secs * 1_000_000 + (self.nanos / 1000) as i64
    }

    pub fn millis(&self) -> i64 {
        self.secs * 1000 + (self.nanos / 1_000_000) as i64
    }

    pub fn total_nanos(&self) -> i128 {
        self.secs as i128 * 1_000_000_000 + self.nanos as i128
    }

    /// RFC 3339 with nanosecond precision, always in UTC, e.g.
    /// `1970-01-01T00:00:00.000000001Z`. This is the `@ts` wire form.
    pub fn to_rfc3339(&self) -> String {
        use chrono::{DateTime, Utc};
        let dt: DateTime<Utc> = DateTime::from_timestamp(self.secs, self.nanos)
            .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());
        dt.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()
    }

    /// Parse an ISO-8601 instant. Accepts what `java.time.Instant.parse`
    /// accepts (fractional seconds up to nanos, `Z` or offsets).
    pub fn parse(s: &str) -> Option<Timestamp> {
        let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
        Some(Timestamp {
            secs: dt.timestamp(),
            nanos: dt.timestamp_subsec_nanos(),
        })
    }

    pub fn to_naive(&self) -> chrono::NaiveDateTime {
        chrono::DateTime::from_timestamp(self.secs, self.nanos)
            .map(|d| d.naive_utc())
            .unwrap_or_default()
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Time({})", self.to_rfc3339())
    }
}

/// A calendar date, like `java.time.LocalDate`.
pub type Date = chrono::NaiveDate;

/// A Fauna ref, e.g. `Ref(Collection("users"), "123")`.
///
/// `id` is the last path element. `collection` is the parent ref
/// (`Collection("users")` for a document, `Ref("collections")` for a
/// collection, and `None` for a native collection ref such as
/// `Collections()`). `database` is only set for scoped refs, which the
/// emulator does not support beyond carrying them through.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Ref {
    pub id: String,
    pub collection: Option<Box<Ref>>,
    pub database: Option<Box<Ref>>,
}

impl Ref {
    pub fn native(name: &str) -> Ref {
        Ref { id: name.to_string(), collection: None, database: None }
    }

    pub fn collection(name: &str) -> Ref {
        Ref::child(Ref::native("collections"), name)
    }

    pub fn index(name: &str) -> Ref {
        Ref::child(Ref::native("indexes"), name)
    }

    pub fn function(name: &str) -> Ref {
        Ref::child(Ref::native("functions"), name)
    }

    pub fn role(name: &str) -> Ref {
        Ref::child(Ref::native("roles"), name)
    }

    pub fn database(name: &str) -> Ref {
        Ref::child(Ref::native("databases"), name)
    }

    pub fn document(collection: &str, id: &str) -> Ref {
        Ref::child(Ref::collection(collection), id)
    }

    pub fn child(parent: Ref, id: &str) -> Ref {
        Ref { id: id.to_string(), collection: Some(Box::new(parent)), database: None }
    }

    /// `Some("collections")` for `Collection("users")`, `Some("users")` for a
    /// document in `users`, `None` for a native collection ref.
    pub fn parent_id(&self) -> Option<&str> {
        self.collection.as_deref().map(|c| c.id.as_str())
    }

    pub fn is_native(&self) -> bool {
        self.collection.is_none()
    }

    /// True for `Collection("x")`, i.e. a ref whose parent is the native
    /// `collections` collection.
    pub fn is_collection_ref(&self) -> bool {
        matches!(self.collection.as_deref(), Some(c) if c.is_native() && c.id == "collections")
    }

    pub fn is_index_ref(&self) -> bool {
        matches!(self.collection.as_deref(), Some(c) if c.is_native() && c.id == "indexes")
    }

    pub fn is_function_ref(&self) -> bool {
        matches!(self.collection.as_deref(), Some(c) if c.is_native() && c.id == "functions")
    }

    pub fn is_role_ref(&self) -> bool {
        matches!(self.collection.as_deref(), Some(c) if c.is_native() && c.id == "roles")
    }

    pub fn is_database_ref(&self) -> bool {
        matches!(self.collection.as_deref(), Some(c) if c.is_native() && c.id == "databases")
    }

    /// True for a ref to a document in a user collection.
    pub fn is_document_ref(&self) -> bool {
        matches!(self.collection.as_deref(), Some(c) if c.is_collection_ref())
    }

    /// Collection name for a document ref.
    pub fn document_collection(&self) -> Option<&str> {
        if self.is_document_ref() {
            self.parent_id()
        } else {
            None
        }
    }

    /// Human readable, e.g. `Ref(Collection("users"), "1")`.
    pub fn display(&self) -> String {
        match &self.collection {
            None => format!("Ref(\"{}\")", self.id),
            Some(c) if c.is_native() => {
                let ctor = match c.id.as_str() {
                    "collections" => "Collection",
                    "indexes" => "Index",
                    "functions" => "Function",
                    "roles" => "Role",
                    "databases" => "Database",
                    "keys" => "Key",
                    other => other,
                };
                format!("{}(\"{}\")", ctor, self.id)
            }
            Some(c) => format!("Ref({}, \"{}\")", c.display(), self.id),
        }
    }
}

impl fmt::Debug for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Lambda parameter pattern: `"_"`, `"x"`, or `["a", ["b", "c"]]`.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Ignore,
    Name(String),
    Array(Vec<Pattern>),
}

impl Pattern {
    pub fn arity(&self) -> usize {
        match self {
            Pattern::Ignore => 0,
            Pattern::Name(_) => 1,
            Pattern::Array(ps) => ps.iter().map(|p| p.arity()).sum(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Pattern::Ignore => serde_json::Value::String("_".into()),
            Pattern::Name(n) => serde_json::Value::String(n.clone()),
            Pattern::Array(ps) => serde_json::Value::Array(ps.iter().map(|p| p.to_json()).collect()),
        }
    }
}

/// A lambda closure. `body` is the raw expression JSON, evaluated in `env`
/// extended with the parameter bindings.
///
/// `pos` is where the lambda was parsed: the server reports arity errors
/// at `pos ++ ["lambda"]` and errors inside the body under
/// `pos ++ ["expr"]` (`EvalContext.evalLambdaApply`). Stored UDF bodies
/// are parsed at the root, so their body errors are at `["expr", ...]`.
#[derive(Clone)]
pub struct Lambda {
    pub params: Pattern,
    pub body: Rc<serde_json::Value>,
    pub env: Env,
    pub pos: crate::error::Position,
}

impl PartialEq for Lambda {
    fn eq(&self, other: &Self) -> bool {
        self.params == other.params && self.body == other.body
    }
}

impl fmt::Debug for Lambda {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Lambda({:?} => {})", self.params, self.body)
    }
}

/// A set expression. Sets are lazy; they are only materialised by
/// `Paginate`, the fold functions, `Count`, etc.
#[derive(Clone, Debug, PartialEq)]
pub enum SetExpr {
    /// `Match(Index("x"), terms...)`; the index ref is resolved.
    Match { index: Ref, terms: Vec<Value> },
    /// `Documents(Collection("x"))`
    Documents(Ref),
    /// `Singleton(ref)`
    Singleton(Ref),
    Union(Vec<SetExpr>),
    Intersection(Vec<SetExpr>),
    Difference(Vec<SetExpr>),
    Distinct(Box<SetExpr>),
    Reverse(Box<SetExpr>),
    Range { set: Box<SetExpr>, from: Box<Value>, to: Box<Value> },
    Join { source: Box<SetExpr>, with: Box<JoinTarget> },
    Filter { lambda: Lambda, source: Box<SetExpr> },
    /// `Events(...)`: carried for wire round-tripping, never evaluated.
    Events(Box<Value>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum JoinTarget {
    Lambda(Lambda),
    Index(Ref),
}

/// An object with insertion-ordered keys, like `fauna.ast.ObjectL`.
pub type Object = IndexMap<String, Value>;

#[derive(Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    String(String),
    Bytes(Vec<u8>),
    Time(Timestamp),
    Date(Date),
    Ref(Ref),
    Set(SetExpr),
    Lambda(Lambda),
    Array(Vec<Value>),
    Object(Object),
    /// A page produced by `Paginate`. Renders as `{before?, after?, data}`.
    Page { data: Vec<Value>, before: Option<Box<Value>>, after: Option<Box<Value>> },
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Long(l) => write!(f, "{}", l),
            Value::Double(d) => write!(f, "{:?}", d),
            Value::String(s) => write!(f, "{:?}", s),
            Value::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Value::Time(t) => write!(f, "{:?}", t),
            Value::Date(d) => write!(f, "Date({})", d),
            Value::Ref(r) => write!(f, "{:?}", r),
            Value::Set(s) => write!(f, "Set({:?})", s),
            Value::Lambda(l) => write!(f, "{:?}", l),
            Value::Array(a) => f.debug_list().entries(a).finish(),
            Value::Object(o) => f.debug_map().entries(o.iter()).finish(),
            Value::Page { data, before, after } => f
                .debug_struct("Page")
                .field("data", data)
                .field("before", before)
                .field("after", after)
                .finish(),
        }
    }
}

/// Type names as rendered in Fauna error descriptions
/// (`fauna.ast.Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Any,
    Scalar,
    Number,
    Double,
    Integer,
    Boolean,
    String,
    Null,
    Bytes,
    Object,
    Lambda,
    AbstractIterable,
    AbstractArray,
    Page,
    Array,
    Identifier,
    Ref,
    CollectionRef,
    IndexRef,
    DatabaseRef,
    KeyRef,
    UserFunctionRef,
    RoleRef,
    Set,
    Temporal,
    Time,
    Date,
    TimeUnit,
    Field,
}

impl Type {
    pub fn name(&self) -> &'static str {
        match self {
            Type::Any => "Any",
            Type::Scalar => "Scalar",
            Type::Number => "Number",
            Type::Double => "Double",
            Type::Integer => "Integer",
            Type::Boolean => "Boolean",
            Type::String => "String",
            Type::Null => "Null",
            Type::Bytes => "Bytes",
            Type::Object => "Object",
            Type::Lambda => "Lambda",
            Type::AbstractIterable => "Array, Set, or Page",
            Type::AbstractArray => "Array or Page",
            Type::Page => "Page",
            Type::Array => "Array",
            Type::Identifier => "Identifier",
            Type::Ref => "Ref",
            Type::CollectionRef => "Collection Ref",
            Type::IndexRef => "Index Ref",
            Type::DatabaseRef => "Database Ref",
            Type::KeyRef => "Key Ref",
            Type::UserFunctionRef => "User Function Ref",
            Type::RoleRef => "Role Ref",
            Type::Set => "Set",
            Type::Temporal => "Temporal",
            Type::Time => "Time",
            Type::Date => "Date",
            Type::TimeUnit => "Time Unit",
            Type::Field => "Field/Value",
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Value {
    pub fn rtype(&self) -> Type {
        match self {
            Value::Null => Type::Null,
            Value::Bool(_) => Type::Boolean,
            Value::Long(_) => Type::Integer,
            Value::Double(_) => Type::Double,
            Value::String(_) => Type::String,
            Value::Bytes(_) => Type::Bytes,
            Value::Time(_) => Type::Time,
            Value::Date(_) => Type::Date,
            // `Type.getType` matches on the ref's collection id, so the
            // native collection refs type as their own kind too:
            // `Collections()` is a Collection Ref, `Indexes()` an Index Ref.
            Value::Ref(r) => {
                let native = if r.is_native() { r.id.as_str() } else { "" };
                if r.is_collection_ref() || native == "collections" || native == "classes" {
                    Type::CollectionRef
                } else if r.is_index_ref() || native == "indexes" {
                    Type::IndexRef
                } else if r.is_database_ref() || native == "databases" {
                    Type::DatabaseRef
                } else if r.is_function_ref() || native == "functions" {
                    Type::UserFunctionRef
                } else if native == "keys" {
                    Type::KeyRef
                } else if r.is_role_ref() {
                    Type::RoleRef
                } else {
                    Type::Ref
                }
            }
            Value::Set(_) => Type::Set,
            Value::Lambda(_) => Type::Lambda,
            Value::Array(_) => Type::Array,
            Value::Object(_) => Type::Object,
            Value::Page { .. } => Type::Page,
        }
    }

    pub fn is_scalar(&self) -> bool {
        !matches!(self, Value::Array(_) | Value::Object(_) | Value::Page { .. } | Value::Lambda(_))
    }

    pub fn str(s: &str) -> Value {
        Value::String(s.to_string())
    }

    pub fn obj(entries: Vec<(&str, Value)>) -> Value {
        Value::Object(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Option<&Ref> {
        match self {
            Value::Ref(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_long(&self) -> Option<i64> {
        match self {
            Value::Long(l) => Some(*l),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Equality as used by `Equals`: numbers compare numerically across
    /// Long/Double, everything else structurally.
    pub fn fauna_equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Long(a), Value::Long(b)) => a == b,
            (Value::Double(a), Value::Double(b)) => a.total_cmp(b) == Ordering::Equal,
            (Value::Long(a), Value::Double(b)) => (*a as f64).total_cmp(b) == Ordering::Equal,
            (Value::Double(a), Value::Long(b)) => a.total_cmp(&(*b as f64)) == Ordering::Equal,
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.fauna_equals(y))
            }
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(k, v)| b.get(k).map(|w| v.fauna_equals(w)).unwrap_or(false))
            }
            (Value::Page { data: a, .. }, Value::Page { data: b, .. }) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.fauna_equals(y))
            }
            _ => self == other,
        }
    }
}
