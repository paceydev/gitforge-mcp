//! From what a section holds and what should change, the operations to run;
//! what each will change, in words a person can check; and the operations
//! that undo it.

use serde_json::{Map, Value, json};

use super::fields::{self, check_all, without_secrets};
use super::{Done, Item, Op, Section, State};
use crate::version::ForgeKind;

/// What a change comes to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Planned {
    pub ops: Vec<Op>,
    /// Asked for, but nothing to do: already so, or already gone.
    pub notes: Vec<String>,
}

/// The operations that turn `state` into what `changes` asks for. Fields
/// already as asked are left out, so applying twice changes nothing the
/// second time.
pub fn plan(
    section: Section,
    state: &State,
    changes: &Value,
    kind: ForgeKind,
) -> Result<Planned, String> {
    let changes = changes
        .as_object()
        .ok_or_else(|| "`changes` should be an object".to_owned())?;
    match (section, state) {
        (_, State::Values(current)) => plan_values(section, current, changes, kind),
        (Section::BranchProtection, State::Items(items)) => plan_rules(items, changes, kind),
        (_, State::Items(items)) => plan_hooks(items, changes, kind),
    }
}

fn plan_values(
    section: Section,
    current: &Map<String, Value>,
    changes: &Map<String, Value>,
    kind: ForgeKind,
) -> Result<Planned, String> {
    let checked = check_all(section, changes, kind, None)?;
    let mut set = Map::new();
    for (name, value) in checked {
        match current.get(&name) {
            None => {
                return Err(format!(
                    "{kind} didn't report `{name}` to this credential, so it can't be changed here"
                ));
            }
            Some(now) if *now == value => {}
            Some(_) => {
                set.insert(name, value);
            }
        }
    }
    let mut planned = Planned::default();
    if set.is_empty() {
        planned
            .notes
            .push(format!("{} is already as asked", section.name()));
    } else {
        planned.ops.push(Op::SetRepo { fields: set });
    }
    Ok(planned)
}

/// The keys `changes` may have for a list section.
fn only_keys(changes: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match changes.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!(
            "`changes` has `{k}`; it takes {}",
            allowed
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => Ok(()),
    }
}

fn list<'a>(changes: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], String> {
    match changes.get(key) {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(format!("`changes.{key}` should be a list")),
    }
}

fn find<'a>(items: &'a [Item], section: Section, key: &str) -> Option<&'a Item> {
    items.iter().find(|i| i.key(section) == key)
}

/// The fields of `asked` that differ from `item`'s.
fn differing(item: &Item, asked: Map<String, Value>) -> Map<String, Value> {
    asked
        .into_iter()
        .filter(|(k, v)| item.fields.get(k) != Some(v))
        .collect()
}

fn plan_rules(
    items: &[Item],
    changes: &Map<String, Value>,
    kind: ForgeKind,
) -> Result<Planned, String> {
    only_keys(changes, &["set", "delete"])?;
    let mut planned = Planned::default();
    let mut seen = Vec::new();
    for rule in list(changes, "set")? {
        let rule = rule
            .as_object()
            .ok_or("each rule in `set` should be an object")?;
        let pattern = rule
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or("each rule in `set` needs a `pattern`")?;
        if seen.contains(&pattern) {
            return Err(format!("`set` has the rule `{pattern}` twice"));
        }
        seen.push(pattern);
        let checked = check_all(Section::BranchProtection, rule, kind, Some("pattern"))?;
        let existing = find(items, Section::BranchProtection, pattern);
        check_rule(existing, &checked, kind, pattern)?;
        match existing {
            Some(item) => {
                let fields = differing(item, checked);
                if fields.is_empty() {
                    planned
                        .notes
                        .push(format!("rule `{pattern}` is already as asked"));
                } else {
                    planned.ops.push(Op::EditRule {
                        pattern: pattern.to_owned(),
                        fields,
                        raw: None,
                    });
                }
            }
            None => {
                let mut fields = Map::new();
                fields.insert("pattern".into(), json!(pattern));
                fields.extend(checked);
                planned.ops.push(Op::CreateRule { fields, raw: None });
            }
        }
    }
    for pattern in list(changes, "delete")? {
        let pattern = pattern
            .as_str()
            .ok_or("`delete` should list rule patterns")?;
        if seen.contains(&pattern) {
            return Err(format!("rule `{pattern}` is both set and deleted"));
        }
        match find(items, Section::BranchProtection, pattern) {
            Some(_) => planned.ops.push(Op::DeleteRule {
                pattern: pattern.to_owned(),
            }),
            None => planned
                .notes
                .push(format!("there is no rule `{pattern}` to delete")),
        }
    }
    Ok(planned)
}

