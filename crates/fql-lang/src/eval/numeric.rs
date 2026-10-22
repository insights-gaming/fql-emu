//! Numeric and trigonometric functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/NumericFunctions.scala and
//! TrigFunctions.scala. Long arithmetic stays Long (and overflows wrap like
//! the JVM); any Double operand promotes to Double. Rust's `as` casts from
//! f64 saturate and map NaN to 0, exactly like the JVM's `toLong`/`toInt`.

use super::cast::{self, Num};
use super::forms::{Args, Registry};
use super::string::jformat::java_double_to_string;
use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::Backend;
use crate::value::Value;

pub fn register<B: Backend>(r: &mut Registry<B>) {
    r.add("add", &["add"], &[], |c, e, a, p| fold("add", "add", c, e, a, p));
    r.add("subtract", &["subtract"], &[], |c, e, a, p| fold("subtract", "subtract", c, e, a, p));
    r.add("multiply", &["multiply"], &[], |c, e, a, p| fold("multiply", "multiply", c, e, a, p));
    r.add("divide", &["divide"], &[], |c, e, a, p| fold("divide", "divide", c, e, a, p));
    r.add("modulo", &["modulo"], &[], |c, e, a, p| fold("modulo", "modulo", c, e, a, p));
    r.add("bitand", &["bitand"], &[], |c, e, a, p| fold("bitand", "bit_and", c, e, a, p));
    r.add("bitor", &["bitor"], &[], |c, e, a, p| fold("bitor", "bit_or", c, e, a, p));
    r.add("bitxor", &["bitxor"], &[], |c, e, a, p| fold("bitxor", "bit_xor", c, e, a, p));
    r.add("bitnot", &["bitnot"], &[], |c, e, a, p| unary("bitnot", "bit_not", c, e, a, p));
    r.add("abs", &["abs"], &[], |c, e, a, p| unary("abs", "abs", c, e, a, p));
    r.add("ceil", &["ceil"], &[], |c, e, a, p| unary("ceil", "ceil", c, e, a, p));
    r.add("floor", &["floor"], &[], |c, e, a, p| unary("floor", "floor", c, e, a, p));
    r.add("sign", &["sign"], &[], |c, e, a, p| unary("sign", "sign", c, e, a, p));
    r.add("sqrt", &["sqrt"], &[], |c, e, a, p| unary("sqrt", "sqrt", c, e, a, p));
    r.add("acos", &["acos"], &[], |c, e, a, p| unary("acos", "acos", c, e, a, p));
    r.add("asin", &["asin"], &[], |c, e, a, p| unary("asin", "asin", c, e, a, p));
    r.add("atan", &["atan"], &[], |c, e, a, p| unary("atan", "atan", c, e, a, p));
    r.add("cos", &["cos"], &[], |c, e, a, p| unary("cos", "cos", c, e, a, p));
    r.add("cosh", &["cosh"], &[], |c, e, a, p| unary("cosh", "cosh", c, e, a, p));
    r.add("degrees", &["degrees"], &[], |c, e, a, p| unary("degrees", "degrees", c, e, a, p));
    r.add("exp", &["exp"], &[], |c, e, a, p| unary("exp", "exp", c, e, a, p));
    r.add("ln", &["ln"], &[], |c, e, a, p| unary("ln", "ln", c, e, a, p));
    r.add("log", &["log"], &[], |c, e, a, p| unary("log", "log", c, e, a, p));
    r.add("radians", &["radians"], &[], |c, e, a, p| unary("radians", "radians", c, e, a, p));
    r.add("sin", &["sin"], &[], |c, e, a, p| unary("sin", "sin", c, e, a, p));
    r.add("sinh", &["sinh"], &[], |c, e, a, p| unary("sinh", "sinh", c, e, a, p));
    r.add("tan", &["tan"], &[], |c, e, a, p| unary("tan", "tan", c, e, a, p));
    r.add("tanh", &["tanh"], &[], |c, e, a, p| unary("tanh", "tanh", c, e, a, p));
    r.add("round", &["round"], &["precision"], |c, e, a, p| precision_fn("round", c, e, a, p));
    r.add("trunc", &["trunc"], &["precision"], |c, e, a, p| precision_fn("trunc", c, e, a, p));
    r.add("hypot", &["hypot"], &["b"], hypot);
    r.add("pow", &["pow"], &["exp"], pow);
}

