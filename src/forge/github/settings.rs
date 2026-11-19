//! Repository settings on GitHub: the repository's own fields, classic
//! branch protection, and webhooks (all in GHES 3.14's API description).
//!
//! Classic branch protection is per branch: a rule's `pattern` is the name
//! of an existing branch, not a wildcard. Its PUT replaces the whole rule,
//! so an edit reads the rule, changes the fields asked for, and sends all
//! of it back; signed commits are a separate endpoint.
//!
//! | shared name | GitHub |
//! |---|---|
//! | `website` | `homepage` |
//! | `allowed_styles` | `allow_merge_commit`, `allow_rebase_merge`, `allow_squash_merge` |
//! | `delete_branch_after_merge` | `delete_branch_on_merge` |
//! | `require_pull_request` | `required_pull_request_reviews` is set |
//! | `required_approvals` | `required_pull_request_reviews.required_approving_review_count` |
//! | `required_checks`, `require_up_to_date` | `required_status_checks.checks`, `.strict` |
//! | `push_allowlist` | `restrictions` |
//! | `require_signed_commits` | `protection/required_signatures` |

use serde_json::{Map, Value, json};

use super::Github;
use crate::forge::{ApiError, segment};
use crate::pulls::MergeStyle;
use crate::remote::RepoRef;
use crate::settings::{Delivery, Done, Item, Op, strip_credentials};

const STYLE_FLAGS: [(MergeStyle, &str); 3] = [
    (MergeStyle::Merge, "allow_merge_commit"),
    (MergeStyle::Rebase, "allow_rebase_merge"),
    (MergeStyle::Squash, "allow_squash_merge"),
];

/// The fields of a protection that are `{"enabled": bool}` when read and a
/// plain bool when written.
const TOGGLES: [&str; 7] = [
    "required_linear_history",
    "allow_force_pushes",
    "allow_deletions",
    "block_creations",
    "required_conversation_resolution",
    "lock_branch",
    "allow_fork_syncing",
];

const PAGE: usize = 100;
const PAGES: u32 = 10;

impl Github {
    pub(super) async fn settings_repo(
        &self,
        repo: &RepoRef,
    ) -> Result<Map<String, Value>, ApiError> {
        let body: Value = self.http.get_json(&self.repo_api(repo, "")).await?;
        Ok(repo_fields(&body))
    }

    pub(super) async fn settings_rules(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        let branches: Vec<String> = self
            .http
            .paged(
                PAGES,
                PAGE,
                |page| {
                    self.repo_api(
                        repo,
                        &format!("/branches?protected=true&per_page={PAGE}&page={page}"),
                    )
                },
                |page: Vec<Value>| {
                    page.into_iter()
                        .filter_map(|b| b["name"].as_str().map(str::to_owned))
                        .collect()
                },
            )
            .await?;
        let mut items = Vec::new();
        for branch in branches {
            if let Some(raw) = self
                .http
                .get_json_opt::<Value>(&self.protection_url(repo, &branch))
                .await?
            {
                items.push(rule_item(&branch, raw));
            }
        }
        Ok(items)
    }

    pub(super) async fn settings_hooks(&self, repo: &RepoRef) -> Result<Vec<Item>, ApiError> {
        let hooks: Vec<Value> = self
            .http
            .paged(
                PAGES,
                PAGE,
                |page| self.repo_api(repo, &format!("/hooks?per_page={PAGE}&page={page}")),
                |page: Vec<Value>| page,
            )
            .await?;
        Ok(hooks.into_iter().map(hook_item).collect())
    }

