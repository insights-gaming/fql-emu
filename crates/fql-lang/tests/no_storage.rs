//! fql-lang runs queries with no storage at all: a trivial [`Backend`]
//! with no documents, the language forms, and one storage form of our own.

use fql_lang::error::{one, FaunaError, Position, R};
use fql_lang::eval::{self, language_forms, Args, Ctx, Registry};
use fql_lang::value::{Lambda, Ref, SetExpr, Timestamp, Value};
use fql_lang::{env::Env, wire, Backend, Mode};
use serde_json::{json, Value as Json};
use std::collections::BTreeMap;

/// No collections, no functions, no sets: only a key/value map that the
/// `get` form below reads from, and an id counter.
#[derive(Default)]
struct NoStorage {
    next_id: u64,
    docs: BTreeMap<String, Value>,
}

impl Backend for NoStorage {
    fn mode(&self) -> Mode {
        Mode::ReadWrite
    }

    fn identity(&self) -> Option<&Ref> {
        None
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        self.next_id.to_string()
    }

    fn exists_by_ref(&self, _: &Ref) -> bool {
        false
    }

    fn function_body(&self, _: &str) -> Option<&Lambda> {
        None
    }

    fn materialize(_: &mut Ctx<'_, Self>, _: &SetExpr, pos: &Position) -> R<Vec<Value>> {
        one(FaunaError::not_emulated("sets", pos))
    }
}

/// A storage form registered on top of the language forms: `Get(ref)`
/// answers from the map by ref id.
fn get(ctx: &mut Ctx<NoStorage>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let p = pos.key("get");
    match args.eval(ctx, env, "get", pos)? {
        Value::Ref(r) => match ctx.backend.docs.get(&r.id) {
            Some(doc) => Ok(doc.clone()),
            None => one(FaunaError::not_emulated("missing documents", &p)),
        },
        other => one(FaunaError::invalid_argument(&[fql_lang::value::Type::Ref], other.rtype(), &p)),
    }
}

fn registry() -> Registry<NoStorage> {
    let mut r = language_forms();
    r.add("get", &["get"], &["ts"], get);
    r
}

/// One query: the wire-encoded result, or the errors as the server
/// renders them.
fn query(backend: &mut NoStorage, registry: &Registry<NoStorage>, q: Json) -> Result<Json, Json> {
    let mut ctx = Ctx::new(backend, registry, Timestamp::of_micros(1_000_000));
    match eval::eval(&mut ctx, &Env::empty(), &q, &Position::root()) {
        Ok(v) => Ok(wire::encode(&v)),
        Err(errs) => Err(Json::Array(errs.iter().map(|e| e.to_json()).collect())),
    }
}

#[test]
fn language_forms_run_without_storage() {
    let registry = registry();
    let mut db = NoStorage::default();
    let mut q = |j: Json| query(&mut db, &registry, j);

    assert_eq!(q(json!({"add": [1, 2]})), Ok(json!(3)));
    assert_eq!(
        q(json!({
            "let": [{"step": 10}],
            "in": {"map": {"lambda": "n", "expr": {"add": [{"var": "n"}, {"var": "step"}]}}, "collection": [1, 2, 3]}
        })),
        Ok(json!([11, 12, 13]))
    );
    assert_eq!(q(json!({"select": ["a", "b"], "from": {"object": {"a": {"object": {}}}}, "default": 7})), Ok(json!(7)));
    assert_eq!(q(json!({"uppercase": {"concat": ["fql", "-lang"]}})), Ok(json!("FQL-LANG")));
    assert_eq!(q(json!({"new_id": null})), Ok(json!("1")));
    assert_eq!(q(json!({"now": null})), Ok(json!({"@ts": "1970-01-01T00:00:01Z"})));
}

#[test]
fn errors_carry_server_codes_and_positions() {
    let registry = registry();
    let mut db = NoStorage::default();
    let mut q = |j: Json| query(&mut db, &registry, j);

    let errs = q(json!({"let": {"x": 1}, "in": {"add": [{"var": "x"}, "two"]}})).unwrap_err();
    assert_eq!(errs[0]["code"], "invalid argument");
    assert_eq!(errs[0]["description"], "Number expected, String provided.");
    assert_eq!(errs[0]["position"], json!(["in", "add", 1]));

    let errs = q(json!({"select": ["a"], "from": {"object": {}}})).unwrap_err();
    assert_eq!(errs[0]["code"], "value not found");
    assert_eq!(errs[0]["position"], json!(["from"]));

    // A ref the backend does not know is unresolved, as on the server.
    let errs = q(json!({"call": {"function": "nope"}, "arguments": []})).unwrap_err();
    assert_eq!(errs[0]["code"], "invalid ref");
    assert_eq!(errs[0]["position"], json!(["call"]));
}

#[test]
fn storage_forms_are_registered_by_the_backend() {
    let registry = registry();
    let mut db = NoStorage::default();
    db.docs.insert("1".into(), Value::obj(vec![("data", Value::obj(vec![("name", Value::String("ada".into()))]))]));
    let r = json!({"@ref": {"id": "1", "collection": {"@ref": {"id": "users", "collection": {"@ref": {"id": "collections"}}}}}});
    assert_eq!(
        query(&mut db, &registry, json!({"select": ["data", "name"], "from": {"get": r}})),
        Ok(json!("ada"))
    );
    // Without the backend's form, `get` is not a form at all.
    let errs = query(&mut db, &language_forms(), json!({"get": r})).unwrap_err();
    assert_eq!(errs[0]["code"], "invalid expression");
}
