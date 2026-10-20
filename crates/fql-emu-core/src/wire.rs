//! JSON wire codec for values.
//!
//! Decoding handles the `@`-escapes the driver emits (`@ref`, `@ts`,
//! `@date`, `@set`, `@bytes`, `@query`, `@obj`). Encoding produces what
//! the v4 HTTP API returns, so the driver's own `parseJSON` turns it back
//! into `values.Ref`, `values.FaunaTime`, etc. See `EscapesParser` in
//! ext/model/src/main/scala/ast/Parser.scala and `LiteralEncoder` in
//! ext/model/src/main/scala/Renderable.scala.

use crate::env::Env;
use crate::error::{one, FaunaError, Position, R};
use crate::value::{JoinTarget, Lambda, Object, Pattern, Ref, SetExpr, Timestamp, Value};
use base64::Engine;
use serde_json::{json, Map, Value as Json};
use std::rc::Rc;

/// Convert a plain JSON number to a value: integers become `Long`,
/// anything with a fraction or exponent becomes `Double`.
pub fn number(n: &serde_json::Number, pos: &Position) -> R<Value> {
    if let Some(i) = n.as_i64() {
        Ok(Value::Long(i))
    } else if let Some(f) = n.as_f64() {
        if n.is_u64() {
            return one(FaunaError::invalid_json(format!("Number {} is out of range for a Long.", n)));
        }
        let _ = pos;
        Ok(Value::Double(f))
    } else {
        one(FaunaError::invalid_json("Invalid number."))
    }
}

/// If `obj` is a single-key `@xxx` escape, decode it. Returns `None` when
/// the object is not an escape, so the caller can treat it as a form or a
/// literal object.
pub fn decode_escape(obj: &Map<String, Json>, pos: &Position) -> Option<R<Value>> {
    if obj.len() != 1 {
        return None;
    }
    let (k, v) = obj.iter().next().unwrap();
    let r = match k.as_str() {
        "@ref" => decode_ref(v, pos),
        "@ts" => match v.as_str().and_then(Timestamp::parse) {
            Some(ts) => Ok(Value::Time(ts)),
            None => one(FaunaError::invalid_time_expr(pos)),
        },
        "@date" => match v.as_str().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()) {
            Some(d) => Ok(Value::Date(d)),
            None => one(FaunaError::invalid_date_expr(pos)),
        },
        "@bytes" => match v.as_str().and_then(decode_base64) {
            Some(b) => Ok(Value::Bytes(b)),
            None => one(FaunaError::invalid_bytes_expr(pos)),
        },
        "@set" => match v.as_object() {
            Some(o) => decode_set(o, pos).map(Value::Set),
            None => one(FaunaError::invalid_set_expr(pos)),
        },
        "@query" => decode_lambda(v, pos).map(Value::Lambda),
        "@obj" => match v.as_object() {
            Some(o) => decode_literal_object(o, pos).map(Value::Object),
            None => one(FaunaError::invalid_object_expr(pos)),
        },
        _ => return None,
    };
    Some(r)
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let std = base64::engine::general_purpose::STANDARD;
    let url = base64::engine::general_purpose::URL_SAFE;
    let std_np = base64::engine::general_purpose::STANDARD_NO_PAD;
    let url_np = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    std.decode(s)
        .or_else(|_| url.decode(s))
        .or_else(|_| std_np.decode(s))
        .or_else(|_| url_np.decode(s))
        .ok()
}

pub fn decode_ref(v: &Json, pos: &Position) -> R<Value> {
    match v {
        Json::String(s) => {
            // Legacy "collections/users/1" string form.
            let mut parts = s.split('/');
            let mut r: Option<Ref> = None;
            for p in parts.by_ref() {
                r = Some(match r {
                    None => Ref::native(p),
                    Some(parent) => Ref::child(parent, p),
                });
            }
            match r {
                Some(r) => Ok(Value::Ref(r)),
                None => one(FaunaError::invalid_ref_expr(pos)),
            }
        }
        Json::Object(o) => {
            let id = match o.get("id") {
                Some(Json::String(s)) => s.clone(),
                Some(Json::Number(n)) if n.is_i64() => n.to_string(),
                _ => return one(FaunaError::invalid_ref_expr(pos)),
            };
            let parent = o.get("collection").or_else(|| o.get("class"));
            let collection = match parent {
                None | Some(Json::Null) => None,
                Some(p) => match decode_literal(p, &pos.key("collection"))? {
                    Value::Ref(r) => Some(Box::new(r)),
                    _ => return one(FaunaError::invalid_ref_expr(pos)),
                },
            };
            let database = match o.get("database") {
                None | Some(Json::Null) => None,
                Some(p) => match decode_literal(p, &pos.key("database"))? {
                    Value::Ref(r) => Some(Box::new(r)),
                    _ => return one(FaunaError::invalid_ref_expr(pos)),
                },
            };
            Ok(Value::Ref(Ref { id, collection, database }))
        }
        _ => one(FaunaError::invalid_ref_expr(pos)),
    }
}

