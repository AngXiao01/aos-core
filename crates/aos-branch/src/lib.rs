//! Branches, rebase, revert, and change summaries over the hash-chained log.
//!
//! Forking clones the persistent map root. It does not copy objects.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use aos_log::{
    referenced_blobs, verify_chain, BlobStore, Hamt, LedgerOp, LogEntry, LogStore, MemoryBlob,
    MemoryLog, WriteRecord,
};
use aos_query::{execute, Page};
use aos_types::{
    absent_shell, apply_patch, diff_objects, invert_patch, Actor, Clock, IdGen, ManualClock,
    ManualRng, Object, ObjectId, ObjectView, Predicate, Query, RefEdge, Rng, Value,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchError {
    MissingBranch,
    Exists,
    Conflict,
    Precondition(String),
    NotFound,
    Referenced,
    Patch,
    Store,
    BadEntry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchStatus {
    Active,
    PendingReview { reason: String },
    Submitted,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaSpent {
    pub cpu_ms: i64,
    pub gpu_ms: i64,
    pub money_fen: i64,
}

#[derive(Clone, Debug)]
struct IdemRec {
    input: [u8; 32],
    result: Value,
}

#[derive(Clone)]
struct BranchState {
    root: Hamt<ObjectId, Object>,
    rev: Hamt<ObjectId, Vec<RefEdge>>,
    changed: Hamt<ObjectId, ()>,
    base_root: Hamt<ObjectId, Object>,
    idem: Hamt<String, IdemRec>,
    status: BranchStatus,
    forked_from: String,
    fork_seq: u64,
}

impl BranchState {
    fn new() -> Self {
        Self {
            root: Hamt::new(),
            rev: Hamt::new(),
            changed: Hamt::new(),
            base_root: Hamt::new(),
            idem: Hamt::new(),
            status: BranchStatus::Active,
            forked_from: String::new(),
            fork_seq: 0,
        }
    }
}

pub struct World<S, B, C, R> {
    pub store: S,
    pub blobs: B,
    pub clock: C,
    pub rng: R,
    branches: BTreeMap<String, BranchState>,
    spent: BTreeMap<ObjectId, QuotaSpent>,
    nonces: BTreeSet<Vec<u8>>,
    ids: IdGen,
}

pub type MemWorld = World<MemoryLog, MemoryBlob, ManualClock, ManualRng>;

impl MemWorld {
    pub fn memory(seed: u64) -> Self {
        Self::new(
            MemoryLog::default(),
            MemoryBlob::default(),
            ManualClock::new(1_700_000_000_000),
            ManualRng::new(seed),
        )
    }
}

impl<S, B, C, R> World<S, B, C, R>
where
    S: LogStore,
    B: BlobStore,
    C: Clock,
    R: Rng,
{
    pub fn new(store: S, blobs: B, clock: C, rng: R) -> Self {
        let mut branches = BTreeMap::new();
        branches.insert("main".into(), BranchState::new());
        Self {
            store,
            blobs,
            clock,
            rng,
            branches,
            spent: BTreeMap::new(),
            nonces: BTreeSet::new(),
            ids: IdGen::new(),
        }
    }

    pub fn mint(&mut self) -> ObjectId {
        let ms = self.clock.now_ms();
        let rnd = self.rng.next_u128();
        self.ids.next(ms, rnd)
    }

    pub fn tip_seq(&self) -> u64 {
        self.store.len() as u64
    }

    pub fn spent(&self, cap: &ObjectId) -> QuotaSpent {
        self.spent.get(cap).cloned().unwrap_or_default()
    }

    pub fn nonce_seen(&self, nonce: &[u8]) -> bool {
        self.nonces.contains(nonce)
    }

    pub fn status(&self, branch: &str) -> Result<&BranchStatus, BranchError> {
        Ok(&self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?
            .status)
    }

    pub fn get(&self, branch: &str, id: &ObjectId) -> Option<Object> {
        self.branches.get(branch)?.root.get(id).cloned()
    }

    pub fn fork(&mut self, from: &str, name: &str) -> Result<(), BranchError> {
        if name == "main" || self.branches.contains_key(name) {
            return Err(BranchError::Exists);
        }
        if !self.branches.contains_key(from) {
            return Err(BranchError::MissingBranch);
        }
        let mut inputs = BTreeMap::new();
        inputs.insert("from".into(), Value::Text(from.into()));
        inputs.insert("name".into(), Value::Text(name.into()));
        inputs.insert("at_seq".into(), Value::Int(self.tip_seq() as i64));
        self.append(
            name,
            Actor::System,
            "sys.branch.fork",
            inputs,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    pub fn open(&self, branch: &str, actor: Actor, action: &str) -> Result<OpenTx, BranchError> {
        let state = self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?;
        Ok(OpenTx {
            branch: branch.into(),
            actor,
            action: action.into(),
            capability: None,
            inputs: BTreeMap::new(),
            preconditions: Vec::new(),
            ledger: Vec::new(),
            intents: Vec::new(),
            snap: state.root.clone(),
            pending: BTreeMap::new(),
            reads: Vec::new(),
            tainted_reads: Vec::new(),
            idem_key: None,
            idem_input: None,
            failed: None,
            snap_seq: self.tip_seq(),
        })
    }

    pub fn commit(&mut self, tx: OpenTx) -> Result<ObjectId, BranchError> {
        if let Some(reason) = &tx.failed {
            return Err(BranchError::Precondition(reason.clone()));
        }
        let state = self
            .branches
            .get(&tx.branch)
            .ok_or(BranchError::MissingBranch)?;
        for id in tx.pending.keys() {
            let snap_v = tx.snap.get(id).map(|o| o.version).unwrap_or(0);
            let cur_v = state.root.get(id).map(|o| o.version).unwrap_or(0);
            let snap_has = tx.snap.get(id).is_some();
            let cur_has = state.root.get(id).is_some();
            if snap_v != cur_v || snap_has != cur_has {
                return Err(BranchError::Conflict);
            }
        }
        let tx_id = {
            let ms = self.clock.now_ms();
            let rnd = self.rng.next_u128();
            self.ids.next(ms, rnd)
        };
        let mut writes = Vec::new();
        for (id, mut after) in tx.pending {
            let before = tx
                .snap
                .get(&id)
                .cloned()
                .unwrap_or_else(|| absent_shell(id));
            let prev = tx.snap.get(&id).map(|o| o.version).unwrap_or(0);
            after.version = prev + 1;
            after.provenance.tx = Some(tx_id);
            after.provenance.actor = tx.actor.clone();
            let patch = diff_objects(&before, &after);
            let inverse = invert_patch(&before, &patch);
            let mut check = before.clone();
            apply_patch(&mut check, &patch).map_err(|_| BranchError::Patch)?;
            if check.fields != after.fields || check.tombstone != after.tombstone {
                return Err(BranchError::Patch);
            }
            writes.push(WriteRecord {
                id,
                prev_version: prev,
                new_version: after.version,
                patch,
                inverse,
                before,
                after,
            });
        }
        self.append(
            &tx.branch,
            tx.actor,
            &tx.action,
            tx.inputs,
            writes,
            tx.intents,
            tx.ledger,
            tx.preconditions,
            tx.idem_key.map(|k| (k, tx.idem_input.unwrap_or([0u8; 32]))),
        )?;
        let _ = tx.reads;
        let _ = tx.tainted_reads;
        let _ = tx.snap_seq;
        Ok(tx_id)
    }

    pub fn revert(
        &mut self,
        branch: &str,
        tx: ObjectId,
        actor: Actor,
    ) -> Result<ObjectId, BranchError> {
        let entry = self.find_tx(branch, tx).ok_or(BranchError::NotFound)?;
        let state = self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?;
        let mut writes = Vec::new();
        let new_tx = {
            let ms = self.clock.now_ms();
            let rnd = self.rng.next_u128();
            self.ids.next(ms, rnd)
        };
        for w in &entry.writes {
            let current = state
                .root
                .get(&w.id)
                .cloned()
                .unwrap_or_else(|| absent_shell(w.id));
            let prev = state.root.get(&w.id).map(|o| o.version).unwrap_or(0);
            let mut restored = w.before.clone();
            restored.version = prev + 1;
            restored.provenance.tx = Some(new_tx);
            restored.provenance.actor = actor.clone();
            let patch = diff_objects(&current, &restored);
            let inverse = invert_patch(&current, &patch);
            writes.push(WriteRecord {
                id: w.id,
                prev_version: prev,
                new_version: restored.version,
                patch,
                inverse,
                before: current,
                after: restored,
            });
        }
        let ledger = entry.ledger.iter().map(LedgerOp::negated).collect();
        let mut inputs = BTreeMap::new();
        inputs.insert("reverts".into(), Value::Text(tx.to_text()));
        self.append(
            branch,
            actor,
            "sys.revert",
            inputs,
            writes,
            Vec::new(),
            ledger,
            Vec::new(),
            None,
        )?;
        Ok(new_tx)
    }

    pub fn rebase(&mut self, branch: &str) -> Result<(), BranchError> {
        if branch == "main" {
            return Ok(());
        }
        let state = self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?
            .clone();
        let main = self
            .branches
            .get("main")
            .ok_or(BranchError::MissingBranch)?
            .clone();
        let mut writes = Vec::new();
        let new_tx = {
            let ms = self.clock.now_ms();
            let rnd = self.rng.next_u128();
            self.ids.next(ms, rnd)
        };
        let changed = state.changed.keys();
        for id in &changed {
            let Some(cur) = state.root.get(id) else {
                continue;
            };
            let at_fork = state.base_root.get(id);
            let on_main = main.root.get(id);
            if at_fork.is_none() {
                if on_main.is_some() {
                    return Err(BranchError::Conflict);
                }
            } else {
                let fork_ver = at_fork.map(|o| o.version).unwrap_or(0);
                let main_ver = on_main.map(|o| o.version).unwrap_or(0);
                if main_ver != fork_ver {
                    return Err(BranchError::Conflict);
                }
            }
            let prev = on_main.map(|o| o.version).unwrap_or(0);
            let mut after = cur.clone();
            after.version = prev + 1;
            after.provenance.tx = Some(new_tx);
            let before = on_main.cloned().unwrap_or_else(|| absent_shell(*id));
            let patch = diff_objects(&before, &after);
            let inverse = invert_patch(&before, &patch);
            writes.push(WriteRecord {
                id: *id,
                prev_version: prev,
                new_version: after.version,
                patch,
                inverse,
                before,
                after,
            });
        }
        self.append(
            branch,
            Actor::System,
            "sys.branch.rebase",
            BTreeMap::new(),
            writes,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    pub fn preconditions_hold(&self, branch: &str) -> Result<(), BranchError> {
        let n = self.store.len();
        for i in 0..n {
            let entry = self.store.get(i).ok_or(BranchError::BadEntry)?;
            if entry.branch != branch || entry.preconditions.is_empty() {
                continue;
            }
            let ok = entry.preconditions.iter().all(|p| {
                aos_types::eval_predicate(p, &mut |path| {
                    aos_types::resolve_input_path(path, &entry.inputs, &mut |id| {
                        self.get(branch, id)
                    })
                })
            });
            if !ok {
                return Err(BranchError::Precondition("precondition failed".into()));
            }
        }
        Ok(())
    }

    pub fn mark_pending(&mut self, branch: &str, reason: &str) -> Result<(), BranchError> {
        let mut inputs = BTreeMap::new();
        inputs.insert("reason".into(), Value::Text(reason.into()));
        self.append(
            branch,
            Actor::System,
            "sys.branch.review",
            inputs,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    /// Rebase, re-check preconditions, then squash branch objects onto `main`.
    /// Egress intents are returned and not executed here.
    pub fn submit(&mut self, branch: &str, actor: Actor) -> Result<Vec<ObjectId>, BranchError> {
        self.rebase(branch)?;
        if let Err(BranchError::Precondition(reason)) = self.preconditions_hold(branch) {
            self.mark_pending(branch, &reason)?;
            return Err(BranchError::Precondition(reason));
        }
        let state = self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?
            .clone();
        let main = self
            .branches
            .get("main")
            .ok_or(BranchError::MissingBranch)?
            .clone();
        let mut writes = Vec::new();
        let mut intents = Vec::new();
        let new_tx = {
            let ms = self.clock.now_ms();
            let rnd = self.rng.next_u128();
            self.ids.next(ms, rnd)
        };
        for id in state.changed.keys() {
            let Some(cur) = state.root.get(&id) else {
                continue;
            };
            if cur.type_name == "sys/EgressIntent" {
                intents.push(id);
            }
            let before = main
                .root
                .get(&id)
                .cloned()
                .unwrap_or_else(|| absent_shell(id));
            let prev = main.root.get(&id).map(|o| o.version).unwrap_or(0);
            let mut after = cur.clone();
            after.version = prev + 1;
            after.provenance.tx = Some(new_tx);
            after.provenance.actor = actor.clone();
            let patch = diff_objects(&before, &after);
            let inverse = invert_patch(&before, &patch);
            writes.push(WriteRecord {
                id,
                prev_version: prev,
                new_version: after.version,
                patch,
                inverse,
                before,
                after,
            });
        }
        self.append(
            "main",
            actor,
            "sys.submit",
            BTreeMap::new(),
            writes,
            intents.clone(),
            Vec::new(),
            Vec::new(),
            None,
        )?;
        self.append(
            branch,
            Actor::System,
            "sys.branch.submitted",
            BTreeMap::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        )?;
        Ok(intents)
    }

    pub fn diff(&self, branch: &str) -> Result<DiffReport, BranchError> {
        let state = self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?;
        let mut by_type: BTreeMap<String, TypeCount> = BTreeMap::new();
        let mut intents = Vec::new();
        let mut new_types = Vec::new();
        let mut new_actions = Vec::new();
        let mut task_report = None;
        for id in state.changed.keys() {
            let Some(cur) = state.root.get(&id).cloned() else {
                continue;
            };
            let base = state.base_root.get(&id).cloned();
            if cur.type_name == "sys/EgressIntent" {
                intents.push(intent_summary(&cur));
                continue;
            }
            if cur.type_name == "sys/TypeDef" && base.is_none() {
                if let Some(Value::Text(name)) = cur.fields.get("name") {
                    new_types.push(name.clone());
                }
                continue;
            }
            if cur.type_name == "sys/ActionDef" && base.is_none() {
                if let Some(Value::Text(name)) = cur.fields.get("name") {
                    new_actions.push(name.clone());
                }
                continue;
            }
            if cur.type_name == "sys/TaskReport" && task_report.is_none() {
                task_report = Some(cur.clone());
            }
            let kind = classify(base.as_ref(), &cur);
            let bucket = by_type.entry(cur.type_name.clone()).or_default();
            match kind {
                Kind::Created => bucket.created += 1,
                Kind::Updated => bucket.updated += 1,
                Kind::Tombstoned => bucket.tombstoned += 1,
            }
        }
        intents.sort_by_key(|i| rev_rank(&i.reversibility));
        Ok(DiffReport {
            task_report,
            intents,
            by_type,
            new_types,
            new_actions,
        })
    }

    pub fn drill(
        &self,
        branch: &str,
        type_name: &str,
        mut query: Query,
    ) -> Result<Page, BranchError> {
        let state = self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?;
        query.from = type_name.into();
        let view = ChangedView {
            root: &state.root,
            changed: &state.changed,
            rev: &state.rev,
            ty: type_name,
        };
        execute(&view, &query).map_err(|_| BranchError::BadEntry)
    }

    pub fn verify(&self) -> Result<(), BranchError> {
        let entries = self.store.entries();
        verify_chain(&entries).map_err(|_| BranchError::BadEntry)
    }

    pub fn gc_blobs(&mut self) -> usize {
        let keep = referenced_blobs(&self.store.entries());
        let mut removed = 0;
        for hash in self.blobs.list_hashes() {
            if !keep.contains(&hash) && self.blobs.remove(&hash) {
                removed += 1;
            }
        }
        removed
    }

    /// Objects that appear in the log, or that still have incoming edges, cannot
    /// be removed. There is no successful physical delete of a retained object.
    pub fn physical_delete(&self, branch: &str, id: &ObjectId) -> Result<(), BranchError> {
        let state = self
            .branches
            .get(branch)
            .ok_or(BranchError::MissingBranch)?;
        if !state.rev.get(id).map(|v| v.is_empty()).unwrap_or(true) {
            return Err(BranchError::Referenced);
        }
        for entry in self.store.entries() {
            if entry.writes.iter().any(|w| w.id == *id) {
                return Err(BranchError::Referenced);
            }
        }
        if state.root.get(id).is_some() {
            return Err(BranchError::Referenced);
        }
        Err(BranchError::NotFound)
    }

    pub fn branch_view<'a>(
        &'a self,
        branch: &'a str,
    ) -> Result<BranchView<'a, S, B, C, R>, BranchError> {
        if !self.branches.contains_key(branch) {
            return Err(BranchError::MissingBranch);
        }
        Ok(BranchView {
            world: self,
            branch,
        })
    }

    pub fn idempotent(&self, branch: &str, key: &str) -> Option<([u8; 32], Value)> {
        let rec = self.branches.get(branch)?.idem.get(&key.to_string())?;
        Some((rec.input, rec.result.clone()))
    }

    /// Rebuild branch state from the log alone and return it beside `self`'s store.
    pub fn replayed(&self) -> Result<Self, BranchError>
    where
        S: Clone,
        B: Clone,
        C: Clone,
        R: Clone,
    {
        let mut world = Self::new(
            self.store.clone(),
            self.blobs.clone(),
            self.clock.clone(),
            self.rng.clone(),
        );
        world.branches.clear();
        world.branches.insert("main".into(), BranchState::new());
        world.spent.clear();
        world.nonces.clear();
        let n = world.store.len();
        for i in 0..n {
            let entry = world.store.get(i).ok_or(BranchError::BadEntry)?;
            world.apply(&entry)?;
        }
        Ok(world)
    }

    pub fn same_visible(&self, other: &Self, branch: &str) -> bool {
        let Some(a) = self.branches.get(branch) else {
            return false;
        };
        let Some(b) = other.branches.get(branch) else {
            return false;
        };
        if a.root.len() != b.root.len() {
            return false;
        }
        let mut ok = true;
        a.root.for_each(&mut |id, obj| match b.root.get(id) {
            Some(o) if visible_eq(obj, o) => {}
            _ => ok = false,
        });
        ok
    }

    fn find_tx(&self, branch: &str, tx: ObjectId) -> Option<LogEntry> {
        self.store
            .entries()
            .into_iter()
            .find(|e| e.branch == branch && e.tx == tx)
    }

    #[allow(clippy::too_many_arguments)]
    fn append(
        &mut self,
        branch: &str,
        actor: Actor,
        action: &str,
        inputs: BTreeMap<String, Value>,
        writes: Vec<WriteRecord>,
        intents: Vec<ObjectId>,
        ledger: Vec<LedgerOp>,
        preconditions: Vec<Predicate>,
        idem: Option<(String, [u8; 32])>,
    ) -> Result<(), BranchError> {
        let (idempotency_key, idempotency_input) = match idem {
            Some((k, h)) => (Some(k), Some(h)),
            None => (None, None),
        };
        let tx = {
            let ms = self.clock.now_ms();
            let rnd = self.rng.next_u128();
            self.ids.next(ms, rnd)
        };
        // Writes already carry their own tx id in provenance when the caller set it.
        // Fork and markers mint the entry id here; object provenance may differ by
        // one mint. That is fine: the entry tx is the audit id.
        let _ = tx;
        let entry_tx = writes
            .first()
            .and_then(|w| w.after.provenance.tx)
            .unwrap_or(tx);
        let entry = LogEntry {
            seq: self.store.len() as u64,
            branch: branch.into(),
            tx: entry_tx,
            prev_hash: self.store.tip_hash(),
            actor,
            capability: None,
            action: action.into(),
            inputs,
            writes,
            intents,
            timestamp_ms: self.clock.now_ms(),
            ledger,
            preconditions,
            idempotency_key,
            idempotency_input,
            hash: [0u8; 32],
        }
        .seal();
        self.store
            .append(entry.clone())
            .map_err(|_| BranchError::Store)?;
        self.apply(&entry)
    }

    fn apply(&mut self, entry: &LogEntry) -> Result<(), BranchError> {
        match entry.action.as_str() {
            "sys.branch.fork" => self.apply_fork(entry),
            "sys.branch.rebase" => self.apply_rebase(entry),
            "sys.branch.review" => {
                let reason = match entry.inputs.get("reason") {
                    Some(Value::Text(s)) => s.clone(),
                    _ => String::from("precondition failed"),
                };
                let state = self
                    .branches
                    .get_mut(&entry.branch)
                    .ok_or(BranchError::MissingBranch)?;
                state.status = BranchStatus::PendingReview { reason };
                Ok(())
            }
            "sys.branch.submitted" => {
                let state = self
                    .branches
                    .get_mut(&entry.branch)
                    .ok_or(BranchError::MissingBranch)?;
                state.status = BranchStatus::Submitted;
                Ok(())
            }
            _ => self.apply_writes(entry),
        }
    }

    fn apply_fork(&mut self, entry: &LogEntry) -> Result<(), BranchError> {
        let from = match entry.inputs.get("from") {
            Some(Value::Text(s)) => s.clone(),
            _ => return Err(BranchError::BadEntry),
        };
        let parent = self
            .branches
            .get(&from)
            .ok_or(BranchError::MissingBranch)?
            .clone();
        let mut child = parent;
        child.base_root = child.root.clone();
        child.changed = Hamt::new();
        child.status = BranchStatus::Active;
        child.forked_from = from;
        child.fork_seq = entry.seq;
        self.branches.insert(entry.branch.clone(), child);
        Ok(())
    }

    fn apply_rebase(&mut self, entry: &LogEntry) -> Result<(), BranchError> {
        let main = self
            .branches
            .get("main")
            .ok_or(BranchError::MissingBranch)?
            .clone();
        let state = self
            .branches
            .get(&entry.branch)
            .ok_or(BranchError::MissingBranch)?
            .clone();
        let mut rev = main.rev.clone();
        let mut root = main.root.clone();
        for w in &entry.writes {
            if let Some(on_main) = main.root.get(&w.id) {
                rev = remove_edges(rev, on_main);
            }
            rev = add_edges(rev, &w.after);
            root = root.insert(w.id, w.after.clone());
        }
        let state_mut = self
            .branches
            .get_mut(&entry.branch)
            .ok_or(BranchError::MissingBranch)?;
        state_mut.base_root = main.root;
        state_mut.root = root;
        state_mut.rev = rev;
        state_mut.changed = state.changed;
        state_mut.fork_seq = entry.seq;
        Ok(())
    }

    fn apply_writes(&mut self, entry: &LogEntry) -> Result<(), BranchError> {
        if !self.branches.contains_key(&entry.branch) {
            return Err(BranchError::MissingBranch);
        }
        for op in &entry.ledger {
            let spent = self.spent.entry(op.cap).or_default();
            spent.cpu_ms += op.cpu_ms;
            spent.gpu_ms += op.gpu_ms;
            spent.money_fen += op.money_fen;
        }
        if let Some(Value::Bytes(n)) = entry.inputs.get("egress_nonce") {
            self.nonces.insert(n.clone());
        }
        let state = self
            .branches
            .get_mut(&entry.branch)
            .ok_or(BranchError::MissingBranch)?;
        if let (Some(key), Some(hash)) = (&entry.idempotency_key, entry.idempotency_input) {
            let result = entry
                .inputs
                .get("idem_result")
                .cloned()
                .unwrap_or(Value::Null);
            state.idem = state.idem.insert(
                key.clone(),
                IdemRec {
                    input: hash,
                    result,
                },
            );
        }
        for w in &entry.writes {
            let current = state.root.get(&w.id).cloned();
            let mut rev = state.rev.clone();
            if let Some(cur) = &current {
                rev = remove_edges(rev, cur);
            }
            rev = add_edges(rev, &w.after);
            state.rev = rev;
            state.root = state.root.insert(w.id, w.after.clone());
            state.changed = state.changed.insert(w.id, ());
        }
        let _ = state;
        Ok(())
    }
}

fn add_edges(
    mut rev: Hamt<ObjectId, Vec<RefEdge>>,
    source: &Object,
) -> Hamt<ObjectId, Vec<RefEdge>> {
    for (field, target) in source.ref_edges() {
        let mut list = rev.get(&target).cloned().unwrap_or_default();
        if !list
            .iter()
            .any(|e| e.source == source.id && e.field == field)
        {
            list.push(RefEdge {
                source: source.id,
                field: field.into(),
            });
        }
        rev = rev.insert(target, list);
    }
    rev
}

fn remove_edges(
    mut rev: Hamt<ObjectId, Vec<RefEdge>>,
    source: &Object,
) -> Hamt<ObjectId, Vec<RefEdge>> {
    for (field, target) in source.ref_edges() {
        if let Some(list) = rev.get(&target).cloned() {
            let list: Vec<_> = list
                .into_iter()
                .filter(|e| !(e.source == source.id && e.field == field))
                .collect();
            rev = if list.is_empty() {
                rev.remove(&target)
            } else {
                rev.insert(target, list)
            };
        }
    }
    rev
}

fn visible_eq(a: &Object, b: &Object) -> bool {
    a.type_name == b.type_name
        && a.major == b.major
        && a.fields == b.fields
        && a.extra == b.extra
        && a.blobs == b.blobs
        && a.provenance.taint == b.provenance.taint
        && a.sensitivity == b.sensitivity
        && a.tombstone == b.tombstone
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeCount {
    pub created: u64,
    pub updated: u64,
    pub tombstoned: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentSummary {
    pub id: ObjectId,
    pub channel: String,
    pub reversibility: String,
    pub tainted: bool,
    pub cost_fen: i64,
}

#[derive(Clone, Debug)]
pub struct DiffReport {
    pub task_report: Option<Object>,
    pub intents: Vec<IntentSummary>,
    pub by_type: BTreeMap<String, TypeCount>,
    pub new_types: Vec<String>,
    pub new_actions: Vec<String>,
}

enum Kind {
    Created,
    Updated,
    Tombstoned,
}

fn classify(before: Option<&Object>, after: &Object) -> Kind {
    if after.tombstone && !before.map(|b| b.tombstone).unwrap_or(false) {
        Kind::Tombstoned
    } else if before.is_none() || before.map(|b| b.tombstone).unwrap_or(false) {
        Kind::Created
    } else {
        Kind::Updated
    }
}

fn rev_rank(rev: &str) -> u8 {
    match rev {
        "irreversible" => 0,
        "compensate" => 1,
        _ => 2,
    }
}

fn intent_summary(obj: &Object) -> IntentSummary {
    let channel = match obj.fields.get("channel") {
        Some(Value::Text(s)) => s.clone(),
        _ => String::new(),
    };
    let reversibility = match obj.fields.get("external_reversibility") {
        Some(Value::Text(s)) => s.clone(),
        _ => String::from("irreversible"),
    };
    let tainted = matches!(obj.fields.get("lineage_tainted"), Some(Value::Bool(true)));
    let cost_fen = match obj.fields.get("cost_money_fen") {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    IntentSummary {
        id: obj.id,
        channel,
        reversibility,
        tainted,
        cost_fen,
    }
}

pub struct OpenTx {
    pub branch: String,
    pub actor: Actor,
    pub action: String,
    pub capability: Option<ObjectId>,
    pub inputs: BTreeMap<String, Value>,
    pub preconditions: Vec<Predicate>,
    pub ledger: Vec<LedgerOp>,
    pub intents: Vec<ObjectId>,
    snap: Hamt<ObjectId, Object>,
    pending: BTreeMap<ObjectId, Object>,
    pub reads: Vec<ObjectId>,
    pub tainted_reads: Vec<ObjectId>,
    pub idem_key: Option<String>,
    pub idem_input: Option<[u8; 32]>,
    failed: Option<String>,
    snap_seq: u64,
}

impl OpenTx {
    pub fn get(&self, id: &ObjectId) -> Option<Object> {
        if let Some(obj) = self.pending.get(id) {
            return Some(obj.clone());
        }
        self.snap.get(id).cloned()
    }

    pub fn put(&mut self, obj: Object) {
        self.pending.insert(obj.id, obj);
    }

    pub fn note_read(&mut self, obj: &Object) {
        self.reads.push(obj.id);
        if obj.provenance.taint == aos_types::Taint::External {
            self.tainted_reads.push(obj.id);
        }
    }

    pub fn fail(&mut self, reason: impl Into<String>) {
        self.failed = Some(reason.into());
    }

    pub fn failed(&self) -> bool {
        self.failed.is_some()
    }

    pub fn for_each(&self, f: &mut dyn FnMut(&Object)) {
        self.snap.for_each(&mut |id, obj| {
            if !self.pending.contains_key(id) {
                f(obj);
            }
        });
        for obj in self.pending.values() {
            f(obj);
        }
    }
}

pub struct BranchView<'a, S, B, C, R> {
    world: &'a World<S, B, C, R>,
    branch: &'a str,
}

impl<'a, S, B, C, R> ObjectView for BranchView<'a, S, B, C, R>
where
    S: LogStore,
    B: BlobStore,
    C: Clock,
    R: Rng,
{
    fn get(&self, id: &ObjectId) -> Option<Object> {
        self.world.get(self.branch, id)
    }

    fn for_each(&self, f: &mut dyn FnMut(&Object)) {
        if let Some(state) = self.world.branches.get(self.branch) {
            state.root.for_each(&mut |_id, obj| f(obj));
        }
    }

    fn reverse(&self, target: &ObjectId) -> Vec<RefEdge> {
        self.world
            .branches
            .get(self.branch)
            .and_then(|s| s.rev.get(target).cloned())
            .unwrap_or_default()
    }
}

struct ChangedView<'a> {
    root: &'a Hamt<ObjectId, Object>,
    changed: &'a Hamt<ObjectId, ()>,
    rev: &'a Hamt<ObjectId, Vec<RefEdge>>,
    ty: &'a str,
}

impl ObjectView for ChangedView<'_> {
    fn get(&self, id: &ObjectId) -> Option<Object> {
        if self.changed.get(id).is_none() {
            return None;
        }
        let obj = self.root.get(id).cloned()?;
        if obj.type_name == self.ty {
            Some(obj)
        } else {
            None
        }
    }

    fn for_each(&self, f: &mut dyn FnMut(&Object)) {
        let ids = self.changed.keys();
        for id in ids {
            if let Some(obj) = self.root.get(&id) {
                if obj.type_name == self.ty {
                    f(obj);
                }
            }
        }
    }

    fn reverse(&self, target: &ObjectId) -> Vec<RefEdge> {
        self.rev
            .get(target)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| self.changed.get(&e.source).is_some())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aos_types::{Sensitivity, Taint};

    fn note(id: ObjectId, text: &str) -> Object {
        let mut obj = Object::new(id, "sys/Note", 1, Actor::User);
        obj.fields.insert("text".into(), Value::Text(text.into()));
        obj.sensitivity = Sensitivity::Private;
        obj
    }

    #[test]
    fn fork_is_isolated_and_chain_detects_tamper() {
        let mut world = MemWorld::memory(1);
        let id = world.mint();
        let mut tx = world
            .open("main", Actor::User, "sys.object.create")
            .unwrap();
        tx.put(note(id, "a"));
        world.commit(tx).unwrap();
        world.fork("main", "task").unwrap();
        let mut tx = world
            .open("task", Actor::User, "sys.object.update")
            .unwrap();
        let mut obj = world.get("task", &id).unwrap();
        obj.fields.insert("text".into(), Value::Text("b".into()));
        tx.put(obj);
        world.commit(tx).unwrap();
        assert_eq!(
            world.get("main", &id).unwrap().fields.get("text"),
            Some(&Value::Text("a".into()))
        );
        assert_eq!(
            world.get("task", &id).unwrap().fields.get("text"),
            Some(&Value::Text("b".into()))
        );
        world.verify().unwrap();
        let mut entries = world.store.entries();
        entries[0].action = "tampered".into();
        assert!(verify_chain(&entries).is_err());
        let folded = world.replayed().unwrap();
        assert!(world.same_visible(&folded, "task"));
    }

    #[test]
    fn conflict_and_revert_restore_fields() {
        let mut world = MemWorld::memory(2);
        let id = world.mint();
        let mut tx = world
            .open("main", Actor::User, "sys.object.create")
            .unwrap();
        tx.put(note(id, "v1"));
        world.commit(tx).unwrap();
        let snap = world
            .open("main", Actor::User, "sys.object.update")
            .unwrap();
        let mut tx1 = world
            .open("main", Actor::User, "sys.object.update")
            .unwrap();
        let mut obj = tx1.get(&id).unwrap();
        obj.fields.insert("text".into(), Value::Text("left".into()));
        tx1.put(obj);
        let tx_id = world.commit(tx1).unwrap();
        let mut obj = snap.get(&id).unwrap();
        obj.fields
            .insert("text".into(), Value::Text("right".into()));
        let mut snap = snap;
        snap.put(obj);
        assert_eq!(world.commit(snap).unwrap_err(), BranchError::Conflict);
        world.revert("main", tx_id, Actor::User).unwrap();
        let back = world.get("main", &id).unwrap();
        assert_eq!(back.fields.get("text"), Some(&Value::Text("v1".into())));
        assert!(!back.tombstone);
        let _ = Taint::Clean;
    }

    #[test]
    fn submit_reevaluates_preconditions() {
        let mut world = MemWorld::memory(3);
        world.fork("main", "task").unwrap();
        let id = world.mint();
        let mut tx = world
            .open("task", Actor::AgentTask(id), "sys.object.create")
            .unwrap();
        let mut obj = note(id, "x");
        obj.fields.insert("n".into(), Value::Int(0));
        tx.put(obj);
        tx.inputs.insert("row".into(), Value::Ref(id));
        tx.preconditions.push(Predicate::Cmp {
            field: "row.n".into(),
            op: aos_types::CmpOp::Gt,
            value: Value::Int(0),
        });
        world.commit(tx).unwrap();
        let err = world.submit("task", Actor::User).unwrap_err();
        assert!(matches!(err, BranchError::Precondition(_)));
        assert!(matches!(
            world.status("task").unwrap(),
            BranchStatus::PendingReview { .. }
        ));
    }
}
