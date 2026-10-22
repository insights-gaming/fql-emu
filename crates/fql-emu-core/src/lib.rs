//! # fql-emu-core
//!
//! An in-memory emulator of the FQL v4 query language, for running large
//! numbers of tiny query tests without a `fauna/faunadb` container.
//!
//! **This is not a database.** Indexes are brute-force scans, every
//! transaction clones the store, and there is a hard cap on document
//! count. It exists so that query *logic* can be tested quickly and
//! deterministically. Anything it cannot emulate fails with a
//! `not emulated` error rather than guessing.
//!
//! Semantics are ported from the open-sourced Fauna server
//! (https://github.com/fauna/faunadb, `ext/model/src/main/scala/ast`).
//! Module docs point at the specific Scala files.
//!
//! The entry point is [`Emulator::query`], which takes the JSON body the
//! `faunadb` JS driver would POST and returns the HTTP status and JSON
//! body the server would answer with.
//!
//! The language itself (wire format, values, errors, ordering, the
//! interpreter and every form that needs no storage) is the `fql-lang`
//! crate; this crate adds the in-memory [`store`], implements
//! [`fql_lang::Backend`] for it, and registers the set, read and write
//! forms ([`eval`]). `value`, `wire`, `error`, `env`, `ordering` and
//! [`Mode`] are re-exported from `fql-lang`.

pub mod eval;
pub mod store;

pub use fql_lang::{env, error, ordering, value, wire, Mode};

use error::{FaunaError, Position};
use serde_json::{json, Value as Json};
use store::{Clock, Database};
use value::{Timestamp, Value};

/// What the HTTP layer would have returned.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Json,
    /// Transaction time in microseconds (the server's `x-txn-time`
    /// header). `None` when the request never reached a transaction
    /// (e.g. the body was not valid JSON).
    pub txn_time_micros: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Emulator {
    pub db: Database,
}

impl Emulator {
    pub fn new() -> Emulator {
        Emulator { db: Database::new() }
    }

    /// A fork is an independent copy in the same mode. Seed once, fork per
    /// test.
    pub fn fork(&self) -> Emulator {
        Emulator { db: self.db.fork() }
    }

    /// A fork in the given [`Mode`]. This is the only way to obtain a
    /// read-only emulator: the base stays writable, so the schema and
    /// fixtures are loaded there and the read-only copy is handed to tests
    /// that only read (and can therefore share it without further forks).
    /// `Mode::ReadOnlyPureCalls` additionally allows `Call` of functions
    /// that do not write.
    pub fn fork_as(&self, mode: Mode) -> Emulator {
        let mut db = self.db.fork();
        db.mode = mode;
        Emulator { db }
    }

    pub fn mode(&self) -> Mode {
        self.db.mode
    }

    /// Pin `Now()` and all written timestamps to a fixed instant.
    pub fn set_now_micros(&mut self, micros: i64) {
        self.db.clock = Clock::Fixed(Timestamp::of_micros(micros));
    }

    /// Return to the monotonic auto clock, starting after `micros`.
    pub fn set_auto_clock_from_micros(&mut self, micros: i64) {
        self.db.clock = Clock::Auto { last: Timestamp::of_micros(micros) };
    }

    /// Seed for `NewId()`.
    pub fn set_next_id(&mut self, id: u64) {
        self.db.next_id = id;
    }

    /// Set what `CurrentIdentity()` returns (`None` clears it).
    pub fn set_identity(&mut self, identity: Option<value::Ref>) {
        self.db.identity = identity;
    }

    pub fn set_max_documents(&mut self, n: usize) {
        self.db.limits.max_documents = n;
    }


    /// Run one query (one transaction) against the store. Writes are
    /// committed only if the whole query succeeds.
    pub fn query(&mut self, expr: &Json) -> Response {
        let (result, ts, wrote) = self.transact(expr);
        match result {
            Ok(v) => Response {
                // The server answers 201 when the transaction wrote.
                status: if wrote { 201 } else { 200 },
                body: json!({ "resource": wire::encode(&v) }),
                txn_time_micros: Some(ts.micros()),
            },
            Err(errs) => Response { txn_time_micros: Some(ts.micros()), ..error_response(errs) },
        }
    }

    /// Convenience for callers holding a JSON string.
    pub fn query_str(&mut self, expr: &str) -> Response {
        match serde_json::from_str::<Json>(expr) {
            Ok(j) => self.query(&j),
            Err(e) => error_response(vec![FaunaError::invalid_json(format!("Invalid JSON: {}", e))]),
        }
    }

    /// Evaluate and return the value (for Rust-side tests).
    pub fn eval(&mut self, expr: &Json) -> error::R<Value> {
        self.transact(expr).0
    }

