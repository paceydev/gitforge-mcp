//! Repository settings on Gitea and Forgejo: the repository's own fields,
//! `branch_protections` and `hooks`, the same on both forks in every
//! version supported (checked against Gitea 1.22..v28 and Forgejo 9..17).
//!
//! | shared name | Gitea and Forgejo |
//! |---|---|
//! | `website` | `website` |
//! | `allowed_styles` | `allow_merge_commits`, `allow_rebase`, `allow_rebase_explicit`, `allow_squash_merge`, `allow_fast_forward_only_merge` |
//! | `default_style` | `default_merge_style` |
//! | `delete_branch_after_merge` | `default_delete_branch_after_merge` |
//! | rule `pattern` | `rule_name` (`branch_name` before rules had names) |
//! | `require_pull_request` | not `enable_push` |
//! | `push_allowlist` | `enable_push_whitelist` with `push_whitelist_usernames` and `_teams` |
//! | `required_checks` | `status_check_contexts`, when `enable_status_check` |
//! | `require_up_to_date` | `block_on_outdated_branch` |
//! | webhook `url`, `content_type`, `secret` | `config.url`, `config.content_type`, `config.secret` |

use serde_json::{Map, Value, json};

use super::GiteaFamily;
use crate::forge::{ApiError, segment};
use crate::pulls::MergeStyle;
use crate::remote::RepoRef;
use crate::settings::{Done, Item, Op, strip_credentials};

/// Each merge style and the repository flag that allows it.
const STYLE_FLAGS: [(MergeStyle, &str); 5] = [
    (MergeStyle::Merge, "allow_merge_commits"),
    (MergeStyle::Rebase, "allow_rebase"),
    (MergeStyle::RebaseMerge, "allow_rebase_explicit"),
    (MergeStyle::Squash, "allow_squash_merge"),
    (MergeStyle::FastForwardOnly, "allow_fast_forward_only_merge"),
];

/// Fields the forge sets itself, left out of what is compared and restored.
const STAMPS: [&str; 2] = ["created_at", "updated_at"];

const HOOK_PAGE: usize = 50;
const HOOK_PAGES: u32 = 10;

impl GiteaFamily {
    pub(super) async fn settings_repo(
        &self,
        repo: &RepoRef,
    ) -> Result<Map<String, Value>, ApiError> {
        let body: Value = self.http.get_json(&self.repo_api(repo, "")).await?;
        Ok(repo_fields(&body))
    }

    pub(super) async fn settings_rules(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        let body: Vec<Value> = self
            .http
            .get_json(&self.repo_api(repo, "/branch_protections"))
            .await?;
        Ok(body.into_iter().map(rule_item).collect())
    }

    pub(super) async fn settings_hooks(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        let hooks: Vec<Value> = self
            .http
            .paged(
                HOOK_PAGES,
                HOOK_PAGE,
                |page| self.repo_api(repo, &format!("/hooks?page={page}&limit={HOOK_PAGE}")),
                |page: Vec<Value>| page,
            )
            .await?;
        Ok(hooks.into_iter().map(hook_item).collect())
    }

    /// Runs one operation: one request, or for a webhook edit, a read and
    /// one request. Never retried.
    pub(super) async fn settings_apply(&self, repo: &RepoRef, op: &Op) -> Result<Done, ApiError> {
        match op {
            Op::SetRepo { fields } => {
                self.http
                    .patch_json(&self.repo_api(repo, ""), &Value::Object(repo_body(fields)))
                    .await?;
            }
            Op::CreateRule { fields, raw } => {
                let body = match raw {
                    Some(raw) => restorable(raw),
                    None => {
                        let mut body = rule_body(fields);
                        let pattern = fields.get("pattern").cloned().unwrap_or_default();
                        body.insert("rule_name".into(), pattern.clone());
                        body.insert("branch_name".into(), pattern);
                        body
                    }
                };
                self.http
                    .post_json(
                        &self.repo_api(repo, "/branch_protections"),
                        &Value::Object(body),
                    )
                    .await?;
            }
            Op::EditRule {
                pattern,
                fields,
                raw,
            } => {
                let body = match raw {
                    Some(raw) => restorable(raw),
                    None => rule_body(fields),
                };
                self.http
                    .patch_json(&self.rule_url(repo, pattern), &Value::Object(body))
                    .await?;
            }
            Op::DeleteRule { pattern } => self.http.delete(&self.rule_url(repo, pattern)).await?,
            Op::CreateWebhook { fields, raw } => {
                return self.create_hook(repo, fields, raw.as_ref()).await;
            }
            Op::EditWebhook { id, fields, raw } => {
                self.edit_hook(repo, *id, fields, raw.as_ref()).await?
            }
            Op::DeleteWebhook { id } => {
                self.http
                    .delete(&self.repo_api(repo, &format!("/hooks/{id}")))
                    .await?;
            }
        }
        Ok(Done::default())
    }

