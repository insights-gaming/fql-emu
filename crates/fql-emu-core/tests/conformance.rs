//! Data-driven conformance suite.
//!
//! Every `conformance/cases/*.json` file is a suite:
//!
//! ```json
//! {
//!   "description": "what this file covers",
//!   "source": "ext/api/src/test/scala/queries/StringFunctionsSpec.scala",
//!   "now_micros": 1600000000000000,          // optional: pin Now()
//!   "schema": { "collections": [...], ... },  // optional: Emulator::load_schema bundle
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
//! they unexpectedly pass, so the marker can be removed).

use fql_emu_core::Emulator;
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
    if let Some(Json::Array(setup)) = suite.get("setup") {
        for (i, q) in setup.iter().enumerate() {
            let r = base.query(q);
            assert!(r.status == 200 || r.status == 201, "{}: setup[{}] failed: {}", file, i, r.body);
        }
    }

    let mut out = Outcome { failures: vec![], unexpected_passes: vec![], passed: 0, todo: 0 };
    let cases = suite.get("cases").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    for case in cases {
        let name = case.get("name").and_then(|n| n.as_str()).unwrap_or("<unnamed>").to_string();
        let label = format!("{} :: {}", file, name);
        let todo = case.get("todo").is_some();
        let mut emu = base.fork();
        let resp = emu.query(case.get("query").unwrap_or(&Json::Null));

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
            (Err(_), true) => out.todo += 1,
            (Err(msg), false) => out.failures.push(format!("{}\n    {}", label, msg)),
        }
    }
    out
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
