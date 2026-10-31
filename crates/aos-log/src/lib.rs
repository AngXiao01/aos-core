//! Append-only hash-chained log and the persistent map used for branch state.
//!
//! Storage is a trait. The memory backend is the one this crate ships.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod entry;
pub mod hamt;
pub mod store;

pub use entry::{hash_entry, referenced_blobs, verify_chain, LedgerOp, LogEntry, WriteRecord};
pub use hamt::{Hamt, HamtKey};
pub use store::{BlobStore, ChunkRef, LogStore, MemoryBlob, MemoryLog, StoreError};
