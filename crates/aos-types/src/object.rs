use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::cbor::{Cbor, CborError};
use crate::hash::ContentHash;
use crate::id::ObjectId;
use crate::value::{Value, TAG_REF};

/// Who produced an object. People and agents are both actors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    User,
    AgentTask(ObjectId),
    Adapter(String),
    System,
}

impl Actor {
    pub fn parse(s: &str) -> Result<Self, ()> {
        if s == "user" {
            return Ok(Actor::User);
        }
        if s == "system" {
            return Ok(Actor::System);
        }
        if let Some(rest) = s.strip_prefix("agent:task/") {
            return Ok(Actor::AgentTask(ObjectId::parse(rest).map_err(|_| ())?));
        }
        if let Some(rest) = s.strip_prefix("adapter:") {
            if rest.is_empty() || rest.contains(':') {
                return Err(());
            }
            return Ok(Actor::Adapter(rest.into()));
        }
        Err(())
    }

    pub fn as_text(&self) -> String {
        match self {
            Actor::User => "user".into(),
            Actor::System => "system".into(),
            Actor::AgentTask(id) => {
                let mut s = String::from("agent:task/");
                s.push_str(&id.to_text());
                s
            }
            Actor::Adapter(name) => {
                let mut s = String::from("adapter:");
                s.push_str(name);
                s
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Taint {
    Clean,
    External,
}

impl Taint {
    pub fn as_str(self) -> &'static str {
        match self {
            Taint::Clean => "clean",
            Taint::External => "external",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ()> {
        match s {
            "clean" => Ok(Taint::Clean),
            "external" => Ok(Taint::External),
            _ => Err(()),
        }
    }
}

/// `public < internal < private < secret`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sensitivity {
    Public = 0,
    Internal = 1,
    Private = 2,
    Secret = 3,
}

impl Sensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Sensitivity::Public => "public",
            Sensitivity::Internal => "internal",
            Sensitivity::Private => "private",
            Sensitivity::Secret => "secret",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ()> {
        match s {
            "public" => Ok(Sensitivity::Public),
            "internal" => Ok(Sensitivity::Internal),
            "private" => Ok(Sensitivity::Private),
            "secret" => Ok(Sensitivity::Secret),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub actor: Actor,
    pub origin: String,
    pub taint: Taint,
    /// `id@version` of inputs this object was derived from.
    pub derived_from: Vec<(ObjectId, u64)>,
    pub tx: Option<ObjectId>,
}

impl Provenance {
    pub fn internal(actor: Actor) -> Self {
        Self {
            actor,
            origin: "internal".into(),
            taint: Taint::Clean,
            derived_from: Vec::new(),
            tx: None,
        }
    }
}

/// An object derived from any tainted parent is tainted. v1 is per object.
pub fn propagate_taint(parents: &[Taint]) -> Taint {
    if parents.iter().any(|t| *t == Taint::External) {
        Taint::External
    } else {
        Taint::Clean
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    pub id: ObjectId,
    pub type_name: String,
    pub major: u32,
    pub version: u64,
    pub fields: BTreeMap<String, Value>,
    pub blobs: Vec<ContentHash>,
    pub provenance: Provenance,
    pub sensitivity: Sensitivity,
    pub extra: BTreeMap<String, Value>,
    pub tombstone: bool,
}

impl Object {
    pub fn new(id: ObjectId, type_name: impl Into<String>, major: u32, actor: Actor) -> Self {
        let sensitivity = match actor {
            Actor::User => Sensitivity::Private,
            _ => Sensitivity::Private,
        };
        Self {
            id,
            type_name: type_name.into(),
            major,
            version: 1,
            fields: BTreeMap::new(),
            blobs: Vec::new(),
            provenance: Provenance::internal(actor),
            sensitivity,
            extra: BTreeMap::new(),
            tombstone: false,
        }
    }

    pub fn to_cbor(&self) -> Result<Cbor, CborError> {
        let mut derived = Vec::new();
        for (id, ver) in &self.provenance.derived_from {
            derived.push(Cbor::Array(alloc::vec![
                Cbor::Bytes(id.0.to_vec()),
                Cbor::Uint(*ver),
            ]));
        }
        let mut fields = Vec::new();
        for (k, v) in &self.fields {
            fields.push((Cbor::Text(k.clone()), v.to_cbor()?));
        }
        let mut extra = Vec::new();
        for (k, v) in &self.extra {
            extra.push((Cbor::Text(k.clone()), v.to_cbor()?));
        }
        let mut blobs = Vec::new();
        for b in &self.blobs {
            blobs.push(Cbor::Bytes(b.0.to_vec()));
        }
        Ok(Cbor::Map(alloc::vec![
            (Cbor::Text("id".into()), Cbor::Bytes(self.id.0.to_vec())),
            (
                Cbor::Text("type".into()),
                Cbor::Text(self.type_name.clone())
            ),
            (Cbor::Text("major".into()), Cbor::Uint(self.major as u64)),
            (Cbor::Text("version".into()), Cbor::Uint(self.version)),
            (Cbor::Text("fields".into()), Cbor::Map(fields)),
            (Cbor::Text("blobs".into()), Cbor::Array(blobs)),
            (
                Cbor::Text("provenance".into()),
                Cbor::Map(alloc::vec![
                    (
                        Cbor::Text("actor".into()),
                        Cbor::Text(self.provenance.actor.as_text()),
                    ),
                    (
                        Cbor::Text("origin".into()),
                        Cbor::Text(self.provenance.origin.clone()),
                    ),
                    (
                        Cbor::Text("taint".into()),
                        Cbor::Text(self.provenance.taint.as_str().into()),
                    ),
                    (Cbor::Text("derived_from".into()), Cbor::Array(derived)),
                    (
                        Cbor::Text("tx".into()),
                        match self.provenance.tx {
                            Some(id) => Cbor::Bytes(id.0.to_vec()),
                            None => Cbor::Null,
                        },
                    ),
                ]),
            ),
            (
                Cbor::Text("sensitivity".into()),
                Cbor::Text(self.sensitivity.as_str().into()),
            ),
            (Cbor::Text("extra".into()), Cbor::Map(extra)),
            (Cbor::Text("tombstone".into()), Cbor::Bool(self.tombstone)),
        ]))
    }

    /// Top-level reference edges, for the reverse index. Declared fields use
    /// their bare name; loose fields kept in `extra` use `extra.<name>`, so a
    /// ref that `field_of` can resolve is always reachable from its target.
    pub fn ref_edges(&self) -> Vec<(String, ObjectId)> {
        let mut out = Vec::new();
        for (name, value) in &self.fields {
            if let Value::Ref(id) | Value::RefAt(id, _) = value {
                out.push((name.clone(), *id));
            }
        }
        for (name, value) in &self.extra {
            if let Value::Ref(id) | Value::RefAt(id, _) = value {
                out.push((format!("extra.{name}"), *id));
            }
        }
        out
    }
}

/// Field-level patch. Paths are `fields.<name>`, `extra.<name>`, or one of
/// `tombstone`, `taint`, `sensitivity`, `blobs`, `type`, `major`, `origin`.
#[derive(Clone, Debug, PartialEq)]
pub struct Patch {
    pub sets: BTreeMap<String, Value>,
    pub unsets: Vec<String>,
}

impl Patch {
    pub fn empty() -> Self {
        Self {
            sets: BTreeMap::new(),
            unsets: Vec::new(),
        }
    }

    pub fn to_cbor(&self) -> Result<Cbor, CborError> {
        let mut sets = Vec::new();
        for (k, v) in &self.sets {
            sets.push((Cbor::Text(k.clone()), v.to_cbor()?));
        }
        let unsets = self
            .unsets
            .iter()
            .map(|p| Cbor::Text(p.clone()))
            .collect::<Vec<_>>();
        Ok(Cbor::Map(alloc::vec![
            (Cbor::Text("sets".into()), Cbor::Map(sets)),
            (Cbor::Text("unsets".into()), Cbor::Array(unsets)),
        ]))
    }
}

pub fn diff_objects(before: &Object, after: &Object) -> Patch {
    let mut sets = BTreeMap::new();
    let mut unsets = Vec::new();
    diff_map(
        "fields",
        &before.fields,
        &after.fields,
        &mut sets,
        &mut unsets,
    );
    diff_map("extra", &before.extra, &after.extra, &mut sets, &mut unsets);
    if before.tombstone != after.tombstone {
        sets.insert("tombstone".into(), Value::Bool(after.tombstone));
    }
    if before.provenance.taint != after.provenance.taint {
        sets.insert(
            "taint".into(),
            Value::Text(after.provenance.taint.as_str().into()),
        );
    }
    if before.sensitivity != after.sensitivity {
        sets.insert(
            "sensitivity".into(),
            Value::Text(after.sensitivity.as_str().into()),
        );
    }
    if before.blobs != after.blobs {
        sets.insert(
            "blobs".into(),
            Value::List(
                after
                    .blobs
                    .iter()
                    .map(|b| Value::Bytes(b.0.to_vec()))
                    .collect(),
            ),
        );
    }
    if before.type_name != after.type_name {
        sets.insert("type".into(), Value::Text(after.type_name.clone()));
    }
    if before.major != after.major {
        sets.insert("major".into(), Value::Int(after.major as i64));
    }
    if before.provenance.origin != after.provenance.origin {
        sets.insert(
            "origin".into(),
            Value::Text(after.provenance.origin.clone()),
        );
    }
    Patch { sets, unsets }
}

fn diff_map(
    prefix: &str,
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
    sets: &mut BTreeMap<String, Value>,
    unsets: &mut Vec<String>,
) {
    for (k, v) in after {
        if before.get(k) != Some(v) {
            sets.insert(format!("{prefix}.{k}"), v.clone());
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) {
            unsets.push(format!("{prefix}.{k}"));
        }
    }
}

/// Inverse of `forward` relative to `before`, so applying `forward` then the
/// inverse restores the visible fields of `before`.
pub fn invert_patch(before: &Object, forward: &Patch) -> Patch {
    let mut sets = BTreeMap::new();
    let mut unsets = Vec::new();
    for path in &forward.unsets {
        if let Some(old) = get_path(before, path) {
            sets.insert(path.clone(), old);
        }
    }
    for path in forward.sets.keys() {
        match get_path(before, path) {
            Some(old) => {
                sets.insert(path.clone(), old);
            }
            None => unsets.push(path.clone()),
        }
    }
    Patch { sets, unsets }
}

pub fn apply_patch(obj: &mut Object, patch: &Patch) -> Result<(), ()> {
    for path in &patch.unsets {
        unset_path(obj, path)?;
    }
    for (path, value) in &patch.sets {
        set_path(obj, path, value.clone())?;
    }
    Ok(())
}

fn get_path(obj: &Object, path: &str) -> Option<Value> {
    if let Some(name) = path.strip_prefix("fields.") {
        return obj.fields.get(name).cloned();
    }
    if let Some(name) = path.strip_prefix("extra.") {
        return obj.extra.get(name).cloned();
    }
    match path {
        "tombstone" => Some(Value::Bool(obj.tombstone)),
        "taint" => Some(Value::Text(obj.provenance.taint.as_str().into())),
        "sensitivity" => Some(Value::Text(obj.sensitivity.as_str().into())),
        "type" => Some(Value::Text(obj.type_name.clone())),
        "major" => Some(Value::Int(obj.major as i64)),
        "origin" => Some(Value::Text(obj.provenance.origin.clone())),
        "blobs" => Some(Value::List(
            obj.blobs
                .iter()
                .map(|b| Value::Bytes(b.0.to_vec()))
                .collect(),
        )),
        _ => None,
    }
}

fn set_path(obj: &mut Object, path: &str, value: Value) -> Result<(), ()> {
    if let Some(name) = path.strip_prefix("fields.") {
        if name.is_empty() {
            return Err(());
        }
        obj.fields.insert(name.into(), value);
        return Ok(());
    }
    if let Some(name) = path.strip_prefix("extra.") {
        if name.is_empty() {
            return Err(());
        }
        obj.extra.insert(name.into(), value);
        return Ok(());
    }
    match path {
        "tombstone" => {
            obj.tombstone = match value {
                Value::Bool(b) => b,
                _ => return Err(()),
            };
        }
        "taint" => {
            let Value::Text(s) = value else {
                return Err(());
            };
            obj.provenance.taint = Taint::parse(&s)?;
        }
        "sensitivity" => {
            let Value::Text(s) = value else {
                return Err(());
            };
            obj.sensitivity = Sensitivity::parse(&s)?;
        }
        "type" => {
            let Value::Text(s) = value else {
                return Err(());
            };
            obj.type_name = s;
        }
        "major" => {
            let Value::Int(n) = value else {
                return Err(());
            };
            obj.major = u32::try_from(n).map_err(|_| ())?;
        }
        "origin" => {
            let Value::Text(s) = value else {
                return Err(());
            };
            obj.provenance.origin = s;
        }
        "blobs" => {
            let Value::List(xs) = value else {
                return Err(());
            };
            let mut blobs = Vec::new();
            for x in xs {
                let Value::Bytes(b) = x else {
                    return Err(());
                };
                if b.len() != 32 {
                    return Err(());
                }
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                blobs.push(ContentHash(h));
            }
            obj.blobs = blobs;
        }
        _ => return Err(()),
    }
    Ok(())
}

fn unset_path(obj: &mut Object, path: &str) -> Result<(), ()> {
    if let Some(name) = path.strip_prefix("fields.") {
        obj.fields.remove(name);
        return Ok(());
    }
    if let Some(name) = path.strip_prefix("extra.") {
        obj.extra.remove(name);
        return Ok(());
    }
    Err(())
}

/// Blank shell used as the "before" image of a create, so the inverse is a tombstone.
pub fn absent_shell(id: ObjectId) -> Object {
    let mut obj = Object::new(id, "", 0, Actor::System);
    obj.version = 0;
    obj.tombstone = true;
    obj.sensitivity = Sensitivity::Private;
    obj
}

pub fn object_from_cbor(c: &Cbor) -> Result<Object, ()> {
    let Cbor::Map(pairs) = c else {
        return Err(());
    };
    let mut id = None;
    let mut type_name = None;
    let mut major = 1u32;
    let mut version = 1u64;
    let mut fields = BTreeMap::new();
    let mut blobs = Vec::new();
    let mut actor = Actor::System;
    let mut origin = String::from("internal");
    let mut taint = Taint::Clean;
    let mut derived_from = Vec::new();
    let mut tx = None;
    let mut sensitivity = Sensitivity::Private;
    let mut extra = BTreeMap::new();
    let mut tombstone = false;
    for (k, v) in pairs {
        let Cbor::Text(key) = k else {
            return Err(());
        };
        match key.as_str() {
            "id" => {
                let Cbor::Bytes(b) = v else { return Err(()) };
                if b.len() != 16 {
                    return Err(());
                }
                let mut raw = [0u8; 16];
                raw.copy_from_slice(b);
                id = Some(ObjectId(raw));
            }
            "type" => {
                let Cbor::Text(s) = v else { return Err(()) };
                type_name = Some(s.clone());
            }
            "major" => {
                let Cbor::Uint(n) = v else { return Err(()) };
                major = u32::try_from(*n).map_err(|_| ())?;
            }
            "version" => {
                let Cbor::Uint(n) = v else { return Err(()) };
                version = *n;
            }
            "fields" => fields = map_values(v)?,
            "extra" => extra = map_values(v)?,
            "blobs" => {
                let Cbor::Array(xs) = v else { return Err(()) };
                for x in xs {
                    let Cbor::Bytes(b) = x else { return Err(()) };
                    if b.len() != 32 {
                        return Err(());
                    }
                    let mut h = [0u8; 32];
                    h.copy_from_slice(b);
                    blobs.push(ContentHash(h));
                }
            }
            "sensitivity" => {
                let Cbor::Text(s) = v else { return Err(()) };
                sensitivity = Sensitivity::parse(s)?;
            }
            "tombstone" => {
                let Cbor::Bool(b) = v else { return Err(()) };
                tombstone = *b;
            }
            "provenance" => {
                let Cbor::Map(ps) = v else { return Err(()) };
                for (pk, pv) in ps {
                    let Cbor::Text(pk) = pk else { return Err(()) };
                    match pk.as_str() {
                        "actor" => {
                            let Cbor::Text(s) = pv else { return Err(()) };
                            actor = Actor::parse(s)?;
                        }
                        "origin" => {
                            let Cbor::Text(s) = pv else { return Err(()) };
                            origin = s.clone();
                        }
                        "taint" => {
                            let Cbor::Text(s) = pv else { return Err(()) };
                            taint = Taint::parse(s)?;
                        }
                        "tx" => match pv {
                            Cbor::Null => tx = None,
                            Cbor::Bytes(b) if b.len() == 16 => {
                                let mut raw = [0u8; 16];
                                raw.copy_from_slice(b);
                                tx = Some(ObjectId(raw));
                            }
                            _ => return Err(()),
                        },
                        "derived_from" => {
                            let Cbor::Array(xs) = pv else { return Err(()) };
                            for item in xs {
                                let Cbor::Array(pair) = item else {
                                    return Err(());
                                };
                                if pair.len() != 2 {
                                    return Err(());
                                }
                                let Cbor::Bytes(b) = &pair[0] else {
                                    return Err(());
                                };
                                let Cbor::Uint(ver) = pair[1] else {
                                    return Err(());
                                };
                                if b.len() != 16 {
                                    return Err(());
                                }
                                let mut raw = [0u8; 16];
                                raw.copy_from_slice(b);
                                derived_from.push((ObjectId(raw), ver));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let id = id.ok_or(())?;
    let type_name = type_name.ok_or(())?;
    Ok(Object {
        id,
        type_name,
        major,
        version,
        fields,
        blobs,
        provenance: Provenance {
            actor,
            origin,
            taint,
            derived_from,
            tx,
        },
        sensitivity,
        extra,
        tombstone,
    })
}

fn map_values(c: &Cbor) -> Result<BTreeMap<String, Value>, ()> {
    let Cbor::Map(pairs) = c else {
        return Err(());
    };
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        let Cbor::Text(s) = k else {
            return Err(());
        };
        m.insert(s.clone(), Value::from_cbor(v).map_err(|_| ())?);
    }
    Ok(m)
}

/// Silence unused import if tag const is only documented via value module.
#[allow(dead_code)]
fn _tag_ref_documented() -> u64 {
    TAG_REF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_restores_fields_and_taint_propagates() {
        assert_eq!(
            propagate_taint(&[Taint::Clean, Taint::External]),
            Taint::External
        );
        assert_eq!(propagate_taint(&[Taint::Clean, Taint::Clean]), Taint::Clean);

        let id = ObjectId::from_parts(1, 1);
        let mut before = Object::new(id, "sys/Note", 1, Actor::User);
        before.fields.insert("text".into(), Value::Text("a".into()));
        let mut after = before.clone();
        after.fields.insert("text".into(), Value::Text("b".into()));
        after.extra.insert("keep".into(), Value::Text("x".into()));
        after.provenance.taint = Taint::External;
        let forward = diff_objects(&before, &after);
        let inverse = invert_patch(&before, &forward);
        let mut obj = before.clone();
        apply_patch(&mut obj, &forward).unwrap();
        apply_patch(&mut obj, &inverse).unwrap();
        assert_eq!(obj.fields, before.fields);
        assert_eq!(obj.extra, before.extra);
        assert_eq!(obj.provenance.taint, before.provenance.taint);
    }

    #[test]
    fn actor_roundtrip() {
        let id = ObjectId::from_parts(9, 8);
        let a = Actor::AgentTask(id);
        assert_eq!(Actor::parse(&a.as_text()).unwrap(), a);
        assert!(Actor::parse("nope").is_err());
    }
}