fn numbers(v: &Value, p: &Position) -> R<Vec<Num>> {
    let es = cast::one_or_more(v, p)?;
    let mut nums = Vec::with_capacity(es.len());
    let mut errs = Vec::new();
    for (e, ep) in &es {
        match cast::number(e, ep) {
            Ok(n) => nums.push(n),
            Err(e) => errs.extend(e),
        }
    }
    if errs.is_empty() {
        Ok(nums)
    } else {
        Err(errs)
    }
}

/// `NumberFolds`: once the accumulator is a Double it stays one.
fn fold<B: Backend>(key: &str, op: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let nums = numbers(&v, &pos.key(key))?;
    let mut acc = nums[0];
    for n in &nums[1..] {
        acc = match (op, acc, *n) {
            ("add", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_add(b)),
            ("add", a, b) => Num::Double(a.as_f64() + b.as_f64()),
            ("subtract", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_sub(b)),
            ("subtract", a, b) => Num::Double(a.as_f64() - b.as_f64()),
            ("multiply", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_mul(b)),
            ("multiply", a, b) => Num::Double(a.as_f64() * b.as_f64()),
            ("divide" | "modulo", _, b) if b.as_f64() == 0.0 => return one(FaunaError::divide_by_zero(pos)),
            ("divide", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_div(b)),
            ("divide", a, b) => Num::Double(a.as_f64() / b.as_f64()),
            ("modulo", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_rem(b)),
            ("modulo", a, b) => Num::Double(a.as_f64() % b.as_f64()),
            (bit, a, b) => {
                let f = match bit {
                    "bit_and" => |x: i64, y: i64| x & y,
                    "bit_or" => |x: i64, y: i64| x | y,
                    _ => |x: i64, y: i64| x ^ y,
                };
                match (a, b) {
                    (Num::Long(x), Num::Long(y)) => Num::Long(f(x, y)),
                    (x, y) => Num::Double(f(to_long(x), to_long(y)) as f64),
                }
            }
        };
    }
    Ok(acc.to_value())
}

fn to_long(n: Num) -> i64 {
    match n {
        Num::Long(l) => l,
        Num::Double(d) => d as i64,
    }
}

/// `BoundsError(value.toString, predicate, pos)`.
fn bounds(n: Num, predicate: &str, pos: &Position) -> R<Value> {
    let s = match n {
        Num::Long(l) => l.to_string(),
        Num::Double(d) => java_double_to_string(d),
    };
    one(FaunaError::bounds(&s, predicate, pos))
}

fn unary<B: Backend>(key: &str, op: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let n = cast::number(&v, &pos.key(key))?;
    let x = n.as_f64();
    let d = Value::Double;
    Ok(match (op, n) {
        ("abs", Num::Long(l)) => Value::Long(l.wrapping_abs()),
        ("abs", Num::Double(f)) => d(f.abs()),
        ("ceil" | "floor", Num::Long(l)) => Value::Long(l),
        ("ceil", Num::Double(f)) => d(f.ceil()),
        ("floor", Num::Double(f)) => d(f.floor()),
        ("sign", Num::Long(l)) => Value::Long(l.signum()),
        // NaN has sign 1.
        ("sign", Num::Double(f)) => Value::Long(if f < 0.0 { -1 } else { 1 }),
        ("bit_not", n) => Value::Long(!to_long(n)),
        ("sqrt", _) if x.is_nan() || x < 0.0 => return bounds(n, "positive", pos),
        ("sqrt", _) => d(x.sqrt()),
        ("acos" | "asin", _) if !(-1.0..=1.0).contains(&x) => {
            return bounds(n, "between -1 and 1 inclusive", pos)
        }
        ("acos", _) => d(x.acos()),
        ("asin", _) => d(x.asin()),
        ("atan", _) => d(x.atan()),
        ("cos", _) => d(x.cos()),
        ("cosh", _) => d(x.cosh()),
        ("degrees", _) => d(x.to_degrees()),
        ("exp", _) => d(x.exp()),
        ("ln" | "log", _) if x.is_nan() || x <= 0.0 => return bounds(n, "greater than 0", pos),
        ("ln", _) => d(x.ln()),
        ("log", _) => d(x.log10()),
        ("radians", _) => d(x.to_radians()),
        ("sin", _) => d(x.sin()),
        ("sinh", _) => d(x.sinh()),
        ("tan", _) => d(x.tan()),
        ("tanh", _) => d(x.tanh()),
        _ => unreachable!("{}", op),
    })
}

