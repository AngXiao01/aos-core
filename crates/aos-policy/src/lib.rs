//! Standing-policy evaluation.
//!
//! `allow` matches when sensitivity is at most the rule's ceiling.
//! `deny` matches when sensitivity is at least the rule's floor.
//! A tainted recipient never produces `allow`.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use aos_types::{crypto, Cbor, Predicate, Sensitivity};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Allow = 0,
    Confirm = 1,
    Deny = 2,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Confirm => "confirm",
            Decision::Deny => "deny",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "allow" => Decision::Allow,
            "confirm" => Decision::Confirm,
            "deny" => Decision::Deny,
            _ => return None,
        })
    }

    pub fn stricter(self, other: Self) -> Self {
        if self > other {
            self
        } else {
            other
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub effect: Decision,
    pub channel: alloc::string::String,
    pub max_sensitivity: Option<Sensitivity>,
    pub max_cost_fen_per_day: Option<u64>,
    pub when: Option<Predicate>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    pub name: alloc::string::String,
    pub rules: Vec<Rule>,
    pub default: Decision,
}

#[derive(Clone, Debug)]
pub struct IntentFacts {
    pub channel: alloc::string::String,
    pub sensitivity: Sensitivity,
    pub cost_fen: u64,
    pub day_spent_fen: u64,
    pub recipient_tainted: bool,
    pub channel_tainted: bool,
    /// `when` predicates read fields through this object-level resolver.
    pub when_value: Option<aos_types::Value>,
}

pub fn evaluate(policy: &Policy, facts: &IntentFacts) -> Decision {
    let mut matched: Option<Decision> = None;
    for rule in &policy.rules {
        if !rule_matches(rule, facts) {
            continue;
        }
        matched = Some(match matched {
            None => rule.effect,
            Some(prev) => prev.stricter(rule.effect),
        });
    }
    let mut decision = matched.unwrap_or(policy.default);
    if (facts.recipient_tainted || facts.channel_tainted) && decision == Decision::Allow {
        decision = Decision::Confirm;
    }
    decision
}

fn rule_matches(rule: &Rule, facts: &IntentFacts) -> bool {
    if !aos_cap_covers(&rule.channel, &facts.channel) {
        return false;
    }
    if let Some(limit) = rule.max_sensitivity {
        let hit = match rule.effect {
            Decision::Allow => facts.sensitivity <= limit,
            Decision::Deny | Decision::Confirm => facts.sensitivity >= limit,
        };
        if !hit {
            return false;
        }
    }
    if let Some(max) = rule.max_cost_fen_per_day {
        let projected = facts.day_spent_fen.saturating_add(facts.cost_fen);
        if projected > max {
            return false;
        }
    }
    if let Some(pred) = &rule.when {
        let ok = aos_types::eval_predicate(pred, &mut |path| match path {
            "recipient.taint" => Some(aos_types::Value::Text(
                if facts.recipient_tainted {
                    "external"
                } else {
                    "clean"
                }
                .into(),
            )),
            "channel" => Some(aos_types::Value::Text(facts.channel.clone())),
            _ => facts.when_value.clone(),
        });
        if !ok {
            return false;
        }
    }
    true
}

fn aos_cap_covers(pattern: &str, needed: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return needed.starts_with(prefix);
    }
    pattern == needed
}

pub fn policy_signing_bytes(policy: &Policy) -> alloc::vec::Vec<u8> {
    policy_cbor(policy).canonical().expect("policy cbor")
}

pub fn sign_policy(policy: &Policy, seed32: &[u8; 32]) -> Result<[u8; 64], ()> {
    crypto::sign(seed32, &policy_signing_bytes(policy))
}

pub fn verify_policy(policy: &Policy, public_key: &[u8], signature: &[u8]) -> bool {
    crypto::verify(public_key, &policy_signing_bytes(policy), signature)
}

fn policy_cbor(policy: &Policy) -> Cbor {
    let mut rules = alloc::vec::Vec::new();
    for rule in &policy.rules {
        let mut pairs = alloc::vec![
            (
                Cbor::Text("effect".into()),
                Cbor::Text(rule.effect.as_str().into())
            ),
            (
                Cbor::Text("channel".into()),
                Cbor::Text(rule.channel.clone())
            ),
        ];
        if let Some(s) = rule.max_sensitivity {
            pairs.push((
                Cbor::Text("max_sensitivity".into()),
                Cbor::Text(s.as_str().into()),
            ));
        }
        if let Some(c) = rule.max_cost_fen_per_day {
            pairs.push((Cbor::Text("max_cost_fen_per_day".into()), Cbor::Uint(c)));
        }
        if let Some(pred) = &rule.when {
            if let Ok(c) = pred.to_cbor() {
                pairs.push((Cbor::Text("when".into()), c));
            }
        }
        rules.push(Cbor::Map(pairs));
    }
    Cbor::Map(alloc::vec![
        (Cbor::Text("name".into()), Cbor::Text(policy.name.clone())),
        (
            Cbor::Text("default".into()),
            Cbor::Text(policy.default.as_str().into()),
        ),
        (Cbor::Text("rules".into()), Cbor::Array(rules)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nightly() -> Policy {
        Policy {
            name: "nightly-ml".into(),
            default: Decision::Confirm,
            rules: alloc::vec![
                Rule {
                    effect: Decision::Allow,
                    channel: "fetch:registered-sources".into(),
                    max_sensitivity: None,
                    max_cost_fen_per_day: None,
                    when: None,
                },
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
        }
    }

    fn facts(channel: &str, sensitivity: Sensitivity) -> IntentFacts {
        IntentFacts {
            channel: channel.into(),
            sensitivity,
            cost_fen: 0,
            day_spent_fen: 0,
            recipient_tainted: false,
            channel_tainted: false,
            when_value: None,
        }
    }

    #[test]
    fn deny_beats_allow_and_default_is_confirm() {
        let p = nightly();
        assert_eq!(
            evaluate(&p, &facts("model:default", Sensitivity::Internal)),
            Decision::Allow
        );
        assert_eq!(
            evaluate(&p, &facts("model:default", Sensitivity::Secret)),
            Decision::Deny
        );
        assert_eq!(
            evaluate(&p, &facts("deliver:minio", Sensitivity::Private)),
            Decision::Confirm
        );
        assert_eq!(
            evaluate(&p, &facts("notify:email", Sensitivity::Private)),
            Decision::Confirm
        );
        let mut tainted = facts("model:default", Sensitivity::Internal);
        tainted.recipient_tainted = true;
        assert_eq!(evaluate(&p, &tainted), Decision::Confirm);
    }
}