pub fn decode_pattern(v: &Json, pos: &Position) -> R<Pattern> {
    match v {
        Json::String(s) if s == "_" => Ok(Pattern::Ignore),
        Json::String(s) => Ok(Pattern::Name(s.clone())),
        Json::Array(items) => {
            let mut ps = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                ps.push(decode_pattern(it, &pos.idx(i))?);
            }
            Ok(Pattern::Array(ps))
        }
        _ => one(FaunaError::invalid_lambda_expr(pos)),
    }
}

/// Decode a `{"lambda": pattern, "expr": body}` object into a closure
/// over the empty environment (stored lambdas have no free variables).
pub fn decode_lambda(v: &Json, pos: &Position) -> R<Lambda> {
    let o = match v.as_object() {
        Some(o) => o,
        None => return one(FaunaError::invalid_lambda_expr(pos)),
    };
    let (params, body) = match (o.get("lambda"), o.get("expr")) {
        (Some(p), Some(b)) => (p, b),
        _ => return one(FaunaError::invalid_lambda_expr(pos)),
    };
    for k in o.keys() {
        if k != "lambda" && k != "expr" && k != "api_version" {
            return one(FaunaError::invalid_lambda_expr(pos));
        }
    }
    Ok(Lambda {
        params: decode_pattern(params, &pos.key("lambda"))?,
        body: Rc::new(body.clone()),
        env: Env::empty(),
        pos: pos.clone(),
    })
}

fn decode_set_arg(v: &Json, pos: &Position) -> R<SetExpr> {
    match decode_literal(v, pos)? {
        Value::Set(s) => Ok(s),
        other => one(FaunaError::invalid_argument(&[crate::value::Type::Set], other.rtype(), pos)),
    }
}

fn decode_set_list(v: &Json, pos: &Position) -> R<Vec<SetExpr>> {
    match v {
        Json::Array(items) => items.iter().enumerate().map(|(i, it)| decode_set_arg(it, &pos.idx(i))).collect(),
        other => Ok(vec![decode_set_arg(other, pos)?]),
    }
}

