//! Form registry: maps the key set of a query object to a handler, like
//! `fauna.ast.QueryParser.forms` (a `MapRouter` keyed on exact key sets).
//!
//! Each module registers its own forms in `register()`. A form matches an
//! object when the object's keys contain every `required` key and nothing
//! outside `required ∪ optional`. The first registered match wins, so
//! register more specific key sets first.

use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::Backend;
use crate::value::Value;
use serde_json::{Map, Value as Json};
use std::collections::HashSet;

pub type Handler<B> = fn(&mut Ctx<B>, &Env, &Args, &Position) -> R<Value>;

pub struct Form<B> {
    pub name: &'static str,
    pub required: &'static [&'static str],
    pub optional: &'static [&'static str],
    /// `None` for a plain stub, which answers `not emulated`.
    pub handler: Option<Handler<B>>,
    /// False for stubs, including those registered with a handler
    /// ([`Registry::stub_with`]).
    pub implemented: bool,
}

pub struct Registry<B> {
    pub forms: Vec<Form<B>>,
}

impl<B> Default for Registry<B> {
    fn default() -> Self {
        Registry { forms: Vec::new() }
    }
}

impl<B> Registry<B> {
    pub fn add(
        &mut self,
        name: &'static str,
        required: &'static [&'static str],
        optional: &'static [&'static str],
        handler: Handler<B>,
    ) {
        self.forms.push(Form { name, required, optional, handler: Some(handler), implemented: true });
    }

    /// Register a form that is recognised but not implemented. Calling it
    /// fails loudly with a `not emulated` error naming the form.
    pub fn stub(&mut self, name: &'static str, required: &'static [&'static str], optional: &'static [&'static str]) {
        self.forms.push(Form { name, required, optional, handler: None, implemented: false });
    }

    /// A stub whose handler runs the checks the server makes before the
    /// point where the form is not implemented (for example a permission
    /// check) and then answers `not emulated` itself. Listed as not
    /// implemented by [`Registry::names`].
    pub fn stub_with(
        &mut self,
        name: &'static str,
        required: &'static [&'static str],
        optional: &'static [&'static str],
        handler: Handler<B>,
    ) {
        self.forms.push(Form { name, required, optional, handler: Some(handler), implemented: false });
    }

    pub fn lookup(&self, keys: &HashSet<&str>) -> Option<&Form<B>> {
        self.forms.iter().find(|f| {
            f.required.iter().all(|k| keys.contains(k))
                && keys.iter().all(|k| f.required.contains(k) || f.optional.contains(k))
        })
    }

    /// Every registered form name, with whether it is implemented.
    pub fn names(&self) -> Vec<(&'static str, bool)> {
        self.forms.iter().map(|f| (f.name, f.implemented)).collect()
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
    pub fn eval<B: Backend>(&self, ctx: &mut Ctx<B>, env: &Env, key: &str, pos: &Position) -> R<Value> {
        super::eval(ctx, env, self.req(key), &pos.key(key))
    }

    /// Evaluate an optional argument at `pos[key]`.
    pub fn eval_opt<B: Backend>(&self, ctx: &mut Ctx<B>, env: &Env, key: &str, pos: &Position) -> R<Option<Value>> {
        match self.get(key) {
            Some(j) => super::eval(ctx, env, j, &pos.key(key)).map(Some),
            None => Ok(None),
        }
    }
}

/// The storage-independent forms: everything in `basic`, `logic`,
/// `numeric`, `string`, `time` and `collection`. A backend adds its own
/// forms (`get`, `create`, `match`, `paginate`, ...) on top with
/// [`Registry::add`]. Forms registered later only win for key sets no
/// language form matches, since the first match wins.
pub fn language_forms<B: Backend>() -> Registry<B> {
    let mut r = Registry::default();
    super::basic::register(&mut r);
    super::logic::register(&mut r);
    super::numeric::register(&mut r);
    super::string::register(&mut r);
    super::time::register(&mut r);
    super::collection::register(&mut r);
    r
}

/// Dispatch `obj` to the form `ctx.registry` matches it to, or `None` if
/// it is not a form. A stub answers `not emulated`.
pub fn dispatch<B: Backend>(ctx: &mut Ctx<B>, env: &Env, obj: &Map<String, Json>, pos: &Position) -> Option<R<Value>> {
    let keys: HashSet<&str> = obj.keys().map(|k| k.as_str()).collect();
    let registry = ctx.registry;
    let form = registry.lookup(&keys)?;
    Some(match form.handler {
        Some(h) => h(ctx, env, &Args { obj }, pos),
        None => one(FaunaError::not_emulated(&format!("the `{}` function", form.name), pos)),
    })
}