    /// Sends the webhook a test delivery, as for a push of the default
    /// branch's head.
    pub(super) async fn settings_test_hook(&self, repo: &RepoRef, id: u64) -> Result<(), ApiError> {
        self.http
            .post_json(
                &self.repo_api(repo, &format!("/hooks/{id}/tests")),
                &json!({}),
            )
            .await?;
        Ok(())
    }

    fn rule_url(&self, repo: &RepoRef, pattern: &str) -> String {
        self.repo_api(repo, &format!("/branch_protections/{}", segment(pattern)))
    }

    async fn create_hook(
        &self,
        repo: &RepoRef,
        fields: &Map<String, Value>,
        raw: Option<&Value>,
    ) -> Result<Done, ApiError> {
        let body = match raw {
            Some(raw) => hook_restore_body(raw),
            None => {
                let default_type = if self.is_forgejo() {
                    "forgejo"
                } else {
                    "gitea"
                };
                let mut body = hook_body(fields, &Map::new());
                body.insert(
                    "type".into(),
                    fields.get("type").cloned().unwrap_or(json!(default_type)),
                );
                body
            }
        };
        let created = self
            .http
            .post_json(&self.repo_api(repo, "/hooks"), &Value::Object(body))
            .await?;
        Ok(Done {
            created_id: created.and_then(|v| v["id"].as_u64()),
        })
    }

    /// Sends the whole webhook back with the changes on top: the forge
    /// resets a field left out of an edit (`branch_filter`).
    async fn edit_hook(
        &self,
        repo: &RepoRef,
        id: u64,
        fields: &Map<String, Value>,
        raw: Option<&Value>,
    ) -> Result<(), ApiError> {
        let url = self.repo_api(repo, &format!("/hooks/{id}"));
        let body = match raw {
            Some(raw) => hook_restore_body(raw),
            None => {
                let now: Value = self.http.get_json(&url).await?;
                hook_body(fields, &hook_item(now).fields)
            }
        };
        self.http.patch_json(&url, &Value::Object(body)).await?;
        Ok(())
    }
}

/// General and merge settings under their shared names. A repository with
/// pull requests turned off has no merge settings to change.
fn repo_fields(body: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let mut names = vec![
        ("description", "description"),
        ("website", "website"),
        ("default_branch", "default_branch"),
        ("private", "private"),
        ("has_issues", "has_issues"),
        ("has_wiki", "has_wiki"),
    ];
    let pulls = body["has_pull_requests"] != json!(false);
    if pulls {
        names.push(("default_style", "default_merge_style"));
        names.push((
            "delete_branch_after_merge",
            "default_delete_branch_after_merge",
        ));
    }
    for (shared, own) in names {
        if let Some(v) = body.get(own).filter(|v| !v.is_null()) {
            out.insert(shared.into(), v.clone());
        }
    }
    if pulls {
        let styles: Vec<&str> = STYLE_FLAGS
            .iter()
            .filter(|(_, flag)| body[*flag] == json!(true))
            .map(|(style, _)| style.as_forge())
            .collect();
        out.insert("allowed_styles".into(), json!(styles));
    }
    out
}

/// Gitea before v28 changes merge settings only in an edit that also says
/// pull requests are on; they are, or there'd be no merge settings to send.
fn repo_body(fields: &Map<String, Value>) -> Map<String, Value> {
    let mut body = Map::new();
    let merge = [
        "allowed_styles",
        "default_style",
        "delete_branch_after_merge",
    ];
    if fields.keys().any(|k| merge.contains(&k.as_str())) {
        body.insert("has_pull_requests".into(), json!(true));
    }
    for (k, v) in fields {
        match k.as_str() {
            "allowed_styles" => {
                let on: Vec<&str> = v
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                for (style, flag) in STYLE_FLAGS {
                    body.insert(flag.into(), json!(on.contains(&style.as_forge())));
                }
            }
            "default_style" => {
                body.insert("default_merge_style".into(), v.clone());
            }
            "delete_branch_after_merge" => {
                body.insert("default_delete_branch_after_merge".into(), v.clone());
            }
            _ => {
                body.insert(k.clone(), v.clone());
            }
        }
    }
    body
}

