use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::cbor::{Cbor, CborError};
use crate::id::ObjectId;
use crate::object::Object;
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
}

impl CmpOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CmpOp::Eq => "eq",
            CmpOp::Ne => "ne",
            CmpOp::Lt => "lt",
            CmpOp::Lte => "lte",
            CmpOp::Gt => "gt",
            CmpOp::Gte => "gte",
            CmpOp::In => "in",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "eq" => CmpOp::Eq,
            "ne" => CmpOp::Ne,
            "lt" => CmpOp::Lt,
            "lte" => CmpOp::Lte,
            "gt" => CmpOp::Gt,
            "gte" => CmpOp::Gte,
            "in" => CmpOp::In,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Predicate {
    All(Vec<Predicate>),
    Any(Vec<Predicate>),
    Not(Box<Predicate>),
    Cmp {
        field: String,
        op: CmpOp,
        value: Value,
    },
}

use alloc::boxed::Box;

impl Predicate {
    pub fn to_cbor(&self) -> Result<Cbor, CborError> {
        Ok(match self {
            Predicate::All(xs) => Cbor::Map(alloc::vec![(
                Cbor::Text("all".into()),
                Cbor::Array(map_preds(xs)?),
            )]),
            Predicate::Any(xs) => Cbor::Map(alloc::vec![(
                Cbor::Text("any".into()),
                Cbor::Array(map_preds(xs)?),
            )]),
            Predicate::Not(p) => Cbor::Map(alloc::vec![(Cbor::Text("not".into()), p.to_cbor()?)]),
            Predicate::Cmp { field, op, value } => Cbor::Map(alloc::vec![
                (Cbor::Text("field".into()), Cbor::Text(field.clone())),
                (Cbor::Text(op.as_str().into()), value.to_cbor()?),
            ]),
        })
    }
}

fn map_preds(xs: &[Predicate]) -> Result<Vec<Cbor>, CborError> {
    let mut out = Vec::with_capacity(xs.len());
    for p in xs {
        out.push(p.to_cbor()?);
    }
    Ok(out)
}

pub fn eval_predicate(pred: &Predicate, resolve: &mut dyn FnMut(&str) -> Option<Value>) -> bool {
    match pred {
        Predicate::All(xs) => xs.iter().all(|p| eval_predicate(p, resolve)),
        Predicate::Any(xs) => xs.iter().any(|p| eval_predicate(p, resolve)),
        Predicate::Not(p) => !eval_predicate(p, resolve),
        Predicate::Cmp { field, op, value } => {
            let Some(left) = resolve(field) else {
                return false;
            };
            compare(*op, &left, value)
        }
    }
}

fn compare(op: CmpOp, left: &Value, right: &Value) -> bool {
    match op {
        CmpOp::Eq => values_eq(left, right),
        CmpOp::Ne => !values_eq(left, right),
        CmpOp::In => match right {
            Value::List(xs) => xs.iter().any(|v| values_eq(left, v)),
            _ => false,
        },
        CmpOp::Lt | CmpOp::Lte | CmpOp::Gt | CmpOp::Gte => {
            let Some(ord) = cmp_numbers(left, right) else {
                return false;
            };
            match op {
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Lte => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Gte => ord != Ordering::Less,
                _ => false,
            }
        }
    }
}

fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Ref(x), Value::Ref(y)) => x == y,
        (Value::RefAt(x, xv), Value::RefAt(y, yv)) => x == y && xv == yv,
        (Value::Ref(x), Value::RefAt(y, _)) => x == y,
        (Value::RefAt(x, _), Value::Ref(y)) => x == y,
        (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_)) => {
            cmp_numbers(a, b) == Some(Ordering::Equal)
        }
        _ => a == b,
    }
}

/// Exact numeric ordering. Integers never pass through `f64`, so values above
/// 2^53 (timestamps in µs, money in fen at scale) compare correctly.
/// `None` when either side is not a number or is NaN.
pub fn cmp_numbers(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Float(y)) => cmp_int_float(*x, *y),
        (Value::Float(x), Value::Int(y)) => cmp_int_float(*y, *x).map(Ordering::reverse),
        _ => None,
    }
}

