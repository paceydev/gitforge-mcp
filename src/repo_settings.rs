//! The `repo_settings` tool: read a repository's settings, plan a change,
//! apply it, and revert it.
//!
//! A plan names a fingerprint of what the section held; apply refuses if
//! the section changed since, so what was reviewed is what gets changed.
//! Apply returns the operations that undo it, with the section's new
//! fingerprint, and revert refuses if the section changed after the apply.
//! Webhook secrets are write-only: no reply ever carries one, not even an
//! error that quotes the request.

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::context::Session;
use crate::forge::{ApiError, Forge};
use crate::settings::plan::{self, describe, fingerprint, inverse};
use crate::settings::{Op, Section, State, fields};
use crate::tools::ToolError;

type ToolResult = Result<Value, ToolError>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettingsAction {
    /// The settings now: one section, or all of them.
    #[default]
    Get,
    /// What `changes` would change, and the fingerprint `apply` needs.
    Plan,
    /// Make the planned change. Returns what undoes it.
    Apply,
    /// Undo an apply, with the `revert` it returned.
    Revert,
    /// Have the forge send a webhook a test delivery.
    TestWebhook,
    /// A webhook's recent deliveries and how each went.
    WebhookDeliveries,
}

impl SettingsAction {
    pub fn name(self) -> &'static str {
        match self {
            SettingsAction::Get => "get",
            SettingsAction::Plan => "plan",
            SettingsAction::Apply => "apply",
            SettingsAction::Revert => "revert",
            SettingsAction::TestWebhook => "test_webhook",
            SettingsAction::WebhookDeliveries => "webhook_deliveries",
        }
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RepoSettingsParams {
    /// `get` (default), `plan`, `apply`, `revert`, `test_webhook` or
    /// `webhook_deliveries`. Only the actions your access allows are listed.
    #[serde(default)]
    pub action: SettingsAction,
    /// `general`, `merge`, `branch_protection` or `webhooks`. Needed by
    /// `plan` and `apply`; `get` without it reads every section.
    pub section: Option<Section>,
    /// For `plan` and `apply`: what should change. `general` and `merge`:
    /// the fields to set, e.g. `{"allowed_styles": ["merge", "squash"]}`.
    /// `branch_protection`: `{"set": [rules], "delete": [patterns]}`, where
    /// a rule is `{"pattern": ..., fields to set}`, created if there's no
    /// rule with that pattern. `webhooks`: `{"create": [webhooks],
    /// "update": [{"id": ..., fields to set}], "delete": [ids]}`. `get`
    /// shows each section's fields.
    pub changes: Option<Value>,
    /// For `apply`: the fingerprint `plan` returned. Refused if the section
    /// changed since.
    pub fingerprint: Option<String>,
    /// For `revert`: the `revert` object `apply` returned, as it was.
    pub revert: Option<Value>,
    /// For `test_webhook` and `webhook_deliveries`: the webhook's id.
    pub webhook_id: Option<u64>,
    /// For `webhook_deliveries`: how many (default 10, at most 50).
    pub limit: Option<u32>,
}

/// What `apply` returns for `revert`.
#[derive(Debug, Serialize, Deserialize)]
struct Revert {
    section: Section,
    ops: Vec<Op>,
    /// What the section held right after the apply; revert refuses if it
    /// changed since.
    fingerprint: String,
}

const DEFAULT_DELIVERIES: u32 = 10;
const MAX_DELIVERIES: u32 = 50;

pub async fn repo_settings(session: &Session, params: &RepoSettingsParams) -> ToolResult {
    let secrets = secrets_in(params.changes.as_ref());
    let result = match params.action {
        SettingsAction::Get => get(session, params.section).await,
        SettingsAction::Plan => plan_change(session, params).await,
        SettingsAction::Apply => apply(session, params).await,
        SettingsAction::Revert => revert(session, params).await,
        SettingsAction::TestWebhook => test_webhook(session, params).await,
        SettingsAction::WebhookDeliveries => deliveries(session, params).await,
    };
    match result {
        Ok(v) => Ok(redact(v, &secrets)),
        Err(ToolError(message)) => Err(ToolError(redact_text(message, &secrets))),
    }
}

fn kind(session: &Session) -> crate::version::ForgeKind {
    session.forge.server().kind
}

/// A forge's refusal, with the access settings need: which scope or role
/// can't be checked before trying.
fn refused(e: ApiError, session: &Session, section: Section) -> ToolError {
    let need = match (kind(session), section) {
        (crate::version::ForgeKind::Github, Section::Webhooks) => {
            "admin access to the repository, and a token with the admin:repo_hook scope (classic) or Webhooks permission (fine-grained)"
        }
        (crate::version::ForgeKind::Github, _) => {
            "admin access to the repository, and a token with the repo scope (classic) or Administration permission (fine-grained)"
        }
        _ => "admin access to the repository, and a token with the write:repository scope",
    };
    match e {
        ApiError::Refused { .. } => {
            ToolError(format!("{e}. {} settings need {need}", section.name()))
        }
        other => other.into(),
    }
}

async fn read(session: &Session, section: Section) -> Result<State, ToolError> {
    let forge = &session.forge;
    let repo = &session.repo;
    let state = match section {
        Section::General | Section::Merge => {
            let all = forge.repo_settings(repo).await;
            let all = all.map_err(|e| refused(e, session, section))?;
            let names: Vec<&str> = fields::fields(section).iter().map(|f| f.name).collect();
            State::Values(
                all.into_iter()
                    .filter(|(k, _)| names.contains(&k.as_str()))
                    .collect(),
            )
        }
        Section::BranchProtection => State::Items(
            forge
                .branch_rules(repo)
                .await
                .map_err(|e| refused(e, session, section))?,
        ),
        Section::Webhooks => State::Items(
            forge
                .webhooks(repo)
                .await
                .map_err(|e| refused(e, session, section))?,
        ),
    };
    Ok(state)
}

/// A section as the agent sees it: the fields, never the forge's own
/// record.
fn shown(state: &State) -> Value {
    match state {
        State::Values(v) => Value::Object(v.clone()),
        State::Items(items) => Value::Array(
            items
                .iter()
                .map(|i| Value::Object(i.fields.clone()))
                .collect(),
        ),
    }
}

async fn get(session: &Session, section: Option<Section>) -> ToolResult {
    if let Some(section) = section {
        let state = read(session, section).await?;
        return Ok(json!({ "section": section.name(), section.name(): shown(&state) }));
    }
    let mut out = Map::new();
    for section in Section::ALL {
        let value = match read(session, section).await {
            Ok(state) => shown(&state),
            Err(ToolError(e)) => json!({ "error": e }),
        };
        out.insert(section.name().into(), value);
    }
    Ok(Value::Object(out))
}

fn section_of(params: &RepoSettingsParams, action: &str) -> Result<Section, ToolError> {
    params
        .section
        .ok_or_else(|| ToolError(format!("`{action}` needs `section`")))
}

fn changes_of<'a>(params: &'a RepoSettingsParams, action: &str) -> Result<&'a Value, ToolError> {
    params
        .changes
        .as_ref()
        .ok_or_else(|| ToolError(format!("`{action}` needs `changes`")))
}

