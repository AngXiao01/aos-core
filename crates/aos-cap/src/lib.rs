//! Capability tokens: attenuation, verification, and quota arithmetic.
//!
//! The token is immutable after it is signed. Spending lives in the log ledger
//! (`aos_log::LedgerOp`) and is not part of the signature.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use aos_log::LedgerOp;
use aos_types::{crypto, Actor, Cbor, ObjectId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub cpu_ms: u64,
    pub gpu_ms: u64,
    pub money_fen: u64,
    pub egress_channels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Spent {
    pub cpu_ms: i64,
    pub gpu_ms: i64,
    pub money_fen: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub id: ObjectId,
    pub subject: Actor,
    pub rights: Vec<String>,
    pub object_scope: BTreeMap<String, Vec<ObjectId>>,
    pub limits: Limits,
    pub expires_at_ms: u64,
    pub parent: Option<ObjectId>,
    pub signature: [u8; 64],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapError {
    SelfIssue,
    NotSubset,
    Expired,
    Right,
    Scope,
    Quota,
    BadSignature,
    Channel,
}

impl Capability {
    pub fn signing_bytes(&self) -> Vec<u8> {
        unsigned_cbor(self).canonical().expect("cap cbor")
    }
}

pub fn sign_token(cap: &mut Capability, seed32: &[u8; 32]) -> Result<(), CapError> {
    let bytes = cap.signing_bytes();
    cap.signature = crypto::sign(seed32, &bytes).map_err(|_| CapError::BadSignature)?;
    Ok(())
}

pub fn verify_token(cap: &Capability, public_key: &[u8]) -> Result<(), CapError> {
    if crypto::verify(public_key, &cap.signing_bytes(), &cap.signature) {
        Ok(())
    } else {
        Err(CapError::BadSignature)
    }
}

/// `issuer` must be the parent subject. The child subject must be someone else.
///
/// The parent must carry a valid signature under the same key that will sign
/// the child. Otherwise a caller could hand in an unsigned "parent" with any
/// rights it likes and get them back as a properly signed child.
pub fn attenuate(
    parent: &Capability,
    mut child: Capability,
    issuer: &Actor,
    seed32: &[u8; 32],
) -> Result<Capability, CapError> {
    let signer = crypto::public_uncompressed(seed32).map_err(|_| CapError::BadSignature)?;
    verify_token(parent, &signer)?;
    if issuer != &parent.subject {
        return Err(CapError::NotSubset);
    }
    if issuer == &child.subject {
        return Err(CapError::SelfIssue);
    }
    if !rights_subset(&parent.rights, &child.rights) {
        return Err(CapError::NotSubset);
    }
    if !scope_subset(&parent.object_scope, &child.object_scope) {
        return Err(CapError::NotSubset);
    }
    if child.limits.cpu_ms > parent.limits.cpu_ms
        || child.limits.gpu_ms > parent.limits.gpu_ms
        || child.limits.money_fen > parent.limits.money_fen
        || !channels_subset(
            &parent.limits.egress_channels,
            &child.limits.egress_channels,
        )
    {
        return Err(CapError::NotSubset);
    }
    if child.expires_at_ms > parent.expires_at_ms {
        return Err(CapError::NotSubset);
    }
    child.parent = Some(parent.id);
    sign_token(&mut child, seed32)?;
    Ok(child)
}

pub fn covers_right(rights: &[String], needed: &str) -> bool {
    rights.iter().any(|pat| covers(pat, needed))
}

pub fn covers(pattern: &str, needed: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return needed.starts_with(prefix);
    }
    pattern == needed
}

fn rights_subset(parent: &[String], child: &[String]) -> bool {
    child.iter().all(|c| parent.iter().any(|p| covers(p, c)))
}

fn channels_subset(parent: &[String], child: &[String]) -> bool {
    child.iter().all(|c| parent.iter().any(|p| covers(p, c)))
}

/// Dropping a parent restriction widens the token, so the child must keep it
/// and may only shrink the id list. Extra child restrictions are allowed.
fn scope_subset(
    parent: &BTreeMap<String, Vec<ObjectId>>,
    child: &BTreeMap<String, Vec<ObjectId>>,
) -> bool {
    for (field, ids) in parent {
        let Some(child_ids) = child.get(field) else {
            return false;
        };
        if child_ids.iter().any(|id| !ids.contains(id)) {
            return false;
        }
    }
    true
}

pub fn scope_allows(scope: &BTreeMap<String, Vec<ObjectId>>, field: &str, id: &ObjectId) -> bool {
    match scope.get(field) {
        None => true,
        Some(ids) => ids.contains(id),
    }
}

/// `delta` is a debit. Refunds only come from reverting the transaction that
/// charged, which writes the negated ledger entry directly; a negative debit
/// here would let any call hand itself quota back.
pub fn ensure_quota(limits: &Limits, spent: &Spent, delta: &LedgerOp) -> Result<(), CapError> {
    if delta.cpu_ms < 0 || delta.gpu_ms < 0 || delta.money_fen < 0 {
        return Err(CapError::Quota);
    }
    let cpu = spent.cpu_ms.saturating_add(delta.cpu_ms);
    let gpu = spent.gpu_ms.saturating_add(delta.gpu_ms);
    let money = spent.money_fen.saturating_add(delta.money_fen);
    if cpu < 0 || gpu < 0 || money < 0 {
        return Err(CapError::Quota);
    }
    if cpu as u64 > limits.cpu_ms || gpu as u64 > limits.gpu_ms || money as u64 > limits.money_fen {
        return Err(CapError::Quota);
    }
    Ok(())
}

pub fn check_call(
    cap: &Capability,
    public_key: &[u8],
    now_ms: u64,
    right: &str,
    spent: &Spent,
    delta: &LedgerOp,
) -> Result<(), CapError> {
    verify_token(cap, public_key)?;
    if now_ms >= cap.expires_at_ms {
        return Err(CapError::Expired);
    }
    if !covers_right(&cap.rights, right) {
        return Err(CapError::Right);
    }
    ensure_quota(&cap.limits, spent, delta)
}

/// An empty `egress_channels` list allows no channel. Reading it as "any"
/// would also let `attenuate` widen a token, since an empty child list is
/// trivially a subset of every parent list.
pub fn check_channel(cap: &Capability, channel: &str) -> Result<(), CapError> {
    if cap
        .limits
        .egress_channels
        .iter()
        .any(|p| covers(p, channel))
    {
        Ok(())
    } else {
        Err(CapError::Channel)
    }
}

fn unsigned_cbor(cap: &Capability) -> Cbor {
    let mut rights = Vec::new();
    for r in &cap.rights {
        rights.push(Cbor::Text(r.clone()));
    }
    let mut scope = Vec::new();
    for (k, ids) in &cap.object_scope {
        let mut arr = Vec::new();
        for id in ids {
            arr.push(Cbor::Bytes(id.0.to_vec()));
        }
        scope.push((Cbor::Text(k.clone()), Cbor::Array(arr)));
    }
    let mut channels = Vec::new();
    for c in &cap.limits.egress_channels {
        channels.push(Cbor::Text(c.clone()));
    }
    Cbor::Map(alloc::vec![
        (Cbor::Text("id".into()), Cbor::Bytes(cap.id.0.to_vec())),
        (
            Cbor::Text("subject".into()),
            Cbor::Text(cap.subject.as_text())
        ),
        (Cbor::Text("rights".into()), Cbor::Array(rights)),
        (Cbor::Text("object_scope".into()), Cbor::Map(scope)),
        (Cbor::Text("cpu_ms".into()), Cbor::Uint(cap.limits.cpu_ms)),
        (Cbor::Text("gpu_ms".into()), Cbor::Uint(cap.limits.gpu_ms)),
        (
            Cbor::Text("money_fen".into()),
            Cbor::Uint(cap.limits.money_fen)
        ),
        (Cbor::Text("egress_channels".into()), Cbor::Array(channels)),
        (
            Cbor::Text("expires_at".into()),
            Cbor::Uint(cap.expires_at_ms)
        ),
        (
            Cbor::Text("parent".into()),
            match cap.parent {
                Some(id) => Cbor::Bytes(id.0.to_vec()),
                None => Cbor::Null,
            },
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(id: ObjectId) -> Capability {
        Capability {
            id,
            subject: Actor::User,
            rights: alloc::vec!["ml.dataset.*".into(), "deliver:*".into()],
            object_scope: BTreeMap::new(),
            limits: Limits {
                cpu_ms: 1000,
                gpu_ms: 0,
                money_fen: 50,
                egress_channels: alloc::vec!["model:*".into(), "deliver:*".into()],
            },
            expires_at_ms: 100,
            parent: None,
            signature: [0u8; 64],
        }
    }

    #[test]
    fn attenuate_rejects_wider_rights_and_self_issue() {
        let seed = [9u8; 32];
        let id = ObjectId::from_parts(1, 1);
        let mut parent = root(id);
        sign_token(&mut parent, &seed).unwrap();
        let pk = crypto::public_uncompressed(&seed).unwrap();
        verify_token(&parent, &pk).unwrap();

        let child_id = ObjectId::from_parts(1, 2);
        let mut too_wide = root(child_id);
        too_wide.subject = Actor::AgentTask(child_id);
        too_wide.rights = alloc::vec!["other.thing".into()];
        assert_eq!(
            attenuate(&parent, too_wide, &Actor::User, &seed).unwrap_err(),
            CapError::NotSubset
        );

        let mut self_issue = root(child_id);
        self_issue.subject = Actor::User;
        self_issue.rights = alloc::vec!["ml.dataset.read".into()];
        self_issue.limits.egress_channels = alloc::vec!["model:default".into()];
        assert_eq!(
            attenuate(&parent, self_issue, &Actor::User, &seed).unwrap_err(),
            CapError::SelfIssue
        );

        let mut ok = root(child_id);
        ok.subject = Actor::AgentTask(child_id);
        ok.rights = alloc::vec!["ml.dataset.read".into()];
        ok.limits.cpu_ms = 10;
        ok.limits.egress_channels = alloc::vec!["model:default".into()];
        ok.expires_at_ms = 90;
        let child = attenuate(&parent, ok, &Actor::User, &seed).unwrap();
        verify_token(&child, &pk).unwrap();
        assert!(covers_right(&child.rights, "ml.dataset.read"));
        assert!(!covers_right(&child.rights, "ml.other"));
    }

    #[test]
    fn forged_parent_and_empty_channel_list_do_not_widen() {
        let seed = [9u8; 32];
        let mut forged = root(ObjectId::from_parts(1, 1));
        forged.rights = alloc::vec!["*".into()];
        let mut child = root(ObjectId::from_parts(1, 2));
        child.subject = Actor::AgentTask(child.id);
        child.rights = alloc::vec!["anything".into()];
        assert_eq!(
            attenuate(&forged, child, &Actor::User, &seed).unwrap_err(),
            CapError::BadSignature
        );

        let mut parent = root(ObjectId::from_parts(1, 1));
        sign_token(&mut parent, &seed).unwrap();
        let mut child = root(ObjectId::from_parts(1, 3));
        child.subject = Actor::AgentTask(child.id);
        child.rights = alloc::vec!["ml.dataset.read".into()];
        child.limits.egress_channels = Vec::new();
        let child = attenuate(&parent, child, &Actor::User, &seed).unwrap();
        assert_eq!(
            check_channel(&child, "deliver:anywhere").unwrap_err(),
            CapError::Channel
        );
        assert!(check_channel(&parent, "deliver:x").is_ok());
    }

    #[test]
    fn quota_rolls_back_when_caller_does_not_commit() {
        let limits = Limits {
            cpu_ms: 100,
            gpu_ms: 0,
            money_fen: 0,
            egress_channels: Vec::new(),
        };
        let spent = Spent::default();
        let delta = LedgerOp {
            cap: ObjectId::ZERO,
            cpu_ms: 40,
            gpu_ms: 0,
            money_fen: 0,
        };
        ensure_quota(&limits, &spent, &delta).unwrap();
        let over = LedgerOp {
            cpu_ms: 70,
            ..delta.clone()
        };
        // A rejected debit leaves the caller's spent map unchanged.
        assert_eq!(
            ensure_quota(
                &limits,
                &Spent {
                    cpu_ms: 40,
                    ..Spent::default()
                },
                &over
            )
            .unwrap_err(),
            CapError::Quota
        );
        assert_eq!(spent.cpu_ms, 0);
        let refund = LedgerOp {
            cpu_ms: -40,
            ..delta.clone()
        };
        let busy = Spent {
            cpu_ms: 90,
            ..Spent::default()
        };
        assert_eq!(
            ensure_quota(&limits, &busy, &refund).unwrap_err(),
            CapError::Quota
        );
    }
}
