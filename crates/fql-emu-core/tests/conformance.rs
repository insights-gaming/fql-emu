//! Data-driven conformance suite.
//!
//! Every `conformance/cases/*.json` file is a suite:
//!
//! ```json
//! {
//!   "description": "what this file covers",
//!   "source": "ext/api/src/test/scala/queries/StringFunctionsSpec.scala",
//!   "now_micros": 1600000000000000,          // optional: pin Now()
//!   "read_only": true,                        // optional: run the cases as a server-readonly key
//!   "schema": { "collections": [...], ... },  // optional: Emulator::load_schema bundle
//!   "documents": [                            // optional: seeded after `schema`, before `setup`
//!     { "collection": "users", "id": "1", "data": {...}, "ts": 1600000000000000 }
//!   ],
//!   "setup": [ <query>, ... ],                // optional: queries run before the cases
//!   "cases": [
//!     { "name": "adds longs", "query": {"add": [1, 2]}, "expect": 3 },
//!     { "name": "no empty",   "query": {"add": []},
//!       "error": { "code": "invalid argument", "description": "Non-empty array expected.", "position": ["add"] } },
//!     { "name": "not yet",    "query": {...}, "expect": ..., "todo": "why this is expected to fail for now" }
//!   ]
//! }
//! ```
//!
//! Queries and expectations are in **wire format**, i.e. exactly what the
//! `faunadb` driver would send / the HTTP API would return. Each case
//! runs on a fork of the suite's seeded emulator, so cases are
//! independent and order does not matter.
//!
//! A successful case may answer 200 or 201 (writes).
//! `error` matches on `code` always, and on `description` / `position`
//! only when given. `todo` cases are allowed to fail (and reported when
//! they unexpectedly pass, so the marker can be removed). Set
//! `FQL_CONFORMANCE_SHOW_TODO=1` to print why each `todo` case fails.
//!
//! `read_only` replaces the seeded emulator with a read-only fork
//! (`Emulator::fork_as(Mode::ReadOnly)`) after `schema`, `documents` and
//! `setup` have run, so the cases see what a server-readonly key sees.
//!
//! `documents` seeds documents directly into the store
//! (`Database::seed_document`), bypassing `Create`: `collection` (must
//! exist) and `id` (a numeric string or number) are required; every other
//! key except `ts` becomes a document field in wire format (usually just
//! `data`); `ts` is the document's micros timestamp and defaults to the
//! suite clock. Seeded documents are part of the suite's base state.

use fql_emu_core::error::Position;
use fql_emu_core::value::{Object, Timestamp, Value};
use fql_emu_core::{wire, Emulator, Mode};
use serde_json::Value as Json;
use std::fs;
use std::path::PathBuf;

fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases")
}

struct Outcome {
    failures: Vec<String>,
    unexpected_passes: Vec<String>,
    passed: usize,
    todo: usize,
}

