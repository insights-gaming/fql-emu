//! Time, date and conversion functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/TimeFunctions.scala and
//! ConversionFunctions.scala. `Now()` is the transaction timestamp and is
//! constant for the whole query.

use super::cast;
use super::forms::{Args, Registry};
use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::value::{Timestamp, Value};

pub fn register(r: &mut Registry) {
    r.add("time", &["time"], &[], time);
    r.add("now", &["now"], &[], now);
    r.stub("epoch", &["epoch", "unit"], &[]);
    r.add("date", &["date"], &[], date);
    r.add("to_micros", &["to_micros"], &[], |c, e, a, p| to_unit("to_micros", c, e, a, p));
    r.add("to_millis", &["to_millis"], &[], |c, e, a, p| to_unit("to_millis", c, e, a, p));
    r.add("to_seconds", &["to_seconds"], &[], |c, e, a, p| to_unit("to_seconds", c, e, a, p));
    r.stub("second", &["second"], &[]);
    r.stub("minute", &["minute"], &[]);
    r.stub("hour", &["hour"], &[]);
    r.stub("day_of_month", &["day_of_month"], &[]);
    r.stub("day_of_week", &["day_of_week"], &[]);
    r.stub("day_of_year", &["day_of_year"], &[]);
    r.stub("month", &["month"], &[]);
    r.stub("year", &["year"], &[]);
    r.stub("time_add", &["time_add", "offset", "unit"], &[]);
    r.stub("time_subtract", &["time_subtract", "offset", "unit"], &[]);
    r.stub("time_diff", &["time_diff", "other", "unit"], &[]);
    r.stub("to_string", &["to_string"], &[]);
    r.stub("to_number", &["to_number"], &[]);
    r.stub("to_double", &["to_double"], &[]);
    r.stub("to_integer", &["to_integer"], &[]);
    r.stub("to_time", &["to_time"], &[]);
    r.stub("to_date", &["to_date"], &[]);
}

fn time(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "time", pos)?;
    let p = pos.key("time");
    let s = cast::string(&v, &p)?;
    if s == "now" {
        return Ok(Value::Time(ctx.ts));
    }
    match Timestamp::parse(s.trim()) {
        Some(t) => Ok(Value::Time(t)),
        None => one(FaunaError::invalid_time_argument(s, &p)),
    }
}

fn now(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "now", pos)?;
    cast::null(&v, &pos.key("now"))?;
    Ok(Value::Time(ctx.ts))
}

fn date(ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, "date", pos)?;
    let p = pos.key("date");
    let s = cast::string(&v, &p)?;
    match chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
        Ok(d) => Ok(Value::Date(d)),
        Err(_) => one(FaunaError::invalid_date_argument(s, &p)),
    }
}

fn to_unit(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let t = cast::timestamp(&v, &pos.key(key))?;
    Ok(Value::Long(match key {
        "to_micros" => t.micros(),
        "to_millis" => t.millis(),
        _ => t.secs,
    }))
}
