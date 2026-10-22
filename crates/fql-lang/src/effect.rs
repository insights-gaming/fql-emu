//! Static write-effect analysis (`Expression.maxEffect`).
//!
//! Spec: ext/model/src/main/scala/ast/Expression.scala (`maxEffect`),
//! ext/model/src/main/scala/runtime/Effect.scala and the `effect` of each
//! `QFunction`.
//!
//! The analysis walks the unevaluated query JSON. Forms are recognised by
//! the same exact-key-set lookup the interpreter uses, so it takes the
//! [`Registry`] in use: an expression is rated by the forms that registry
//! would dispatch it to (a `replace_str` object is not a `replace`).

use crate::eval::forms::Registry;
use crate::value::Lambda;
use serde_json::Value as Json;
use std::collections::HashSet;

/// `fauna.model.runtime.Effect`, ordered by strength. The server computes
/// a lambda's maximum effect statically and rejects impure lambdas in some
/// contexts (`Join` targets must be pure, set `Filter`s may not write,
/// index bindings must be pure).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    Pure,
    Observation,
    Read,
    Write,
}

/// The static maximum effect of a lambda's body.
pub fn lambda_effect<B>(reg: &Registry<B>, l: &Lambda) -> Effect {
    expr_effect(reg, &l.body)
}

pub fn is_inline_lambda(j: &Json) -> bool {
    match j {
        Json::Object(o) => {
            o.contains_key("lambda")
                && o.contains_key("expr")
                && o.keys().all(|k| k == "lambda" || k == "expr" || k == "api_version")
        }
        _ => false,
    }
}

/// The function effects from the server's `QFunction.effect` values.
/// Forms not listed are pure.
pub fn form_effect(name: &str) -> Effect {
    match name {
        "get" | "exists" | "paginate" | "first" | "last" | "key_from_secret" | "current_identity" | "identity"
        | "has_current_identity" | "has_identity" | "identify" | "current_token" | "has_current_token"
        | "contains_value" | "count" | "sum" | "mean" | "max" | "min" | "any" | "all" | "reduce" => Effect::Read,
        "create" | "update" | "replace" | "delete" | "insert" | "remove" | "move_database" | "new_id" | "next_id"
        | "login" | "logout" | "issue_access_jwt" | "create_collection" | "create_class" | "create_database"
        | "create_index" | "create_key" | "create_function" | "create_role" | "create_access_provider" | "call" => {
            Effect::Write
        }
        _ => Effect::Pure,
    }
}

/// `Expression.maxEffect` of an unevaluated expression: the maximum of the
/// form's own effect and its arguments' effects. Lambda literals are pure
/// values, except where the server adds the lambda body's effect (the
/// lambda of `Map`/`Foreach`/`Filter`/`Reduce`/`Merge`/`Join`).
pub fn expr_effect<B>(reg: &Registry<B>, expr: &Json) -> Effect {
    effect_of(reg, expr, true)
}

/// Whether a function body may write, for `Mode::ReadOnlyPureCalls`.
/// Unlike the server's `maxEffect`, a `call` form counts as a read: the
/// callee is checked when that `Call` runs.
pub fn body_writes<B>(reg: &Registry<B>, expr: &Json) -> bool {
    effect_of(reg, expr, false) == Effect::Write
}

fn effect_of<B>(reg: &Registry<B>, expr: &Json, cw: bool) -> Effect {
    match expr {
        Json::Array(items) => items.iter().map(|e| effect_of(reg, e, cw)).max().unwrap_or(Effect::Pure),
        Json::Object(o) => {
            if o.len() == 1 && o.keys().next().map(|k| k.starts_with('@')).unwrap_or(false) {
                return Effect::Pure;
            }
            if is_inline_lambda(expr) {
                return Effect::Pure;
            }
            let keys: HashSet<&str> = o.keys().map(|k| k.as_str()).collect();
            let name = reg.lookup(&keys).map(|f| f.name);
            match name {
                Some("quote") => return Effect::Pure,
                Some("object") => {
                    return match o.get("object") {
                        Some(Json::Object(fields)) => {
                            fields.values().map(|e| effect_of(reg, e, cw)).max().unwrap_or(Effect::Pure)
                        }
                        Some(other) => effect_of(reg, other, cw),
                        None => Effect::Pure,
                    }
                }
                Some("let") => {
                    let bindings = match o.get("let") {
                        Some(Json::Array(items)) => items
                            .iter()
                            .map(|b| match b {
                                Json::Object(bo) => bo.values().map(|e| effect_of(reg, e, cw)).max().unwrap_or(Effect::Pure),
                                other => effect_of(reg, other, cw),
                            })
                            .max()
                            .unwrap_or(Effect::Pure),
                        Some(Json::Object(bo)) => bo.values().map(|e| effect_of(reg, e, cw)).max().unwrap_or(Effect::Pure),
                        Some(other) => effect_of(reg, other, cw),
                        None => Effect::Pure,
                    };
                    return bindings.max(o.get("in").map(|e| effect_of(reg, e, cw)).unwrap_or(Effect::Pure));
                }
                _ => {}
            }
            let mut eff = match name {
                Some("call") if !cw => Effect::Read,
                Some(n) => form_effect(n),
                None => Effect::Pure,
            };
            for (k, v) in o {
                let arg = if is_inline_lambda(v) {
                    let body_counts = matches!(
                        (name, k.as_str()),
                        (Some("map"), "map")
                            | (Some("foreach"), "foreach")
                            | (Some("filter"), "filter")
                            | (Some("reduce"), "reduce")
                            | (Some("merge"), "lambda")
                            | (Some("join"), "with")
                    );
                    if body_counts {
                        v.get("expr").map(|e| effect_of(reg, e, cw)).unwrap_or(Effect::Pure)
                    } else {
                        Effect::Pure
                    }
                } else {
                    effect_of(reg, v, cw)
                };
                eff = eff.max(arg);
            }
            eff
        }
        _ => Effect::Pure,
    }
}

