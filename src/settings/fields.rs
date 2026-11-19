//! The settings each section holds, under names every forge shares, and
//! what a value of each must look like.

use serde_json::{Map, Value, json};

use super::Section;
use crate::pulls::MergeStyle;
use crate::version::ForgeKind;

/// What a field's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Flag,
    Count,
    /// A list of strings.
    Texts,
    /// A list of merge styles, kept in one order.
    Styles,
    Style,
    /// `null`, or `{"users": [...], "teams": [...]}`.
    Allowlist,
}

/// Which forges have a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum On {
    Every,
    /// Gitea and Forgejo.
    GiteaFamily,
    Github,
}

impl On {
    pub fn includes(self, kind: ForgeKind) -> bool {
        match self {
            On::Every => true,
            On::GiteaFamily => kind != ForgeKind::Github,
            On::Github => kind == ForgeKind::Github,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    pub kind: Kind,
    pub on: On,
    /// Write-only: never read back, never shown.
    pub secret: bool,
}

const fn field(name: &'static str, kind: Kind, on: On) -> Field {
    Field {
        name,
        kind,
        on,
        secret: false,
    }
}

const fn secret(name: &'static str, on: On) -> Field {
    Field {
        name,
        kind: Kind::Text,
        on,
        secret: true,
    }
}

pub const GENERAL: &[Field] = &[
    field("description", Kind::Text, On::Every),
    field("website", Kind::Text, On::Every),
    field("default_branch", Kind::Text, On::Every),
    field("private", Kind::Flag, On::Every),
    field("has_issues", Kind::Flag, On::Every),
    field("has_wiki", Kind::Flag, On::Every),
];

pub const MERGE: &[Field] = &[
    field("allowed_styles", Kind::Styles, On::Every),
    field("default_style", Kind::Style, On::GiteaFamily),
    field("delete_branch_after_merge", Kind::Flag, On::Every),
    field("allow_auto_merge", Kind::Flag, On::Github),
];

/// A branch protection rule. `pattern` names it.
pub const RULE: &[Field] = &[
    field("pattern", Kind::Text, On::Every),
    field("require_pull_request", Kind::Flag, On::Every),
    field("required_approvals", Kind::Count, On::Every),
    field("dismiss_stale_approvals", Kind::Flag, On::Every),
    field("required_checks", Kind::Texts, On::Every),
    field("require_up_to_date", Kind::Flag, On::Every),
    field("require_signed_commits", Kind::Flag, On::Every),
    field("push_allowlist", Kind::Allowlist, On::Every),
    field("enforce_admins", Kind::Flag, On::Github),
    field("block_on_rejected_reviews", Kind::Flag, On::GiteaFamily),
];

/// A webhook. `id` names it, and the forge gives it.
pub const HOOK: &[Field] = &[
    field("id", Kind::Count, On::Every),
    field("type", Kind::Text, On::GiteaFamily),
    field("url", Kind::Text, On::Every),
    field("content_type", Kind::Text, On::Every),
    field("events", Kind::Texts, On::Every),
    field("active", Kind::Flag, On::Every),
    field("branch_filter", Kind::Text, On::GiteaFamily),
    secret("secret", On::Every),
    secret("authorization_header", On::GiteaFamily),
];

pub fn fields(section: Section) -> &'static [Field] {
    match section {
        Section::General => GENERAL,
        Section::Merge => MERGE,
        Section::BranchProtection => RULE,
        Section::Webhooks => HOOK,
    }
}

/// The merge styles a forge has, in the order they're listed.
pub fn styles_on(kind: ForgeKind) -> &'static [MergeStyle] {
    use MergeStyle::*;
    match kind {
        ForgeKind::Github => &[Merge, Rebase, Squash],
        _ => &[Merge, Rebase, RebaseMerge, Squash, FastForwardOnly],
    }
}

pub fn style_named(name: &str) -> Option<MergeStyle> {
    use MergeStyle::*;
    [Merge, Rebase, RebaseMerge, Squash, FastForwardOnly]
        .into_iter()
        .find(|s| s.as_forge() == name)
}

/// `changes` checked field by field against `section` on `kind`, each value
/// in canonical form. `key` is the field that names an item and may not be
/// changed (`pattern`, `id`), or `None`.
pub fn check_all(
    section: Section,
    changes: &Map<String, Value>,
    kind: ForgeKind,
    key: Option<&str>,
) -> Result<Map<String, Value>, String> {
    let mut checked = Map::new();
    for (name, value) in changes {
        if Some(name.as_str()) == key {
            continue;
        }
        let field = fields(section)
            .iter()
            .find(|f| f.name == name)
            .filter(|f| Some(f.name) != key_of(section))
            .ok_or_else(|| unknown(section, name, kind))?;
        if !field.on.includes(kind) {
            return Err(format!("{kind} has no {} setting `{name}`", section.name()));
        }
        let value = check(field, value, kind).map_err(|why| format!("`{name}`: {why}"))?;
        checked.insert(name.clone(), value);
    }
    Ok(checked)
}

/// The field that names an item in `section`.
pub fn key_of(section: Section) -> Option<&'static str> {
    match section {
        Section::BranchProtection => Some("pattern"),
        Section::Webhooks => Some("id"),
        _ => None,
    }
}

