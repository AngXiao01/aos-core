use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::object::{Object, Sensitivity};
use crate::value::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum FieldType {
    Ref { target: String },
    Int { min: Option<i64>, max: Option<i64> },
    Float { min: Option<f64>, max: Option<f64> },
    Text { max_chars: Option<u64> },
    Bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub ty: FieldType,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub name: String,
    pub target: String,
    pub via: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewHint {
    pub title: String,
    pub primary_fields: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeDef {
    pub name: String,
    pub major: u32,
    pub fields: Vec<FieldDef>,
    pub relations: Vec<Relation>,
    pub view_hint: Option<ViewHint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaError {
    Missing(String),
    Unknown(String),
    Type(String),
    Range(String),
}

impl TypeDef {
    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn validate(&self, obj: &Object) -> Result<(), SchemaError> {
        if obj.type_name != self.name {
            return Err(SchemaError::Type(obj.type_name.clone()));
        }
        if obj.major != self.major {
            return Err(SchemaError::Type("major".into()));
        }
        for f in &self.fields {
            match obj.fields.get(&f.name) {
                None if f.required => return Err(SchemaError::Missing(f.name.clone())),
                None => {}
                Some(v) => check_value(&f.name, &f.ty, v)?,
            }
        }
        Ok(())
    }
}

fn check_float(name: &str, min: Option<f64>, max: Option<f64>, n: f64) -> Result<(), SchemaError> {
    if let Some(min) = min {
        if n < min {
            return Err(SchemaError::Range(name.into()));
        }
    }
    if let Some(max) = max {
        if n > max {
            return Err(SchemaError::Range(name.into()));
        }
    }
    Ok(())
}

fn check_value(name: &str, ty: &FieldType, v: &Value) -> Result<(), SchemaError> {
    match (ty, v) {
        (FieldType::Bool, Value::Bool(_)) => Ok(()),
        (FieldType::Text { max_chars }, Value::Text(s)) => {
            if let Some(max) = max_chars {
                if s.chars().count() as u64 > *max {
                    return Err(SchemaError::Range(name.into()));
                }
            }
            Ok(())
        }
        (FieldType::Int { min, max }, Value::Int(n)) => {
            if let Some(min) = min {
                if n < min {
                    return Err(SchemaError::Range(name.into()));
                }
            }
            if let Some(max) = max {
                if n > max {
                    return Err(SchemaError::Range(name.into()));
                }
            }
            Ok(())
        }
        (FieldType::Float { min, max }, Value::Float(n)) => check_float(name, *min, *max, *n),
        (FieldType::Float { min, max }, Value::Int(n)) => check_float(name, *min, *max, *n as f64),
        (FieldType::Ref { .. }, Value::Ref(_)) | (FieldType::Ref { .. }, Value::RefAt(_, _)) => {
            Ok(())
        }
        _ => Err(SchemaError::Type(name.into())),
    }
}

fn text(name: &str, required: bool, max: Option<u64>) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty: FieldType::Text { max_chars: max },
        required,
    }
}

fn int(name: &str, required: bool) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty: FieldType::Int {
            min: None,
            max: None,
        },
        required,
    }
}

fn bool_f(name: &str, required: bool) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty: FieldType::Bool,
        required,
    }
}

fn ref_f(name: &str, target: &str, required: bool) -> FieldDef {
    FieldDef {
        name: name.into(),
        ty: FieldType::Ref {
            target: target.into(),
        },
        required,
    }
}

fn td(name: &str, fields: Vec<FieldDef>) -> TypeDef {
    TypeDef {
        name: name.into(),
        major: 1,
        fields,
        relations: Vec::new(),
        view_hint: None,
    }
}

