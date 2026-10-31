//! Action declarations, host ABI names, and the checks a host must run
//! before user code touches an object.
//!
//! This crate does not link a WASM engine. Versions A and B bring wasmtime
//! or wasmi. `aos-conformance` uses wasmi only as a dev-dependency.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use aos_cap::{check_call, check_channel, scope_allows, CapError, Capability, Spent};
use aos_log::LedgerOp;
use aos_types::{sha256, ActionDecl, ObjectId, Value};

pub const ABI_VERSION: u32 = 1;
pub const IMPORT_MODULE: &str = "aos";

/// The nine host functions from spec §8.4. Argument blobs are canonical CBOR
/// maps that include `v` = [`ABI_VERSION`].
pub const IMPORTS: &[&str] = &[
    "query",
    "read",
    "write",
    "create",
    "blob_read",
    "blob_write",
    "intent_create",
    "job_submit",
    "log",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionError {
    UndeclaredType,
    DirectIo,
    Capability(CapError),
    Precondition,
    IdempotencyConflict,
    Reversibility,
    AbiVersion,
    JobTransition,
    RemoteJob,
    /// An input has no canonical CBOR form, so it cannot be hashed or logged.
    Encoding,
}

pub fn is_foreign_import(module: &str, name: &str) -> bool {
    module != IMPORT_MODULE || !IMPORTS.iter().any(|n| *n == name)
}

pub fn check_type_allowed(declared: &[String], type_name: &str) -> Result<(), ActionError> {
    // `sys/Object` in a declaration is not a wildcard. Callers that register
    // sys actions list the concrete type they touch. A declaration entry equal
    // to the concrete type, or equal to the type's provider prefix ending in
    // `*`, is accepted. The sys human-edit actions use the concrete type name
    // passed by the host, not a free-form read.
    if declared
        .iter()
        .any(|d| d == type_name || d == "*" || prefix_cover(d, type_name))
    {
        Ok(())
    } else {
        Err(ActionError::UndeclaredType)
    }
}

fn prefix_cover(pattern: &str, type_name: &str) -> bool {
    pattern
        .strip_suffix('*')
        .is_some_and(|p| type_name.starts_with(p))
}

pub fn check_reversibility(decl: &ActionDecl) -> Result<(), ActionError> {
    match decl.reversibility.as_str() {
        "undo" => Ok(()),
        "compensate" | "irreversible" if !decl.egress.is_empty() => Ok(()),
        "compensate" | "irreversible" => Err(ActionError::Reversibility),
        _ => Err(ActionError::Reversibility),
    }
}

#[derive(Clone, Debug)]
pub enum IdemHit {
    Fresh,
    Replay(Value),
}

pub fn check_idempotency(
    existing_input: Option<&[u8; 32]>,
    existing_result: Option<&Value>,
    input_hash: &[u8; 32],
) -> Result<IdemHit, ActionError> {
    match existing_input {
        None => Ok(IdemHit::Fresh),
        Some(prev) if prev == input_hash => Ok(IdemHit::Replay(
            existing_result.cloned().unwrap_or(Value::Null),
        )),
        Some(_) => Err(ActionError::IdempotencyConflict),
    }
}

/// Length-prefixed canonical CBOR of each part. A part that cannot be encoded
/// is an error: skipping it would make `[NaN]` and `[]` hash the same and
/// replay one call's result for another.
pub fn input_hash(parts: &[Value]) -> Result<[u8; 32], ActionError> {
    let mut buf = Vec::new();
    for p in parts {
        let bytes = p.canonical().map_err(|_| ActionError::Encoding)?;
        let len = u32::try_from(bytes.len()).map_err(|_| ActionError::Encoding)?;
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&bytes);
    }
    Ok(sha256(&buf))
}

pub fn authorize(
    cap: &Capability,
    public_key: &[u8],
    now_ms: u64,
    decl: &ActionDecl,
    spent: &Spent,
    delta: &LedgerOp,
    scope_field: Option<(&str, &ObjectId)>,
) -> Result<(), ActionError> {
    check_reversibility(decl)?;
    check_call(cap, public_key, now_ms, &decl.capability, spent, delta)
        .map_err(ActionError::Capability)?;
    if let Some((field, id)) = scope_field {
        if !scope_allows(&cap.object_scope, field, id) {
            return Err(ActionError::Capability(CapError::Scope));
        }
    }
    Ok(())
}