/// Decode the body of a `@set` escape. The forms mirror
/// `EscapesParser.sets`.
pub fn decode_set(o: &Map<String, Json>, pos: &Position) -> R<SetExpr> {
    let mut keys: Vec<&str> = o.keys().map(|k| k.as_str()).collect();
    keys.sort();
    let get = |k: &str| o.get(k).unwrap();
    let decode_terms = |v: &Json, p: &Position| -> R<Vec<Value>> {
        match v {
            Json::Array(items) => items.iter().enumerate().map(|(i, it)| decode_literal(it, &p.idx(i))).collect(),
            other => Ok(vec![decode_literal(other, p)?]),
        }
    };
    let index_ref = |v: &Json, p: &Position| -> R<Ref> {
        match decode_literal(v, p)? {
            Value::Ref(r) => Ok(r),
            Value::String(s) => Ok(Ref::index(&s)),
            other => one(FaunaError::invalid_argument(&[crate::value::Type::Identifier], other.rtype(), p)),
        }
    };
    match keys.as_slice() {
        ["match"] => Ok(SetExpr::Match { index: index_ref(get("match"), &pos.key("match"))?, terms: vec![] }),
        ["index", "match"] => Ok(SetExpr::Match {
            index: index_ref(get("index"), &pos.key("index"))?,
            terms: decode_terms(get("match"), &pos.key("match"))?,
        }),
        ["match", "terms"] => Ok(SetExpr::Match {
            index: index_ref(get("match"), &pos.key("match"))?,
            terms: decode_terms(get("terms"), &pos.key("terms"))?,
        }),
        ["documents"] => match decode_literal(get("documents"), &pos.key("documents"))? {
            Value::Ref(r) => Ok(SetExpr::Documents(r)),
            other => one(FaunaError::invalid_argument(&[crate::value::Type::CollectionRef], other.rtype(), &pos.key("documents"))),
        },
        ["singleton"] => match decode_literal(get("singleton"), &pos.key("singleton"))? {
            Value::Ref(r) => Ok(SetExpr::Singleton(r)),
            other => one(FaunaError::invalid_argument(&[crate::value::Type::Ref], other.rtype(), &pos.key("singleton"))),
        },
        ["events"] => Ok(SetExpr::Events(Box::new(decode_literal(get("events"), &pos.key("events"))?))),
        ["union"] => Ok(SetExpr::Union(decode_set_list(get("union"), &pos.key("union"))?)),
        ["intersection"] => Ok(SetExpr::Intersection(decode_set_list(get("intersection"), &pos.key("intersection"))?)),
        ["difference"] => Ok(SetExpr::Difference(decode_set_list(get("difference"), &pos.key("difference"))?)),
        ["distinct"] => Ok(SetExpr::Distinct(Box::new(decode_set_arg(get("distinct"), &pos.key("distinct"))?))),
        ["reverse"] => Ok(SetExpr::Reverse(Box::new(decode_set_arg(get("reverse"), &pos.key("reverse"))?))),
        ["from", "range", "to"] => Ok(SetExpr::Range {
            set: Box::new(decode_set_arg(get("range"), &pos.key("range"))?),
            from: Box::new(decode_literal(get("from"), &pos.key("from"))?),
            to: Box::new(decode_literal(get("to"), &pos.key("to"))?),
        }),
        ["join", "with"] => {
            let source = decode_set_arg(get("join"), &pos.key("join"))?;
            let with = match get("with") {
                Json::Object(o) if o.contains_key("lambda") => JoinTarget::Lambda(decode_lambda(get("with"), &pos.key("with"))?),
                other => JoinTarget::Index(index_ref(other, &pos.key("with"))?),
            };
            Ok(SetExpr::Join { source: Box::new(source), with: Box::new(with) })
        }
        ["collection", "filter"] => Ok(SetExpr::Filter {
            lambda: decode_lambda(get("filter"), &pos.key("filter"))?,
            source: Box::new(decode_set_arg(get("collection"), &pos.key("collection"))?),
        }),
        _ => one(FaunaError::invalid_set_expr(pos)),
    }
}

pub fn decode_literal_object(o: &Map<String, Json>, pos: &Position) -> R<Object> {
    let mut out = Object::with_capacity(o.len());
    for (k, v) in o {
        out.insert(k.clone(), decode_literal(v, &pos.key(k))?);
    }
    Ok(out)
}

/// Decode JSON that is known to be a *value* (not an expression): stored
/// document data, cursors, schema bundles. Escapes are expanded; any other
/// object is a plain object.
pub fn decode_literal(v: &Json, pos: &Position) -> R<Value> {
    match v {
        Json::Null => Ok(Value::Null),
        Json::Bool(b) => Ok(Value::Bool(*b)),
        Json::Number(n) => number(n, pos),
        Json::String(s) => Ok(Value::String(s.clone())),
        Json::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                out.push(decode_literal(it, &pos.idx(i))?);
            }
            Ok(Value::Array(out))
        }
        Json::Object(o) => match decode_escape(o, pos) {
            Some(r) => r,
            None => decode_literal_object(o, pos).map(Value::Object),
        },
    }
}

// ---------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------

pub fn encode_ref(r: &Ref) -> Json {
    let mut o = Map::new();
    o.insert("id".into(), json!(r.id));
    if let Some(c) = &r.collection {
        o.insert("collection".into(), encode_ref(c));
    }
    if let Some(d) = &r.database {
        o.insert("database".into(), encode_ref(d));
    }
    json!({ "@ref": Json::Object(o) })
}

pub fn encode_lambda(l: &Lambda) -> Json {
    json!({ "@query": { "lambda": l.params.to_json(), "expr": (*l.body).clone(), "api_version": "4" } })
}

