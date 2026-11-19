//! Repository settings in terms every forge shares: what a repository says
//! about itself (general), how pull requests merge (merge), branch
//! protection rules, and webhooks.
//!
//! A change is planned against what the forge says now, applied as a list
//! of operations, and undone by the operations that apply hands back. The
//! drivers translate each operation into their forge's own calls.

pub mod fields;
pub mod plan;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A part of a repository's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    /// Description, website, default branch, visibility, issues and wiki.
    General,
    /// Which merge styles pull requests may use, the default one, and
    /// whether the head branch is deleted after a merge.
    Merge,
    /// Branch protection rules, each named by its `pattern`.
    BranchProtection,
    /// Webhooks, each named by its `id`.
    Webhooks,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::General,
        Section::Merge,
        Section::BranchProtection,
        Section::Webhooks,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Section::General => "general",
            Section::Merge => "merge",
            Section::BranchProtection => "branch_protection",
            Section::Webhooks => "webhooks",
        }
    }
}

/// One branch protection rule or webhook: its fields under shared names,
/// and the forge's own record of it, kept to recreate it exactly. Neither
/// holds a secret: forges don't return them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub fields: Map<String, Value>,
    pub raw: Value,
}

impl Item {
    /// The rule's pattern or the webhook's id, as text.
    pub fn key(&self, section: Section) -> String {
        let key = fields::key_of(section).unwrap_or_default();
        match &self.fields.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }
}

/// What a section holds now.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// General or merge: field by field.
    Values(Map<String, Value>),
    Items(Vec<Item>),
}

/// One change to make. Apply returns the operations that undo what it did,
/// and revert runs those.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// General and merge fields, by shared name.
    SetRepo {
        fields: Map<String, Value>,
    },
    /// A new rule from `fields`, or exactly as `raw` when recreating one.
    CreateRule {
        fields: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    /// `fields` changed on a rule; or, with `raw`, the whole rule put back
    /// exactly as it was (`fields` then only says what that changes).
    EditRule {
        pattern: String,
        fields: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    DeleteRule {
        pattern: String,
    },
    CreateWebhook {
        fields: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    /// As `EditRule`, for a webhook. Its secret is never in `raw`, so it
    /// stays as it is.
    EditWebhook {
        id: u64,
        fields: Map<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    DeleteWebhook {
        id: u64,
    },
}

/// What a driver reports after an operation: the id of a webhook it
/// created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Done {
    pub created_id: Option<u64>,
}

/// A webhook delivery, as GitHub lists them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Delivery {
    pub id: u64,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    pub delivered_at: String,
    pub status: String,
    pub status_code: u64,
    pub duration_seconds: f64,
    pub redelivery: bool,
}

/// `raw` with every value under a key that might name a credential taken
/// out, at any depth: for a webhook as a forge returns it.
pub fn strip_credentials(mut raw: Value) -> Value {
    fn looks_secret(key: &str) -> bool {
        let k = key.to_ascii_lowercase();
        ["secret", "token", "password", "authorization"]
            .iter()
            .any(|s| k.contains(s))
    }
    if let Some(map) = raw.as_object_mut() {
        map.retain(|k, _| !looks_secret(k));
        for value in map.values_mut() {
            if value.is_object() {
                *value = strip_credentials(value.take());
            }
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credentials_are_stripped_at_any_depth() {
        let raw = json!({
            "id": 3,
            "authorization_header": "Bearer x",
            "config": {"url": "https://example.com/hook", "secret": "s", "api_token": "t"},
        });
        assert_eq!(
            strip_credentials(raw),
            json!({"id": 3, "config": {"url": "https://example.com/hook"}})
        );
    }

    #[test]
    fn ops_round_trip_through_json() {
        let op = Op::CreateRule {
            fields: json!({"pattern": "main"}).as_object().cloned().unwrap(),
            raw: None,
        };
        let text = serde_json::to_value(&op).unwrap();
        assert_eq!(
            text,
            json!({"op": "create_rule", "fields": {"pattern": "main"}})
        );
        assert_eq!(serde_json::from_value::<Op>(text).unwrap(), op);
    }
}
