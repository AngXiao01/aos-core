//! §16.1 conformance checks against the in-memory backend.
//!
//! The cases are generic over [`LogStore`](aos_log::LogStore) where the world
//! is generic, and the shipped tests construct [`MemWorld`](aos_branch::MemWorld).
//! A version repository can call the same functions with its own store.

use std::collections::BTreeMap;

use aos_action::{
    check_remote_job, check_type_allowed, transition, JobState, ABI_VERSION, IMPORTS,
};
use aos_branch::{BranchError, BranchStatus, MemWorld};
use aos_cap::{
    attenuate, check_call, covers_right, sign_token, verify_token, Capability, Limits, Spent,
};
#[cfg(feature = "dev-confirmer")]
use aos_confirm::dev;
use aos_confirm::{
    encode_request, verify_decision, ApprovalItem, ApprovalRequest, Kind, SummaryLine,
};
use aos_egress::{
    execute_gate, facts_from_intent, ingress_receipt, invoke, reject_foreign_import, GateError,
    Invoke, Permit, RecordingTransport,
};
use aos_log::LogStore;
use aos_log::{verify_chain, LedgerOp};
use aos_policy::{evaluate, sign_policy, verify_policy, Decision, IntentFacts, Policy, Rule};
use aos_query::parse_query;
use aos_types::{
    crypto, propagate_taint, sha256, Actor, CmpOp, ManualRng, Object, ObjectId, ObjectView,
    Predicate, Query, Rng, Sensitivity, Taint, Value,
};

pub fn log_chain() {
    let mut world = MemWorld::memory(11);
    let id = world.mint();
    write_note(&mut world, id, "hello");
    world.verify().unwrap();
    let mut entries = world.store.entries();
    assert!(verify_chain(&entries).is_ok());
    entries[0].action = "tampered".into();
    assert!(verify_chain(&entries).is_err());
}