fn run_suite(path: &PathBuf) -> Outcome {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let suite: Json = serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e));
    let file = path.file_name().unwrap().to_string_lossy().to_string();

    let mut base = Emulator::new();
    if let Some(n) = suite.get("now_micros").and_then(|n| n.as_i64()) {
        base.set_now_micros(n);
    }
    if let Some(schema) = suite.get("schema") {
        base.load_schema(schema).unwrap_or_else(|e| panic!("{}: schema failed: {:?}", file, e));
    }
    if let Some(Json::Array(docs)) = suite.get("documents") {
        for (i, d) in docs.iter().enumerate() {
            seed(&mut base, d).unwrap_or_else(|e| panic!("{}: documents[{}] failed: {}", file, i, e));
        }
    }
    if let Some(Json::Array(setup)) = suite.get("setup") {
        for (i, q) in setup.iter().enumerate() {
            let r = base.query(q);
            assert!(r.status == 200 || r.status == 201, "{}: setup[{}] failed: {}", file, i, r.body);
        }
    }

    let read_only = suite.get("read_only").and_then(|v| v.as_bool()).unwrap_or(false);
    if read_only {
        base = base.fork_as(Mode::ReadOnly);
    }

    let mut out = Outcome { failures: vec![], unexpected_passes: vec![], passed: 0, todo: 0 };
    let cases = suite.get("cases").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    for case in cases {
        let name = case.get("name").and_then(|n| n.as_str()).unwrap_or("<unnamed>").to_string();
        let label = format!("{} :: {}", file, name);
        let todo = case.get("todo").is_some();
        let mut emu = base.fork();
        let resp = emu.query(case.get("query").unwrap_or(&Json::Null));
        // Read-only queries evaluate in place, so a write that slipped past
        // its permission check would change the shared instance.
        if read_only {
            assert_eq!(stored_state(&emu), stored_state(&base), "{}: read-only query changed the store", label);
        }

        let result: Result<(), String> = if let Some(expect) = case.get("expect") {
            if resp.status >= 400 {
                Err(format!("expected {}, got error {}", expect, resp.body))
            } else if &resp.body["resource"] != expect {
                Err(format!("expected {}, got {}", expect, resp.body["resource"]))
            } else {
                Ok(())
            }
        } else if let Some(err) = case.get("error") {
            if resp.status < 400 {
                Err(format!("expected error {}, got {}", err, resp.body["resource"]))
            } else {
                let got = &resp.body["errors"][0];
                let mut problems = vec![];
                for key in ["code", "description", "position", "cause", "failures"] {
                    if let Some(want) = err.get(key) {
                        if &got[key] != want {
                            problems.push(format!("{}: expected {}, got {}", key, want, got[key]));
                        }
                    }
                }
                if let Some(status) = err.get("status").and_then(|s| s.as_u64()) {
                    if resp.status as u64 != status {
                        problems.push(format!("status: expected {}, got {}", status, resp.status));
                    }
                }
                if problems.is_empty() {
                    Ok(())
                } else {
                    Err(format!("{} (full: {})", problems.join("; "), resp.body))
                }
            }
        } else {
            Err("case has neither `expect` nor `error`".to_string())
        };

        match (result, todo) {
            (Ok(()), false) => out.passed += 1,
            (Ok(()), true) => out.unexpected_passes.push(label),
            (Err(msg), true) => {
                if std::env::var_os("FQL_CONFORMANCE_SHOW_TODO").is_some() {
                    eprintln!("todo: {}\n    {}", label, msg);
                }
                out.todo += 1
            }
            (Err(msg), false) => out.failures.push(format!("{}\n    {}", label, msg)),
        }
    }
    out
}

fn seed(emu: &mut Emulator, d: &Json) -> Result<(), String> {
    let o = d.as_object().ok_or("expected an object")?;
    let collection = o.get("collection").and_then(|c| c.as_str()).ok_or("missing `collection`")?;
    let id = match o.get("id") {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Number(n)) => n.to_string(),
        _ => return Err("missing `id`".into()),
    };
    let ts = match o.get("ts").and_then(|t| t.as_i64()) {
        Some(micros) => Timestamp::of_micros(micros),
        None => emu.db.clock.next(),
    };
    let mut fields = Object::new();
    for (k, v) in o {
        if k == "collection" || k == "id" || k == "ts" {
            continue;
        }
        let value: Value = wire::decode_literal(v, &Position::root()).map_err(|e| format!("{}: {:?}", k, e))?;
        fields.insert(k.clone(), value);
    }
    emu.db.seed_document(collection, &id, fields, ts).map_err(|e| format!("{:?}", e))
}

#[test]
fn conformance_cases() {
    let dir = cases_dir();
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("list {}: {}", dir.display(), e))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no conformance cases in {}", dir.display());

    let mut failures = vec![];
    let mut unexpected = vec![];
    let (mut passed, mut todo) = (0, 0);
    for f in &files {
        let o = run_suite(f);
        failures.extend(o.failures);
        unexpected.extend(o.unexpected_passes);
        passed += o.passed;
        todo += o.todo;
    }
    eprintln!("conformance: {} passed, {} todo, {} failed", passed, todo, failures.len());
    if !unexpected.is_empty() {
        eprintln!("todo cases that now pass (remove their `todo` marker):\n  {}", unexpected.join("\n  "));
    }
    if !failures.is_empty() {
        panic!("{} conformance failures:\n{}", failures.len(), failures.join("\n"));
    }
}

/// Everything a query could change apart from the clock and id counter.
fn stored_state(e: &Emulator) -> String {
    let db = &e.db;
    format!(
        "{:?}",
        (&db.collections, &db.indexes, &db.functions, &db.roles, &db.documents, &db.pending, db.doc_count, &db.identity)
    )
}
