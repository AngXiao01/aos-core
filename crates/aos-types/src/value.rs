use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::cbor::{decode, Cbor, CborError};
use crate::id::{ObjectId, UlidError};

/// Field value. Refs are not bare byte strings, so they cannot be confused
/// with blobs or hashes.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Ref(ObjectId),
    RefAt(ObjectId, u64),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

/// Private-use tags. 32768 = object id, 32769 = id plus version.
pub const TAG_REF: u64 = 32768;
pub const TAG_REF_AT: u64 = 32769;

impl Value {
    pub fn to_cbor(&self) -> Result<Cbor, CborError> {
        Ok(match self {
            Value::Null => Cbor::Null,
            Value::Bool(b) => Cbor::Bool(*b),
            Value::Int(i) => int_cbor(*i),
            Value::Float(n) => Cbor::Float(*n),
            Value::Text(s) => Cbor::Text(s.clone()),
            Value::Bytes(b) => Cbor::Bytes(b.clone()),
            Value::Ref(id) => Cbor::Tag(TAG_REF, Box::new(Cbor::Bytes(id.0.to_vec()))),
            Value::RefAt(id, v) => Cbor::Tag(
                TAG_REF_AT,
                Box::new(Cbor::Array(alloc::vec![
                    Cbor::Bytes(id.0.to_vec()),
                    Cbor::Uint(*v),
                ])),
            ),
            Value::List(xs) => {
                let mut out = Vec::with_capacity(xs.len());
                for x in xs {
                    out.push(x.to_cbor()?);
                }
                Cbor::Array(out)
            }
            Value::Map(m) => {
                let mut pairs = Vec::with_capacity(m.len());
                for (k, v) in m {
                    pairs.push((Cbor::Text(k.clone()), v.to_cbor()?));
                }
                Cbor::Map(pairs)
            }
        })
    }

    pub fn canonical(&self) -> Result<Vec<u8>, CborError> {
        self.to_cbor()?.canonical()
    }

    pub fn from_cbor(c: &Cbor) -> Result<Self, CborError> {
        Ok(match c {
            Cbor::Null => Value::Null,
            Cbor::Bool(b) => Value::Bool(*b),
            Cbor::Uint(n) => {
                if *n > i64::MAX as u64 {
                    return Err(CborError::Unsupported);
                }
                Value::Int(*n as i64)
            }
            Cbor::Nint(n) => {
                let v = -1i128 - (*n as i128);
                if v < i64::MIN as i128 {
                    return Err(CborError::Unsupported);
                }
                Value::Int(v as i64)
            }
            Cbor::Float(n) => Value::Float(*n),
            Cbor::Text(s) => Value::Text(s.clone()),
            Cbor::Bytes(b) => Value::Bytes(b.clone()),
            Cbor::Array(xs) => {
                let mut out = Vec::with_capacity(xs.len());
                for x in xs {
                    out.push(Value::from_cbor(x)?);
                }
                Value::List(out)
            }
            Cbor::Map(pairs) => {
                let mut m = BTreeMap::new();
                for (k, v) in pairs {
                    let Cbor::Text(s) = k else {
                        return Err(CborError::Unsupported);
                    };
                    m.insert(s.clone(), Value::from_cbor(v)?);
                }
                Value::Map(m)
            }
            Cbor::Tag(TAG_REF, inner) => {
                let Cbor::Bytes(b) = inner.as_ref() else {
                    return Err(CborError::Unsupported);
                };
                if b.len() != 16 {
                    return Err(CborError::Unsupported);
                }
                let mut id = [0u8; 16];
                id.copy_from_slice(b);
                Value::Ref(ObjectId(id))
            }
            Cbor::Tag(TAG_REF_AT, inner) => {
                let Cbor::Array(xs) = inner.as_ref() else {
                    return Err(CborError::Unsupported);
                };
                if xs.len() != 2 {
                    return Err(CborError::Unsupported);
                }
                let Cbor::Bytes(b) = &xs[0] else {
                    return Err(CborError::Unsupported);
                };
                let Cbor::Uint(ver) = xs[1] else {
                    return Err(CborError::Unsupported);
                };
                if b.len() != 16 {
                    return Err(CborError::Unsupported);
                }
                let mut id = [0u8; 16];
                id.copy_from_slice(b);
                Value::RefAt(ObjectId(id), ver)
            }
            Cbor::Tag(_, _) => return Err(CborError::Unsupported),
        })
    }

    pub fn from_canonical(bytes: &[u8]) -> Result<Self, CborError> {
        Value::from_cbor(&decode(bytes)?)
    }

    /// JSON object `{"ref":"<ulid>"}` or `{"ref":"<ulid>@<ver>"}`.
    pub fn parse_ref_text(s: &str) -> Result<Self, UlidError> {
        if let Some((id, ver)) = s.split_once('@') {
            let id = ObjectId::parse(id)?;
            let ver: u64 = ver.parse().map_err(|_| UlidError)?;
            Ok(Value::RefAt(id, ver))
        } else {
            Ok(Value::Ref(ObjectId::parse(s)?))
        }
    }
}

fn int_cbor(i: i64) -> Cbor {
    if i >= 0 {
        Cbor::Uint(i as u64)
    } else {
        let n = (-1i128 - (i as i128)) as u64;
        Cbor::Nint(n)
    }
}

use alloc::boxed::Box;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_roundtrip_is_not_bare_bytes() {
        let id = ObjectId::from_parts(5, 9);
        let v = Value::RefAt(id, 3);
        let bytes = v.canonical().unwrap();
        assert_eq!(Value::from_canonical(&bytes).unwrap(), v);
        assert_ne!(bytes[0] & 0xe0, 0x40); // not a bare byte string
    }
}