pub fn transactions_property() {
    let mut rng = ManualRng::new(99);
    for case in 0..10_000 {
        let mut world = MemWorld::memory(1_000 + case);
        let n = 1 + (rng.next_u64() % 3) as usize;
        let mut ids = Vec::new();
        for _ in 0..n {
            let id = world.mint();
            write_note(&mut world, id, "base");
            ids.push(id);
        }
        let snap = world
            .open("main", Actor::User, "sys.object.update")
            .unwrap();
        let mut first = world
            .open("main", Actor::User, "sys.object.update")
            .unwrap();
        let touch = rng.next_u64() as usize % n;
        let mut obj = first.get(&ids[touch]).unwrap();
        obj.fields
            .insert("text".into(), Value::Text(format!("c{case}")));
        first.put(obj);
        world.commit(first).unwrap();
        let overlap = rng.next_u64() % 2 == 0;
        let mut snap = snap;
        if overlap {
            let mut obj = snap.get(&ids[touch]).unwrap();
            obj.fields
                .insert("text".into(), Value::Text("other".into()));
            snap.put(obj);
            assert_eq!(world.commit(snap).unwrap_err(), BranchError::Conflict);
            let kept = world.get("main", &ids[touch]).unwrap();
            assert_eq!(
                kept.fields.get("text"),
                Some(&Value::Text(format!("c{case}")))
            );
        } else if n > 1 {
            let other = (touch + 1) % n;
            let mut obj = snap.get(&ids[other]).unwrap();
            obj.fields.insert("text".into(), Value::Text("side".into()));
            snap.put(obj);
            world.commit(snap).unwrap();
            assert_eq!(
                world.get("main", &ids[other]).unwrap().fields.get("text"),
                Some(&Value::Text("side".into()))
            );
            assert_eq!(
                world.get("main", &ids[touch]).unwrap().fields.get("text"),
                Some(&Value::Text(format!("c{case}")))
            );
        }
        let _ = case;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Vis {
    type_name: String,
    fields: BTreeMap<String, String>,
    taint: Taint,
    sensitivity: Sensitivity,
    tombstone: bool,
    major: u32,
}

fn vis_of(obj: &Object) -> Vis {
    let mut fields = BTreeMap::new();
    for (k, v) in &obj.fields {
        let text = match v {
            Value::Text(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => String::from("?"),
        };
        fields.insert(k.clone(), text);
    }
    Vis {
        type_name: obj.type_name.clone(),
        fields,
        taint: obj.provenance.taint,
        sensitivity: obj.sensitivity,
        tombstone: obj.tombstone,
        major: obj.major,
    }
}

fn shell() -> Vis {
    Vis {
        type_name: String::new(),
        fields: BTreeMap::new(),
        taint: Taint::Clean,
        sensitivity: Sensitivity::Private,
        tombstone: true,
        major: 0,
    }
}

/// Reference model for revert. Each transaction remembers, per object, the
/// visible state before and after it. Reverting puts back only the attributes
/// that transaction changed, which is what applying the stored inverse patch
/// does; later changes to other attributes stay.
struct Naive {
    objs: BTreeMap<ObjectId, Vis>,
    txs: Vec<NaiveTx>,
}

struct NaiveTx {
    id: ObjectId,
    touched: Vec<(ObjectId, Vis, Vis)>,
    reverted: bool,
}

impl Naive {
    fn apply(&mut self, tx: ObjectId, touched: Vec<(ObjectId, Vis, Vis)>) {
        for (id, _, after) in &touched {
            self.objs.insert(*id, after.clone());
        }
        self.txs.push(NaiveTx {
            id: tx,
            touched,
            reverted: false,
        });
    }

    fn revert(&mut self, index: usize) {
        let touched = self.txs[index].touched.clone();
        self.txs[index].reverted = true;
        for (id, before, after) in touched {
            let cur = self.objs.entry(id).or_insert_with(shell);
            let keys: std::collections::BTreeSet<String> = before
                .fields
                .keys()
                .chain(after.fields.keys())
                .cloned()
                .collect();
            for k in keys {
                if before.fields.get(&k) != after.fields.get(&k) {
                    match before.fields.get(&k) {
                        Some(v) => cur.fields.insert(k, v.clone()),
                        None => cur.fields.remove(&k),
                    };
                }
            }
            if before.type_name != after.type_name {
                cur.type_name = before.type_name.clone();
            }
            if before.taint != after.taint {
                cur.taint = before.taint;
            }
            if before.sensitivity != after.sensitivity {
                cur.sensitivity = before.sensitivity;
            }
            if before.tombstone != after.tombstone {
                cur.tombstone = before.tombstone;
            }
            if before.major != after.major {
                cur.major = before.major;
            }
        }
    }
}

pub fn revert_property() {
    let mut rng = ManualRng::new(12345);
    for case in 0..10_000 {
        let mut world = MemWorld::memory(50_000 + case);
        let mut model = Naive {
            objs: BTreeMap::new(),
            txs: Vec::new(),
        };
        let ops = 3 + (rng.next_u64() % 4);
        for _ in 0..ops {
            let roll = rng.next_u64() % 5;
            if model.txs.is_empty() || roll < 3 {
                if model.objs.is_empty() || roll == 0 {
                    let id = world.mint();
                    let label = format!("n{}", rng.next_u64() % 50);
                    write_note(&mut world, id, &label);
                    // The create tx is the latest entry.
                    let tx = last_tx(&world);
                    let mut vis = shell();
                    vis.type_name = "sys/Note".into();
                    vis.major = 1;
                    vis.tombstone = false;
                    vis.fields.insert("text".into(), label);
                    model.apply(tx, vec![(id, shell(), vis)]);
                } else {
                    let keys: Vec<ObjectId> = model
                        .objs
                        .iter()
                        .filter(|(_, v)| !v.tombstone)
                        .map(|(id, _)| *id)
                        .collect();
                    if keys.is_empty() {
                        continue;
                    }
                    let id = keys[(rng.next_u64() as usize) % keys.len()];
                    let before_vis = model.objs.get(&id).cloned().unwrap_or_else(shell);
                    // Two independent attributes, so a revert of one update
                    // has to leave a later update of the other alone.
                    let field = if rng.next_u64() % 2 == 0 {
                        "text"
                    } else {
                        "tag"
                    };
                    let label = format!("u{}", rng.next_u64() % 50);
                    let mut tx = world
                        .open("main", Actor::User, "sys.object.update")
                        .unwrap();
                    let mut obj = tx.get(&id).unwrap();
                    obj.fields.insert(field.into(), Value::Text(label.clone()));
                    if rng.next_u64() % 4 == 0 {
                        obj.provenance.taint = Taint::External;
                    }
                    let taint = obj.provenance.taint;
                    tx.put(obj);
                    let committed = world.commit(tx).unwrap();
                    let mut vis = before_vis.clone();
                    vis.fields.insert(field.into(), label);
                    vis.taint = taint;
                    vis.tombstone = false;
                    model.apply(committed, vec![(id, before_vis, vis)]);
                }
            } else {
                let index = (rng.next_u64() as usize) % model.txs.len();
                let tx = model.txs[index].id;
                if model.txs[index].reverted {
                    assert_eq!(
                        world.revert("main", tx, Actor::User).unwrap_err(),
                        BranchError::AlreadyReverted,
                        "case {case}"
                    );
                } else {
                    world.revert("main", tx, Actor::User).unwrap();
                    model.revert(index);
                }
            }
            assert_same(&world, &model, case);
        }
        let folded = world.replayed().unwrap();
        assert!(world.same_visible(&folded, "main"), "case {case} replay");
    }
}

fn assert_same(world: &MemWorld, model: &Naive, case: u64) {
    let view = world.branch_view("main").unwrap();
    let mut seen = BTreeMap::new();
    view.for_each(&mut |obj| {
        seen.insert(obj.id, vis_of(obj));
    });
    for (id, vis) in &model.objs {
        let got = seen
            .get(id)
            .unwrap_or_else(|| panic!("case {case} missing {id}"));
        assert_eq!(got, vis, "case {case} object {id}");
    }
    for (id, vis) in &seen {
        let expect = model.objs.get(id).cloned().unwrap_or_else(shell);
        assert_eq!(vis, &expect, "case {case} extra {id}");
    }
}

fn last_tx(world: &MemWorld) -> ObjectId {
    world.store.entries().last().unwrap().tx
}

pub fn branch_rules() {
    let mut world = MemWorld::memory(7);
    let id = world.mint();
    write_note(&mut world, id, "main");
    let before = world.tip_seq();
    world.fork("main", "task").unwrap();
    assert!(world.tip_seq() == before + 1);
    let mut tx = world
        .open("task", Actor::User, "sys.object.update")
        .unwrap();
    let mut obj = tx.get(&id).unwrap();
    obj.fields
        .insert("text".into(), Value::Text("branch".into()));
    tx.put(obj);
    world.commit(tx).unwrap();
    assert_eq!(
        world.get("main", &id).unwrap().fields.get("text"),
        Some(&Value::Text("main".into()))
    );
    let other = world.mint();
    write_note(&mut world, other, "on-main");
    world.rebase("task").unwrap();
    assert_eq!(
        world.get("task", &other).unwrap().fields.get("text"),
        Some(&Value::Text("on-main".into()))
    );
    assert_eq!(
        world.get("task", &id).unwrap().fields.get("text"),
        Some(&Value::Text("branch".into()))
    );
    let mut tx = world
        .open("main", Actor::User, "sys.object.update")
        .unwrap();
    let mut obj = tx.get(&id).unwrap();
    obj.fields
        .insert("text".into(), Value::Text("raced".into()));
    tx.put(obj);
    world.commit(tx).unwrap();
    let mut world2 = MemWorld::memory(8);
    let id2 = world2.mint();
    write_note(&mut world2, id2, "base");
    world2.fork("main", "task").unwrap();
    let mut tx = world2
        .open("task", Actor::User, "sys.object.update")
        .unwrap();
    let mut obj = tx.get(&id2).unwrap();
    obj.fields.insert("text".into(), Value::Text("task".into()));
    tx.put(obj);
    world2.commit(tx).unwrap();
    let mut tx = world2
        .open("main", Actor::User, "sys.object.update")
        .unwrap();
    let mut obj = tx.get(&id2).unwrap();
    obj.fields.insert("text".into(), Value::Text("main".into()));
    tx.put(obj);
    world2.commit(tx).unwrap();
    assert_eq!(world2.rebase("task").unwrap_err(), BranchError::Conflict);

    world2.fork("main", "ok").unwrap();
    let id3 = world2.mint();
    let mut tx = world2.open("ok", Actor::User, "sys.object.create").unwrap();
    let mut obj = Object::new(id3, "sys/Note", 1, Actor::User);
    obj.fields.insert("text".into(), Value::Text("n".into()));
    obj.fields.insert("n".into(), Value::Int(0));
    tx.inputs.insert("row".into(), Value::Ref(id3));
    tx.preconditions.push(Predicate::Cmp {
        field: "row.n".into(),
        op: CmpOp::Gt,
        value: Value::Int(0),
    });
    tx.put(obj);
    world2.commit(tx).unwrap();
    let err = world2.submit("ok", Actor::User).unwrap_err();
    assert!(matches!(err, BranchError::Precondition(_)));
    assert!(matches!(
        world2.status("ok").unwrap(),
        BranchStatus::PendingReview { .. }
    ));
    assert!(world2.get("main", &id3).is_none());
}

pub fn capability_rules() {
    let seed = [5u8; 32];
    let pk = crypto::public_uncompressed(&seed).unwrap();
    let parent_id = ObjectId::from_parts(3, 1);
    let child_id = ObjectId::from_parts(3, 2);
    let mut parent = Capability {
        id: parent_id,
        subject: Actor::User,
        rights: vec!["ml.dataset.*".into(), "deliver:*".into()],
        object_scope: BTreeMap::from([("dataset".into(), vec![ObjectId::from_parts(9, 1)])]),
        limits: Limits {
            cpu_ms: 100,
            gpu_ms: 0,
            money_fen: 10,
            egress_channels: vec!["model:*".into()],
        },
        expires_at_ms: 1_000,
        parent: None,
        signature: [0u8; 64],
    };
    sign_token(&mut parent, &seed).unwrap();
    verify_token(&parent, &pk).unwrap();
    let mut child = parent.clone();
    child.id = child_id;
    child.subject = Actor::AgentTask(child_id);
    child.rights = vec!["ml.dataset.read".into()];
    child.limits.cpu_ms = 40;
    child.limits.egress_channels = vec!["model:default".into()];
    child.expires_at_ms = 500;
    let child = attenuate(&parent, child, &Actor::User, &seed).unwrap();
    assert!(covers_right(&child.rights, "ml.dataset.read"));
    assert!(!covers_right(&child.rights, "ml.other.write"));
    let spent = Spent::default();
    let delta = LedgerOp {
        cap: child.id,
        cpu_ms: 40,
        gpu_ms: 0,
        money_fen: 0,
    };
    check_call(&child, &pk, 100, "ml.dataset.read", &spent, &delta).unwrap();
    assert!(check_call(&child, &pk, 100, "deliver:minio", &spent, &delta).is_err());
    assert!(check_call(&child, &pk, 500, "ml.dataset.read", &spent, &delta).is_err());
    let spent = Spent {
        cpu_ms: 40,
        ..Spent::default()
    };
    let over = LedgerOp { cpu_ms: 1, ..delta };
    assert!(check_call(&child, &pk, 100, "ml.dataset.read", &spent, &over).is_err());
    let mut wider = child.clone();
    wider.id = ObjectId::from_parts(3, 3);
    wider.subject = Actor::AgentTask(wider.id);
    wider.rights = vec!["*".into()];
    assert!(attenuate(&child, wider, &child.subject, &seed).is_err());
}

pub fn policy_priority() {
    let policy = Policy {
        name: "nightly-ml".into(),
        default: Decision::Confirm,
        rules: vec![
            Rule {
                effect: Decision::Allow,
                channel: "model:default".into(),
                max_sensitivity: Some(Sensitivity::Internal),
                max_cost_fen_per_day: None,
                when: None,
            },
            Rule {
                effect: Decision::Confirm,
                channel: "deliver:*".into(),
                max_sensitivity: None,
                max_cost_fen_per_day: None,
                when: None,
            },
            Rule {
                effect: Decision::Deny,
                channel: "*".into(),
                max_sensitivity: Some(Sensitivity::Secret),
                max_cost_fen_per_day: None,
                when: None,
            },
        ],
    };
    let facts = |channel: &str, sensitivity: Sensitivity, tainted: bool| IntentFacts {
        channel: channel.into(),
        sensitivity,
        cost_fen: 0,
        day_spent_fen: 0,
        recipient_tainted: tainted,
        channel_tainted: false,
        when_value: None,
    };
    assert_eq!(
        evaluate(
            &policy,
            &facts("model:default", Sensitivity::Internal, false)
        ),
        Decision::Allow
    );
    assert_eq!(
        evaluate(&policy, &facts("model:default", Sensitivity::Secret, false)),
        Decision::Deny
    );
    assert_eq!(
        evaluate(
            &policy,
            &facts("deliver:minio", Sensitivity::Private, false)
        ),
        Decision::Confirm
    );
    assert_eq!(
        evaluate(&policy, &facts("notify:email", Sensitivity::Private, false)),
        Decision::Confirm
    );
    assert_eq!(
        evaluate(
            &policy,
            &facts("model:default", Sensitivity::Internal, true)
        ),
        Decision::Confirm
    );
}

#[cfg(feature = "dev-confirmer")]
pub fn egress_gate() {
    let seed = [42u8; 32];
    let pk = dev::public_key(&seed).unwrap();
    let payload = b"payload-v1";
    let hash = sha256(payload);
    let req = ApprovalRequest {
        request_id: ObjectId::from_parts(8, 8),
        created_at_ms: 1_000,
        expires_at_ms: 5_000,
        nonce: [7u8; 16],
        kind: Kind::Egress,
        summary: vec![SummaryLine {
            label: "what".into(),
            value: "deliver".into(),
        }],
        items: vec![ApprovalItem {
            channel: "deliver:test".into(),
            target: "box".into(),
            payload_hash: hash,
            cost_fen: 0,
            external_reversibility: "irreversible".into(),
            tainted: false,
        }],
        branch: "task".into(),
        diff_hash: [1u8; 32],
    };
    let bytes = encode_request(&req).unwrap();
    let sig = dev::sign_decision(&seed, &bytes, "approve").unwrap();
    let os_seed = [9u8; 32];
    let os_sig = aos_confirm::sign_request(&os_seed, &bytes).unwrap();
    let os_pk = crypto::public_uncompressed(&os_seed).unwrap();
    assert!(aos_confirm::verify_request(&os_pk, &bytes, &os_sig));
    verify_decision(&pk, &bytes, "approve", &sig).unwrap();

    let permit = Permit::Approval {
        request_bytes: bytes.clone(),
        decision: "approve".into(),
        signature: sig.to_vec(),
    };
    // Every call checks that a rejected send never reaches the transport.
    let gate = |replay: &mut aos_confirm::MemoryReplay,
                trust: &[u8],
                now: u64,
                channel: &str,
                target: &str,
                body: &[u8],
                expected: &[u8; 32],
                permit: Option<&Permit>| {
        let mut transport = RecordingTransport::default();
        let out = execute_gate(
            &mut transport,
            replay,
            trust,
            now,
            channel,
            target,
            body,
            expected,
            permit,
        );
        match &out {
            Ok(_) => assert_eq!(transport.sent.len(), 1),
            Err(_) => assert!(transport.sent.is_empty()),
        }
        out
    };
    let mut replay = aos_confirm::MemoryReplay::default();
    let no_permit = gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:test",
        "box",
        payload,
        &[0u8; 32],
        None,
    );
    assert_eq!(no_permit.unwrap_err(), GateError::NoPermit);

    // "Reviewed A, sent B": the bytes must hash to what the confirmer signed.
    let tampered = gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:test",
        "box",
        b"tampered",
        &hash,
        Some(&permit),
    );
    assert_eq!(tampered.unwrap_err(), GateError::Tamper);
    let other_hash = sha256(b"tampered");
    let swapped = gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:test",
        "box",
        b"tampered",
        &other_hash,
        Some(&permit),
    );
    assert_eq!(swapped.unwrap_err(), GateError::Tamper);

    // The approval names one channel and one target; it covers no other.
    let elsewhere = gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:evil",
        "box",
        payload,
        &hash,
        Some(&permit),
    );
    assert_eq!(elsewhere.unwrap_err(), GateError::Tamper);
    let redirected = gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:test",
        "attacker",
        payload,
        &hash,
        Some(&permit),
    );
    assert_eq!(redirected.unwrap_err(), GateError::Tamper);

    // A signature from any key other than the paired confirmer is worthless,
    // even a valid one over the same bytes.
    let rogue = [66u8; 32];
    let rogue_pk = dev::public_key(&rogue).unwrap();
    let self_signed = Permit::Approval {
        request_bytes: bytes.clone(),
        decision: "approve".into(),
        signature: dev::sign_decision(&rogue, &bytes, "approve")
            .unwrap()
            .to_vec(),
    };
    let forged = gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:test",
        "box",
        payload,
        &hash,
        Some(&self_signed),
    );
    assert!(matches!(forged.unwrap_err(), GateError::Confirm(_)));
    let _ = rogue_pk;

    // A failed check does not burn the nonce; the real send still goes through once.
    gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:test",
        "box",
        payload,
        &hash,
        Some(&permit),
    )
    .unwrap();
    let again = gate(
        &mut replay,
        &pk,
        1_500,
        "deliver:test",
        "box",
        payload,
        &hash,
        Some(&permit),
    );
    assert_eq!(again.unwrap_err(), GateError::Replay);

    // Expiry is the signed `expires_at`, not a field the caller supplies.
    let mut fresh = aos_confirm::MemoryReplay::default();
    let late = gate(
        &mut fresh,
        &pk,
        5_000,
        "deliver:test",
        "box",
        payload,
        &hash,
        Some(&permit),
    );
    assert_eq!(late.unwrap_err(), GateError::Expired);

    // A deny decision never sends.
    let deny = Permit::Approval {
        request_bytes: bytes.clone(),
        decision: "deny".into(),
        signature: dev::sign_decision(&seed, &bytes, "deny").unwrap().to_vec(),
    };
    let denied = gate(
        &mut fresh,
        &pk,
        1_500,
        "deliver:test",
        "box",
        payload,
        &hash,
        Some(&deny),
    );
    assert_eq!(denied.unwrap_err(), GateError::Denied);

    // One approval with two identical items allows exactly two sends.
    let mut twice = req.clone();
    twice.nonce = [8u8; 16];
    twice.items.push(twice.items[0].clone());
    let twice_bytes = encode_request(&twice).unwrap();
    let twice_permit = Permit::Approval {
        request_bytes: twice_bytes.clone(),
        decision: "approve".into(),
        signature: dev::sign_decision(&seed, &twice_bytes, "approve")
            .unwrap()
            .to_vec(),
    };
    let mut r2 = aos_confirm::MemoryReplay::default();
    for _ in 0..2 {
        gate(
            &mut r2,
            &pk,
            1_500,
            "deliver:test",
            "box",
            payload,
            &hash,
            Some(&twice_permit),
        )
        .unwrap();
    }
    let third = gate(
        &mut r2,
        &pk,
        1_500,
        "deliver:test",
        "box",
        payload,
        &hash,
        Some(&twice_permit),
    );
    assert_eq!(third.unwrap_err(), GateError::Replay);

    // Standing policies: signed by the confirmer, then evaluated for this send.
    let policy = Policy {
        name: "allow-fetch".into(),
        default: Decision::Confirm,
        rules: vec![Rule {
            effect: Decision::Allow,
            channel: "fetch:registered-sources".into(),
            max_sensitivity: None,
            max_cost_fen_per_day: None,
            when: None,
        }],
    };
    let psig = sign_policy(&policy, &seed).unwrap();
    assert!(verify_policy(&policy, &pk, &psig));
    let facts = |channel: &str, tainted: bool| IntentFacts {
        channel: channel.into(),
        sensitivity: Sensitivity::Internal,
        cost_fen: 0,
        day_spent_fen: 0,
        recipient_tainted: tainted,
        channel_tainted: false,
        when_value: None,
    };
    let allow = |channel: &str, tainted: bool| Permit::Allow {
        policy: policy.clone(),
        signature: psig.to_vec(),
        facts: facts(channel, tainted),
    };
    let mut r3 = aos_confirm::MemoryReplay::default();
    let fetch = allow("fetch:registered-sources", false);
    gate(
        &mut r3,
        &pk,
        1,
        "fetch:registered-sources",
        "src",
        payload,
        &hash,
        Some(&fetch),
    )
    .unwrap();
    // The same signed policy says nothing about other channels.
    let off_policy = allow("deliver:customer", false);
    let blocked = gate(
        &mut r3,
        &pk,
        1,
        "deliver:customer",
        "x",
        payload,
        &hash,
        Some(&off_policy),
    );
    assert_eq!(blocked.unwrap_err(), GateError::NoPermit);
    // Facts for one channel cannot be replayed against another.
    let mismatched = gate(
        &mut r3,
        &pk,
        1,
        "deliver:customer",
        "x",
        payload,
        &hash,
        Some(&fetch),
    );
    assert_eq!(mismatched.unwrap_err(), GateError::Tamper);
    // Tainted lineage turns allow into confirm.
    let tainted = allow("fetch:registered-sources", true);
    let held = gate(
        &mut r3,
        &pk,
        1,
        "fetch:registered-sources",
        "src",
        payload,
        &hash,
        Some(&tainted),
    );
    assert_eq!(held.unwrap_err(), GateError::NoPermit);
    // A policy signed by some other key is rejected.
    let rogue_policy = Permit::Allow {
        policy: policy.clone(),
        signature: sign_policy(&policy, &rogue).unwrap().to_vec(),
        facts: facts("fetch:registered-sources", false),
    };
    let rogue_sent = gate(
        &mut r3,
        &pk,
        1,
        "fetch:registered-sources",
        "src",
        payload,
        &hash,
        Some(&rogue_policy),
    );
    assert_eq!(rogue_sent.unwrap_err(), GateError::Policy);
}