/// On Gitea and Forgejo, a push allowlist is who may push directly, so it
/// can't go with requiring pull requests, which turns direct pushes off.
fn check_rule(
    existing: Option<&Item>,
    asked: &Map<String, Value>,
    kind: ForgeKind,
    pattern: &str,
) -> Result<(), String> {
    if kind == ForgeKind::Github {
        return Ok(());
    }
    let value = |k: &str| {
        asked
            .get(k)
            .or_else(|| existing.and_then(|i| i.fields.get(k)))
            .cloned()
            .unwrap_or(Value::Null)
    };
    if value("require_pull_request") == json!(true) && !value("push_allowlist").is_null() {
        return Err(format!(
            "rule `{pattern}`: on {kind}, `push_allowlist` lists who may push directly, which `require_pull_request` turns off; set one of them, or `push_allowlist` to null"
        ));
    }
    Ok(())
}

fn plan_hooks(
    items: &[Item],
    changes: &Map<String, Value>,
    kind: ForgeKind,
) -> Result<Planned, String> {
    only_keys(changes, &["create", "update", "delete"])?;
    let mut planned = Planned::default();
    let mut urls: Vec<String> = Vec::new();
    for hook in list(changes, "create")? {
        let hook = hook
            .as_object()
            .ok_or("each webhook in `create` should be an object")?;
        let fields = new_hook(hook, kind)?;
        let url = fields["url"].as_str().unwrap_or_default().to_owned();
        if let Some(same) = items
            .iter()
            .find(|i| i.fields.get("url") == Some(&json!(url)))
        {
            return Err(format!(
                "webhook {} already sends to {url}; update it instead",
                same.key(Section::Webhooks)
            ));
        }
        if urls.contains(&url) {
            return Err(format!("`create` has two webhooks for {url}"));
        }
        urls.push(url);
        planned.ops.push(Op::CreateWebhook { fields, raw: None });
    }
    let mut updated = Vec::new();
    for hook in list(changes, "update")? {
        let hook = hook
            .as_object()
            .ok_or("each webhook in `update` should be an object")?;
        let id = hook
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("each webhook in `update` needs its `id`")?;
        let item = find(items, Section::Webhooks, &id.to_string())
            .ok_or_else(|| format!("there is no webhook {id}"))?;
        let checked = check_all(Section::Webhooks, hook, kind, Some("id"))?;
        if checked.contains_key("type") {
            return Err("a webhook's `type` is set when it is created".into());
        }
        updated.push(id);
        // A secret can't be compared: it is never read back.
        let secrets: Map<String, Value> = checked
            .iter()
            .filter(|(k, _)| is_secret(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut fields = differing(item, without_secrets(Section::Webhooks, checked));
        fields.extend(secrets);
        if fields.is_empty() {
            planned
                .notes
                .push(format!("webhook {id} is already as asked"));
        } else {
            planned.ops.push(Op::EditWebhook {
                id,
                fields,
                raw: None,
            });
        }
    }
    for id in list(changes, "delete")? {
        let id = id.as_u64().ok_or("`delete` should list webhook ids")?;
        if updated.contains(&id) {
            return Err(format!("webhook {id} is both updated and deleted"));
        }
        match find(items, Section::Webhooks, &id.to_string()) {
            Some(_) => planned.ops.push(Op::DeleteWebhook { id }),
            None => planned
                .notes
                .push(format!("there is no webhook {id} to delete")),
        }
    }
    Ok(planned)
}

/// A webhook to create, checked, with the defaults filled in: JSON, on
/// push, active.
fn new_hook(hook: &Map<String, Value>, kind: ForgeKind) -> Result<Map<String, Value>, String> {
    let mut fields = check_all(Section::Webhooks, hook, kind, None)?;
    if !fields.contains_key("url") {
        return Err("each webhook in `create` needs a `url`".into());
    }
    if let Some(ct) = fields.get("content_type").and_then(Value::as_str)
        && ct != "json"
        && ct != "form"
    {
        return Err("`content_type` is `json` or `form`".into());
    }
    let defaults = [
        ("content_type", json!("json")),
        ("events", json!(["push"])),
        ("active", json!(true)),
    ];
    for (k, v) in defaults {
        fields.entry(k).or_insert(v);
    }
    Ok(fields)
}

fn is_secret(name: &str) -> bool {
    fields::HOOK.iter().any(|f| f.secret && f.name == name)
}

/// What `op` will change, entry by entry, with secrets shown only as set.
pub fn describe(section: Section, state: &State, op: &Op) -> Vec<Value> {
    let current = |key: &str| item_of(section, state, key).map(|i| i.fields.clone());
    match op {
        Op::SetRepo { fields } => fields
            .iter()
            .map(|(k, to)| {
                let from = match state {
                    State::Values(v) => v.get(k).cloned().unwrap_or(Value::Null),
                    State::Items(_) => Value::Null,
                };
                json!({ "field": k, "from": from, "to": to })
            })
            .collect(),
        Op::CreateRule { fields, .. } => vec![json!({
            "rule": fields.get("pattern"), "change": "create", "to": fields,
        })],
        Op::EditRule {
            pattern, fields, ..
        } => edits("rule", json!(pattern), current(pattern), fields),
        Op::DeleteRule { pattern } => vec![json!({
            "rule": pattern, "change": "delete", "from": current(pattern),
        })],
        Op::CreateWebhook { fields, .. } => vec![json!({
            "webhook": fields.get("url"), "change": "create", "to": shown(fields),
        })],
        Op::EditWebhook { id, fields, .. } => {
            edits("webhook", json!(id), current(&id.to_string()), fields)
        }
        Op::DeleteWebhook { id } => vec![json!({
            "webhook": id, "change": "delete", "from": current(&id.to_string()),
        })],
    }
}

fn edits(
    what: &str,
    name: Value,
    current: Option<Map<String, Value>>,
    fields: &Map<String, Value>,
) -> Vec<Value> {
    fields
        .iter()
        .map(|(k, to)| {
            if is_secret(k) {
                return json!({
                    what: name, "field": k,
                    "change": "set (write-only: the old value can't be read, so revert can't restore it)",
                });
            }
            let from = current
                .as_ref()
                .and_then(|c| c.get(k).cloned())
                .unwrap_or(Value::Null);
            json!({ what: name, "field": k, "from": from, "to": to })
        })
        .collect()
}

/// `fields` with each secret shown as set rather than by value.
fn shown(fields: &Map<String, Value>) -> Map<String, Value> {
    fields
        .iter()
        .map(|(k, v)| {
            if is_secret(k) {
                (k.clone(), json!("(set)"))
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

fn item_of<'a>(section: Section, state: &'a State, key: &str) -> Option<&'a Item> {
    match state {
        State::Items(items) => find(items, section, key),
        State::Values(_) => None,
    }
}

/// The operation that undoes `op`, run against `state` as it was before,
/// and anything it can't put back. `None` when it can't be undone at all.
pub fn inverse(
    section: Section,
    state: &State,
    op: &Op,
    done: Done,
) -> (Option<Op>, Option<String>) {
    let item = |key: &str| item_of(section, state, key);
    match op {
        Op::SetRepo { fields } => {
            let State::Values(now) = state else {
                return (None, None);
            };
            let back = fields
                .keys()
                .filter_map(|k| now.get(k).map(|v| (k.clone(), v.clone())))
                .collect();
            (Some(Op::SetRepo { fields: back }), None)
        }
        Op::CreateRule { fields, .. } => {
            let pattern = fields
                .get("pattern")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (
                Some(Op::DeleteRule {
                    pattern: pattern.to_owned(),
                }),
                None,
            )
        }
        Op::EditRule {
            pattern, fields, ..
        } => (
            item(pattern).map(|i| Op::EditRule {
                pattern: pattern.clone(),
                fields: old_values(i, fields),
                raw: Some(i.raw.clone()),
            }),
            None,
        ),
        Op::DeleteRule { pattern } => (item(pattern).map(recreate_rule), None),
        Op::CreateWebhook { .. } => match done.created_id {
            Some(id) => (Some(Op::DeleteWebhook { id }), None),
            None => (
                None,
                Some(
                    "the forge didn't say which id the new webhook got, so revert can't delete it"
                        .into(),
                ),
            ),
        },
        Op::EditWebhook { id, fields, .. } => {
            let note = fields.keys().any(|k| is_secret(k)).then(|| {
                format!(
                    "webhook {id}: revert can't restore the old secret, which was never readable"
                )
            });
            let open = without_secrets(Section::Webhooks, fields.clone());
            let back =
                item(&id.to_string())
                    .filter(|_| !open.is_empty())
                    .map(|i| Op::EditWebhook {
                        id: *id,
                        fields: old_values(i, &open),
                        raw: Some(i.raw.clone()),
                    });
            (back, note)
        }
        Op::DeleteWebhook { id } => (
            item(&id.to_string()).map(recreate_hook),
            Some(format!(
                "webhook {id}: revert recreates it with a new id, and without its secret if it had one"
            )),
        ),
    }
}

fn old_values(item: &Item, fields: &Map<String, Value>) -> Map<String, Value> {
    fields
        .keys()
        .map(|k| {
            (
                k.clone(),
                item.fields.get(k).cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

fn recreate_rule(item: &Item) -> Op {
    Op::CreateRule {
        fields: item.fields.clone(),
        raw: Some(item.raw.clone()),
    }
}

fn recreate_hook(item: &Item) -> Op {
    let mut fields = item.fields.clone();
    fields.remove("id");
    Op::CreateWebhook {
        fields,
        raw: Some(item.raw.clone()),
    }
}

/// FNV-1a's 64-bit offset basis and prime.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// A short, stable digest of what a section holds, to tell whether it
/// changed between a plan and its apply, or an apply and its revert.
pub fn fingerprint(state: &State) -> String {
    let text = match state {
        State::Values(v) => canonical(&Value::Object(v.clone())),
        State::Items(items) => {
            let mut keyed: Vec<String> = items
                .iter()
                .map(|i| canonical(&json!({ "fields": i.fields, "raw": i.raw })))
                .collect();
            keyed.sort();
            keyed.join("\n")
        }
    };
    // FNV-1a, 64-bit: not for security, only to notice a change.
    let mut hash: u64 = FNV_OFFSET;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// `value` as JSON with every object's keys in order, so the same content
/// always reads the same.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", json!(k), canonical(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn rule(pattern: &str, approvals: u64) -> Item {
        Item {
            fields: map(json!({
                "pattern": pattern, "required_approvals": approvals,
                "require_pull_request": true, "push_allowlist": null,
            })),
            raw: json!({ "rule_name": pattern, "required_approvals": approvals, "enable_push": false }),
        }
    }

    fn hook(id: u64, url: &str) -> Item {
        Item {
            fields: map(
                json!({"id": id, "url": url, "events": ["push"], "active": true, "content_type": "json"}),
            ),
            raw: json!({"id": id, "config": {"url": url}}),
        }
    }

    #[test]
    fn values_already_as_asked_are_left_out() {
        let state = State::Values(map(json!({"description": "x", "has_wiki": true})));
        let planned = plan(
            Section::General,
            &state,
            &json!({"description": "x", "has_wiki": false}),
            ForgeKind::Forgejo,
        )
        .unwrap();
        assert_eq!(
            planned.ops,
            vec![Op::SetRepo {
                fields: map(json!({"has_wiki": false}))
            }]
        );
        let again = plan(
            Section::General,
            &state,
            &json!({"description": "x"}),
            ForgeKind::Forgejo,
        )
        .unwrap();
        assert!(again.ops.is_empty());
        assert_eq!(again.notes, vec!["general is already as asked"]);
    }

    #[test]
    fn rules_are_edited_created_and_deleted_and_each_has_an_inverse() {
        let state = State::Items(vec![rule("main", 1), rule("release/*", 2)]);
        let planned = plan(
            Section::BranchProtection,
            &state,
            &json!({
                "set": [{"pattern": "main", "required_approvals": 2}, {"pattern": "dev", "required_approvals": 1}],
                "delete": ["release/*", "gone"],
            }),
            ForgeKind::Gitea,
        )
        .unwrap();
        assert_eq!(planned.ops.len(), 3);
        assert_eq!(planned.notes, vec!["there is no rule `gone` to delete"]);
        let undo: Vec<Op> = planned
            .ops
            .iter()
            .filter_map(|op| inverse(Section::BranchProtection, &state, op, Done::default()).0)
            .collect();
        assert!(matches!(
            &undo[0],
            Op::EditRule { pattern, fields, raw: Some(raw) }
                if pattern == "main" && *fields == map(json!({"required_approvals": 1})) && raw["enable_push"] == false
        ));
        assert_eq!(
            undo[1],
            Op::DeleteRule {
                pattern: "dev".into()
            }
        );
        assert!(
            matches!(&undo[2], Op::CreateRule { raw: Some(raw), .. } if raw["rule_name"] == "release/*")
        );
    }

    #[test]
    fn an_allowlist_and_required_pull_requests_conflict_on_gitea_only() {
        let state = State::Items(vec![rule("main", 1)]);
        let asked = json!({"set": [{"pattern": "main", "push_allowlist": {"users": ["ci"]}}]});
        let err = plan(
            Section::BranchProtection,
            &state,
            &asked,
            ForgeKind::Forgejo,
        )
        .unwrap_err();
        assert!(
            err.contains("`push_allowlist` lists who may push directly"),
            "{err}"
        );
        let ok = json!({"set": [{"pattern": "main", "require_pull_request": false, "push_allowlist": {"users": ["ci"]}}]});
        assert!(plan(Section::BranchProtection, &state, &ok, ForgeKind::Forgejo).is_ok());
    }

    #[test]
    fn webhook_secrets_are_always_sent_but_never_shown_or_reverted() {
        let state = State::Items(vec![hook(7, "https://ci.example.com/hook")]);
        let planned = plan(
            Section::Webhooks,
            &state,
            &json!({"update": [{"id": 7, "secret": "s3cret", "active": true}]}),
            ForgeKind::Forgejo,
        )
        .unwrap();
        assert_eq!(
            planned.ops,
            vec![Op::EditWebhook {
                id: 7,
                fields: map(json!({"secret": "s3cret"})),
                raw: None
            }]
        );
        let shown = describe(Section::Webhooks, &state, &planned.ops[0]);
        assert!(!serde_json::to_string(&shown).unwrap().contains("s3cret"));
        let (undo, note) = inverse(Section::Webhooks, &state, &planned.ops[0], Done::default());
        assert_eq!(undo, None);
        assert!(note.unwrap().contains("can't restore the old secret"));
    }

    #[test]
    fn creating_a_webhook_fills_defaults_and_refuses_a_duplicate_url() {
        let state = State::Items(vec![hook(7, "https://ci.example.com/hook")]);
        let planned = plan(
            Section::Webhooks,
            &state,
            &json!({"create": [{"url": "https://chat.example.com/in"}]}),
            ForgeKind::Gitea,
        )
        .unwrap();
        assert_eq!(
            planned.ops,
            vec![Op::CreateWebhook {
                fields: map(
                    json!({"url": "https://chat.example.com/in", "content_type": "json", "events": ["push"], "active": true})
                ),
                raw: None,
            }]
        );
        let (undo, _) = inverse(
            Section::Webhooks,
            &state,
            &planned.ops[0],
            Done {
                created_id: Some(9),
            },
        );
        assert_eq!(undo, Some(Op::DeleteWebhook { id: 9 }));
        let dup = plan(
            Section::Webhooks,
            &state,
            &json!({"create": [{"url": "https://ci.example.com/hook"}]}),
            ForgeKind::Gitea,
        )
        .unwrap_err();
        assert!(dup.contains("webhook 7 already sends to"), "{dup}");
    }

    #[test]
    fn fingerprints_ignore_key_and_item_order_but_not_values() {
        let a = State::Items(vec![rule("main", 1), rule("dev", 1)]);
        let b = State::Items(vec![rule("dev", 1), rule("main", 1)]);
        let c = State::Items(vec![rule("dev", 1), rule("main", 2)]);
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&c));
        let x = State::Values(map(json!({"a": 1, "b": 2})));
        let y = State::Values(map(json!({"b": 2, "a": 1})));
        assert_eq!(fingerprint(&x), fingerprint(&y));
    }
}