fn cmp_int_float(i: i64, f: f64) -> Option<Ordering> {
    // 2^63 is exact in f64; every i64 lies in [-2^63, 2^63).
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if f.is_nan() {
        return None;
    }
    if f >= TWO_63 {
        return Some(Ordering::Less);
    }
    if f < -TWO_63 {
        return Some(Ordering::Greater);
    }
    // In range, `as` truncates toward zero exactly. The truncated value is an
    // integer of magnitude <= |f|, so converting it back to f64 is also exact.
    let t = f as i64;
    match i.cmp(&t) {
        Ordering::Equal => {
            let frac = f - t as f64;
            Some(if frac > 0.0 {
                Ordering::Less
            } else if frac < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            })
        }
        other => Some(other),
    }
}

/// Resolve `input.field` against a map of named inputs.
/// A bare name returns the input value. A ref input loads the object.
pub fn resolve_input_path(
    path: &str,
    inputs: &BTreeMap<String, Value>,
    get: &mut dyn FnMut(&ObjectId) -> Option<Object>,
) -> Option<Value> {
    let (head, rest) = match path.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (path, None),
    };
    let value = inputs.get(head)?.clone();
    let Some(rest) = rest else {
        return Some(value);
    };
    let id = match value {
        Value::Ref(id) | Value::RefAt(id, _) => id,
        _ => return None,
    };
    let obj = get(&id)?;
    field_of(&obj, rest)
}

pub fn field_of(obj: &Object, path: &str) -> Option<Value> {
    match path {
        "id" => Some(Value::Text(obj.id.to_text())),
        "type" => Some(Value::Text(obj.type_name.clone())),
        "version" => Some(Value::Int(obj.version as i64)),
        "provenance.taint" | "taint" => Some(Value::Text(obj.provenance.taint.as_str().into())),
        "sensitivity" => Some(Value::Text(obj.sensitivity.as_str().into())),
        "tombstone" => Some(Value::Bool(obj.tombstone)),
        _ => {
            if let Some(name) = path.strip_prefix("fields.") {
                return obj.fields.get(name).cloned();
            }
            if let Some(name) = path.strip_prefix("extra.") {
                return obj.extra.get(name).cloned();
            }
            if let Some(v) = obj.fields.get(path) {
                return Some(v.clone());
            }
            obj.extra.get(path).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ObjectId;
    use crate::object::{Actor, Object};

    #[test]
    fn all_of_cmp() {
        let id = ObjectId::from_parts(1, 2);
        let mut obj = Object::new(id, "sys/Note", 1, Actor::System);
        obj.fields.insert("n".into(), Value::Int(3));
        let pred = Predicate::All(alloc::vec![Predicate::Cmp {
            field: "n".into(),
            op: CmpOp::Gt,
            value: Value::Int(1),
        }]);
        let ok = eval_predicate(&pred, &mut |p| field_of(&obj, p));
        assert!(ok);
    }

    #[test]
    fn numeric_comparison_is_exact_above_2_pow_53() {
        let big = Value::Int(9_007_199_254_740_993); // 2^53 + 1
        let below = Value::Int(9_007_199_254_740_992);
        assert_eq!(cmp_numbers(&big, &below), Some(Ordering::Greater));
        assert_eq!(
            cmp_numbers(&big, &Value::Float(9_007_199_254_740_992.0)),
            Some(Ordering::Greater)
        );
        assert!(!values_eq(&big, &Value::Float(9_007_199_254_740_992.0)));
        assert!(values_eq(&Value::Int(3), &Value::Float(3.0)));
        assert_eq!(
            cmp_numbers(&Value::Int(3), &Value::Float(3.5)),
            Some(Ordering::Less)
        );
        assert_eq!(
            cmp_numbers(&Value::Int(-3), &Value::Float(-3.5)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            cmp_numbers(&Value::Int(i64::MAX), &Value::Float(9.3e18)),
            Some(Ordering::Less)
        );
        assert_eq!(
            cmp_numbers(
                &Value::Int(i64::MIN),
                &Value::Float(-9_223_372_036_854_775_808.0)
            ),
            Some(Ordering::Equal)
        );
        assert_eq!(cmp_numbers(&Value::Int(0), &Value::Float(f64::NAN)), None);
    }
}