pub fn taint_and_recipient() {
    assert_eq!(
        propagate_taint(&[Taint::Clean, Taint::External]),
        Taint::External
    );
    let mut world = MemWorld::memory(15);
    let parent = world.mint();
    let mut tx = world
        .open("main", Actor::Adapter("src".into()), "sys.object.create")
        .unwrap();
    let mut obj = Object::new(parent, "sys/Note", 1, Actor::Adapter("src".into()));
    obj.fields
        .insert("text".into(), Value::Text("from-outside".into()));
    obj.provenance.taint = Taint::External;
    obj.provenance.origin = "fetch:src".into();
    tx.put(obj);
    world.commit(tx).unwrap();
    let child = world.mint();
    let mut tx = world
        .open("main", Actor::System, "sys.object.create")
        .unwrap();
    let mut obj = Object::new(child, "sys/Note", 1, Actor::System);
    obj.fields
        .insert("text".into(), Value::Text("derived".into()));
    obj.provenance.derived_from.push((parent, 1));
    let parent_taint = world.get("main", &parent).unwrap().provenance.taint;
    obj.provenance.taint = propagate_taint(&[parent_taint]);
    tx.put(obj);
    world.commit(tx).unwrap();
    assert_eq!(
        world.get("main", &child).unwrap().provenance.taint,
        Taint::External
    );

    let receipt = ingress_receipt(world.mint(), "fetch:src", parent, true);
    assert_eq!(receipt.provenance.taint, Taint::External);

    let seed = [6u8; 32];
    let pk = crypto::public_uncompressed(&seed).unwrap();
    let cap_id = world.mint();
    let mut cap = Capability {
        id: cap_id,
        subject: Actor::User,
        rights: vec!["sys.object.write".into()],
        object_scope: BTreeMap::new(),
        limits: Limits {
            cpu_ms: 10,
            gpu_ms: 0,
            money_fen: 0,
            egress_channels: vec!["*".into()],
        },
        expires_at_ms: u64::MAX,
        parent: None,
        signature: [0u8; 64],
    };
    sign_token(&mut cap, &seed).unwrap();
    let intent_id = world.mint();
    invoke(
        &mut world,
        Invoke {
            branch: "main",
            actor: Actor::User,
            cap: &cap,
            cap_pubkey: &pk,
            decl_name: "sys.intent.create",
            decl_capability: "sys.object.write",
            reads: &["sys/Note", "sys/EgressIntent"],
            writes: &["sys/EgressIntent"],
            reversibility: "undo",
            egress: &["deliver:*"],
            inputs: BTreeMap::new(),
            preconditions: Vec::new(),
            delta_cpu_ms: 0,
            idem_key: None,
            idem_parts: Vec::new(),
        },
        |host| {
            host.read(&parent)?;
            host.create_intent(
                intent_id,
                "deliver:customer",
                "person",
                Some(parent),
                aos_types::ContentHash([2u8; 32]),
                "irreversible",
                0,
                Actor::User,
            )?;
            Ok(Value::Null)
        },
    )
    .unwrap();
    let intent = world.get("main", &intent_id).unwrap();
    assert_eq!(
        intent.fields.get("lineage_tainted"),
        Some(&Value::Bool(true))
    );
    let facts = facts_from_intent(&intent, 0);
    let policy = Policy {
        name: "default".into(),
        default: Decision::Confirm,
        rules: vec![Rule {
            effect: Decision::Allow,
            channel: "deliver:*".into(),
            max_sensitivity: Some(Sensitivity::Secret),
            max_cost_fen_per_day: None,
            when: None,
        }],
    };
    assert_eq!(evaluate(&policy, &facts), Decision::Confirm);
}