fn encode_set_body(s: &SetExpr) -> Json {
    match s {
        SetExpr::Match { index, terms } => {
            json!({ "match": encode_ref(index), "terms": Json::Array(terms.iter().map(encode).collect()) })
        }
        SetExpr::Documents(r) => json!({ "documents": encode_ref(r) }),
        SetExpr::Singleton(r) => json!({ "singleton": encode_ref(r) }),
        SetExpr::Events(v) => json!({ "events": encode(v) }),
        SetExpr::Union(ss) => json!({ "union": Json::Array(ss.iter().map(encode_set).collect()) }),
        SetExpr::Intersection(ss) => json!({ "intersection": Json::Array(ss.iter().map(encode_set).collect()) }),
        SetExpr::Difference(ss) => json!({ "difference": Json::Array(ss.iter().map(encode_set).collect()) }),
        SetExpr::Distinct(s) => json!({ "distinct": encode_set(s) }),
        SetExpr::Reverse(s) => json!({ "reverse": encode_set(s) }),
        SetExpr::Range { set, from, to } => json!({ "range": encode_set(set), "from": encode(from), "to": encode(to) }),
        SetExpr::Join { source, with } => {
            let w = match &**with {
                JoinTarget::Lambda(l) => json!({ "lambda": l.params.to_json(), "expr": (*l.body).clone() }),
                JoinTarget::Index(r) => encode_ref(r),
            };
            json!({ "join": encode_set(source), "with": w })
        }
        SetExpr::Filter { lambda, source } => json!({
            "filter": { "lambda": lambda.params.to_json(), "expr": (*lambda.body).clone() },
            "collection": encode_set(source)
        }),
    }
}

pub fn encode_set(s: &SetExpr) -> Json {
    json!({ "@set": encode_set_body(s) })
}

/// Encode a value in the v4 response format.
pub fn encode(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Long(l) => json!(l),
        Value::Double(d) => {
            if d.is_finite() {
                json!(d)
            } else {
                // Fauna renders non-finite doubles as strings.
                json!(d.to_string())
            }
        }
        Value::String(s) => Json::String(s.clone()),
        Value::Bytes(b) => json!({ "@bytes": base64::engine::general_purpose::STANDARD.encode(b) }),
        Value::Time(t) => json!({ "@ts": t.to_rfc3339() }),
        Value::Date(d) => json!({ "@date": d.format("%Y-%m-%d").to_string() }),
        Value::Ref(r) => encode_ref(r),
        Value::Set(s) => encode_set(s),
        Value::Lambda(l) => encode_lambda(l),
        Value::Array(items) => Json::Array(items.iter().map(encode).collect()),
        Value::Object(o) => {
            let needs_escape = o.keys().any(|k| k.starts_with('@'));
            let mut m = Map::with_capacity(o.len());
            for (k, v) in o {
                m.insert(k.clone(), encode(v));
            }
            if needs_escape {
                json!({ "@obj": Json::Object(m) })
            } else {
                Json::Object(m)
            }
        }
        Value::Page { data, before, after } => {
            let mut m = Map::new();
            if let Some(b) = before {
                m.insert("before".into(), encode(b));
            }
            if let Some(a) = after {
                m.insert("after".into(), encode(a));
            }
            m.insert("data".into(), Json::Array(data.iter().map(encode).collect()));
            Json::Object(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_round_trip() {
        let j = json!({"@ref": {"id": "1", "collection": {"@ref": {"id": "users", "collection": {"@ref": {"id": "collections"}}}}}});
        let v = decode_literal(&j, &Position::root()).unwrap();
        assert_eq!(v, Value::Ref(Ref::document("users", "1")));
        assert_eq!(encode(&v), j);
    }

    #[test]
    fn time_round_trip() {
        let j = json!({"@ts": "2020-01-02T03:04:05.000000006Z"});
        let v = decode_literal(&j, &Position::root()).unwrap();
        assert_eq!(encode(&v), j);
    }

    #[test]
    fn at_keys_get_obj_escaped() {
        let v = Value::obj(vec![("@foo", Value::Long(1))]);
        assert_eq!(encode(&v), json!({"@obj": {"@foo": 1}}));
        assert_eq!(decode_literal(&encode(&v), &Position::root()).unwrap(), v);
    }
}
