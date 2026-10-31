//! Egress gate. This is the only library path that calls [`EgressTransport`].
//!
//! The in-memory action host lives here so a version can run M2 and M3 checks
//! without linking a WASM engine. Engines stay in the version repositories;
//! `aos-conformance` uses wasmi only in its own tests.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use aos_action::{
    authorize, authorize_intent, check_idempotency, check_type_allowed, ActionError, IdemHit,
};
use aos_branch::{OpenTx, QuotaSpent, World};
use aos_cap::{covers, CapError, Capability, Spent};
use aos_confirm::{decode_request, verify_decision, ConfirmError, Kind, ReplayGuard};
use aos_log::{BlobStore, LogStore};
use aos_policy::{evaluate, verify_policy, Decision, IntentFacts, Policy};
use aos_query::{execute_with, Page};
use aos_types::{
    sha256, Actor, Clock, ContentHash, Object, ObjectId, ObjectView, Query, Rng, Taint, Value,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    Failed(String),
}

pub trait EgressTransport {
    fn send(
        &mut self,
        channel: &str,
        target: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, TransportError>;
}

/// Test adapter. It records bytes and does not open a socket.
#[derive(Clone, Debug, Default)]
pub struct RecordingTransport {
    pub sent: Vec<RecordedSend>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedSend {
    pub channel: String,
    pub target: String,
    pub payload: Vec<u8>,
}

impl EgressTransport for RecordingTransport {
    fn send(
        &mut self,
        channel: &str,
        target: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, TransportError> {
        self.sent.push(RecordedSend {
            channel: channel.into(),
            target: target.into(),
            payload: payload.to_vec(),
        });
        Ok(b"ok".to_vec())
    }
}

/// What lets one send through the gate. Neither variant carries a public key:
/// the gate verifies against the confirmer key it is given as its trust root,
/// so a caller cannot sign its own permit.
#[derive(Clone, Debug)]
pub enum Permit {
    /// A standing policy signed by the confirmer (§9.3). The gate evaluates
    /// it against `facts` and sends only when the result is `allow`.
    /// `facts.channel` must be the channel being sent on.
    Allow {
        policy: Policy,
        signature: Vec<u8>,
        facts: IntentFacts,
    },
    /// A one-time approval (§12.4). Channel, target, payload hash, nonce, and
    /// expiry are read from `request_bytes`, which the signature covers;
    /// nothing outside the signed bytes is trusted.
    Approval {
        request_bytes: Vec<u8>,
        decision: String,
        signature: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateError {
    NoPermit,
    Tamper,
    Signature,
    Expired,
    Replay,
    Denied,
    Policy,
    Transport(String),
    Confirm(ConfirmError),
}

fn gate_error(e: ConfirmError) -> GateError {
    match e {
        ConfirmError::Expired => GateError::Expired,
        ConfirmError::Replay => GateError::Replay,
        other => GateError::Confirm(other),
    }
}

/// Replay key for one approved item: the signed nonce followed by the item's
/// index as a big-endian u32. One approval can list several items (§12.3);
/// each may be sent once.
pub fn replay_key(nonce: &[u8; 16], item_index: usize) -> Vec<u8> {
    let mut key = nonce.to_vec();
    key.extend_from_slice(&(item_index as u32).to_be_bytes());
    key
}

/// Run the gate. `transport.send` is called only after every check succeeds.
///
/// `confirmer_pubkey` is the paired iPhone key (65-byte SEC1), the trust root
/// for both approvals and standing policies (§9.3, §12.2). Invariants (§11.4):
/// the payload hash is recomputed and must equal the hash inside the signed
/// request; each approval item is consumed once, before its expiry, keyed by
/// the signed nonce.
#[allow(clippy::too_many_arguments)]
pub fn execute_gate<T: EgressTransport, G: ReplayGuard>(
    transport: &mut T,
    replay: &mut G,
    confirmer_pubkey: &[u8],
    now_ms: u64,
    channel: &str,
    target: &str,
    payload: &[u8],
    expected_hash: &[u8; 32],
    permit: Option<&Permit>,
) -> Result<Vec<u8>, GateError> {
    let Some(permit) = permit else {
        return Err(GateError::NoPermit);
    };
    let actual = sha256(payload);
    if &actual != expected_hash {
        return Err(GateError::Tamper);
    }
    match permit {
        Permit::Allow {
            policy,
            signature,
            facts,
        } => {
            if !verify_policy(policy, confirmer_pubkey, signature) {
                return Err(GateError::Policy);
            }
            if facts.channel != channel {
                return Err(GateError::Tamper);
            }
            match evaluate(policy, facts) {
                Decision::Allow => {}
                Decision::Confirm => return Err(GateError::NoPermit),
                Decision::Deny => return Err(GateError::Denied),
            }
        }
        Permit::Approval {
            request_bytes,
            decision,
            signature,
        } => {
            if decision != "approve" {
                let _ = verify_decision(confirmer_pubkey, request_bytes, decision, signature);
                return Err(GateError::Denied);
            }
            verify_decision(confirmer_pubkey, request_bytes, decision, signature)
                .map_err(gate_error)?;
            let req = decode_request(request_bytes).map_err(gate_error)?;
            if req.kind != Kind::Egress {
                return Err(GateError::Tamper);
            }
            let mut outcome = Err(GateError::Tamper);
            for (index, item) in req.items.iter().enumerate() {
                if item.channel != channel || item.target != target || item.payload_hash != actual {
                    continue;
                }
                outcome = replay
                    .consume(&replay_key(&req.nonce, index), req.expires_at_ms, now_ms)
                    .map_err(gate_error);
                // An identical item later in the list may still be unused.
                if !matches!(outcome, Err(GateError::Replay)) {
                    break;
                }
            }
            outcome?;
        }
    }
    transport
        .send(channel, target, payload)
        .map_err(|e| match e {
            TransportError::Failed(s) => GateError::Transport(s),
        })
}

pub struct Host<'a> {
    pub tx: &'a mut OpenTx,
    cap: &'a Capability,
    reads: Vec<String>,
    writes: Vec<String>,
    /// Channel patterns the action declared (`egress:` in §8.1).
    egress: Vec<String>,
    pub violation: Option<String>,
}

impl<'a> Host<'a> {
    pub fn read(&mut self, id: &ObjectId) -> Result<Object, ActionError> {
        let Some(obj) = self.tx.get(id) else {
            self.violate("missing object");
            return Err(ActionError::UndeclaredType);
        };
        if let Err(e) = check_type_allowed(&self.reads, &obj.type_name) {
            self.violate("undeclared read");
            return Err(e);
        }
        self.tx.note_read(&obj);
        Ok(obj)
    }

