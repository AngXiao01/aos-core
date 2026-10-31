use alloc::string::String;
use core::fmt;

use crate::hash::hex_encode;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 128-bit ULID. CBOR stores the raw 16 bytes; JSON stores Crockford text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UlidError;

impl ObjectId {
    pub const ZERO: Self = Self([0u8; 16]);

    pub fn from_parts(timestamp_ms: u64, random: u128) -> Self {
        let ms = timestamp_ms & 0x0000_ffff_ffff_ffff;
        let rnd = random & ((1u128 << 80) - 1);
        let mut bytes = [0u8; 16];
        let ms_be = ms.to_be_bytes();
        bytes[0..6].copy_from_slice(&ms_be[2..8]);
        let rnd_be = rnd.to_be_bytes();
        bytes[6..16].copy_from_slice(&rnd_be[6..16]);
        Self(bytes)
    }

    pub fn timestamp_ms(self) -> u64 {
        let mut tmp = [0u8; 8];
        tmp[2..8].copy_from_slice(&self.0[0..6]);
        u64::from_be_bytes(tmp)
    }

    pub fn random_bits(self) -> u128 {
        let mut tmp = [0u8; 16];
        tmp[6..16].copy_from_slice(&self.0[6..16]);
        u128::from_be_bytes(tmp)
    }

    pub fn to_text(self) -> String {
        let mut n: u128 = 0;
        for b in &self.0 {
            n = (n << 8) | (*b as u128);
        }
        let mut out = [0u8; 26];
        for i in 0..26 {
            let shift = (25 - i) * 5;
            let idx = ((n >> shift) & 31) as usize;
            out[i] = CROCKFORD[idx];
        }
        String::from_utf8(out.to_vec()).unwrap()
    }

    pub fn parse(s: &str) -> Result<Self, UlidError> {
        if s.len() != 26 {
            return Err(UlidError);
        }
        let mut n: u128 = 0;
        for (i, c) in s.bytes().enumerate() {
            let v = crockford_val(c).ok_or(UlidError)?;
            if i == 0 && v > 7 {
                return Err(UlidError);
            }
            n = (n << 5) | (v as u128);
        }
        Ok(Self(n.to_be_bytes()))
    }
}

fn crockford_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A' | b'a' => Some(10),
        b'B' | b'b' => Some(11),
        b'C' | b'c' => Some(12),
        b'D' | b'd' => Some(13),
        b'E' | b'e' => Some(14),
        b'F' | b'f' => Some(15),
        b'G' | b'g' => Some(16),
        b'H' | b'h' => Some(17),
        b'J' | b'j' => Some(18),
        b'K' | b'k' => Some(19),
        b'M' | b'm' => Some(20),
        b'N' | b'n' => Some(21),
        b'P' | b'p' => Some(22),
        b'Q' | b'q' => Some(23),
        b'R' | b'r' => Some(24),
        b'S' | b's' => Some(25),
        b'T' | b't' => Some(26),
        b'V' | b'v' => Some(27),
        b'W' | b'w' => Some(28),
        b'X' | b'x' => Some(29),
        b'Y' | b'y' => Some(30),
        b'Z' | b'z' => Some(31),
        _ => None,
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

/// Monotonic ULID generator. Same millisecond increments the random field.
#[derive(Clone, Debug)]
pub struct IdGen {
    last_ms: u64,
    last_rand: u128,
}

impl IdGen {
    pub fn new() -> Self {
        Self {
            last_ms: 0,
            last_rand: 0,
        }
    }

    pub fn next(&mut self, now_ms: u64, random: u128) -> ObjectId {
        let mut rnd = random & ((1u128 << 80) - 1);
        if now_ms == self.last_ms || now_ms < self.last_ms {
            rnd = self.last_rand.saturating_add(1) & ((1u128 << 80) - 1);
            if rnd == 0 {
                rnd = 1;
            }
        }
        let ms = if now_ms < self.last_ms {
            self.last_ms
        } else {
            now_ms
        };
        self.last_ms = ms;
        self.last_rand = rnd;
        ObjectId::from_parts(ms, rnd)
    }
}

impl Default for IdGen {
    fn default() -> Self {
        Self::new()
    }
}

pub fn debug_hex_id(id: &ObjectId) -> String {
    hex_encode(&id.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_max_roundtrip() {
        let zero = ObjectId([0u8; 16]);
        assert_eq!(zero.to_text(), "00000000000000000000000000");
        assert_eq!(ObjectId::parse(&zero.to_text()).unwrap(), zero);

        let max = ObjectId([0xff; 16]);
        assert_eq!(max.to_text(), "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
        assert_eq!(ObjectId::parse(&max.to_text()).unwrap(), max);
    }

    #[test]
    fn spec_example_decodes() {
        let id = ObjectId::parse("01JC8Z3K9Q6V2X4M7N1P0R5T8W").unwrap();
        assert_eq!(id.to_text(), "01JC8Z3K9Q6V2X4M7N1P0R5T8W");
    }

    #[test]
    fn parts_roundtrip() {
        let id = ObjectId::from_parts(1_700_000_000_000, 0xabcdu128);
        assert_eq!(id.timestamp_ms(), 1_700_000_000_000);
        assert_eq!(id.random_bits(), 0xabcd);
    }

    #[test]
    fn monotonic_same_millisecond() {
        let mut gen = IdGen::new();
        let a = gen.next(10, 5);
        let b = gen.next(10, 5);
        assert!(b.random_bits() > a.random_bits());
        assert_eq!(a.timestamp_ms(), b.timestamp_ms());
    }
}
