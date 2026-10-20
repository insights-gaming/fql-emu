//! wasm-bindgen surface for `fql-emu-core`.
//!
//! Strings in, strings out: the JS side passes the exact JSON the driver
//! would have POSTed and gets back `{status, body}` as a JSON string.

use fql_emu_core::{wire, Emulator, Response};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct FqlEmulator {
    inner: Emulator,
}

fn response_json(r: Response) -> String {
    serde_json::json!({ "status": r.status, "body": r.body, "txn_time_micros": r.txn_time_micros }).to_string()
}

#[wasm_bindgen]
impl FqlEmulator {
    #[wasm_bindgen(constructor)]
    pub fn new() -> FqlEmulator {
        FqlEmulator { inner: Emulator::new() }
    }

    /// Run one query. `expr_json` is the serialised query expression.
    /// Returns `{"status": 200, "body": {"resource": ...}, "txn_time_micros": n}`
    /// or `{"status": 4xx, "body": {"errors": [...]}, "txn_time_micros": n}`
    /// (`txn_time_micros` is `null` when the body was not valid JSON).
    pub fn query(&mut self, expr_json: &str) -> String {
        response_json(self.inner.query_str(expr_json))
    }

    pub fn fork(&self) -> FqlEmulator {
        FqlEmulator { inner: self.inner.fork() }
    }

    /// Load a schema bundle (see `Emulator::load_schema`). Returns `null`
    /// on success or a JSON array of errors.
    pub fn load_schema(&mut self, bundle_json: &str) -> String {
        let bundle: serde_json::Value = match serde_json::from_str(bundle_json) {
            Ok(j) => j,
            Err(e) => return serde_json::json!([{"code": "invalid expression", "description": e.to_string()}]).to_string(),
        };
        match self.inner.load_schema(&bundle) {
            Ok(()) => "null".to_string(),
            Err(errs) => serde_json::Value::Array(errs.iter().map(|e| e.to_json()).collect()).to_string(),
        }
    }

    pub fn set_now_micros(&mut self, micros: f64) {
        self.inner.set_now_micros(micros as i64);
    }

    pub fn set_auto_clock_from_micros(&mut self, micros: f64) {
        self.inner.set_auto_clock_from_micros(micros as i64);
    }

    /// `id` is a JS `BigInt`: Fauna ids (~3e17) do not fit in an f64.
    pub fn set_next_id(&mut self, id: u64) {
        self.inner.set_next_id(id);
    }

    /// `identity_json` is a `{"@ref": ...}` value, or `null`.
    pub fn set_identity(&mut self, identity_json: &str) -> String {
        let j: serde_json::Value = match serde_json::from_str(identity_json) {
            Ok(j) => j,
            Err(e) => return e.to_string(),
        };
        if j.is_null() {
            self.inner.set_identity(None);
            return "null".into();
        }
        match wire::decode_literal(&j, &fql_emu_core::error::Position::root()) {
            Ok(fql_emu_core::value::Value::Ref(r)) => {
                self.inner.set_identity(Some(r));
                "null".into()
            }
            Ok(_) => "identity must be a ref".into(),
            Err(e) => serde_json::Value::Array(e.iter().map(|x| x.to_json()).collect()).to_string(),
        }
    }

    pub fn set_max_documents(&mut self, n: f64) {
        self.inner.set_max_documents(n as usize);
    }

    /// Simulate a server-readonly key; see `Emulator::set_read_only`.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.inner.set_read_only(read_only);
    }

    pub fn read_only(&self) -> bool {
        self.inner.read_only()
    }

    pub fn stats(&self) -> String {
        self.inner.stats().to_string()
    }

    /// JSON array of `[name, implemented]` pairs.
    pub fn supported_forms() -> String {
        serde_json::Value::Array(
            Emulator::supported_forms()
                .into_iter()
                .map(|(n, ok)| serde_json::json!([n, ok]))
                .collect(),
        )
        .to_string()
    }
}

impl Default for FqlEmulator {
    fn default() -> Self {
        Self::new()
    }
}
