//! Approval-request encoding and signature checks.
//!
//! The decision message is `request_bytes || utf8(decision)` where decision is
//! `approve` or `deny`. ECDSA P-256 hashes that message once.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

#[cfg(all(feature = "dev-confirmer", not(debug_assertions)))]
compile_error!(
    "dev-confirmer cannot be enabled in a release build; it is a software stand-in for the iPhone Secure Enclave"
);

use alloc::string::String;
use alloc::vec::Vec;

use aos_types::{crypto, Cbor, ObjectId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Egress,
    TaskCapabilities,
    PolicyChange,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Egress => "egress",
            Kind::TaskCapabilities => "task_capabilities",
            Kind::PolicyChange => "policy_change",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryLine {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalItem {
    pub channel: String,
    pub target: String,
    pub payload_hash: [u8; 32],
    pub cost_fen: i64,
    pub external_reversibility: String,
    pub tainted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub request_id: ObjectId,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub nonce: [u8; 16],
    pub kind: Kind,
    pub summary: Vec<SummaryLine>,
    pub items: Vec<ApprovalItem>,
    pub branch: String,
    pub diff_hash: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmError {
    SummaryTooLong,
    Decision,
    Signature,
    Expired,
    Replay,
    HashMismatch,
}

pub const MAX_SUMMARY_LINES: usize = 12;

pub fn encode_request(req: &ApprovalRequest) -> Result<Vec<u8>, ConfirmError> {
    if req.summary.len() > MAX_SUMMARY_LINES {
        return Err(ConfirmError::SummaryTooLong);
    }
    Ok(request_cbor(req).canonical().expect("request cbor"))
}

/// OS signs the canonical request bytes with its own key.
pub fn sign_request(os_seed: &[u8; 32], request_bytes: &[u8]) -> Result<[u8; 64], ()> {
    crypto::sign(os_seed, request_bytes)
}

pub fn verify_request(os_public: &[u8], request_bytes: &[u8], os_sig: &[u8]) -> bool {
    crypto::verify(os_public, request_bytes, os_sig)
}

pub fn decision_message(request_bytes: &[u8], decision: &str) -> Result<Vec<u8>, ConfirmError> {
    if decision != "approve" && decision != "deny" {
        return Err(ConfirmError::Decision);
    }
    let mut msg = Vec::with_capacity(request_bytes.len() + decision.len());
    msg.extend_from_slice(request_bytes);
    msg.extend_from_slice(decision.as_bytes());
    Ok(msg)
}

pub fn verify_decision(
    confirmer_public: &[u8],
    request_bytes: &[u8],
    decision: &str,
    sig: &[u8],
) -> Result<(), ConfirmError> {
    let msg = decision_message(request_bytes, decision)?;
    if crypto::verify(confirmer_public, &msg, sig) {
        Ok(())
    } else {
        Err(ConfirmError::Signature)
    }
}

pub trait ReplayGuard {
    fn consume(
        &mut self,
        nonce: &[u8],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<(), ConfirmError>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryReplay {
    seen: alloc::collections::BTreeSet<Vec<u8>>,
}

impl ReplayGuard for MemoryReplay {
    fn consume(
        &mut self,
        nonce: &[u8],
        expires_at_ms: u64,
        now_ms: u64,
    ) -> Result<(), ConfirmError> {
        if now_ms >= expires_at_ms {
            return Err(ConfirmError::Expired);
        }
        if !self.seen.insert(nonce.to_vec()) {
            return Err(ConfirmError::Replay);
        }
        Ok(())
    }
}

fn request_cbor(req: &ApprovalRequest) -> Cbor {
    let mut summary = Vec::new();
    for line in &req.summary {
        summary.push(Cbor::Map(alloc::vec![
            (Cbor::Text("label".into()), Cbor::Text(line.label.clone())),
            (Cbor::Text("value".into()), Cbor::Text(line.value.clone())),
        ]));
    }
    let mut items = Vec::new();
    for item in &req.items {
        items.push(Cbor::Map(alloc::vec![
            (
                Cbor::Text("channel".into()),
                Cbor::Text(item.channel.clone())
            ),
            (Cbor::Text("target".into()), Cbor::Text(item.target.clone())),
            (
                Cbor::Text("payload_hash".into()),
                Cbor::Bytes(item.payload_hash.to_vec()),
            ),
            (Cbor::Text("cost".into()), i64_cbor(item.cost_fen)),
            (
                Cbor::Text("external_reversibility".into()),
                Cbor::Text(item.external_reversibility.clone()),
            ),
            (Cbor::Text("tainted".into()), Cbor::Bool(item.tainted)),
        ]));
    }
    Cbor::Map(alloc::vec![
        (
            Cbor::Text("request_id".into()),
            Cbor::Bytes(req.request_id.0.to_vec()),
        ),
        (
            Cbor::Text("created_at".into()),
            Cbor::Uint(req.created_at_ms)
        ),
        (
            Cbor::Text("expires_at".into()),
            Cbor::Uint(req.expires_at_ms)
        ),
        (Cbor::Text("nonce".into()), Cbor::Bytes(req.nonce.to_vec())),
        (
            Cbor::Text("kind".into()),
            Cbor::Text(req.kind.as_str().into())
        ),
        (Cbor::Text("summary".into()), Cbor::Array(summary)),
        (Cbor::Text("items".into()), Cbor::Array(items)),
        (Cbor::Text("branch".into()), Cbor::Text(req.branch.clone())),
        (
            Cbor::Text("diff_hash".into()),
            Cbor::Bytes(req.diff_hash.to_vec()),
        ),
    ])
}

fn i64_cbor(n: i64) -> Cbor {
    if n >= 0 {
        Cbor::Uint(n as u64)
    } else {
        Cbor::Nint((-1i128 - n as i128) as u64)
    }
}

/// Debug confirmer: a fixed software key. The red banner belongs to the version UI.
#[cfg(feature = "dev-confirmer")]
pub mod dev {
    use super::*;

    pub fn sign_decision(
        seed32: &[u8; 32],
        request_bytes: &[u8],
        decision: &str,
    ) -> Result<[u8; 64], ConfirmError> {
        let msg = decision_message(request_bytes, decision)?;
        crypto::sign(seed32, &msg).map_err(|_| ConfirmError::Signature)
    }

    pub fn public_key(seed32: &[u8; 32]) -> Result<[u8; 65], ()> {
        crypto::public_uncompressed(seed32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ApprovalRequest {
        ApprovalRequest {
            request_id: ObjectId::from_parts(10, 10),
            created_at_ms: 1_000,
            expires_at_ms: 5_000,
            nonce: [3u8; 16],
            kind: Kind::Egress,
            summary: alloc::vec![SummaryLine {
                label: "channel".into(),
                value: "deliver:test".into(),
            }],
            items: alloc::vec![ApprovalItem {
                channel: "deliver:test".into(),
                target: "box".into(),
                payload_hash: [9u8; 32],
                cost_fen: 0,
                external_reversibility: "irreversible".into(),
                tainted: true,
            }],
            branch: "task-1".into(),
            diff_hash: [4u8; 32],
        }
    }

    #[test]
    fn summary_cap_and_replay() {
        let mut req = sample();
        for i in 0..13 {
            req.summary.push(SummaryLine {
                label: "x".into(),
                value: alloc::format!("{i}"),
            });
        }
        assert_eq!(
            encode_request(&req).unwrap_err(),
            ConfirmError::SummaryTooLong
        );

        let mut guard = MemoryReplay::default();
        guard.consume(&[1, 2, 3], 100, 10).unwrap();
        assert_eq!(
            guard.consume(&[1, 2, 3], 100, 10).unwrap_err(),
            ConfirmError::Replay
        );
        assert_eq!(
            guard.consume(&[9], 10, 10).unwrap_err(),
            ConfirmError::Expired
        );
    }

    #[cfg(feature = "dev-confirmer")]
    #[test]
    fn dev_signature_verifies() {
        let seed = [11u8; 32];
        let bytes = encode_request(&sample()).unwrap();
        let sig = dev::sign_decision(&seed, &bytes, "approve").unwrap();
        let pk = dev::public_key(&seed).unwrap();
        verify_decision(&pk, &bytes, "approve", &sig).unwrap();
        assert!(verify_decision(&pk, &bytes, "deny", &sig).is_err());
    }
}