fn pow10(dp: i64) -> f64 {
    10f64.powf(dp as f64)
}

/// `RoundFunction.roundOff(Long, dp)`.
fn round_long(num: i64, dp: i64) -> i64 {
    if dp > 0 {
        return num;
    }
    let tmp = pow10(dp);
    ((((num as f64 * tmp) + 0.5) as i64) as f64 / tmp) as i64
}

/// `RoundFunction.roundOff(Double, dp)`.
fn round_double(num: f64, dp: i64) -> f64 {
    if dp > 0 {
        let tmp = pow10(dp) as i32;
        let wnum = num as i64;
        let fract = ((num - wnum as f64 + 0.5 / tmp as f64) * tmp as f64) as i64;
        wnum as f64 + fract as f64 / tmp as f64
    } else {
        let tmp = pow10(dp);
        (((((num * tmp) + 0.5) as i64) as f64 / tmp) as i64) as f64
    }
}

/// `TruncFunction.trunc(Long, dp)`.
fn trunc_long(num: i64, dp: i64) -> i64 {
    if dp > 0 {
        return num;
    }
    let tmp = pow10(dp);
    (((num as f64 * tmp) as i64) as f64 / tmp) as i64
}

/// `TruncFunction.truncDouble`.
fn trunc_double(num: f64, dp: i64) -> f64 {
    if dp > 0 {
        let tmp = pow10(dp) as i32;
        let wnum = num as i64;
        let fract = ((num - wnum as f64) * tmp as f64) as i64;
        wnum as f64 + fract as f64 / tmp as f64
    } else {
        let tmp = pow10(dp);
        ((num * tmp) as i64) as f64 / tmp
    }
}

fn precision_fn<B: Backend>(key: &str, ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let n = args.eval(ctx, env, key, pos).and_then(|v| cast::number(&v, &pos.key(key)));
    let dp = match args.eval_opt(ctx, env, "precision", pos) {
        Ok(Some(v)) => cast::long(&v, &pos.key("precision")).map(Some),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    let (n, dp) = super::string::join2(n, dp)?;
    Ok(match (key, n, dp) {
        (_, Num::Long(l), None) => Value::Long(l),
        ("round", Num::Long(l), Some(p)) => Value::Long(round_long(l, p)),
        ("round", Num::Double(d), p) => Value::Double(round_double(d, p.unwrap_or(2))),
        (_, Num::Long(l), Some(p)) => Value::Long(trunc_long(l, p)),
        (_, Num::Double(d), p) => Value::Double(trunc_double(d, p.unwrap_or(2))),
    })
}

fn opt_number<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, key: &str, pos: &Position) -> R<Option<Num>> {
    match args.eval_opt(ctx, env, key, pos)? {
        Some(v) => cast::number(&v, &pos.key(key)).map(Some),
        None => Ok(None),
    }
}

fn hypot<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let a = args.eval(ctx, env, "hypot", pos).and_then(|v| cast::number(&v, &pos.key("hypot")));
    let (a, b) = super::string::join2(a, opt_number(ctx, env, args, "b", pos))?;
    Ok(Value::Double(a.as_f64().hypot(b.unwrap_or(a).as_f64())))
}

fn pow<B: Backend>(ctx: &mut Ctx<B>, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let a = args.eval(ctx, env, "pow", pos).and_then(|v| cast::number(&v, &pos.key("pow")));
    let (a, e) = super::string::join2(a, opt_number(ctx, env, args, "exp", pos))?;
    Ok(Value::Double(a.as_f64().powf(e.map(|e| e.as_f64()).unwrap_or(2.0))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_and_trunc_match_the_scala() {
        assert_eq!(round_double(5.123, 2), 5.12);
        assert_eq!(round_double(5.999, 2), 6.0);
        assert_eq!(round_double(555.666, -2), 600.0);
        assert_eq!(round_double(555.666, 1), 555.7);
        assert_eq!(round_long(555, -1), 560);
        assert_eq!(round_long(444, -2), 400);
        assert_eq!(trunc_double(1234.5678, 2), 1234.56);
        assert_eq!(trunc_double(1234.5678, -2), 1200.0);
        assert_eq!(trunc_long(1234, -2), 1200);
    }
}