/// Schemas shipped with the core so both versions share one sys vocabulary.
/// ML types stay with the ML provider.
pub fn builtin_types() -> Vec<TypeDef> {
    alloc::vec![
        td(
            "sys/TypeDef",
            alloc::vec![
                text("name", true, Some(200)),
                int("major", true),
                text("body", true, None)
            ],
        ),
        td(
            "sys/ActionDef",
            alloc::vec![
                text("name", true, Some(200)),
                int("version", true),
                text("summary", false, Some(2000)),
                text("reversibility", true, Some(32)),
                text("capability", true, Some(200)),
                text("reads", true, None),
                text("writes", true, None),
                text("egress", false, None),
                text("idempotency", false, None),
            ],
        ),
        td(
            "sys/EgressIntent",
            alloc::vec![
                text("channel", true, Some(200)),
                text("target", false, Some(2000)),
                ref_f("target_ref", "sys/Object", false),
                text("payload_hash", true, Some(80)),
                text("external_reversibility", true, Some(32)),
                int("cost_money_fen", true),
                text("status", true, Some(32)),
                bool_f("lineage_tainted", true),
                text("policy_decision", false, Some(32)),
            ],
        ),
        td(
            "sys/ActionViolation",
            alloc::vec![
                text("action", true, Some(200)),
                text("reason", true, Some(2000)),
                text("branch", true, Some(200)),
            ],
        ),
        td(
            "sys/Job",
            alloc::vec![
                text("state", true, Some(32)),
                text("kind", true, Some(16)),
                int("cost_cpu_ms", true),
                int("cost_gpu_ms", true),
                int("cost_money_fen", true),
                text("spec", false, None),
            ],
        ),
        td(
            "sys/Task",
            alloc::vec![
                text("intent", true, Some(8000)),
                text("plan", true, Some(200)),
                text("branch", true, Some(200)),
                text("capability", false, Some(80)),
                text("status", true, Some(32)),
                ref_f("report", "sys/TaskReport", false),
            ],
        ),
        td(
            "sys/TaskReport",
            alloc::vec![
                text("did", true, Some(4000)),
                text("why", true, Some(4000)),
                text("uncertain", false, Some(4000)),
                text("check_ids", false, None),
            ],
        ),
        td("sys/Note", alloc::vec![text("text", true, Some(8000))],),
        td(
            "sys/Fact",
            alloc::vec![
                text("subject", true, Some(500)),
                text("predicate", true, Some(200)),
                text("object", true, Some(2000)),
            ],
        ),
        td(
            "sys/Policy",
            alloc::vec![
                text("name", true, Some(200)),
                text("default_decision", true, Some(16)),
                text("rules", true, None),
                text("signature", true, Some(200)),
            ],
        ),
        td(
            "sys/Approval",
            alloc::vec![
                text("request_id", true, Some(32)),
                text("decision", true, Some(16)),
                text("nonce_hex", true, Some(64)),
            ],
        ),
        td(
            "sys/EgressReceipt",
            alloc::vec![
                ref_f("intent", "sys/EgressIntent", true),
                bool_f("ok", true),
                text("detail", false, Some(2000)),
            ],
        ),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionDecl {
    pub name: String,
    pub version: u32,
    pub summary: String,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub egress: Vec<String>,
    /// `undo`, `compensate`, or `irreversible`.
    pub reversibility: String,
    pub capability: String,
    /// Ordered input paths. Not a string template language.
    pub idempotency_paths: Vec<String>,
}

pub fn builtin_actions() -> Vec<ActionDecl> {
    alloc::vec![
        act(
            "sys.object.create",
            "undo",
            "sys.object.write",
            &["sys/Object"],
            &["sys/Object"]
        ),
        act(
            "sys.object.update",
            "undo",
            "sys.object.write",
            &["sys/Object"],
            &["sys/Object"]
        ),
        act(
            "sys.object.tombstone",
            "undo",
            "sys.object.write",
            &["sys/Object"],
            &["sys/Object"],
        ),
        act(
            "sys.type.propose",
            "undo",
            "sys.type.write",
            &["sys/TypeDef"],
            &["sys/TypeDef"]
        ),
        act(
            "sys.action.propose",
            "undo",
            "sys.action.write",
            &["sys/ActionDef"],
            &["sys/ActionDef"],
        ),
    ]
}

fn act(name: &str, rev: &str, cap: &str, reads: &[&str], writes: &[&str]) -> ActionDecl {
    ActionDecl {
        name: name.into(),
        version: 1,
        summary: name.into(),
        reads: reads.iter().map(|s| (*s).into()).collect(),
        writes: writes.iter().map(|s| (*s).into()).collect(),
        egress: Vec::new(),
        reversibility: rev.into(),
        capability: cap.into(),
        idempotency_paths: Vec::new(),
    }
}

/// User data defaults to private. Callers that omit sensitivity get that.
pub fn default_sensitivity() -> Sensitivity {
    Sensitivity::Private
}

/// Known field names stay in `fields`; everything else is preserved in `extra`.
pub fn split_loose(
    schema: &TypeDef,
    incoming: BTreeMap<String, Value>,
) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut fields = BTreeMap::new();
    let mut extra = BTreeMap::new();
    for (k, v) in incoming {
        if schema.field(&k).is_some() {
            fields.insert(k, v);
        } else {
            extra.insert(k, v);
        }
    }
    (fields, extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ObjectId;
    use crate::object::{Actor, Object};

    #[test]
    fn note_requires_text_and_keeps_extra() {
        let schema = builtin_types()
            .into_iter()
            .find(|t| t.name == "sys/Note")
            .unwrap();
        let mut incoming = BTreeMap::new();
        incoming.insert("text".into(), Value::Text("hi".into()));
        incoming.insert("mood".into(), Value::Text("calm".into()));
        let (fields, extra) = split_loose(&schema, incoming);
        assert!(extra.contains_key("mood"));
        let mut obj = Object::new(ObjectId::from_parts(1, 1), "sys/Note", 1, Actor::User);
        obj.fields = fields;
        obj.extra = extra;
        schema.validate(&obj).unwrap();
        obj.fields.remove("text");
        assert!(matches!(
            schema.validate(&obj),
            Err(SchemaError::Missing(_))
        ));
    }

    #[test]
    fn every_builtin_action_is_internally_undo() {
        for a in builtin_actions() {
            assert_eq!(a.reversibility, "undo");
            assert!(a.egress.is_empty());
        }
    }
}
