use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use aos_types::cbor::CborError;
use aos_types::{sha256, Actor, Cbor, ContentHash, Object, ObjectId, Patch, Predicate, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct WriteRecord {
    pub id: ObjectId,
    pub prev_version: u64,
    pub new_version: u64,
    pub patch: Patch,
    pub inverse: Patch,
    pub before: Object,
    pub after: Object,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerOp {
    pub cap: ObjectId,
    pub cpu_ms: i64,
    pub gpu_ms: i64,
    pub money_fen: i64,
}

impl LedgerOp {
    pub fn negated(&self) -> Self {
        Self {
            cap: self.cap,
            cpu_ms: self.cpu_ms.saturating_neg(),
            gpu_ms: self.gpu_ms.saturating_neg(),
            money_fen: self.money_fen.saturating_neg(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    pub seq: u64,
    pub branch: String,
    pub tx: ObjectId,
    pub prev_hash: [u8; 32],
    pub actor: Actor,
    pub capability: Option<ObjectId>,
    pub action: String,
    pub inputs: BTreeMap<String, Value>,
    pub writes: Vec<WriteRecord>,
    pub intents: Vec<ObjectId>,
    pub timestamp_ms: u64,
    pub ledger: Vec<LedgerOp>,
    pub preconditions: Vec<Predicate>,
    pub idempotency_key: Option<String>,
    pub idempotency_input: Option<[u8; 32]>,
    pub hash: [u8; 32],
}

impl LogEntry {
    /// Fill in `hash`. Fails when any value has no canonical CBOR form
    /// (NaN or infinite floats, nesting deeper than the codec allows); such an
    /// entry cannot be hashed and so must not reach the log.
    pub fn seal(mut self) -> Result<Self, CborError> {
        self.hash = hash_entry(&self)?;
        Ok(self)
    }
}

/// `SHA-256(canonical_cbor(entry without hash))`. Every field is covered; a
/// value that cannot be encoded is an error, never silently left out.
pub fn hash_entry(entry: &LogEntry) -> Result<[u8; 32], CborError> {
    let bytes = entry_cbor(entry)?.canonical()?;
    Ok(sha256(&bytes))
}

pub fn verify_chain(entries: &[LogEntry]) -> Result<(), usize> {
    let mut prev = [0u8; 32];
    for (i, e) in entries.iter().enumerate() {
        if e.seq != i as u64 || e.prev_hash != prev || hash_entry(e) != Ok(e.hash) {
            return Err(i);
        }
        prev = e.hash;
    }
    Ok(())
}

fn entry_cbor(e: &LogEntry) -> Result<Cbor, CborError> {
    let mut writes = Vec::new();
    for w in &e.writes {
        writes.push(write_cbor(w)?);
    }
    let mut ledger = Vec::new();
    for op in &e.ledger {
        ledger.push(Cbor::Map(alloc::vec![
            (Cbor::Text("cap".into()), Cbor::Bytes(op.cap.0.to_vec())),
            (Cbor::Text("cpu_ms".into()), i64_cbor(op.cpu_ms)),
            (Cbor::Text("gpu_ms".into()), i64_cbor(op.gpu_ms)),
            (Cbor::Text("money_fen".into()), i64_cbor(op.money_fen)),
        ]));
    }
    let mut inputs = Vec::new();
    for (k, v) in &e.inputs {
        inputs.push((Cbor::Text(k.clone()), v.to_cbor()?));
    }
    let mut preds = Vec::new();
    for p in &e.preconditions {
        preds.push(p.to_cbor()?);
    }
    let mut intents = Vec::new();
    for id in &e.intents {
        intents.push(Cbor::Bytes(id.0.to_vec()));
    }
    Ok(Cbor::Map(alloc::vec![
        (Cbor::Text("seq".into()), Cbor::Uint(e.seq)),
        (Cbor::Text("branch".into()), Cbor::Text(e.branch.clone())),
        (Cbor::Text("tx".into()), Cbor::Bytes(e.tx.0.to_vec())),
        (
            Cbor::Text("prev_hash".into()),
            Cbor::Bytes(e.prev_hash.to_vec())
        ),
        (Cbor::Text("actor".into()), Cbor::Text(e.actor.as_text())),
        (
            Cbor::Text("capability".into()),
            match e.capability {
                Some(id) => Cbor::Bytes(id.0.to_vec()),
                None => Cbor::Null,
            },
        ),
        (Cbor::Text("action".into()), Cbor::Text(e.action.clone())),
        (Cbor::Text("inputs".into()), Cbor::Map(inputs)),
        (Cbor::Text("writes".into()), Cbor::Array(writes)),
        (Cbor::Text("intents".into()), Cbor::Array(intents)),
        (Cbor::Text("timestamp".into()), Cbor::Uint(e.timestamp_ms)),
        (Cbor::Text("ledger".into()), Cbor::Array(ledger)),
        (Cbor::Text("preconditions".into()), Cbor::Array(preds)),
        (
            Cbor::Text("idempotency_key".into()),
            match &e.idempotency_key {
                Some(k) => Cbor::Text(k.clone()),
                None => Cbor::Null,
            },
        ),
        (
            Cbor::Text("idempotency_input".into()),
            match e.idempotency_input {
                Some(h) => Cbor::Bytes(h.to_vec()),
                None => Cbor::Null,
            },
        ),
    ]))
}

fn write_cbor(w: &WriteRecord) -> Result<Cbor, CborError> {
    let patch = w.patch.to_cbor()?;
    let inverse = w.inverse.to_cbor()?;
    let before = w.before.to_cbor()?;
    let after = w.after.to_cbor()?;
    Ok(Cbor::Map(alloc::vec![
        (Cbor::Text("id".into()), Cbor::Bytes(w.id.0.to_vec())),
        (
            Cbor::Text("prev_version".into()),
            Cbor::Uint(w.prev_version)
        ),
        (Cbor::Text("new_version".into()), Cbor::Uint(w.new_version)),
        (Cbor::Text("patch".into()), patch),
        (Cbor::Text("inverse".into()), inverse),
        (Cbor::Text("before".into()), before),
        (Cbor::Text("after".into()), after),
    ]))
}

fn i64_cbor(n: i64) -> Cbor {
    if n >= 0 {
        Cbor::Uint(n as u64)
    } else {
        Cbor::Nint((-1i128 - n as i128) as u64)
    }
}

/// Blob hashes still referenced by any retained log version.
pub fn referenced_blobs(entries: &[LogEntry]) -> alloc::collections::BTreeSet<ContentHash> {
    let mut keep = alloc::collections::BTreeSet::new();
    for e in entries {
        for w in &e.writes {
            for b in &w.after.blobs {
                keep.insert(*b);
            }
            for b in &w.before.blobs {
                keep.insert(*b);
            }
        }
    }
    keep
}