pub fn authorize_intent(cap: &Capability, channel: &str) -> Result<(), ActionError> {
    check_channel(cap, channel).map_err(ActionError::Capability)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => JobState::Queued,
            "running" => JobState::Running,
            "succeeded" => JobState::Succeeded,
            "failed" => JobState::Failed,
            "cancelled" => JobState::Cancelled,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }
}

pub fn transition(from: JobState, to: JobState) -> Result<JobState, ActionError> {
    let ok = matches!(
        (from, to),
        (JobState::Queued, JobState::Running)
            | (JobState::Queued, JobState::Cancelled)
            | (JobState::Running, JobState::Succeeded)
            | (JobState::Running, JobState::Failed)
            | (JobState::Running, JobState::Cancelled)
    );
    if ok {
        Ok(to)
    } else {
        Err(ActionError::JobTransition)
    }
}

/// Remote submission is an egress intent whose action is `compensate`.
/// A local job runs without network inside the sandbox (§8.5): it is an
/// internal `undo` action and must not carry an egress intent.
pub fn check_remote_job(
    kind: &str,
    reversibility: &str,
    has_intent: bool,
) -> Result<(), ActionError> {
    let ok = match kind {
        "remote" => reversibility == "compensate" && has_intent,
        "local" => reversibility == "undo" && !has_intent,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(ActionError::RemoteJob)
    }
}

pub fn decl_from_object_fields(
    name: &str,
    version: u32,
    summary: &str,
    reversibility: &str,
    capability: &str,
    reads: &str,
    writes: &str,
    egress: &str,
    idempotency: &str,
) -> ActionDecl {
    ActionDecl {
        name: name.into(),
        version,
        summary: summary.into(),
        reads: split_ws(reads),
        writes: split_ws(writes),
        egress: split_ws(egress),
        reversibility: reversibility.into(),
        capability: capability.into(),
        idempotency_paths: split_ws(idempotency),
    }
}

fn split_ws(s: &str) -> Vec<String> {
    s.split_whitespace().map(|p| p.to_string()).collect()
}

/// Actions whose `reads` or `writes` mention `type_name`.
pub fn actions_touching<'a>(decls: &'a [ActionDecl], type_name: &str) -> Vec<&'a ActionDecl> {
    decls
        .iter()
        .filter(|d| {
            d.reads.iter().any(|t| t == type_name) || d.writes.iter().any(|t| t == type_name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_has_nine_names_and_foreign_io_is_rejected() {
        assert_eq!(IMPORTS.len(), 9);
        assert!(is_foreign_import("wasi_snapshot_preview1", "fd_write"));
        assert!(!is_foreign_import("aos", "read"));
        assert_eq!(ABI_VERSION, 1);
    }

    #[test]
    fn undeclared_type_and_job_machine() {
        assert!(check_type_allowed(&["sys/Note".into()], "sys/Fact").is_err());
        assert!(check_type_allowed(&["sys/Note".into()], "sys/Note").is_ok());
        assert_eq!(
            transition(JobState::Queued, JobState::Running).unwrap(),
            JobState::Running
        );
        assert!(transition(JobState::Succeeded, JobState::Running).is_err());
        assert!(check_remote_job("remote", "undo", false).is_err());
        assert!(check_remote_job("remote", "compensate", true).is_ok());
        assert!(check_remote_job("local", "undo", false).is_ok());
        assert!(check_remote_job("local", "undo", true).is_err());
    }

    #[test]
    fn input_hash_does_not_skip_unencodable_parts() {
        assert_eq!(
            input_hash(&[Value::Float(f64::NAN)]).unwrap_err(),
            ActionError::Encoding
        );
        assert_ne!(
            input_hash(&[Value::Int(1)]).unwrap(),
            input_hash(&[]).unwrap()
        );
    }

    #[test]
    fn idempotency_conflict() {
        let h1 = [1u8; 32];
        let h2 = [2u8; 32];
        assert!(matches!(
            check_idempotency(None, None, &h1).unwrap(),
            IdemHit::Fresh
        ));
        assert!(matches!(
            check_idempotency(Some(&h1), Some(&Value::Int(1)), &h1).unwrap(),
            IdemHit::Replay(_)
        ));
        assert_eq!(
            check_idempotency(Some(&h1), Some(&Value::Int(1)), &h2).unwrap_err(),
            ActionError::IdempotencyConflict
        );
    }
}