async fn plan_change(session: &Session, params: &RepoSettingsParams) -> ToolResult {
    let section = section_of(params, "plan")?;
    let changes = changes_of(params, "plan")?;
    let state = read(session, section).await?;
    let planned = plan::plan(section, &state, changes, kind(session)).map_err(ToolError)?;
    let described: Vec<Value> = planned
        .ops
        .iter()
        .flat_map(|op| describe(section, &state, op))
        .collect();
    let next = if planned.ops.is_empty() {
        "nothing to apply"
    } else {
        "to make this change, call apply with the same section and changes, and this fingerprint"
    };
    Ok(json!({
        "section": section.name(),
        "changes": described,
        "notes": planned.notes,
        "fingerprint": fingerprint(&state),
        "next": next,
    }))
}

/// What running a list of operations came to.
struct Executed {
    changes: Vec<Value>,
    undo: Vec<Op>,
    notes: Vec<String>,
    error: Option<String>,
    not_done: Vec<Value>,
}

/// Runs `ops` in order against a section that holds `state`, stopping at
/// the first the forge refuses. Each is sent once.
async fn execute(session: &Session, section: Section, state: &State, ops: &[Op]) -> Executed {
    let mut run = Executed {
        changes: Vec::new(),
        undo: Vec::new(),
        notes: Vec::new(),
        error: None,
        not_done: Vec::new(),
    };
    for op in ops {
        let described = describe(section, state, op);
        if run.error.is_some() {
            run.not_done.extend(described);
            continue;
        }
        match session.forge.apply_setting(&session.repo, op).await {
            Ok(done) => {
                let (back, note) = inverse(section, state, op, done);
                run.undo.extend(back);
                run.notes.extend(note);
                run.changes.extend(described);
            }
            Err(e) => {
                run.error = Some(refused(e, session, section).0);
                run.not_done.extend(described);
            }
        }
    }
    run.undo.reverse();
    run
}

/// The reply for an apply or a revert: what changed, what didn't, and
/// what undoes it.
async fn executed_reply(session: &Session, section: Section, run: Executed, verb: &str) -> Value {
    let after = read(session, section).await.ok().map(|s| fingerprint(&s));
    execution_reply(section, run, verb, after)
}

const REVERT_RECOVERY: &str = "read the current settings with get, compare them with the original plan and the reported changes and not_done, then plan and review a corrective change before applying it";

