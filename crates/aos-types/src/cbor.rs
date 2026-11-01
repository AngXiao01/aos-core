//! Deterministic CBOR (RFC 8949 §4.2): definite lengths, sorted map keys,
//! shortest integers, shortest finite floats.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use half::f16;

#[derive(Clone, Debug, PartialEq)]
pub enum Cbor {
    Uint(u64),
    /// Argument `n` means numeric value `-1 - n`.
    Nint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
    Float(f64),
    Tag(u64, Box<Cbor>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CborError {
    NonFiniteFloat,
    DuplicateKey,
    Truncated,
    Indefinite,
    Unsupported,
    Trailing,
    Depth,
}

const MAX_DEPTH: usize = 64;

impl Cbor {
    pub fn canonical(&self) -> Result<Vec<u8>, CborError> {
        let mut out = Vec::new();
        encode_into(self, &mut out, 0)?;
        Ok(out)
    }
}

pub fn decode(bytes: &[u8]) -> Result<Cbor, CborError> {
    let mut p = Parser { bytes, i: 0 };
    let v = p.read(0)?;
    if p.i != bytes.len() {
        return Err(CborError::Trailing);
    }
    Ok(v)
}

fn encode_into(v: &Cbor, out: &mut Vec<u8>, depth: usize) -> Result<(), CborError> {
    if depth > MAX_DEPTH {
        return Err(CborError::Depth);
    }
    match v {
        Cbor::Uint(n) => write_head(out, 0, *n),
        Cbor::Nint(n) => write_head(out, 1, *n),
        Cbor::Bytes(b) => {
            write_head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Cbor::Text(s) => {
            write_head(out, 3, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Cbor::Array(items) => {
            write_head(out, 4, items.len() as u64);
            for it in items {
                encode_into(it, out, depth + 1)?;
            }
        }
        Cbor::Map(pairs) => {
            let mut enc: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(pairs.len());
            for (k, val) in pairs {
                let mut kb = Vec::new();
                encode_into(k, &mut kb, depth + 1)?;
                let mut vb = Vec::new();
                encode_into(val, &mut vb, depth + 1)?;
                enc.push((kb, vb));
            }
            enc.sort_by(|a, b| a.0.cmp(&b.0));
            for w in enc.windows(2) {
                if w[0].0 == w[1].0 {
                    return Err(CborError::DuplicateKey);
                }
            }
            write_head(out, 5, enc.len() as u64);
            for (k, val) in enc {
                out.extend_from_slice(&k);
                out.extend_from_slice(&val);
            }
        }
        Cbor::Bool(true) => out.push(0xf5),
        Cbor::Bool(false) => out.push(0xf4),
        Cbor::Null => out.push(0xf6),
        Cbor::Float(n) => write_float(out, *n)?,
        Cbor::Tag(t, inner) => {
            write_head(out, 6, *t);
            encode_into(inner, out, depth + 1)?;
        }
    }
    Ok(())
}

fn write_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | (n as u8));
    } else if n <= u8::MAX as u64 {
        out.push(m | 24);
        out.push(n as u8);
    } else if n <= u16::MAX as u64 {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= u32::MAX as u64 {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn write_float(out: &mut Vec<u8>, n: f64) -> Result<(), CborError> {
    if !n.is_finite() {
        return Err(CborError::NonFiniteFloat);
    }
    let n = if n == 0.0 { 0.0 } else { n };
    let h = f16::from_f64(n);
    if f64::from(h) == n {
        out.push(0xf9);
        out.extend_from_slice(&h.to_be_bytes());
        return Ok(());
    }
    let f = n as f32;
    if f as f64 == n {
        out.push(0xfa);
        out.extend_from_slice(&f.to_be_bytes());
        return Ok(());
    }
    out.push(0xfb);
    out.extend_from_slice(&n.to_be_bytes());
    Ok(())
}

struct Parser<'a> {
    bytes: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn read(&mut self, depth: usize) -> Result<Cbor, CborError> {
        if depth > MAX_DEPTH {
            return Err(CborError::Depth);
        }
        let (major, ai) = self.head()?;
        match major {
            0 => Ok(Cbor::Uint(self.argument(ai)?)),
            1 => Ok(Cbor::Nint(self.argument(ai)?)),
            2 => {
                let n = self.length(ai)?;
                Ok(Cbor::Bytes(self.take(n)?.to_vec()))
            }
            3 => {
                let n = self.length(ai)?;
                let b = self.take(n)?;
                let s = core::str::from_utf8(b).map_err(|_| CborError::Unsupported)?;
                Ok(Cbor::Text(s.into()))
            }
            4 => {
                let n = self.length(ai)?;
                // Every item takes at least one byte, so the remaining input
                // bounds the allocation. A hostile header cannot reserve more.
                let mut items = Vec::with_capacity(n.min(self.remaining()));
                for _ in 0..n {
                    items.push(self.read(depth + 1)?);
                }
                Ok(Cbor::Array(items))
            }
            5 => {
                let n = self.length(ai)?;
                let mut pairs = Vec::with_capacity(n.min(self.remaining() / 2));
                for _ in 0..n {
                    let k = self.read(depth + 1)?;
                    let v = self.read(depth + 1)?;
                    pairs.push((k, v));
                }
                Ok(Cbor::Map(pairs))
            }
            6 => {
                let tag = self.argument(ai)?;
                let inner = self.read(depth + 1)?;
                Ok(Cbor::Tag(tag, Box::new(inner)))
            }
            7 => self.simple(ai),
            _ => Err(CborError::Unsupported),
        }
    }

    fn head(&mut self) -> Result<(u8, u8), CborError> {
        let b = *self.bytes.get(self.i).ok_or(CborError::Truncated)?;
        self.i += 1;
        Ok((b >> 5, b & 0x1f))
    }

    fn argument(&mut self, ai: u8) -> Result<u64, CborError> {
        if ai < 24 {
            return Ok(ai as u64);
        }
        match ai {
            24 => Ok(self.take(1)?[0] as u64),
            25 => {
                let b = self.take(2)?;
                Ok(u16::from_be_bytes([b[0], b[1]]) as u64)
            }
            26 => {
                let b = self.take(4)?;
                Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64)
            }
            27 => {
                let b = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                Ok(u64::from_be_bytes(a))
            }
            31 => Err(CborError::Indefinite),
            _ => Err(CborError::Unsupported),
        }
    }

    fn simple(&mut self, ai: u8) -> Result<Cbor, CborError> {
        match ai {
            20 => Ok(Cbor::Bool(false)),
            21 => Ok(Cbor::Bool(true)),
            22 => Ok(Cbor::Null),
            25 => {
                let b = self.take(2)?;
                let h = f16::from_be_bytes([b[0], b[1]]);
                let n = f64::from(h);
                if !n.is_finite() {
                    return Err(CborError::NonFiniteFloat);
                }
                Ok(Cbor::Float(n))
            }
            26 => {
                let b = self.take(4)?;
                let n = f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64;
                if !n.is_finite() {
                    return Err(CborError::NonFiniteFloat);
                }
                Ok(Cbor::Float(n))
            }
            27 => {
                let b = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                let n = f64::from_be_bytes(a);
                if !n.is_finite() {
                    return Err(CborError::NonFiniteFloat);
                }
                Ok(Cbor::Float(n))
            }
            31 => Err(CborError::Indefinite),
            _ => Err(CborError::Unsupported),
        }
    }

    /// Length argument of a string, array, or map header.
    fn length(&mut self, ai: u8) -> Result<usize, CborError> {
        let n = self.argument(ai)?;
        usize::try_from(n).map_err(|_| CborError::Truncated)
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.i
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CborError> {
        if n > self.remaining() {
            return Err(CborError::Truncated);
        }
        let s = &self.bytes[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
}

/// Map helper that callers fill in any order. Encoding sorts keys.
pub fn map(pairs: Vec<(Cbor, Cbor)>) -> Cbor {
    Cbor::Map(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hex_encode;
    use alloc::vec;

    #[test]
    fn shortest_and_sorted() {
        assert_eq!(Cbor::Uint(0).canonical().unwrap(), vec![0x00]);
        assert_eq!(Cbor::Uint(23).canonical().unwrap(), vec![0x17]);
        assert_eq!(Cbor::Uint(24).canonical().unwrap(), vec![0x18, 0x18]);
        assert_eq!(Cbor::Bool(true).canonical().unwrap(), vec![0xf5]);
        assert_eq!(Cbor::Bool(false).canonical().unwrap(), vec![0xf4]);
        assert_eq!(Cbor::Null.canonical().unwrap(), vec![0xf6]);
        let m = Cbor::Map(vec![
            (Cbor::Text("b".into()), Cbor::Uint(1)),
            (Cbor::Text("a".into()), Cbor::Uint(2)),
        ]);
        assert_eq!(
            m.canonical().unwrap(),
            vec![0xa2, 0x61, b'a', 0x02, 0x61, b'b', 0x01]
        );
    }

    #[test]
    fn roundtrip_and_float_width() {
        let v = Cbor::Map(vec![
            (Cbor::Text("n".into()), Cbor::Float(1.0)),
            (Cbor::Text("x".into()), Cbor::Float(0.1)),
            (Cbor::Text("z".into()), Cbor::Uint(1_000_000)),
        ]);
        let bytes = v.canonical().unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.canonical().unwrap(), bytes);
        // 1.0 fits in f16 (0xf9).
        assert!(bytes.windows(2).any(|w| w[0] == 0xf9));
        let _ = hex_encode(&bytes);
    }

    #[test]
    fn rejects_indefinite_and_nan() {
        assert_eq!(decode(&[0x9f, 0xff]).unwrap_err(), CborError::Indefinite);
        assert_eq!(
            Cbor::Float(f64::NAN).canonical().unwrap_err(),
            CborError::NonFiniteFloat
        );
    }

    #[test]
    fn hostile_lengths_are_errors_not_panics() {
        // byte string of 2^64-1 bytes: `i + n` used to overflow.
        assert_eq!(
            decode(&[0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).unwrap_err(),
            CborError::Truncated
        );
        // array of 2^63 items: used to request a 2^63-element allocation.
        assert_eq!(
            decode(&[0x9b, 0x80, 0, 0, 0, 0, 0, 0, 0]).unwrap_err(),
            CborError::Truncated
        );
        assert_eq!(
            decode(&[0xba, 0xff, 0xff, 0xff, 0xff]).unwrap_err(),
            CborError::Truncated
        );
    }

    #[test]
    fn key_order_follows_encoded_bytes_not_string_order() {
        // "z" encodes shorter than "aa", so it sorts first.
        let m = Cbor::Map(vec![
            (Cbor::Text("aa".into()), Cbor::Uint(1)),
            (Cbor::Text("z".into()), Cbor::Uint(2)),
        ]);
        let b = m.canonical().unwrap();
        assert_eq!(b[1], 0x61); // text len 1, the key "z"
    }
}
