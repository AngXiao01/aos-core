//! Shared data model, deterministic CBOR, and hashes.
//!
//! This crate is `no_std + alloc`. I/O, clocks, and randomness are traits.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod cbor;
pub mod crypto;
pub mod hash;
pub mod id;
pub mod object;
pub mod predicate;
pub mod query;
pub mod schema;
pub mod traits;
pub mod value;

pub use cbor::Cbor;
pub use hash::{hex_decode, hex_encode, sha256, ContentHash};
pub use id::{IdGen, ObjectId, UlidError};
pub use object::{
    absent_shell, apply_patch, diff_objects, invert_patch, propagate_taint, Actor, Object, Patch,
    Provenance, Sensitivity, Taint,
};
pub use predicate::{eval_predicate, field_of, resolve_input_path, CmpOp, Predicate};
pub use query::{Aggregate, OrderDir, OrderKey, Query};
pub use schema::{builtin_actions, builtin_types, ActionDecl, FieldType, TypeDef};
pub use traits::{Clock, ManualClock, ManualRng, ObjectView, RefEdge, Rng};
pub use value::Value;