fn sorted_strings(v: &Value) -> Value {
    let mut items: Vec<String> = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    items.sort();
    items.dedup();
    json!(items)
}

fn rule_item(raw: Value) -> Item {
    let flag = |k: &str| raw[k].as_bool().unwrap_or(false);
    let name = raw["rule_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| raw["branch_name"].as_str())
        .unwrap_or_default();
    let allowlist = if flag("enable_push") && flag("enable_push_whitelist") {
        json!({
            "users": sorted_strings(&raw["push_whitelist_usernames"]),
            "teams": sorted_strings(&raw["push_whitelist_teams"]),
        })
    } else {
        Value::Null
    };
    let checks = if flag("enable_status_check") {
        sorted_strings(&raw["status_check_contexts"])
    } else {
        json!([])
    };
    let fields = json!({
        "pattern": name,
        "require_pull_request": !flag("enable_push"),
        "required_approvals": raw["required_approvals"].as_u64().unwrap_or(0),
        "dismiss_stale_approvals": flag("dismiss_stale_approvals"),
        "required_checks": checks,
        "require_up_to_date": flag("block_on_outdated_branch"),
        "require_signed_commits": flag("require_signed_commits"),
        "push_allowlist": allowlist,
        "block_on_rejected_reviews": flag("block_on_rejected_reviews"),
    });
    Item {
        fields: fields.as_object().cloned().unwrap_or_default(),
        raw: without_stamps(raw),
    }
}

fn without_stamps(mut raw: Value) -> Value {
    if let Some(map) = raw.as_object_mut() {
        for k in STAMPS {
            map.remove(k);
        }
    }
    raw
}

/// A rule as the forge returned it, as a body that puts it back. A list
/// the forge reports as `null` (never set) goes back as `[]`: in an edit,
/// `null` means "leave it as it is".
fn restorable(raw: &Value) -> Map<String, Value> {
    let mut body = raw.as_object().cloned().unwrap_or_default();
    for (k, v) in body.iter_mut() {
        if v.is_null() && is_list_field(k) {
            *v = json!([]);
        }
    }
    body
}

/// A rule's list fields: its status checks, and the users and teams of
/// each allowlist.
fn is_list_field(name: &str) -> bool {
    ["_contexts", "_usernames", "_username", "_teams"]
        .iter()
        .any(|end| name.ends_with(end))
}

/// The forge's fields for the shared ones in `fields`.
fn rule_body(fields: &Map<String, Value>) -> Map<String, Value> {
    let mut body = Map::new();
    for (k, v) in fields {
        match k.as_str() {
            "pattern" => {}
            "require_pull_request" => {
                body.insert("enable_push".into(), json!(v != &json!(true)));
            }
            "push_allowlist" if v.is_null() => {
                body.insert("enable_push_whitelist".into(), json!(false));
            }
            "push_allowlist" => {
                body.insert("enable_push".into(), json!(true));
                body.insert("enable_push_whitelist".into(), json!(true));
                body.insert("push_whitelist_usernames".into(), v["users"].clone());
                body.insert("push_whitelist_teams".into(), v["teams"].clone());
            }
            "required_checks" => {
                let any = v.as_array().is_some_and(|a| !a.is_empty());
                body.insert("enable_status_check".into(), json!(any));
                body.insert("status_check_contexts".into(), v.clone());
            }
            "require_up_to_date" => {
                body.insert("block_on_outdated_branch".into(), v.clone());
            }
            _ => {
                body.insert(k.clone(), v.clone());
            }
        }
    }
    body
}

fn hook_item(raw: Value) -> Item {
    let mut raw = without_stamps(strip_credentials(raw));
    // Gitea lists a webhook's events in no fixed order.
    raw["events"] = sorted_strings(&raw["events"]);
    let mut fields = Map::new();
    fields.insert("id".into(), raw["id"].clone());
    fields.insert("type".into(), raw["type"].clone());
    fields.insert("url".into(), raw["config"]["url"].clone());
    fields.insert("content_type".into(), raw["config"]["content_type"].clone());
    fields.insert("events".into(), sorted_strings(&raw["events"]));
    fields.insert(
        "active".into(),
        json!(raw["active"].as_bool().unwrap_or(false)),
    );
    fields.insert(
        "branch_filter".into(),
        json!(raw["branch_filter"].as_str().unwrap_or_default()),
    );
    Item { fields, raw }
}

/// A webhook's body: `base` (the webhook as it is now, or nothing for a
/// new one) with `fields` on top.
fn hook_body(fields: &Map<String, Value>, base: &Map<String, Value>) -> Map<String, Value> {
    let value = |k: &str| fields.get(k).or_else(|| base.get(k)).cloned();
    let mut config = Map::new();
    for k in ["url", "content_type", "secret"] {
        if let Some(v) = value(k) {
            config.insert(k.into(), v);
        }
    }
    let mut body = Map::new();
    body.insert("config".into(), Value::Object(config));
    for k in ["events", "active", "branch_filter", "authorization_header"] {
        if let Some(v) = value(k) {
            body.insert(k.into(), v);
        }
    }
    body
}

/// A webhook as the forge returned it, as a body that puts it back: every
/// field but its id, which a new webhook gets from the forge.
fn hook_restore_body(raw: &Value) -> Map<String, Value> {
    let mut body = raw.as_object().cloned().unwrap_or_default();
    body.remove("id");
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_styles_read_from_and_write_to_their_flags() {
        let fields = repo_fields(&json!({
            "description": "d", "website": "", "default_branch": "main", "private": false,
            "has_issues": true, "has_wiki": false, "allow_merge_commits": true, "allow_rebase": false,
            "allow_rebase_explicit": false, "allow_squash_merge": true,
            "allow_fast_forward_only_merge": false, "default_merge_style": "squash",
            "default_delete_branch_after_merge": true,
        }));
        assert_eq!(fields["allowed_styles"], json!(["merge", "squash"]));
        assert_eq!(fields["default_style"], "squash");
        let body = repo_body(&fields);
        assert_eq!(body["allow_squash_merge"], true);
        assert_eq!(body["allow_rebase"], false);
        assert_eq!(body["default_merge_style"], "squash");
        assert_eq!(body["default_delete_branch_after_merge"], true);
        assert_eq!(body["has_pull_requests"], true);
        assert!(!body.contains_key("allowed_styles"));
        let no_pulls =
            repo_fields(&json!({"has_pull_requests": false, "allow_merge_commits": true}));
        assert!(!no_pulls.contains_key("allowed_styles"));
        let general = json!({"description": "x"});
        assert!(!repo_body(general.as_object().unwrap()).contains_key("has_pull_requests"));
    }

    #[test]
    fn a_restored_rule_sends_never_set_lists_as_empty() {
        let body = restorable(&json!({"status_check_contexts": null, "required_approvals": null}));
        assert_eq!(body["status_check_contexts"], json!([]));
        assert!(body["required_approvals"].is_null());
    }

    #[test]
    fn a_rule_reads_in_shared_terms_and_writes_back_the_same() {
        let raw = json!({
            "rule_name": "release/*", "branch_name": "", "enable_push": true,
            "enable_push_whitelist": true, "push_whitelist_usernames": ["b", "a"],
            "push_whitelist_teams": [], "enable_status_check": true,
            "status_check_contexts": ["ci / test"], "required_approvals": 1,
            "block_on_outdated_branch": true, "created_at": "2026-11-13T00:00:00Z",
        });
        let item = rule_item(raw);
        assert_eq!(item.fields["pattern"], "release/*");
        assert_eq!(item.fields["require_pull_request"], false);
        assert_eq!(
            item.fields["push_allowlist"],
            json!({"users": ["a", "b"], "teams": []})
        );
        assert_eq!(item.fields["required_checks"], json!(["ci / test"]));
        assert!(item.raw.get("created_at").is_none());
        let body = rule_body(&item.fields);
        assert_eq!(body["enable_push"], true);
        assert_eq!(body["enable_status_check"], true);
        assert_eq!(body["block_on_outdated_branch"], true);
        assert!(!body.contains_key("pattern"));
    }

    #[test]
    fn a_webhook_edit_keeps_what_it_doesnt_change_and_never_reads_a_secret() {
        let item = hook_item(json!({
            "id": 4, "type": "forgejo", "active": true, "branch_filter": "main",
            "events": ["push"], "authorization_header": "Bearer x",
            "config": {"url": "https://ci.example.com/hook", "content_type": "json", "secret": "s"},
        }));
        assert!(!serde_json::to_string(&item).unwrap().contains("Bearer"));
        assert!(item.raw["config"].get("secret").is_none());
        let changes = json!({"active": false, "secret": "new"});
        let body = hook_body(changes.as_object().unwrap(), &item.fields);
        assert_eq!(body["branch_filter"], "main");
        assert_eq!(body["active"], false);
        assert_eq!(
            body["config"],
            json!({"url": "https://ci.example.com/hook", "content_type": "json", "secret": "new"})
        );
    }
}