    pub fn write(&mut self, obj: Object) -> Result<(), ActionError> {
        if let Err(e) = check_type_allowed(&self.writes, &obj.type_name) {
            self.violate("undeclared write");
            return Err(e);
        }
        self.tx.put(obj);
        Ok(())
    }

    /// Every object that matched counts as read, not only the rows on this
    /// page: counts, histograms, and means also carry its content to the
    /// guest, and a tainted match must taint what the guest does next.
    pub fn query(&mut self, query: &Query) -> Result<Page, ActionError> {
        if let Err(e) = check_type_allowed(&self.reads, &query.from) {
            self.violate("undeclared query");
            return Err(e);
        }
        let mut matched = Vec::new();
        let page = {
            let view = TxView { tx: self.tx };
            execute_with(&view, query, &mut |obj| matched.push(obj.clone()))
                .map_err(|_| ActionError::UndeclaredType)?
        };
        for obj in &matched {
            self.tx.note_read(obj);
        }
        Ok(page)
    }

    pub fn create_intent(
        &mut self,
        id: ObjectId,
        channel: &str,
        target: &str,
        target_ref: Option<ObjectId>,
        payload_hash: ContentHash,
        reversibility: &str,
        cost_fen: i64,
        actor: Actor,
    ) -> Result<ObjectId, ActionError> {
        // §9.1: creating an intent checks the token. §8.1: the action must
        // have declared the channel. Either failure is a violation.
        if !self.egress.iter().any(|p| covers(p, channel)) {
            self.violate("undeclared egress channel");
            return Err(ActionError::Capability(CapError::Channel));
        }
        if let Err(e) = authorize_intent(self.cap, channel) {
            self.violate("egress channel outside capability");
            return Err(e);
        }
        let recipient_tainted = target_ref
            .and_then(|id| self.tx.get(&id))
            .map(|o| o.provenance.taint == Taint::External)
            .unwrap_or(false)
            || text_from_tainted_read(self.tx, target);
        let channel_tainted = text_from_tainted_read(self.tx, channel);
        let tainted = recipient_tainted || channel_tainted;
        let mut obj = Object::new(id, "sys/EgressIntent", 1, actor);
        obj.fields
            .insert("channel".into(), Value::Text(channel.into()));
        obj.fields
            .insert("target".into(), Value::Text(target.into()));
        if let Some(rid) = target_ref {
            obj.fields.insert("target_ref".into(), Value::Ref(rid));
        }
        obj.fields.insert(
            "payload_hash".into(),
            Value::Text(payload_hash.to_prefixed_hex()),
        );
        obj.fields.insert(
            "external_reversibility".into(),
            Value::Text(reversibility.into()),
        );
        obj.fields
            .insert("cost_money_fen".into(), Value::Int(cost_fen));
        obj.fields
            .insert("status".into(), Value::Text("pending".into()));
        obj.fields
            .insert("lineage_tainted".into(), Value::Bool(tainted));
        obj.provenance.taint = if tainted {
            Taint::External
        } else {
            Taint::Clean
        };
        self.write(obj)?;
        self.tx.intents.push(id);
        Ok(id)
    }

