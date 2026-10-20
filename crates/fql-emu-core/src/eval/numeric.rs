//! Numeric and trigonometric functions.
//!
//! Spec: ext/model/src/main/scala/ast/functions/NumericFunctions.scala and
//! TrigFunctions.scala. Long arithmetic stays Long (and overflows wrap like
//! the JVM); any Double operand promotes to Double.

use super::cast::{self, Num};
use super::forms::{Args, Registry};
use super::{Ctx, Env};
use crate::error::{one, FaunaError, Position, R};
use crate::value::Value;

pub fn register(r: &mut Registry) {
    r.add("add", &["add"], &[], |c, e, a, p| fold("add", c, e, a, p));
    r.add("subtract", &["subtract"], &[], |c, e, a, p| fold("subtract", c, e, a, p));
    r.add("multiply", &["multiply"], &[], |c, e, a, p| fold("multiply", c, e, a, p));
    r.add("divide", &["divide"], &[], |c, e, a, p| fold("divide", c, e, a, p));
    r.add("modulo", &["modulo"], &[], |c, e, a, p| fold("modulo", c, e, a, p));
    r.stub("bit_and", &["bit_and"], &[]);
    r.stub("bitand", &["bitand"], &[]);
    r.stub("bit_or", &["bit_or"], &[]);
    r.stub("bitor", &["bitor"], &[]);
    r.stub("bit_xor", &["bit_xor"], &[]);
    r.stub("bitxor", &["bitxor"], &[]);
    r.stub("bit_not", &["bit_not"], &[]);
    r.stub("bitnot", &["bitnot"], &[]);
    r.add("abs", &["abs"], &[], |c, e, a, p| unary("abs", c, e, a, p));
    r.add("ceil", &["ceil"], &[], |c, e, a, p| unary("ceil", c, e, a, p));
    r.add("floor", &["floor"], &[], |c, e, a, p| unary("floor", c, e, a, p));
    r.add("sign", &["sign"], &[], |c, e, a, p| unary("sign", c, e, a, p));
    r.add("sqrt", &["sqrt"], &[], |c, e, a, p| unary("sqrt", c, e, a, p));
    r.stub("round", &["round"], &["precision"]);
    r.stub("trunc", &["trunc"], &["precision"]);
    r.stub("acos", &["acos"], &[]);
    r.stub("asin", &["asin"], &[]);
    r.stub("atan", &["atan"], &[]);
    r.stub("cos", &["cos"], &[]);
    r.stub("cosh", &["cosh"], &[]);
    r.stub("degrees", &["degrees"], &[]);
    r.stub("exp", &["exp"], &[]);
    r.stub("hypot", &["hypot"], &["b"]);
    r.stub("ln", &["ln"], &[]);
    r.stub("log", &["log"], &[]);
    r.stub("pow", &["pow"], &["exp"]);
    r.stub("radians", &["radians"], &[]);
    r.stub("sin", &["sin"], &[]);
    r.stub("sinh", &["sinh"], &[]);
    r.stub("tan", &["tan"], &[]);
    r.stub("tanh", &["tanh"], &[]);
}

fn fold(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let p = pos.key(key);
    let es = cast::one_or_more(&v, &p)?;
    let mut nums = Vec::with_capacity(es.len());
    let mut errs = Vec::new();
    for (e, ep) in &es {
        match cast::number(e, ep) {
            Ok(n) => nums.push(n),
            Err(e) => errs.extend(e),
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let mut acc = nums[0];
    for n in &nums[1..] {
        acc = match (key, acc, *n) {
            ("add", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_add(b)),
            ("add", a, b) => Num::Double(a.as_f64() + b.as_f64()),
            ("subtract", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_sub(b)),
            ("subtract", a, b) => Num::Double(a.as_f64() - b.as_f64()),
            ("multiply", Num::Long(a), Num::Long(b)) => Num::Long(a.wrapping_mul(b)),
            ("multiply", a, b) => Num::Double(a.as_f64() * b.as_f64()),
            ("divide", Num::Long(a), Num::Long(b)) => {
                if b == 0 {
                    return one(FaunaError::divide_by_zero(&p));
                }
                Num::Long(a.wrapping_div(b))
            }
            ("divide", a, b) => {
                if b.as_f64() == 0.0 {
                    return one(FaunaError::divide_by_zero(&p));
                }
                Num::Double(a.as_f64() / b.as_f64())
            }
            ("modulo", Num::Long(a), Num::Long(b)) => {
                if b == 0 {
                    return one(FaunaError::divide_by_zero(&p));
                }
                Num::Long(a.wrapping_rem(b))
            }
            ("modulo", a, b) => {
                if b.as_f64() == 0.0 {
                    return one(FaunaError::divide_by_zero(&p));
                }
                Num::Double(a.as_f64() % b.as_f64())
            }
            _ => unreachable!(),
        };
    }
    Ok(acc.to_value())
}

fn unary(key: &str, ctx: &mut Ctx, env: &Env, args: &Args, pos: &Position) -> R<Value> {
    let v = args.eval(ctx, env, key, pos)?;
    let p = pos.key(key);
    let n = cast::number(&v, &p)?;
    Ok(match (key, n) {
        ("abs", Num::Long(l)) => Value::Long(l.wrapping_abs()),
        ("abs", Num::Double(d)) => Value::Double(d.abs()),
        ("ceil", Num::Long(l)) => Value::Long(l),
        ("ceil", Num::Double(d)) => Value::Double(d.ceil()),
        ("floor", Num::Long(l)) => Value::Long(l),
        ("floor", Num::Double(d)) => Value::Double(d.floor()),
        ("sign", Num::Long(l)) => Value::Long(l.signum()),
        ("sign", Num::Double(d)) => Value::Double(if d == 0.0 { 0.0 } else { d.signum() }),
        ("sqrt", n) => Value::Double(n.as_f64().sqrt()),
        _ => unreachable!(),
    })
}