    /// One transaction: the result, its timestamp and whether it wrote.
    /// The clock advances even when the query fails, the way a real
    /// transaction attempt would.
    fn transact(&mut self, expr: &Json) -> (error::R<Value>, Timestamp, bool) {
        let ts = self.db.clock.next();
        if self.db.mode.is_read_only() {
            // Every write path refuses before touching the store, so a
            // read-only query evaluates in place: no copy, and only the id
            // counter can move (`NewId()`), which a failed query rolls back.
            let next_id = self.db.next_id;
            let r = {
                let mut ctx = eval::Ctx::new(&mut self.db, eval::registry(), ts);
                eval::eval(&mut ctx, &env::Env::empty(), expr, &Position::root())
            };
            debug_assert!(self.db.pending.is_empty(), "read-only query staged a write");
            if r.is_err() {
                self.db.next_id = next_id;
            }
            return (r, ts, false);
        }
        let mut working = self.db.clone();
        let (r, wrote) = {
            let mut ctx = eval::Ctx::new(&mut working, eval::registry(), ts);
            let r = eval::eval(&mut ctx, &env::Env::empty(), expr, &Position::root());
            (r, ctx.wrote)
        };
        if r.is_ok() {
            working.commit_pending();
            self.db = working;
        }
        (r, ts, wrote)
    }

    /// Load a schema bundle: `{"collections": [params...], "indexes":
    /// [params...], "functions": [params...], "roles": [params...]}` where
    /// each `params` is exactly what you would pass to
    /// `CreateCollection` / `CreateIndex` / `CreateFunction` / `CreateRole`,
    /// in wire format (so lambdas are `{"@query": ...}` and refs are
    /// `{"@ref": ...}`).
    pub fn load_schema(&mut self, bundle: &Json) -> Result<(), Vec<FaunaError>> {
        let pos = Position::root();
        // Schema is loaded into the writable base before forking; a read-only
        // copy is exactly what the server-readonly key would answer.
        if self.db.mode.is_read_only() {
            return Err(vec![FaunaError::permission_denied(&pos)]);
        }
        let ts = self.db.clock.next();
        let sections = [
            ("collections", "create_collection"),
            ("roles", "create_role"),
            ("functions", "create_function"),
            ("indexes", "create_index"),
        ];
        for (section, op) in sections {
            if let Some(Json::Array(items)) = bundle.get(section) {
                for (i, item) in items.iter().enumerate() {
                    let p = pos.key(section).idx(i);
                    let params = match wire::decode_literal(item, &p)? {
                        Value::Object(o) => o,
                        other => {
                            return Err(vec![FaunaError::invalid_argument(
                                &[value::Type::Object],
                                other.rtype(),
                                &p,
                            )])
                        }
                    };
                    match op {
                        "create_collection" => self.db.create_collection(&params, ts, &p)?,
                        "create_role" => self.db.create_role(&params, ts, &p)?,
                        "create_function" => self.db.create_function(&params, ts, &p)?,
                        _ => self.db.create_index(&params, ts, &p)?,
                    };
                    // A bundle is not a transaction: make each object visible at once.
                    self.db.commit_pending();
                }
            }
        }
        Ok(())
    }

    /// Names of every form the registry knows, with whether it is
    /// implemented. Useful for docs and coverage reports.
    pub fn supported_forms() -> Vec<(&'static str, bool)> {
        eval::registry().names()
    }

    pub fn stats(&self) -> Json {
        json!({
            "collections": self.db.collections.len(),
            "indexes": self.db.indexes.len(),
            "functions": self.db.functions.len(),
            "documents": self.db.doc_count,
            "max_documents": self.db.limits.max_documents,
            "mode": self.db.mode.as_str(),
        })
    }
}