pub fn action_sandbox_checks() {
    assert_eq!(IMPORTS.len(), 9);
    assert_eq!(ABI_VERSION, 1);
    assert!(check_type_allowed(&["sys/Note".into()], "sys/Fact").is_err());
    assert!(transition(JobState::Queued, JobState::Running).is_ok());
    assert!(check_remote_job("remote", "compensate", true).is_ok());
    assert!(check_remote_job("remote", "undo", false).is_err());
    let mut world = MemWorld::memory(21);
    let err = reject_foreign_import(
        &mut world,
        "main",
        &Actor::System,
        "wasi_snapshot_preview1",
        "fd_write",
    )
    .unwrap_err();
    assert!(matches!(err, aos_action::ActionError::DirectIo));
    let mut found = false;
    world.branch_view("main").unwrap().for_each(&mut |obj| {
        if obj.type_name == "sys/ActionViolation" {
            found = true;
        }
    });
    assert!(found);
}

pub fn query_page_and_refs() {
    let json = r#"{
      "from": "sys/Note",
      "where": {"field": "about", "eq": {"ref": "00000000000000000000000000"}},
      "select": ["text"],
      "limit": 1,
      "aggregate": {"count": true, "histogram": "text", "mean": "n"}
    }"#;
    let parsed = parse_query(json).unwrap();
    assert_eq!(parsed.effective_limit(), 1);
    let mut world = MemWorld::memory(22);
    let target = ObjectId::ZERO;
    let mut tx = world
        .open("main", Actor::User, "sys.object.create")
        .unwrap();
    let mut obj = Object::new(target, "sys/Note", 1, Actor::User);
    obj.fields.insert("text".into(), Value::Text("root".into()));
    obj.fields.insert("n".into(), Value::Int(2));
    tx.put(obj);
    world.commit(tx).unwrap();
    for i in 0..3 {
        let id = world.mint();
        let mut tx = world
            .open("main", Actor::User, "sys.object.create")
            .unwrap();
        let mut obj = Object::new(id, "sys/Note", 1, Actor::User);
        obj.fields.insert(
            "text".into(),
            Value::Text(if i == 2 { "other" } else { "root" }.into()),
        );
        obj.fields.insert("n".into(), Value::Int(4));
        obj.fields.insert("about".into(), Value::Ref(target));
        tx.put(obj);
        world.commit(tx).unwrap();
    }
    let view = world.branch_view("main").unwrap();
    let page = aos_query::execute(&view, &parsed).unwrap();
    assert_eq!(page.count, Some(3));
    assert_eq!(page.rows.len(), 1);
    assert!(page.next_cursor.is_some());
    assert!(page.mean.is_some());
    let mut full = parsed.clone();
    full.limit = Some(10);
    full.cursor = None;
    let all = aos_query::execute(&view, &full).unwrap();
    assert_eq!(all.rows.len(), 3);
    let diff_world = {
        let mut w = MemWorld::memory(23);
        w.fork("main", "task").unwrap();
        let id = w.mint();
        let mut tx = w.open("task", Actor::User, "sys.object.create").unwrap();
        let mut obj = Object::new(id, "sys/Note", 1, Actor::User);
        obj.fields.insert("text".into(), Value::Text("d".into()));
        tx.put(obj);
        w.commit(tx).unwrap();
        let report = w.diff("task").unwrap();
        assert_eq!(report.by_type.get("sys/Note").unwrap().created, 1);
        let page = w
            .drill("task", "sys/Note", Query::of_type("sys/Note"))
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        w
    };
    let _ = diff_world;
}

