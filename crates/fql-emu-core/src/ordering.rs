//! Total ordering of values, as used by index sorting, `Range`, `Lt`/`Gt`,
//! `Min`/`Max` and set algebra.
//!
//! Fauna orders index terms by comparing their CBOR encodings with
//! `fauna.codex.cbor.CBOROrdering` (ext/codex/src/main/scala/cbor/CBOROrdering.scala).
//! The resulting cross-type order is, lowest first:
//!
//! 1. numbers (Long and Double compared numerically; Double sorts before
//!    non-numbers)
//! 2. bytes (major type 2)
//! 3. strings (major type 3; ICU root collation, binary tie-break)
//! 4. arrays (major type 4; lexicographic)
//! 5. objects (major type 5; keys sorted, then lexicographic by pairs)
//! 6. tagged values (major type 6), ordered by tag: refs (13), times (16),
//!    dates (17), lambdas (18)
//! 7. simple values: `false` < `true` < `null`
//!
//! `null` is therefore the maximum value, which is why missing index
//! values sort last.

use crate::value::{Ref, Value};
use std::cell::RefCell;
use std::cmp::Ordering;

fn rank(v: &Value) -> u8 {
    match v {
        Value::Long(_) | Value::Double(_) => 0,
        Value::Bytes(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) | Value::Page { .. } => 4,
        Value::Object(_) => 5,
        Value::Ref(_) | Value::Time(_) | Value::Date(_) | Value::Lambda(_) => 6,
        Value::Bool(_) | Value::Null => 7,
        Value::Set(_) => 8,
    }
}

fn tag(v: &Value) -> u8 {
    match v {
        Value::Ref(_) => 13,
        Value::Time(_) => 16,
        Value::Date(_) => 17,
        Value::Lambda(_) => 18,
        _ => 0,
    }
}

thread_local! {
    static COLLATOR: RefCell<Option<icu_collator::Collator>> = const { RefCell::new(None) };
}

/// ICU root collation (what the server uses via `Collator.getInstance`
/// in its default locale, which is the root locale on the server), with a
/// binary tie-break so the order is total.
pub fn compare_strings(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let collated = COLLATOR.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            let locale = icu_locid::locale!("und");
            let options = icu_collator::CollatorOptions::new();
            *c = icu_collator::Collator::try_new(&locale.into(), options).ok();
        }
        match c.as_ref() {
            Some(col) => col.compare(a, b),
            None => a.cmp(b),
        }
    });
    if collated == Ordering::Equal {
        a.as_bytes().cmp(b.as_bytes())
    } else {
        collated
    }
}

fn compare_numbers(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Long(x), Value::Long(y)) => x.cmp(y),
        (Value::Double(x), Value::Double(y)) => x.total_cmp(y),
        (Value::Long(x), Value::Double(y)) => (*x as f64).total_cmp(y),
        (Value::Double(x), Value::Long(y)) => x.total_cmp(&(*y as f64)),
        _ => unreachable!(),
    }
}

/// Refs sort by their `DocID` on the server: the numeric document id
/// first, then the collection id
/// (ext/atoms/src/main/scala/DocID.scala `compare`). Collection ids are
/// internal longs we do not have, so the emulator tie-breaks on the
/// collection path; named (schema) refs compare by name.
pub fn compare_refs(a: &Ref, b: &Ref) -> Ordering {
    let path = |r: &Ref| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = Some(r);
        while let Some(x) = cur {
            out.push(x.id.clone());
            cur = x.collection.as_deref();
        }
        out.reverse();
        out
    };
    let pa = path(a);
    let pb = path(b);
    let (ia, ib) = (pa.len() - 1, pb.len() - 1);
    let by_id = match (pa[ia].parse::<u64>(), pb[ib].parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        _ => pa[ia].cmp(&pb[ib]),
    };
    by_id.then_with(|| pa[..ia].cmp(&pb[..ib]))
}

fn compare_arrays(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = compare(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

pub fn compare(a: &Value, b: &Value) -> Ordering {
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Value::Long(_) | Value::Double(_), _) => compare_numbers(a, b),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => compare_strings(x, y),
        (Value::Array(x), Value::Array(y)) => compare_arrays(x, y),
        (Value::Page { data: x, .. }, Value::Page { data: y, .. }) => compare_arrays(x, y),
        (Value::Array(x), Value::Page { data: y, .. }) => compare_arrays(x, y),
        (Value::Page { data: x, .. }, Value::Array(y)) => compare_arrays(x, y),
        (Value::Object(x), Value::Object(y)) => {
            let mut xs: Vec<(&String, &Value)> = x.iter().collect();
            let mut ys: Vec<(&String, &Value)> = y.iter().collect();
            xs.sort_by(|p, q| p.0.cmp(q.0));
            ys.sort_by(|p, q| p.0.cmp(q.0));
            for ((kx, vx), (ky, vy)) in xs.iter().zip(ys.iter()) {
                let o = compare_strings(kx, ky);
                if o != Ordering::Equal {
                    return o;
                }
                let o = compare(vx, vy);
                if o != Ordering::Equal {
                    return o;
                }
            }
            xs.len().cmp(&ys.len())
        }
        (Value::Ref(_) | Value::Time(_) | Value::Date(_) | Value::Lambda(_), _) => {
            let (ta, tb) = (tag(a), tag(b));
            if ta != tb {
                return ta.cmp(&tb);
            }
            match (a, b) {
                (Value::Ref(x), Value::Ref(y)) => compare_refs(x, y),
                (Value::Time(x), Value::Time(y)) => x.cmp(y),
                (Value::Date(x), Value::Date(y)) => x.cmp(y),
                (Value::Lambda(x), Value::Lambda(y)) => x.body.to_string().cmp(&y.body.to_string()),
                _ => Ordering::Equal,
            }
        }
        (Value::Bool(_) | Value::Null, _) => {
            let simple = |v: &Value| match v {
                Value::Bool(false) => 0,
                Value::Bool(true) => 1,
                _ => 2,
            };
            simple(a).cmp(&simple(b))
        }
        (Value::Set(x), Value::Set(y)) => format!("{:?}", x).cmp(&format!("{:?}", y)),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_type_order() {
        let vals = vec![
            Value::Long(-5),
            Value::Double(1.5),
            Value::Long(2),
            Value::Bytes(vec![1]),
            Value::str("a"),
            Value::str("B"),
            Value::Array(vec![Value::Long(1)]),
            Value::obj(vec![("a", Value::Long(1))]),
            Value::Ref(Ref::document("users", "1")),
            Value::Ref(Ref::document("users", "10")),
            Value::Time(crate::value::Timestamp::EPOCH),
            Value::Date(chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()),
            Value::Bool(false),
            Value::Bool(true),
            Value::Null,
        ];
        for w in vals.windows(2) {
            assert_eq!(compare(&w[0], &w[1]), Ordering::Less, "{:?} < {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn string_collation_is_case_insensitive_at_primary_level() {
        // ICU root: "a" < "B" (alphabetic first), unlike byte order.
        assert_eq!(compare_strings("a", "B"), Ordering::Less);
        assert_eq!(compare_strings("apple", "Banana"), Ordering::Less);
        assert_eq!(compare_strings("a", "A"), Ordering::Less);
    }
}
