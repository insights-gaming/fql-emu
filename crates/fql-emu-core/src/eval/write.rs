//! Write functions: `Create`, `Update`, `Replace`, `Delete`,
//! `CreateCollection`, `CreateIndex`, `CreateFunction`, `CreateRole`, and
//! the deliberately unsupported ones (`Insert`, `Remove`, `CreateKey`,
//! `CreateDatabase`, `Login`, ...).
//!
//! Spec: ext/model/src/main/scala/ast/functions/WriteFunctions.scala and
//! WriteAdaptor.scala (unique-constraint checks are in `WriteAdaptor`,
//! raising `InstanceNotUnique`). Fauna's `Update` merges `data` one level
//! deep per nested object (a `null` field value deletes the field) and
//! `Replace` swaps the whole `data`.

use super::cast;
use super::forms::{Args, Registry};
use super::{Ctx, Env};
use crate::error::{Position, R};
use crate::value::Value;

pub fn register(r: &mut Registry) {
    r.stub("create", &["create"], &["params"]);
    r.stub("update", &["update"], &["params"]);
    r.stub("replace", &["replace"], &["params"]);
    r.stub("delete", &["delete"], &[]);
    r.add("create_collection", &["create_collection"], &[], create_collection);
    r.add("create_class", &["create_class"], &[], create_collection);
    r.add("create_index", &["create_index"], &[], create_index);
    r.add("create_function", &["create_function"], &[], create_function);
    r.add("create_role", &["create_role"], &[], create_role);
    r.stub("create_database", &["create_database"], &[]);
    r.stub("create_key", &["create_key"], &[]);
    r.stub("create_access_provider", &["create_access_provider"], &[]);
    r.stub("insert", &["insert", "ts", "action"], &["params"]);
    r.stub("remove", &["remove", "ts", "action"], &[]);
    r.stub("move_database", &["move_database", "to"], &[]);
}

fn create_collection(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let key = if args.get("create_collection").is_some() { "create_collection" } else { "create_class" };
    let v = args.eval(ctx, env, key, pos)?;
    let params = cast::object(&v, &pos.key(key))?;
    let ts = ctx.ts;
    ctx.db.create_collection(params, ts, &pos.key(key))
}

fn create_index(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "create_index", pos)?;
    let params = cast::object(&v, &pos.key("create_index"))?;
    let ts = ctx.ts;
    ctx.db.create_index(params, ts, &pos.key("create_index"))
}

fn create_function(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "create_function", pos)?;
    let params = cast::object(&v, &pos.key("create_function"))?;
    let ts = ctx.ts;
    ctx.db.create_function(params, ts, &pos.key("create_function"))
}

fn create_role(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "create_role", pos)?;
    let params = cast::object(&v, &pos.key("create_role"))?;
    let ts = ctx.ts;
    ctx.db.create_role(params, ts, &pos.key("create_role"))
}