fn unknown(section: Section, name: &str, kind: ForgeKind) -> String {
    let known: Vec<&str> = fields(section)
        .iter()
        .filter(|f| f.on.includes(kind) && Some(f.name) != key_of(section))
        .map(|f| f.name)
        .collect();
    format!(
        "no {} setting `{name}`; on {kind} there are: {}",
        section.name(),
        known.join(", ")
    )
}

/// `value` if it fits `field`, in canonical form.
pub fn check(field: &Field, value: &Value, kind: ForgeKind) -> Result<Value, String> {
    match field.kind {
        Kind::Text => value
            .as_str()
            .map(|s| json!(s))
            .ok_or_else(|| "should be a string".into()),
        Kind::Flag => value
            .as_bool()
            .map(|b| json!(b))
            .ok_or_else(|| "should be true or false".into()),
        Kind::Count => value
            .as_u64()
            .map(|n| json!(n))
            .ok_or_else(|| "should be a whole number, 0 or more".into()),
        // Order means nothing in a list of events or checks; sorted, two
        // lists with the same entries compare equal.
        Kind::Texts => texts(value).map(|mut t| {
            t.sort();
            t.dedup();
            json!(t)
        }),
        Kind::Style => one_style(value, kind).map(|s| json!(s.as_forge())),
        Kind::Styles => styles(value, kind),
        Kind::Allowlist => allowlist(value),
    }
}

fn texts(value: &Value) -> Result<Vec<String>, String> {
    let items = value.as_array().ok_or("should be a list of strings")?;
    items
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "should be a list of strings".to_owned())
        })
        .collect()
}

fn one_style(value: &Value, kind: ForgeKind) -> Result<MergeStyle, String> {
    let allowed = styles_on(kind);
    value
        .as_str()
        .and_then(style_named)
        .filter(|s| allowed.contains(s))
        .ok_or_else(|| {
            let names: Vec<&str> = allowed.iter().map(|s| s.as_forge()).collect();
            format!("{kind} merge styles are {}", names.join(", "))
        })
}

/// Merge styles in the forge's order, each once.
fn styles(value: &Value, kind: ForgeKind) -> Result<Value, String> {
    let items = value.as_array().ok_or("should be a list of merge styles")?;
    let mut chosen = Vec::new();
    for item in items {
        chosen.push(one_style(item, kind)?);
    }
    let ordered: Vec<&str> = styles_on(kind)
        .iter()
        .filter(|s| chosen.contains(s))
        .map(|s| s.as_forge())
        .collect();
    if ordered.is_empty() {
        return Err("at least one merge style must stay allowed".into());
    }
    Ok(json!(ordered))
}

fn allowlist(value: &Value) -> Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let map = value
        .as_object()
        .ok_or("should be null or {\"users\": [...], \"teams\": [...]}")?;
    if let Some(other) = map.keys().find(|k| *k != "users" && *k != "teams") {
        return Err(format!("has `{other}`; only `users` and `teams` are known"));
    }
    let list = |k: &str| map.get(k).map_or(Ok(Vec::new()), texts);
    let (mut users, mut teams) = (list("users")?, list("teams")?);
    users.sort();
    teams.sort();
    Ok(json!({ "users": users, "teams": teams }))
}

/// `value` with every write-only field taken out.
pub fn without_secrets(section: Section, mut value: Map<String, Value>) -> Map<String, Value> {
    for f in fields(section).iter().filter(|f| f.secret) {
        value.remove(f.name);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn styles_are_ordered_and_limited_to_the_forge() {
        let f = MERGE[0];
        assert_eq!(
            check(
                &f,
                &json!(["squash", "merge", "squash"]),
                ForgeKind::Forgejo
            )
            .unwrap(),
            json!(["merge", "squash"])
        );
        assert!(
            check(&f, &json!(["rebase-merge"]), ForgeKind::Github)
                .unwrap_err()
                .contains("GitHub merge styles are merge, rebase, squash")
        );
        assert!(check(&f, &json!([]), ForgeKind::Gitea).is_err());
    }

    #[test]
    fn unknown_and_foreign_fields_are_named() {
        let err = check_all(
            Section::Merge,
            &map(json!({"default_style": "merge"})),
            ForgeKind::Github,
            None,
        )
        .unwrap_err();
        assert_eq!(err, "GitHub has no merge setting `default_style`");
        let err = check_all(
            Section::General,
            &map(json!({"homepage": "x"})),
            ForgeKind::Gitea,
            None,
        )
        .unwrap_err();
        assert!(err.starts_with(
            "no general setting `homepage`; on Gitea there are: description, website"
        ));
    }

    #[test]
    fn allowlists_are_sorted_and_null_stays_null() {
        let f = RULE[7];
        assert_eq!(
            check(&f, &json!({"users": ["b", "a"]}), ForgeKind::Gitea).unwrap(),
            json!({"users": ["a", "b"], "teams": []})
        );
        assert_eq!(
            check(&f, &Value::Null, ForgeKind::Gitea).unwrap(),
            Value::Null
        );
        assert!(check(&f, &json!({"apps": []}), ForgeKind::Github).is_err());
    }

    #[test]
    fn the_key_field_is_skipped_not_checked() {
        let checked = check_all(
            Section::BranchProtection,
            &map(json!({"pattern": "main", "required_approvals": 2})),
            ForgeKind::Forgejo,
            Some("pattern"),
        )
        .unwrap();
        assert_eq!(Value::Object(checked), json!({"required_approvals": 2}));
    }
}