pub fn schemas_match_builtins() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/sys");
    let mut types = Vec::new();
    let mut actions = Vec::new();
    for entry in std::fs::read_dir(&root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        if let Some(rest) = text.split("\"type\"").nth(1) {
            let value = rest.split('"').nth(1).unwrap();
            types.push(value.to_string());
        } else if let Some(rest) = text.split("\"action\"").nth(1) {
            let value = rest.split('"').nth(1).unwrap();
            actions.push(value.to_string());
            assert!(text.contains("\"undo\""));
        } else {
            panic!("schema {name} has no type or action");
        }
    }
    let builtin: Vec<_> = aos_types::builtin_types()
        .into_iter()
        .map(|t| t.name)
        .collect();
    types.sort();
    let mut expect = builtin.clone();
    expect.sort();
    assert_eq!(types, expect);
    let builtin_actions: Vec<_> = aos_types::builtin_actions()
        .into_iter()
        .map(|a| a.name)
        .collect();
    actions.sort();
    let mut expect_a = builtin_actions;
    expect_a.sort();
    assert_eq!(actions, expect_a);
}

#[cfg(feature = "dev-confirmer")]
pub fn committed_vectors() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors");
    let map = std::fs::read_to_string(root.join("cbor/map-ab.hex")).unwrap();
    let expected = aos_types::hex_decode(map.trim()).unwrap();
    let encoded = aos_types::Cbor::Map(vec![
        (aos_types::Cbor::Text("b".into()), aos_types::Cbor::Uint(1)),
        (aos_types::Cbor::Text("a".into()), aos_types::Cbor::Uint(2)),
    ])
    .canonical()
    .unwrap();
    assert_eq!(encoded, expected);
    let empty = aos_types::sha256(b"");
    let sha_file = std::fs::read_to_string(root.join("cbor/sha256-empty.hex")).unwrap();
    assert_eq!(aos_types::hex_encode(&empty), sha_file.trim());
    let request = std::fs::read_to_string(root.join("approval/request.hex")).unwrap();
    let sig = std::fs::read_to_string(root.join("approval/approve.sig.hex")).unwrap();
    let request = aos_types::hex_decode(request.trim()).unwrap();
    let sig = aos_types::hex_decode(sig.trim()).unwrap();
    let pk = dev::public_key(&[42u8; 32]).unwrap();
    verify_decision(&pk, &request, "approve", &sig).unwrap();
}