fn execution_reply(section: Section, run: Executed, verb: &str, after: Option<String>) -> Value {
    let mut notes = run.notes;
    if after.is_none() {
        notes.push(format!(
            "the {} settings couldn't be read back, so automatic revert is unavailable; {REVERT_RECOVERY}",
            section.name()
        ));
    }
    let outcome = match (&run.error, run.changes.is_empty()) {
        (None, _) => json!(true),
        (Some(_), true) => json!(false),
        (Some(_), false) => json!("partly"),
    };
    let mut reply = json!({
        verb: outcome,
        "section": section.name(),
        "changes": run.changes,
        "notes": notes,
    });
    if let Some(fingerprint) = after {
        reply["revert"] = json!(Revert {
            section,
            ops: run.undo,
            fingerprint,
        });
    }
    if let Some(error) = run.error {
        reply["error"] = json!(error);
        reply["not_done"] = json!(run.not_done);
    }
    reply
}

async fn apply(session: &Session, params: &RepoSettingsParams) -> ToolResult {
    let section = section_of(params, "apply")?;
    let changes = changes_of(params, "apply")?;
    let expected = params.fingerprint.as_deref().ok_or_else(|| {
        ToolError("`apply` needs the `fingerprint` from `plan`: plan first".into())
    })?;
    let state = read(session, section).await?;
    let now = fingerprint(&state);
    if now != expected {
        return Err(ToolError(format!(
            "the {} settings changed since the plan (fingerprint {expected}, now {now}); plan again and review the new changes",
            section.name()
        )));
    }
    let planned = plan::plan(section, &state, changes, kind(session)).map_err(ToolError)?;
    if planned.ops.is_empty() {
        return Ok(json!({
            "applied": false,
            "section": section.name(),
            "notes": planned.notes,
        }));
    }
    let mut run = execute(session, section, &state, &planned.ops).await;
    run.notes.splice(0..0, planned.notes);
    Ok(executed_reply(session, section, run, "applied").await)
}

/// The operations a section may hold, so a revert can't run, say, a
/// webhook deletion under `general`.
fn belongs(op: &Op, section: Section) -> bool {
    matches!(
        (op, section),
        (Op::SetRepo { .. }, Section::General | Section::Merge)
            | (
                Op::CreateRule { .. } | Op::EditRule { .. } | Op::DeleteRule { .. },
                Section::BranchProtection
            )
            | (
                Op::CreateWebhook { .. } | Op::EditWebhook { .. } | Op::DeleteWebhook { .. },
                Section::Webhooks
            )
    )
}

/// The `revert` object a call passed, as `apply` returned it: with the
/// section's fingerprint after the apply, or refused.
fn parse_revert(given: Option<&Value>) -> Result<Revert, ToolError> {
    let payload = given.cloned().ok_or_else(|| {
        ToolError("`revert` needs the `revert` object that `apply` returned".into())
    })?;
    serde_json::from_value(payload).map_err(|e| {
        ToolError(format!(
            "`revert` needs a valid object with a post-apply fingerprint: {e}; {REVERT_RECOVERY}"
        ))
    })
}

async fn revert(session: &Session, params: &RepoSettingsParams) -> ToolResult {
    let payload = parse_revert(params.revert.as_ref())?;
    let section = payload.section;
    if let Some(op) = payload.ops.iter().find(|op| !belongs(op, section)) {
        return Err(ToolError(format!(
            "`revert` has an operation that isn't about {}: {}",
            section.name(),
            serde_json::to_string(op).unwrap_or_default()
        )));
    }
    if let Some(Op::SetRepo { fields }) = payload
        .ops
        .iter()
        .find(|op| matches!(op, Op::SetRepo { .. }))
    {
        fields::check_all(section, fields, kind(session), None).map_err(ToolError)?;
    }
    let state = read(session, section).await?;
    let expected = &payload.fingerprint;
    let now = fingerprint(&state);
    if &now != expected {
        return Err(ToolError(format!(
            "the {} settings changed after the apply (fingerprint {expected}, now {now}), so reverting would undo those changes too; read them with get and plan a change instead",
            section.name()
        )));
    }
    if payload.ops.is_empty() {
        return Ok(
            json!({ "reverted": false, "section": section.name(), "notes": ["nothing to revert"] }),
        );
    }
    let run = execute(session, section, &state, &payload.ops).await;
    Ok(executed_reply(session, section, run, "reverted").await)
}

async fn hook_url(session: &Session, id: u64) -> Result<Value, ToolError> {
    let State::Items(hooks) = read(session, Section::Webhooks).await? else {
        return Ok(Value::Null);
    };
    hooks
        .iter()
        .find(|h| h.fields.get("id") == Some(&json!(id)))
        .map(|h| h.fields.get("url").cloned().unwrap_or(Value::Null))
        .ok_or_else(|| ToolError(format!("there is no webhook {id}")))
}