    fn violate(&mut self, reason: &str) {
        self.violation = Some(reason.into());
        self.tx.fail(reason);
    }
}

/// Whether `needle` appears inside any text this transaction read from a
/// tainted object (§10: a channel or recipient that came from external
/// content). Substring, not equality: a model lifts an address out of a
/// sentence, it does not copy whole fields. Loose `extra` fields and nested
/// lists and maps count too.
fn text_from_tainted_read(tx: &OpenTx, needle: &str) -> bool {
    fn contains(v: &Value, needle: &str) -> bool {
        match v {
            Value::Text(s) => s.contains(needle),
            Value::List(xs) => xs.iter().any(|x| contains(x, needle)),
            Value::Map(m) => m
                .iter()
                .any(|(k, x)| k.contains(needle) || contains(x, needle)),
            _ => false,
        }
    }
    if needle.is_empty() {
        return false;
    }
    tx.tainted_reads.iter().any(|id| {
        tx.get(id).is_some_and(|obj| {
            obj.fields
                .values()
                .chain(obj.extra.values())
                .any(|v| contains(v, needle))
        })
    })
}

struct TxView<'a> {
    tx: &'a OpenTx,
}

impl ObjectView for TxView<'_> {
    fn get(&self, id: &ObjectId) -> Option<Object> {
        self.tx.get(id)
    }
    fn for_each(&self, f: &mut dyn FnMut(&Object)) {
        self.tx.for_each(f);
    }
    fn reverse(&self, target: &ObjectId) -> Vec<aos_types::RefEdge> {
        let mut out = Vec::new();
        self.tx.for_each(&mut |obj| {
            for (field, id) in obj.ref_edges() {
                if id == *target {
                    out.push(aos_types::RefEdge {
                        source: obj.id,
                        field,
                    });
                }
            }
        });
        out
    }
}

pub struct Invoke<'a> {
    pub branch: &'a str,
    pub actor: Actor,
    pub cap: &'a Capability,
    pub cap_pubkey: &'a [u8],
    pub decl_name: &'a str,
    pub decl_capability: &'a str,
    pub reads: &'a [&'a str],
    pub writes: &'a [&'a str],
    pub reversibility: &'a str,
    pub egress: &'a [&'a str],
    pub inputs: BTreeMap<String, Value>,
    pub preconditions: Vec<aos_types::Predicate>,
    pub delta_cpu_ms: i64,
    pub idem_key: Option<String>,
    pub idem_parts: Vec<Value>,
}