pub fn error_response(errs: Vec<FaunaError>) -> Response {
    let status = error::status_code(&errs);
    Response {
        status,
        body: json!({ "errors": errs.iter().map(|e| e.to_json()).collect::<Vec<_>>() }),
        txn_time_micros: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(e: &mut Emulator, j: Json) -> Json {
        let r = e.query(&j);
        assert!(r.status == 200 || r.status == 201, "{}", r.body);
        r.body["resource"].clone()
    }

    #[test]
    fn literals_and_let() {
        let mut e = Emulator::new();
        assert_eq!(q(&mut e, json!(1)), json!(1));
        assert_eq!(q(&mut e, json!(1.5)), json!(1.5));
        assert_eq!(q(&mut e, json!("x")), json!("x"));
        assert_eq!(q(&mut e, json!([1, "a", null])), json!([1, "a", null]));
        assert_eq!(
            q(&mut e, json!({"let": [{"x": 1}, {"y": {"add": [{"var": "x"}, 2]}}], "in": {"var": "y"}})),
            json!(3)
        );
    }

    #[test]
    fn plain_objects_must_be_wrapped() {
        let mut e = Emulator::new();
        let r = e.query(&json!({"a": 1}));
        assert_eq!(r.status, 400);
        assert_eq!(r.body["errors"][0]["code"], "invalid expression");
        assert_eq!(q(&mut e, json!({"object": {"a": {"add": [1, 1]}}})), json!({"a": 2}));
    }

    #[test]
    fn select_errors_have_positions() {
        let mut e = Emulator::new();
        let r = e.query(&json!({"select": ["a", "b"], "from": {"object": {"a": {"object": {}}}}}));
        assert_eq!(r.status, 404);
        assert_eq!(r.body["errors"][0]["code"], "value not found");
        assert_eq!(r.body["errors"][0]["description"], "Value not found at path [a,b].");
        assert_eq!(r.body["errors"][0]["position"], json!(["from"]));
        assert_eq!(
            q(&mut e, json!({"select": ["a", "b"], "from": {"object": {"a": {"object": {}}}}, "default": 7})),
            json!(7)
        );
    }

    #[test]
    fn lambdas_close_over_let() {
        let mut e = Emulator::new();
        let j = json!({
            "let": [{"n": 10}],
            "in": {"call": "nope", "arguments": 1}
        });
        // function doesn't exist
        let r = e.query(&j);
        assert_eq!(r.body["errors"][0]["code"], "invalid ref");
    }

    #[test]
    fn abort_and_call_error() {
        let mut e = Emulator::new();
        e.load_schema(&json!({
            "functions": [{"name": "boom", "body": {"@query": {"lambda": "x", "expr": {"abort": {"var": "x"}}}}}]
        }))
        .unwrap();
        let r = e.query(&json!({"call": {"function": "boom"}, "arguments": "bad"}));
        assert_eq!(r.status, 400);
        assert_eq!(r.body["errors"][0]["code"], "call error");
        assert_eq!(r.body["errors"][0]["cause"][0]["code"], "transaction aborted");
        assert_eq!(r.body["errors"][0]["cause"][0]["description"], "bad");
    }

    #[test]
    fn writes_roll_back_on_error() {
        let mut e = Emulator::new();
        let r = e.query(&json!({"do": [{"create_collection": {"object": {"name": "users"}}}, {"abort": "x"}]}));
        assert_eq!(r.status, 400);
        assert!(e.db.collections.is_empty());
        q(&mut e, json!({"create_collection": {"object": {"name": "users"}}}));
        assert!(e.db.collections.contains_key("users"));
        let r = e.query(&json!({"create_collection": {"object": {"name": "users"}}}));
        assert_eq!(r.body["errors"][0]["code"], "instance already exists");
    }

    #[test]
    fn now_is_fixed_within_a_transaction_and_controllable() {
        let mut e = Emulator::new();
        e.set_now_micros(1_000_000);
        assert_eq!(q(&mut e, json!({"now": null})), json!({"@ts": "1970-01-01T00:00:01Z"}));
        assert_eq!(q(&mut e, json!({"to_micros": {"time": "now"}})), json!(1_000_000));
    }

    #[test]
    fn responses_carry_the_transaction_time() {
        let mut e = Emulator::new();
        e.set_now_micros(1_000_000);
        assert_eq!(e.query(&json!(1)).txn_time_micros, Some(1_000_000));
        assert_eq!(e.query(&json!({"abort": "x"})).txn_time_micros, Some(1_000_000));
        e.set_auto_clock_from_micros(5);
        assert_eq!(e.query(&json!(1)).txn_time_micros, Some(6));
        assert_eq!(e.query(&json!(1)).txn_time_micros, Some(7));
        assert_eq!(e.query_str("{").txn_time_micros, None);
    }

    #[test]
    fn fork_is_independent() {
        let mut base = Emulator::new();
        q(&mut base, json!({"create_collection": {"object": {"name": "a"}}}));
        let mut f = base.fork();
        q(&mut f, json!({"create_collection": {"object": {"name": "b"}}}));
        assert_eq!(base.db.collections.len(), 1);
        assert_eq!(f.db.collections.len(), 2);
    }

    #[test]
    fn read_only_queries_move_only_the_clock_and_id_counter() {
        let mut base = Emulator::new();
        base.set_next_id(100);
        q(&mut base, json!({"create_collection": {"object": {"name": "a"}}}));
        let mut ro = base.fork_as(Mode::ReadOnly);
        let new_id = json!({"new_id": null});
        // A failed query rolls back the ids it minted, as a discarded copy did.
        let r = ro.query(&json!({"do": [new_id, {"abort": "x"}]}));
        assert_eq!(r.status, 400);
        assert_eq!(ro.db.next_id, base.db.next_id);
        let r = ro.query(&json!({"create_collection": {"object": {"name": "b"}}}));
        assert_eq!(r.status, 403);
        assert_eq!(r.body["errors"][0]["code"], "permission denied");
        assert_eq!(ro.query(&new_id).status, 200);
        assert_eq!(ro.db.next_id, base.db.next_id + 1);
        assert!(ro.db.clock.clone().next().micros() > base.db.clock.clone().next().micros());
        assert_eq!(ro.db.collections.len(), 1);
        assert!(ro.db.pending.is_empty());
    }

    #[test]
    fn unimplemented_forms_fail_loudly() {
        let mut e = Emulator::new();
        let r = e.query(&json!({"create_key": {"object": {"role": "admin"}}}));
        assert_eq!(r.body["errors"][0]["code"], "not emulated");
    }
}