pub fn human_edit_uses_sys_action() {
    let mut world = MemWorld::memory(24);
    let id = world.mint();
    write_note(&mut world, id, "edited");
    let entry = world.store.entries().pop().unwrap();
    assert_eq!(entry.action, "sys.object.create");
    assert_eq!(entry.actor, Actor::User);
}

fn write_note(world: &mut MemWorld, id: ObjectId, text: &str) {
    let mut tx = world
        .open("main", Actor::User, "sys.object.create")
        .unwrap();
    let mut obj = Object::new(id, "sys/Note", 1, Actor::User);
    obj.fields.insert("text".into(), Value::Text(text.into()));
    tx.put(obj);
    world.commit(tx).unwrap();
}

/// Marked performance probes. They are ignored by default so correctness CI
/// does not depend on a quiet machine. Run with
/// `cargo test -p aos-conformance --release --no-default-features --test perf -- --ignored`;
/// `dev-confirmer` (on by default here) refuses to compile in release.
pub fn perf_fork_and_commit() -> (u128, u128, u128) {
    let mut world = MemWorld::memory(42);
    let started = std::time::Instant::now();
    for i in 0..10_000 {
        let id = ObjectId::from_parts(i, 1);
        write_note(&mut world, id, "x");
    }
    let commit_ms = started.elapsed().as_millis();
    let fork_at = std::time::Instant::now();
    world.fork("main", "perf").unwrap();
    let fork_ms = fork_at.elapsed().as_millis();
    let tx = last_tx(&world);
    // Revert the fork marker is not a 10k write. Revert the last note instead
    // after building one transaction that rewrites 10k fields.
    let mut big = world
        .open("perf", Actor::User, "sys.object.update")
        .unwrap();
    for i in 0..10_000 {
        let id = ObjectId::from_parts(i, 1);
        let mut obj = big.get(&id).unwrap();
        obj.fields.insert("text".into(), Value::Text("y".into()));
        big.put(obj);
    }
    let big_at = std::time::Instant::now();
    let big_tx = world.commit(big).unwrap();
    let big_ms = big_at.elapsed().as_millis();
    let rev_at = std::time::Instant::now();
    world.revert("perf", big_tx, Actor::User).unwrap();
    let rev_ms = rev_at.elapsed().as_millis();
    let _ = (tx, commit_ms);
    (fork_ms, big_ms, rev_ms)
}

pub fn perf_summary(n: u64) -> u128 {
    let mut world = MemWorld::memory(99);
    world.fork("main", "task").unwrap();
    let mut done = 0u64;
    while done < n {
        let end = (done + 2_000).min(n);
        let mut tx = world
            .open("task", Actor::User, "sys.object.create")
            .unwrap();
        for i in done..end {
            let id = ObjectId::from_parts(i, 7);
            let mut obj = Object::new(id, "ml/Sample", 1, Actor::User);
            obj.fields.insert("i".into(), Value::Int(i as i64));
            tx.put(obj);
        }
        world.commit(tx).unwrap();
        done = end;
    }
    let at = std::time::Instant::now();
    let report = world.diff("task").unwrap();
    let ms = at.elapsed().as_millis();
    assert_eq!(report.by_type.get("ml/Sample").unwrap().created, n);
    ms
}