pub fn spent_of(q: &QuotaSpent) -> Spent {
    Spent {
        cpu_ms: q.cpu_ms,
        gpu_ms: q.gpu_ms,
        money_fen: q.money_fen,
    }
}

/// Facts the policy engine needs. Taint bits come from the intent object the
/// host wrote, not from a flag the guest supplied.
pub fn facts_from_intent(obj: &Object, day_spent_fen: u64) -> IntentFacts {
    let channel = text_field(obj, "channel");
    let sensitivity = obj.sensitivity;
    let cost = match obj.fields.get("cost_money_fen") {
        Some(Value::Int(n)) if *n > 0 => *n as u64,
        _ => 0,
    };
    // The intent stores one lineage bit for recipient and channel together.
    // Reading it as recipient taint is the stricter choice for `when` rules.
    let recipient_tainted = matches!(obj.fields.get("lineage_tainted"), Some(Value::Bool(true)));
    IntentFacts {
        channel,
        sensitivity,
        cost_fen: cost,
        day_spent_fen,
        recipient_tainted,
        channel_tainted: false,
        when_value: Some(Value::Text(
            if recipient_tainted {
                "external"
            } else {
                "clean"
            }
            .into(),
        )),
    }
}

fn text_field(obj: &Object, name: &str) -> String {
    match obj.fields.get(name) {
        Some(Value::Text(s)) => s.clone(),
        _ => String::new(),
    }
}

