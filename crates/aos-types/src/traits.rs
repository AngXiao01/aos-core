use alloc::vec::Vec;

use crate::id::ObjectId;
use crate::object::Object;

pub trait Clock {
    fn now_ms(&self) -> u64;
}

pub trait Rng {
    fn next_u64(&mut self) -> u64;

    fn fill(&mut self, dest: &mut [u8]) {
        let mut i = 0;
        while i < dest.len() {
            let n = self.next_u64().to_le_bytes();
            let take = core::cmp::min(8, dest.len() - i);
            dest[i..i + take].copy_from_slice(&n[..take]);
            i += take;
        }
    }

    fn next_u128(&mut self) -> u128 {
        let hi = self.next_u64() as u128;
        let lo = self.next_u64() as u128;
        (hi << 64) | lo
    }
}

#[derive(Clone, Debug)]
pub struct ManualClock {
    pub ms: u64,
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.ms
    }
}

impl ManualClock {
    pub fn new(ms: u64) -> Self {
        Self { ms }
    }
}

/// xorshift64*. Injected so tests do not touch the OS random source.
#[derive(Clone, Debug)]
pub struct ManualRng {
    pub state: u64,
}

impl ManualRng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x1234_5678_9abc_def0
            } else {
                seed
            },
        }
    }
}

impl Rng for ManualRng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefEdge {
    pub source: ObjectId,
    pub field: alloc::string::String,
}

/// Read interface shared by the query crate and the in-memory world.
pub trait ObjectView {
    fn get(&self, id: &ObjectId) -> Option<Object>;
    fn for_each(&self, f: &mut dyn FnMut(&Object));
    fn reverse(&self, target: &ObjectId) -> Vec<RefEdge>;
}
