use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

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
            let Some(ord) = num_cmp(left, right) else {
                return false;
            };
            match op {
                CmpOp::Lt => ord < 0,
                CmpOp::Lte => ord <= 0,
                CmpOp::Gt => ord > 0,
                CmpOp::Gte => ord >= 0,
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
        (Value::Int(x), Value::Float(y)) => *x as f64 == *y,
        (Value::Float(x), Value::Int(y)) => *x == *y as f64,
        _ => a == b,
    }
}

fn num_cmp(a: &Value, b: &Value) -> Option<i8> {
    let af = as_f64(a)?;
    let bf = as_f64(b)?;
    Some(if af < bf {
        -1
    } else if af > bf {
        1
    } else {
        0
    })
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(n) if n.is_finite() => Some(*n),
        _ => None,
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
}