pub fn invoke<S, B, C, R, F>(
    world: &mut World<S, B, C, R>,
    req: Invoke<'_>,
    guest: F,
) -> Result<Value, ActionError>
where
    S: LogStore,
    B: BlobStore,
    C: Clock,
    R: Rng,
    F: FnOnce(&mut Host<'_>) -> Result<Value, ActionError>,
{
    let decl = aos_types::ActionDecl {
        name: req.decl_name.into(),
        version: 1,
        summary: String::new(),
        reads: req.reads.iter().map(|s| (*s).into()).collect(),
        writes: req.writes.iter().map(|s| (*s).into()).collect(),
        egress: req.egress.iter().map(|s| (*s).into()).collect(),
        reversibility: req.reversibility.into(),
        capability: req.decl_capability.into(),
        idempotency_paths: Vec::new(),
    };
    let delta = aos_log::LedgerOp {
        cap: req.cap.id,
        cpu_ms: req.delta_cpu_ms,
        gpu_ms: 0,
        money_fen: 0,
    };
    authorize(
        req.cap,
        req.cap_pubkey,
        world.clock.now_ms(),
        &decl,
        &spent_of(&world.spent(&req.cap.id)),
        &delta,
        None,
    )?;
    let idem_hash = match &req.idem_key {
        Some(_) => Some(aos_action::input_hash(&req.idem_parts)?),
        None => None,
    };
    if let (Some(key), Some(hash)) = (&req.idem_key, idem_hash) {
        if let Some((prev, result)) = world.idempotent(req.branch, key) {
            match check_idempotency(Some(&prev), Some(&result), &hash)? {
                IdemHit::Replay(v) => return Ok(v),
                IdemHit::Fresh => {}
            }
        }
    }
    for pred in &req.preconditions {
        let ok = aos_types::eval_predicate(pred, &mut |path| {
            aos_types::resolve_input_path(path, &req.inputs, &mut |id| world.get(req.branch, id))
        });
        if !ok {
            return Err(ActionError::Precondition);
        }
    }
    let mut tx = world
        .open(req.branch, req.actor.clone(), req.decl_name)
        .map_err(|_| ActionError::Precondition)?;
    tx.capability = Some(req.cap.id);
    tx.inputs = req.inputs.clone();
    tx.preconditions = req.preconditions.clone();
    if req.delta_cpu_ms != 0 {
        tx.ledger.push(delta);
    }
    if let Some(key) = &req.idem_key {
        tx.idem_key = Some(key.clone());
        tx.idem_input = idem_hash;
    }
    let mut host = Host {
        tx: &mut tx,
        cap: req.cap,
        reads: decl.reads.clone(),
        writes: decl.writes.clone(),
        egress: decl.egress.clone(),
        violation: None,
    };
    let result = guest(&mut host);
    let violation = host.violation.clone();
    drop(host);
    if violation.is_some() || result.is_err() {
        let reason = violation.unwrap_or_else(|| String::from("action failed"));
        drop(tx);
        record_violation(world, req.branch, &req.actor, req.decl_name, &reason)
            .map_err(|_| ActionError::UndeclaredType)?;
        return Err(result.err().unwrap_or(ActionError::UndeclaredType));
    }
    let value = result.unwrap();
    if let Some(key) = req.idem_key.clone() {
        tx.inputs.insert("idem_result".into(), value.clone());
        tx.idem_key = Some(key);
    }
    world.commit(tx).map_err(|e| match e {
        aos_branch::BranchError::Encoding => ActionError::Encoding,
        _ => ActionError::Precondition,
    })?;
    Ok(value)
}

/// Instantiation found an import outside the AOS ABI. The offending module
/// never runs; the violation object is the audit record.
pub fn reject_foreign_import<S, B, C, R>(
    world: &mut World<S, B, C, R>,
    branch: &str,
    actor: &Actor,
    module: &str,
    name: &str,
) -> Result<(), ActionError>
where
    S: LogStore,
    B: BlobStore,
    C: Clock,
    R: Rng,
{
    if !aos_action::is_foreign_import(module, name) {
        return Ok(());
    }
    record_violation(world, branch, actor, name, "direct io").map_err(|_| ActionError::DirectIo)?;
    Err(ActionError::DirectIo)
}

fn record_violation<S, B, C, R>(
    world: &mut World<S, B, C, R>,
    branch: &str,
    actor: &Actor,
    action: &str,
    reason: &str,
) -> Result<(), aos_branch::BranchError>
where
    S: LogStore,
    B: BlobStore,
    C: Clock,
    R: Rng,
{
    let id = world.mint();
    let mut obj = Object::new(id, "sys/ActionViolation", 1, actor.clone());
    obj.fields
        .insert("action".into(), Value::Text(action.into()));
    obj.fields
        .insert("reason".into(), Value::Text(reason.into()));
    obj.fields
        .insert("branch".into(), Value::Text(branch.into()));
    let mut tx = world.open(branch, actor.clone(), "sys.violation.record")?;
    tx.put(obj);
    world.commit(tx)?;
    Ok(())
}

/// Mark a fetch receipt as external ingress. Adapters do not open the network.
pub fn ingress_receipt(id: ObjectId, channel: &str, intent: ObjectId, ok: bool) -> Object {
    let actor = if let Some(name) = channel.split(':').nth(1) {
        Actor::Adapter(name.into())
    } else {
        Actor::System
    };
    let mut obj = Object::new(id, "sys/EgressReceipt", 1, actor);
    obj.fields.insert("intent".into(), Value::Ref(intent));
    obj.fields.insert("ok".into(), Value::Bool(ok));
    obj.fields
        .insert("detail".into(), Value::Text(channel.into()));
    if channel.starts_with("fetch:") {
        obj.provenance.taint = Taint::External;
        obj.provenance.origin = channel.into();
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;
    use aos_branch::MemWorld;
    use aos_cap::{Capability, Limits};
    use aos_confirm::MemoryReplay;
    use aos_policy::{Decision, Policy, Rule};
    use aos_types::{crypto, Actor, Query, Sensitivity};

    fn cap(id: ObjectId, seed: &[u8; 32]) -> (Capability, [u8; 65]) {
        let mut c = Capability {
            id,
            subject: Actor::User,
            rights: alloc::vec!["sys.object.write".into()],
            object_scope: BTreeMap::new(),
            limits: Limits {
                cpu_ms: 100,
                gpu_ms: 0,
                money_fen: 0,
                egress_channels: alloc::vec!["*".into()],
            },
            expires_at_ms: u64::MAX,
            parent: None,
            signature: [0u8; 64],
        };
        aos_cap::sign_token(&mut c, seed).unwrap();
        let pk = crypto::public_uncompressed(seed).unwrap();
        (c, pk)
    }

    #[test]
    fn undeclared_query_writes_violation_and_does_not_commit() {
        let mut world = MemWorld::memory(4);
        let seed = [3u8; 32];
        let id = world.mint();
        let (c, pk) = cap(id, &seed);
        let note = world.mint();
        let mut tx = world
            .open("main", Actor::User, "sys.object.create")
            .unwrap();
        let mut obj = Object::new(note, "sys/Note", 1, Actor::User);
        obj.fields
            .insert("text".into(), Value::Text("secret".into()));
        tx.put(obj);
        world.commit(tx).unwrap();
        let err = invoke(
            &mut world,
            Invoke {
                branch: "main",
                actor: Actor::User,
                cap: &c,
                cap_pubkey: &pk,
                decl_name: "sys.object.update",
                decl_capability: "sys.object.write",
                reads: &["sys/Note"],
                writes: &["sys/Note"],
                reversibility: "undo",
                egress: &[],
                inputs: BTreeMap::new(),
                preconditions: Vec::new(),
                delta_cpu_ms: 1,
                idem_key: None,
                idem_parts: Vec::new(),
            },
            |host| {
                let q = Query::of_type("sys/Fact");
                host.query(&q)?;
                Ok(Value::Null)
            },
        );
        assert!(err.is_err());
        let mut found = false;
        world.branch_view("main").unwrap().for_each(&mut |obj| {
            if obj.type_name == "sys/ActionViolation" {
                found = true;
            }
        });
        assert!(found);
        assert_eq!(
            world.get("main", &note).unwrap().fields.get("text"),
            Some(&Value::Text("secret".into()))
        );
    }

    #[test]
    fn gate_rejects_without_touching_transport() {
        let confirmer = crypto::public_uncompressed(&[5u8; 32]).unwrap();
        let mut transport = RecordingTransport::default();
        let mut replay = MemoryReplay::default();
        let err = execute_gate(
            &mut transport,
            &mut replay,
            &confirmer,
            0,
            "deliver:test",
            "box",
            b"payload",
            &[0u8; 32],
            None,
        )
        .unwrap_err();
        assert_eq!(err, GateError::NoPermit);
        assert!(transport.sent.is_empty());

        // A permissive policy signed by a key that is not the confirmer.
        let policy = Policy {
            name: "x".into(),
            rules: alloc::vec![Rule {
                effect: Decision::Allow,
                channel: "*".into(),
                max_sensitivity: None,
                max_cost_fen_per_day: None,
                when: None,
            }],
            default: Decision::Confirm,
        };
        let signature = aos_policy::sign_policy(&policy, &[6u8; 32])
            .unwrap()
            .to_vec();
        let permit = Permit::Allow {
            policy,
            signature,
            facts: IntentFacts {
                channel: "deliver:test".into(),
                sensitivity: Sensitivity::Public,
                cost_fen: 0,
                day_spent_fen: 0,
                recipient_tainted: false,
                channel_tainted: false,
                when_value: None,
            },
        };
        let payload = b"payload";
        let err = execute_gate(
            &mut transport,
            &mut replay,
            &confirmer,
            0,
            "deliver:test",
            "box",
            payload,
            &sha256(payload),
            Some(&permit),
        )
        .unwrap_err();
        assert_eq!(err, GateError::Policy);
        assert!(transport.sent.is_empty());
    }
}
