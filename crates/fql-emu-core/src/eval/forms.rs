//! Form registry: maps the key set of a query object to a handler, like
//! `fauna.ast.QueryParser.forms` (a `MapRouter` keyed on exact key sets).
//!
//! Each module registers its own forms in `register()`. A form matches an
//! object when the object's keys contain every `required` key and nothing
//! outside `required ∪ optional`. The first registered match wins, so
//! register more specific key sets first.

use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::value::Value;
use serde_json::{Map, Value as Json};
use std::collections::HashSet;

pub type Handler = fn(&mut Ctx, &Env, &Args, &Position) -> R<Value>;

pub struct Form {
    pub name: &'static str,
    pub required: &'static [&'static str],
    pub optional: &'static [&'static str],
    pub handler: Option<Handler>,
}

#[derive(Default)]
pub struct Registry {
    pub forms: Vec<Form>,
}

impl Registry {
    pub fn add(
        &mut self,
        name: &'static str,
        required: &'static [&'static str],
        optional: &'static [&'static str],
        handler: Handler,
    ) {
        self.forms.push(Form { name, required, optional, handler: Some(handler) });
    }

    /// Register a form that is recognised but not implemented yet. Calling
    /// it fails loudly with a `not emulated` error naming the form.
    pub fn stub(&mut self, name: &'static str, required: &'static [&'static str], optional: &'static [&'static str]) {
        self.forms.push(Form { name, required, optional, handler: None });
    }

    pub fn lookup(&self, keys: &HashSet<&str>) -> Option<&Form> {
        self.forms.iter().find(|f| {
            f.required.iter().all(|k| keys.contains(k))
                && keys.iter().all(|k| f.required.contains(k) || f.optional.contains(k))
        })
    }

    pub fn names(&self) -> Vec<(&'static str, bool)> {
        self.forms.iter().map(|f| (f.name, f.handler.is_some())).collect()
    }
}

/// The raw (unevaluated) arguments of a form invocation.
pub struct Args<'a> {
    pub obj: &'a Map<String, Json>,
}

impl<'a> Args<'a> {
    pub fn get(&self, key: &str) -> Option<&'a Json> {
        self.obj.get(key)
    }

    /// A required key; forms are only dispatched when all required keys
    /// are present, so this cannot fail for them.
    pub fn req(&self, key: &str) -> &'a Json {
        self.obj.get(key).unwrap_or(&Json::Null)
    }

    /// Evaluate a required argument at `pos[key]`.
    pub fn eval(&self, ctx: &mut Ctx, env: &Env, key: &str, pos: &Position) -> R<Value> {
        super::eval(ctx, env, self.req(key), &pos.key(key))
    }

    /// Evaluate an optional argument at `pos[key]`.
    pub fn eval_opt(&self, ctx: &mut Ctx, env: &Env, key: &str, pos: &Position) -> R<Option<Value>> {
        match self.get(key) {
            Some(j) => super::eval(ctx, env, j, &pos.key(key)).map(Some),
            None => Ok(None),
        }
    }
}

pub fn build() -> Registry {
    let mut r = Registry::default();
    super::basic::register(&mut r);
    super::logic::register(&mut r);
    super::numeric::register(&mut r);
    super::string::register(&mut r);
    super::time::register(&mut r);
    super::collection::register(&mut r);
    super::sets::register(&mut r);
    super::read::register(&mut r);
    super::write::register(&mut r);
    r
}

thread_local! {
    static REGISTRY: Registry = build();
}

pub fn with_registry<T>(f: impl FnOnce(&Registry) -> T) -> T {
    REGISTRY.with(f)
}

pub fn dispatch(ctx: &mut Ctx, env: &Env, obj: &Map<String, Json>, pos: &Position) -> Option<R<Value>> {
    let keys: HashSet<&str> = obj.keys().map(|k| k.as_str()).collect();
    let found = with_registry(|r| r.lookup(&keys).map(|f| (f.name, f.handler)));
    match found {
        None => None,
        Some((name, None)) => Some(one(FaunaError::not_emulated(&format!("the `{}` function", name), pos))),
        Some((_, Some(h))) => Some(h(ctx, env, &Args { obj }, pos)),
    }
}