    pub(super) async fn settings_apply(&self, repo: &RepoRef, op: &Op) -> Result<Done, ApiError> {
        match op {
            Op::SetRepo { fields } => {
                self.http
                    .patch_json(&self.repo_api(repo, ""), &Value::Object(repo_body(fields)))
                    .await?;
            }
            Op::CreateRule { fields, raw } => {
                let pattern = fields
                    .get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.put_rule(repo, pattern, fields, raw.as_ref(), None)
                    .await?;
            }
            Op::EditRule {
                pattern,
                fields,
                raw,
            } => {
                let now = match raw {
                    Some(_) => None,
                    None => Some(
                        self.http
                            .get_json::<Value>(&self.protection_url(repo, pattern))
                            .await?,
                    ),
                };
                self.put_rule(repo, pattern, fields, raw.as_ref(), now.as_ref())
                    .await?;
            }
            Op::DeleteRule { pattern } => {
                self.http
                    .delete(&self.protection_url(repo, pattern))
                    .await?;
            }
            Op::CreateWebhook { fields, raw } => {
                let body = raw
                    .as_ref()
                    .map_or_else(|| new_hook_body(fields), restore_hook_body);
                let created = self
                    .http
                    .post_json(&self.repo_api(repo, "/hooks"), &Value::Object(body))
                    .await?;
                return Ok(Done {
                    created_id: created.and_then(|v| v["id"].as_u64()),
                });
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

    /// A ping: unlike a test push, it reaches a webhook whatever its events.
    pub(super) async fn settings_test_hook(&self, repo: &RepoRef, id: u64) -> Result<(), ApiError> {
        self.http
            .post_json(
                &self.repo_api(repo, &format!("/hooks/{id}/pings")),
                &json!({}),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn settings_deliveries(
        &self,
        repo: &RepoRef,
        id: u64,
        limit: u32,
    ) -> Result<Vec<Delivery>, ApiError> {
        let body: Vec<Value> = self
            .http
            .get_json(&self.repo_api(repo, &format!("/hooks/{id}/deliveries?per_page={limit}")))
            .await?;
        Ok(body.iter().map(delivery).collect())
    }

    fn protection_url(&self, repo: &RepoRef, branch: &str) -> String {
        self.repo_api(repo, &format!("/branches/{}/protection", segment(branch)))
    }

    /// Writes a whole rule: `raw` as it was, or the rule as it is now (or
    /// a new one) with `fields` on top. Then signed commits, which have an
    /// endpoint of their own.
    async fn put_rule(
        &self,
        repo: &RepoRef,
        branch: &str,
        fields: &Map<String, Value>,
        raw: Option<&Value>,
        now: Option<&Value>,
    ) -> Result<(), ApiError> {
        let (body, signed) = match raw {
            Some(raw) => (
                put_body(raw),
                Some(raw["required_signatures"]["enabled"] == json!(true)),
            ),
            None => {
                let mut body = now.map_or_else(empty_rule, put_body);
                overlay(&mut body, fields);
                (
                    body,
                    fields
                        .get("require_signed_commits")
                        .and_then(Value::as_bool),
                )
            }
        };
        let url = self.protection_url(repo, branch);
        self.http.put_json(&url, &Value::Object(body)).await?;
        let signatures = format!("{url}/required_signatures");
        match signed {
            Some(true) => {
                self.http.post_json(&signatures, &json!({})).await?;
            }
            // Already off is as asked.
            Some(false) => match self.http.delete(&signatures).await {
                Ok(()) | Err(ApiError::NotFound { .. }) => {}
                Err(e) => return Err(e),
            },
            None => {}
        }
        Ok(())
    }

    /// Config fields go to the webhook's config endpoint, which changes
    /// only what it's sent; events and `active` to the webhook itself.
    async fn edit_hook(
        &self,
        repo: &RepoRef,
        id: u64,
        fields: &Map<String, Value>,
        raw: Option<&Value>,
    ) -> Result<(), ApiError> {
        let (config, rest) = match raw {
            Some(raw) => (
                raw["config"].as_object().cloned().unwrap_or_default(),
                pick(raw, &["events", "active"]),
            ),
            None => (
                pick(
                    &Value::Object(fields.clone()),
                    &["url", "content_type", "secret"],
                ),
                pick(&Value::Object(fields.clone()), &["events", "active"]),
            ),
        };
        let hook = self.repo_api(repo, &format!("/hooks/{id}"));
        if !config.is_empty() {
            self.http
                .patch_json(&format!("{hook}/config"), &Value::Object(config))
                .await?;
        }
        if !rest.is_empty() {
            self.http.patch_json(&hook, &Value::Object(rest)).await?;
        }
        Ok(())
    }
}

fn pick(from: &Value, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|k| from.get(*k).map(|v| ((*k).to_owned(), v.clone())))
        .collect()
}

fn text(v: &Value) -> Value {
    json!(v.as_str().unwrap_or_default())
}

fn repo_fields(body: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("description".into(), text(&body["description"]));
    out.insert("website".into(), text(&body["homepage"]));
    for k in ["default_branch", "private", "has_issues", "has_wiki"] {
        if let Some(v) = body.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    // GitHub shows these only to admins and those who may push.
    if STYLE_FLAGS
        .iter()
        .any(|(_, flag)| body.get(*flag).is_some())
    {
        let styles: Vec<&str> = STYLE_FLAGS
            .iter()
            .filter(|(_, flag)| body[*flag] == json!(true))
            .map(|(style, _)| style.as_forge())
            .collect();
        out.insert("allowed_styles".into(), json!(styles));
    }
    for (shared, own) in [
        ("delete_branch_after_merge", "delete_branch_on_merge"),
        ("allow_auto_merge", "allow_auto_merge"),
    ] {
        if let Some(v) = body.get(own).filter(|v| !v.is_null()) {
            out.insert(shared.into(), v.clone());
        }
    }
    out
}

fn repo_body(fields: &Map<String, Value>) -> Map<String, Value> {
    let mut body = Map::new();
    for (k, v) in fields {
        match k.as_str() {
            "website" => {
                body.insert("homepage".into(), v.clone());
            }
            "delete_branch_after_merge" => {
                body.insert("delete_branch_on_merge".into(), v.clone());
            }
            "allowed_styles" => {
                let on: Vec<&str> = v
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                for (style, flag) in STYLE_FLAGS {
                    body.insert(flag.into(), json!(on.contains(&style.as_forge())));
                }
            }
            _ => {
                body.insert(k.clone(), v.clone());
            }
        }
    }
    body
}

/// The sorted `key` of each object in `list` (`login` of users, `slug` of
/// teams and apps).
fn names(list: &Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> = list
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x[key].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn rule_item(branch: &str, raw: Value) -> Item {
    let reviews = &raw["required_pull_request_reviews"];
    let checks = &raw["required_status_checks"];
    let mut contexts: Vec<String> = checks["contexts"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    contexts.sort();
    let restrictions = &raw["restrictions"];
    let allowlist = if restrictions.is_object() {
        json!({ "users": names(&restrictions["users"], "login"), "teams": names(&restrictions["teams"], "slug") })
    } else {
        Value::Null
    };
    let fields = json!({
        "pattern": branch,
        "require_pull_request": reviews.is_object(),
        "required_approvals": reviews["required_approving_review_count"].as_u64().unwrap_or(0),
        "dismiss_stale_approvals": reviews["dismiss_stale_reviews"] == json!(true),
        "required_checks": contexts,
        "require_up_to_date": checks["strict"] == json!(true),
        "require_signed_commits": raw["required_signatures"]["enabled"] == json!(true),
        "push_allowlist": allowlist,
        "enforce_admins": raw["enforce_admins"]["enabled"] == json!(true),
    });
    Item {
        fields: fields.as_object().cloned().unwrap_or_default(),
        raw,
    }
}

fn empty_rule() -> Map<String, Value> {
    json!({
        "required_status_checks": null,
        "enforce_admins": false,
        "required_pull_request_reviews": null,
        "restrictions": null,
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// `{"users": [...], "teams": [...], "apps": [...]}` from a read list of
/// people, teams and apps.
fn who(read: &Value) -> Value {
    json!({
        "users": names(&read["users"], "login"),
        "teams": names(&read["teams"], "slug"),
        "apps": names(&read["apps"], "slug"),
    })
}

/// A protection as GitHub returns it, as the body that writes it back.
fn put_body(raw: &Value) -> Map<String, Value> {
    let mut body = empty_rule();
    let checks = &raw["required_status_checks"];
    if checks.is_object() {
        let list: Vec<Value> = checks["checks"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|c| json!({ "context": c["context"], "app_id": c["app_id"] }))
            .collect();
        body.insert(
            "required_status_checks".into(),
            json!({ "strict": checks["strict"] == json!(true), "checks": list }),
        );
    }
    body.insert(
        "enforce_admins".into(),
        json!(raw["enforce_admins"]["enabled"] == json!(true)),
    );
    let reviews = &raw["required_pull_request_reviews"];
    if reviews.is_object() {
        let mut r = pick(
            reviews,
            &[
                "dismiss_stale_reviews",
                "require_code_owner_reviews",
                "required_approving_review_count",
                "require_last_push_approval",
            ],
        );
        for k in ["dismissal_restrictions", "bypass_pull_request_allowances"] {
            if reviews[k].is_object() {
                r.insert(k.into(), who(&reviews[k]));
            }
        }
        body.insert("required_pull_request_reviews".into(), Value::Object(r));
    }
    if raw["restrictions"].is_object() {
        body.insert("restrictions".into(), who(&raw["restrictions"]));
    }
    for k in TOGGLES {
        if let Some(enabled) = raw[k]["enabled"].as_bool() {
            body.insert(k.into(), json!(enabled));
        }
    }
    body
}

/// `fields`, in shared names, written into a protection body.
fn overlay(body: &mut Map<String, Value>, fields: &Map<String, Value>) {
    let reviews = |body: &mut Map<String, Value>| -> Map<String, Value> {
        body.get("required_pull_request_reviews")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| {
                json!({"required_approving_review_count": 0})
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
            })
    };
    if let Some(on) = fields.get("require_pull_request").and_then(Value::as_bool) {
        let value = if on {
            Value::Object(reviews(body))
        } else {
            Value::Null
        };
        body.insert("required_pull_request_reviews".into(), value);
    }
    for (shared, own) in [
        ("required_approvals", "required_approving_review_count"),
        ("dismiss_stale_approvals", "dismiss_stale_reviews"),
    ] {
        if let Some(v) = fields.get(shared) {
            let mut r = reviews(body);
            r.insert(own.into(), v.clone());
            body.insert("required_pull_request_reviews".into(), Value::Object(r));
        }
    }
    overlay_checks(body, fields);
    if let Some(list) = fields.get("push_allowlist") {
        let value = if list.is_null() {
            Value::Null
        } else {
            let apps = body["restrictions"]["apps"].clone();
            json!({ "users": list["users"], "teams": list["teams"], "apps": if apps.is_array() { apps } else { json!([]) } })
        };
        body.insert("restrictions".into(), value);
    }
    if let Some(v) = fields.get("enforce_admins") {
        body.insert("enforce_admins".into(), v.clone());
    }
}

/// Required checks keep the app each was tied to when it stays listed.
fn overlay_checks(body: &mut Map<String, Value>, fields: &Map<String, Value>) {
    if !fields.contains_key("required_checks") && !fields.contains_key("require_up_to_date") {
        return;
    }
    let now = body
        .get("required_status_checks")
        .cloned()
        .unwrap_or(Value::Null);
    let strict = fields
        .get("require_up_to_date")
        .and_then(Value::as_bool)
        .unwrap_or(now["strict"] == json!(true));
    let checks: Vec<Value> = match fields.get("required_checks").and_then(Value::as_array) {
        Some(contexts) => contexts
            .iter()
            .map(|c| {
                let app = now["checks"]
                    .as_array()
                    .and_then(|a| a.iter().find(|x| x["context"] == *c))
                    .map_or(Value::Null, |x| x["app_id"].clone());
                json!({ "context": c, "app_id": app })
            })
            .collect(),
        None => now["checks"].as_array().cloned().unwrap_or_default(),
    };
    let value = if checks.is_empty() && !strict {
        Value::Null
    } else {
        json!({ "strict": strict, "checks": checks })
    };
    body.insert("required_status_checks".into(), value);
}

fn hook_item(raw: Value) -> Item {
    let raw = json!({
        "id": raw["id"],
        "name": raw["name"],
        "active": raw["active"],
        "events": raw["events"],
        "config": strip_credentials(raw["config"].clone()),
    });
    let mut events: Vec<String> = raw["events"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    events.sort();
    let fields = json!({
        "id": raw["id"],
        "url": raw["config"]["url"],
        "content_type": raw["config"]["content_type"],
        "events": events,
        "active": raw["active"].as_bool().unwrap_or(false),
    });
    Item {
        fields: fields.as_object().cloned().unwrap_or_default(),
        raw,
    }
}

fn new_hook_body(fields: &Map<String, Value>) -> Map<String, Value> {
    let config = pick(
        &Value::Object(fields.clone()),
        &["url", "content_type", "secret"],
    );
    let mut body = pick(&Value::Object(fields.clone()), &["events", "active"]);
    body.insert("name".into(), json!("web"));
    body.insert("config".into(), Value::Object(config));
    body
}

fn restore_hook_body(raw: &Value) -> Map<String, Value> {
    let mut body = raw.as_object().cloned().unwrap_or_default();
    body.remove("id");
    body
}

fn delivery(v: &Value) -> Delivery {
    Delivery {
        id: v["id"].as_u64().unwrap_or_default(),
        event: v["event"].as_str().unwrap_or_default().to_owned(),
        action: v["action"].as_str().map(str::to_owned),
        delivered_at: v["delivered_at"].as_str().unwrap_or_default().to_owned(),
        status: v["status"].as_str().unwrap_or_default().to_owned(),
        status_code: v["status_code"].as_u64().unwrap_or_default(),
        duration_seconds: v["duration"].as_f64().unwrap_or_default(),
        redelivery: v["redelivery"].as_bool().unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protection() -> Value {
        json!({
            "required_status_checks": {"strict": true, "contexts": ["test"], "checks": [{"context": "test", "app_id": 15368}]},
            "enforce_admins": {"enabled": false},
            "required_pull_request_reviews": {"dismiss_stale_reviews": false, "require_code_owner_reviews": true, "required_approving_review_count": 1},
            "restrictions": {"users": [{"login": "dev"}], "teams": [], "apps": [{"slug": "ci-bot"}]},
            "required_signatures": {"enabled": true},
            "required_linear_history": {"enabled": true},
            "allow_force_pushes": {"enabled": false},
        })
    }

    #[test]
    fn a_protection_reads_in_shared_terms() {
        let item = rule_item("main", protection());
        assert_eq!(item.fields["require_pull_request"], true);
        assert_eq!(item.fields["required_approvals"], 1);
        assert_eq!(item.fields["required_checks"], json!(["test"]));
        assert_eq!(item.fields["require_up_to_date"], true);
        assert_eq!(item.fields["require_signed_commits"], true);
        assert_eq!(
            item.fields["push_allowlist"],
            json!({"users": ["dev"], "teams": []})
        );
    }

    #[test]
    fn an_edit_keeps_everything_it_doesnt_name() {
        let mut body = put_body(&protection());
        let fields = json!({"required_approvals": 2, "required_checks": ["test", "lint"]});
        overlay(&mut body, fields.as_object().unwrap());
        assert_eq!(
            body["required_pull_request_reviews"]["required_approving_review_count"],
            2
        );
        assert_eq!(
            body["required_pull_request_reviews"]["require_code_owner_reviews"],
            true
        );
        assert_eq!(body["required_linear_history"], true);
        assert_eq!(body["restrictions"]["apps"], json!(["ci-bot"]));
        assert_eq!(
            body["required_status_checks"],
            json!({"strict": true, "checks": [{"context": "test", "app_id": 15368}, {"context": "lint", "app_id": null}]})
        );
    }

    #[test]
    fn turning_pull_requests_and_checks_off_clears_their_objects() {
        let mut body = put_body(&protection());
        let fields = json!({"require_pull_request": false, "required_checks": [], "require_up_to_date": false, "push_allowlist": null});
        overlay(&mut body, fields.as_object().unwrap());
        assert!(body["required_pull_request_reviews"].is_null());
        assert!(body["required_status_checks"].is_null());
        assert!(body["restrictions"].is_null());
    }

    #[test]
    fn a_webhook_never_carries_its_secret() {
        let item = hook_item(json!({
            "id": 1, "name": "web", "active": true, "events": ["push", "pull_request"],
            "config": {"url": "https://ci.example.com/hook", "content_type": "json", "secret": "********", "insecure_ssl": "0"},
            "last_response": {"code": 200},
        }));
        assert!(!serde_json::to_string(&item).unwrap().contains("****"));
        assert_eq!(item.fields["events"], json!(["pull_request", "push"]));
        assert!(item.raw.get("last_response").is_none());
    }
}