async fn test_webhook(session: &Session, params: &RepoSettingsParams) -> ToolResult {
    let id = params
        .webhook_id
        .ok_or_else(|| ToolError("`test_webhook` needs `webhook_id`".into()))?;
    let url = hook_url(session, id).await?;
    session
        .forge
        .test_webhook(&session.repo, id)
        .await
        .map_err(|e| refused(e, session, Section::Webhooks))?;
    let see = if kind(session) == crate::version::ForgeKind::Github {
        "GitHub sent a ping; webhook_deliveries shows how the receiver answered"
    } else {
        "the forge sends a test push delivery; its web UI shows how the receiver answered"
    };
    Ok(json!({ "tested": id, "url": url, "note": see }))
}

async fn deliveries(session: &Session, params: &RepoSettingsParams) -> ToolResult {
    let id = params
        .webhook_id
        .ok_or_else(|| ToolError("`webhook_deliveries` needs `webhook_id`".into()))?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_DELIVERIES)
        .clamp(1, MAX_DELIVERIES);
    let list = session
        .forge
        .webhook_deliveries(&session.repo, id, limit)
        .await
        .map_err(|e| refused(e, session, Section::Webhooks))?;
    Ok(json!({ "webhook": id, "count": list.len(), "deliveries": list }))
}

/// Every write-only value in `changes`, longest first, to keep out of
/// replies.
fn secrets_in(changes: Option<&Value>) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                for (k, v) in map {
                    let secret = fields::HOOK.iter().any(|f| f.secret && f.name == k);
                    match v.as_str() {
                        Some(s) if secret && !s.is_empty() => out.push(s.to_owned()),
                        _ => walk(v, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(changes) = changes {
        walk(changes, &mut out);
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out
}

fn redact_text(mut text: String, secrets: &[String]) -> String {
    for s in secrets {
        text = text.replace(s.as_str(), "(secret)");
    }
    text
}

fn redact(value: Value, secrets: &[String]) -> Value {
    if secrets.is_empty() {
        return value;
    }
    match value {
        Value::String(s) => Value::String(redact_text(s, secrets)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| redact(v, secrets)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (redact_text(k, secrets), redact(v, secrets)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revert_requires_a_post_apply_fingerprint() {
        for invalid in [
            json!({"section": "general", "ops": []}),
            json!({"section": "general", "ops": [], "fingerprint": null}),
        ] {
            let ToolError(message) = parse_revert(Some(&invalid)).unwrap_err();
            assert!(message.contains("post-apply fingerprint"), "{message}");
        }
        assert!(parse_revert(None).is_err());
        let valid = json!({"section": "general", "ops": [], "fingerprint": "0123456789abcdef"});
        assert_eq!(
            parse_revert(Some(&valid)).unwrap().section,
            Section::General
        );
    }

    #[test]
    fn partial_apply_without_read_back_reports_recovery_without_revert() {
        let reply = execution_reply(
            Section::General,
            Executed {
                changes: vec![json!("completed operation")],
                undo: vec![Op::SetRepo { fields: Map::new() }],
                notes: vec![],
                error: Some("write failed".into()),
                not_done: vec![json!("remaining operation")],
            },
            "applied",
            None,
        );
        assert_eq!(reply["applied"], "partly");
        assert_eq!(reply["changes"], json!(["completed operation"]));
        assert_eq!(reply["not_done"], json!(["remaining operation"]));
        assert!(reply.get("revert").is_none());
        assert!(
            reply["notes"][0]
                .as_str()
                .unwrap()
                .contains(REVERT_RECOVERY)
        );
    }

    #[test]
    fn secrets_are_found_anywhere_in_changes_and_scrubbed_from_replies() {
        let changes = json!({
            "create": [{"url": "https://ci.example.com/hook", "secret": "abc123"}],
            "update": [{"id": 2, "authorization_header": "Bearer zzz"}],
        });
        let secrets = secrets_in(Some(&changes));
        assert_eq!(secrets, vec!["Bearer zzz", "abc123"]);
        let reply = json!({"error": "rejected: secret abc123 too short", "echo": {"Bearer zzz": ["abc123"]}});
        let text = serde_json::to_string(&redact(reply, &secrets)).unwrap();
        assert!(!text.contains("abc123") && !text.contains("zzz"), "{text}");
    }

    #[test]
    fn a_revert_may_only_hold_its_own_sections_operations() {
        assert!(belongs(&Op::DeleteWebhook { id: 1 }, Section::Webhooks));
        assert!(!belongs(&Op::DeleteWebhook { id: 1 }, Section::General));
        assert!(belongs(&Op::SetRepo { fields: Map::new() }, Section::Merge));
        assert!(!belongs(
            &Op::DeleteRule {
                pattern: "main".into()
            },
            Section::Webhooks
        ));
    }
}
