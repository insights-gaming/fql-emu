//! Argument casting helpers, mirroring `fauna.ast.Casts`.
//!
//! Each helper takes the *evaluated* argument value and the position of
//! the argument (`pos.key("argname")`), and produces the same
//! `invalid argument` error the server would.

use crate::error::{one, FaunaError, Position, R};
use crate::value::{Lambda, Object, Ref, SetExpr, Timestamp, Type, Value};
use std::fmt;

/// An element of a `Select` / `ContainsPath` / index field path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathElem {
    Index(i64),
    Key(String),
}

impl PathElem {
    pub fn to_value(&self) -> Value {
        match self {
            PathElem::Index(i) => Value::Long(*i),
            PathElem::Key(k) => Value::String(k.clone()),
        }
    }
}

impl fmt::Display for PathElem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathElem::Index(i) => write!(f, "{}", i),
            PathElem::Key(k) => f.write_str(k),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Long(i64),
    Double(f64),
}

impl Num {
    pub fn as_f64(self) -> f64 {
        match self {
            Num::Long(l) => l as f64,
            Num::Double(d) => d,
        }
    }

    pub fn to_value(self) -> Value {
        match self {
            Num::Long(l) => Value::Long(l),
            Num::Double(d) => Value::Double(d),
        }
    }
}

pub fn long(v: &Value, pos: &Position) -> R<i64> {
    match v {
        Value::Long(l) => Ok(*l),
        other => one(FaunaError::invalid_argument(&[Type::Integer], other.rtype(), pos)),
    }
}

pub fn number(v: &Value, pos: &Position) -> R<Num> {
    match v {
        Value::Long(l) => Ok(Num::Long(*l)),
        Value::Double(d) => Ok(Num::Double(*d)),
        other => one(FaunaError::invalid_argument(&[Type::Number], other.rtype(), pos)),
    }
}

pub fn boolean(v: &Value, pos: &Position) -> R<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => one(FaunaError::invalid_argument(&[Type::Boolean], other.rtype(), pos)),
    }
}

pub fn string<'a>(v: &'a Value, pos: &Position) -> R<&'a str> {
    match v {
        Value::String(s) => Ok(s),
        other => one(FaunaError::invalid_argument(&[Type::String], other.rtype(), pos)),
    }
}

pub fn null(v: &Value, pos: &Position) -> R<()> {
    match v {
        Value::Null => Ok(()),
        other => one(FaunaError::invalid_argument(&[Type::Null], other.rtype(), pos)),
    }
}

pub fn object<'a>(v: &'a Value, pos: &Position) -> R<&'a Object> {
    match v {
        Value::Object(o) => Ok(o),
        other => one(FaunaError::invalid_argument(&[Type::Object], other.rtype(), pos)),
    }
}

pub fn array<'a>(v: &'a Value, pos: &Position) -> R<&'a Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a),
        other => one(FaunaError::invalid_argument(&[Type::Array], other.rtype(), pos)),
    }
}

pub fn lambda<'a>(v: &'a Value, pos: &Position) -> R<&'a Lambda> {
    match v {
        Value::Lambda(l) => Ok(l),
        other => one(FaunaError::invalid_argument(&[Type::Lambda], other.rtype(), pos)),
    }
}

pub fn ref_<'a>(v: &'a Value, pos: &Position) -> R<&'a Ref> {
    match v {
        Value::Ref(r) => Ok(r),
        other => one(FaunaError::invalid_argument(&[Type::Ref], other.rtype(), pos)),
    }
}

pub fn collection_ref<'a>(v: &'a Value, pos: &Position) -> R<&'a Ref> {
    match v {
        Value::Ref(r) if r.is_collection_ref() => Ok(r),
        other => one(FaunaError::invalid_argument(&[Type::CollectionRef], other.rtype(), pos)),
    }
}

pub fn set<'a>(v: &'a Value, pos: &Position) -> R<&'a SetExpr> {
    match v {
        Value::Set(s) => Ok(s),
        other => one(FaunaError::invalid_argument(&[Type::Set], other.rtype(), pos)),
    }
}

/// `Casts.Timestamp`: a `Time` or a Long of microseconds.
pub fn timestamp(v: &Value, pos: &Position) -> R<Timestamp> {
    match v {
        Value::Time(t) => Ok(*t),
        Value::Long(l) => Ok(Timestamp::of_micros(*l)),
        other => one(FaunaError::invalid_argument(&[Type::Time], other.rtype(), pos)),
    }
}

/// `Casts.Path`: an array of strings and integers, or a single one.
pub fn path(v: &Value, pos: &Position) -> R<Vec<PathElem>> {
    let mut out = Vec::new();
    for (e, p) in elems(v, pos) {
        match e {
            Value::String(s) => out.push(PathElem::Key(s)),
            Value::Long(l) => out.push(PathElem::Index(l)),
            other => return one(FaunaError::invalid_argument(&[Type::String, Type::Integer], other.rtype(), &p)),
        }
    }
    Ok(out)
}

/// `FunctionHelpers.toElemsIter`: varargs arguments may be given as an
/// array or as a single value. Positions follow the server: `pos` for a
/// single value, `pos[i]` for array elements.
pub fn elems(v: &Value, pos: &Position) -> Vec<(Value, Position)> {
    match v {
        Value::Array(items) => items.iter().enumerate().map(|(i, it)| (it.clone(), pos.idx(i))).collect(),
        other => vec![(other.clone(), pos.clone())],
    }
}

/// `Casts.OneOrMore`.
pub fn one_or_more(v: &Value, pos: &Position) -> R<Vec<(Value, Position)>> {
    let es = elems(v, pos);
    if es.is_empty() {
        return one(FaunaError::empty_array_argument(pos));
    }
    Ok(es)
}

/// Elements of an Array or Page (not a Set). `Casts.Array`-ish for the
/// pure collection functions.
pub fn array_like<'a>(v: &'a Value, pos: &Position) -> R<&'a Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a),
        Value::Page { data, .. } => Ok(data),
        other => one(FaunaError::invalid_argument(&[Type::Array], other.rtype(), pos)),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
