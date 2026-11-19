//! The MCP server: one stdio server bound to one repository.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool,
};
use rmcp::model::{
    CancelTaskParams, CreateTaskResult, GetTaskParams, GetTaskResult, ProgressNotificationParam,
    RequestMetaObject, UpdateTaskParams,
};
use rmcp::service::RequestContext;
use rmcp::task_manager::{TaskExit, TaskManager, TaskOptions};
use rmcp::{ErrorData as McpError, Peer, RoleServer, ServerHandler, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::cache::Cache;
use crate::capability::{Capabilities, Feature, Gate};
use crate::context::{self, Options, Report, Session};
use crate::repo_settings::{self, RepoSettingsParams};
use crate::tools::{
    self, CiControlParams, CiEstimateParams, CiLogsParams, CiStatusParams, CiWaitParams,
    PullRequestParams, ToolError,
};

/// Every tool and when it is offered. A tool missing from this table is
/// never offered: gating fails closed.
const GATES: &[(&str, Gate)] = &[
    ("forge_context", Gate::Always),
    (
        "ci_status",
        Gate::Any(&[Feature::CiRuns, Feature::CiStatuses]),
    ),
    // CiLogs needs CiRuns on every forge, and comes alone from CI outside it.
    ("ci_logs", Gate::All(&[Feature::CiLogs])),
    ("ci_runners", Gate::All(&[Feature::CiRunners])),
    ("pull_request", Gate::All(&[Feature::PullRequestRead])),
    (
        "ci_wait",
        Gate::Any(&[Feature::CiRuns, Feature::CiStatuses]),
    ),
    // Estimates learn from the forge's own run history.
    ("ci_estimate", Gate::All(&[Feature::CiRuns])),
    (
        "ci_control",
        Gate::Any(&[
            Feature::CiCancel,
            Feature::CiRerun,
            Feature::CiRerunFailed,
            Feature::CiDispatch,
        ]),
    ),
    (
        "repo_settings",
        Gate::Any(&[
            Feature::RepoSettingsRead,
            Feature::RepoSettings,
            Feature::WebhookDeliveries,
        ]),
    ),
];

/// Tools whose `action` is gated action by action: each action, the
/// feature it needs, and the action used when none is given. The tool's
/// schema lists only the actions available, and a call naming any other is
/// refused.
/// A tool, its default action, and each action with the feature it needs.
type ActionTable = (
    &'static str,
    Option<&'static str>,
    &'static [(&'static str, Feature)],
);

const ACTIONS: &[ActionTable] = &[
    (
        "pull_request",
        Some("list"),
        &[
            ("list", Feature::PullRequestRead),
            ("get", Feature::PullRequestRead),
            ("reviews", Feature::PullRequestRead),
            ("create", Feature::PullRequestWrite),
            ("merge", Feature::PullRequestWrite),
            ("merge_when_ready", Feature::PullRequestAutoMerge),
            ("cancel_auto_merge", Feature::PullRequestAutoMerge),
        ],
    ),
    (
        "ci_control",
        None,
        &[
            ("cancel", Feature::CiCancel),
            ("rerun", Feature::CiRerun),
            ("rerun_failed", Feature::CiRerunFailed),
            ("dispatch", Feature::CiDispatch),
        ],
    ),
    (
        "repo_settings",
        Some("get"),
        &[
            ("get", Feature::RepoSettingsRead),
            ("plan", Feature::RepoSettingsRead),
            ("apply", Feature::RepoSettings),
            ("revert", Feature::RepoSettings),
            ("test_webhook", Feature::RepoSettings),
            ("webhook_deliveries", Feature::WebhookDeliveries),
        ],
    ),
];

/// How long a client may cache the tool list. Short, so a forge that comes
/// back up (or a token that gains access) shows its tools without a restart.
const TOOL_LIST_TTL: Duration = Duration::from_secs(300);

/// A ci_wait task outlives its wait by this much, for the last poll.
const TASK_TTL_MARGIN_SECONDS: u64 = 300;
const MS_PER_SECOND: u64 = 1000;
/// How often a client is asked to poll a ci_wait task.
const TASK_POLL_INTERVAL_MS: u64 = 5_000;
/// How long the first `tools/list` waits for the forge before answering
/// with the always-on tools alone.
const FIRST_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

const INSTRUCTIONS: &str = "gitforge-mcp works on the git repository this server was started in, \
on whichever forge its remote points at (Gitea, Forgejo or GitHub). Only tools this forge, its \
version and your access support are listed. Call forge_context to see which repository and \
forge this is, who you are signed in as, and why any capability is unavailable. CI tools look at \
the checkout's HEAD unless given a sha, branch or run number.";

/// The instructions, with how to wait for CI without holding up the
/// conversation: an MCP tool call blocks the turn unless the client runs it
/// as a task, but a shell command can run in the background.
fn instructions(dir: &std::path::Path) -> String {
    format!(
        "{INSTRUCTIONS}\n\nTo wait for CI without blocking: if you can run shell commands in the \
background, prefer running `{}` (add one of --sha, --branch or --run to choose the commit) in the \
background over calling ci_wait. {BACKGROUND_WAIT}",
        background_wait(dir)
    )
}

/// What the background `wait` command does, for the instructions and
/// ci_wait's description.
const BACKGROUND_WAIT: &str = "It prints a progress line whenever CI moves on (jobs finished, \
running and at which step, waiting, and when it will likely end), then the ci_wait result as \
JSON; exit code 0 means CI passed, 1 failed, 2 not finished or no runs, 3 couldn't check, 64 the \
command line was wrong (fix it, don't re-run it). Read its output at any time to see where CI has got to.";

/// The command that waits for CI on the bound repository in the
/// background, with this binary's full path so it works without PATH.
fn background_wait(dir: &std::path::Path) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "gitforge-mcp".into());
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let (prefix, quote): (&str, fn(&str) -> String) = if cfg!(windows) {
        ("& ", powershell_quote)
    } else {
        ("", shell_quote)
    };
    format!(
        "{prefix}{} wait --dir {}",
        quote(&exe),
        quote(&dir.display().to_string())
    )
}

/// A literal PowerShell argument. The call operator invokes a quoted executable.
fn powershell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `s` in double quotes for a POSIX shell, with the characters that are
/// still special there escaped.
fn shell_quote(s: &str) -> String {
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// ci_wait's description, ending with the command to run instead when the
/// agent can run it in the background: an agent may read tool
/// descriptions without the server's instructions.
fn describe_ci_wait(tools: &mut [Tool], dir: &std::path::Path) {
    for tool in tools.iter_mut().filter(|t| t.name == "ci_wait") {
        let base = tool.description.as_deref().unwrap_or_default();
        tool.description = Some(
            format!(
                "{base} To wait without holding up the conversation, if you can run shell commands \
in the background, run `{}` (add one of --sha, --branch or --run to choose the commit) in the background \
instead of calling this tool. {BACKGROUND_WAIT}",
                background_wait(dir)
            )
            .into(),
        );
    }
}

fn gate_for(name: &str) -> Option<Gate> {
    GATES.iter().find(|(n, _)| *n == name).map(|(_, g)| *g)
}

/// The tools `caps` allows, in name order, each with only its allowed
/// actions in its schema, and marked read-only when none of them write.
pub fn offered(all: Vec<Tool>, caps: Option<&Capabilities>) -> Vec<Tool> {
    all.into_iter()
        .filter(|t| gate_for(&t.name).is_some_and(|g| g.allows(caps)))
        .map(|t| shape(t, caps))
        .collect()
}

/// The actions of `tool` that `caps` allows. `None` for a tool without
/// gated actions.
pub fn allowed_actions(tool: &str, caps: Option<&Capabilities>) -> Option<Vec<&'static str>> {
    let (_, _, actions) = ACTIONS.iter().find(|(name, _, _)| *name == tool)?;
    Some(
        actions
            .iter()
            .filter(|(_, feature)| caps.is_some_and(|c| c.is_available(*feature)))
            .map(|(action, _)| *action)
            .collect(),
    )
}

fn shape(mut tool: Tool, caps: Option<&Capabilities>) -> Tool {
    let Some(allowed) = allowed_actions(&tool.name, caps) else {
        return tool;
    };
    let mut schema = Value::Object((*tool.input_schema).clone());
    restrict_action_enum(&mut schema, &allowed);
    if let Value::Object(map) = schema {
        tool.input_schema = Arc::new(map);
    }
    let writes = ACTIONS
        .iter()
        .find(|(name, _, _)| *name == tool.name)
        .map(|(_, _, actions)| {
            actions
                .iter()
                .any(|(a, f)| f.is_write() && allowed.contains(a))
        })
        .unwrap_or(false);
    let mut annotations = tool.annotations.clone().unwrap_or_default();
    annotations.read_only_hint = Some(!writes);
    annotations.destructive_hint = Some(writes);
    tool.annotations = Some(annotations);
    tool
}

/// Narrows the `action` property's allowed values to `allowed`, wherever
/// the schema keeps them: an `enum`, or a `oneOf`/`anyOf` of `const`s,
/// inline or behind a `$ref`.
fn restrict_action_enum(schema: &mut Value, allowed: &[&str]) {
    let target = match schema
        .pointer("/properties/action/$ref")
        .and_then(Value::as_str)
    {
        Some(reference) => reference.trim_start_matches('#').to_owned(),
        None => "/properties/action".to_owned(),
    };
    let Some(node) = schema.pointer_mut(&target) else {
        return;
    };
    let keep = |v: &Value| {
        v.as_str()
            .or_else(|| v.get("const").and_then(Value::as_str))
            .or_else(|| {
                v.get("enum")
                    .and_then(Value::as_array)
                    .and_then(|e| e.first())
                    .and_then(Value::as_str)
            })
            .is_some_and(|name| allowed.contains(&name))
    };
    for key in ["enum", "oneOf", "anyOf"] {
        if let Some(Value::Array(items)) = node.get_mut(key) {
            items.retain(&keep);
        }
    }
}

/// The action a call names, or the tool's default.
fn requested_action(
    tool: &str,
    arguments: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    arguments
        .and_then(|a| a.get("action"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            ACTIONS
                .iter()
                .find(|(name, _, _)| *name == tool)
                .and_then(|(_, default, _)| default.map(str::to_owned))
        })
}

/// Why an action isn't allowed, in the capability table's words.
fn action_refusal(tool: &str, action: &str, caps: Option<&Capabilities>) -> Option<String> {
    let allowed = allowed_actions(tool, caps)?;
    if allowed.contains(&action) {
        return None;
    }
    let (_, _, actions) = ACTIONS.iter().find(|(name, _, _)| *name == tool)?;
    // The forge's reason, then each CI system's outside it, by name.
    let reasons: Vec<String> = actions
        .iter()
        .filter(|(a, _)| *a == action)
        .flat_map(|(_, feature)| {
            caps.into_iter()
                .flat_map(|c| c.0.iter())
                .filter(move |a| a.feature == *feature)
        })
        .filter_map(|a| match (&a.provider, &a.reason) {
            (None, Some(why)) => Some(why.clone()),
            (Some(system), Some(why)) => Some(format!("{system}: {why}")),
            _ => None,
        })
        .collect();
    let reason = if reasons.is_empty() {
        format!("{tool} has no action {action}")
    } else {
        reasons.join("; ")
    };
    Some(format!(
        "{tool} {action} isn't available here: {reason}. Available: {}",
        allowed.join(", ")
    ))
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ForgeContextParams {
    /// Probe the forge again instead of returning what was found when the
    /// server started (or last refreshed).
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

#[derive(Default)]
struct State {
    report: Option<Report>,
    session: Option<Arc<Session>>,
}

struct Inner {
    options: Options,
    cache: Cache,
    state: tokio::sync::Mutex<State>,
    /// The tool names last sent in a tools/list, to know when to tell the
    /// client the list changed.
    advertised: Mutex<Option<Vec<String>>>,
    /// ci_wait calls running as MCP tasks, for clients that support them.
    tasks: TaskManager,
}

#[derive(Clone)]
pub struct GitforgeServer {
    inner: Arc<Inner>,
    tool_router: ToolRouter<Self>,
}

fn reply(result: Result<serde_json::Value, ToolError>) -> Result<CallToolResult, McpError> {
    Ok(match result {
        Ok(value) => CallToolResult::structured(value),
        Err(ToolError(message)) => CallToolResult::error(vec![ContentBlock::text(message)]),
    })
}

#[tool_router]
impl GitforgeServer {
    pub fn new(options: Options, cache: Cache) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                cache,
                state: tokio::sync::Mutex::new(State::default()),
                advertised: Mutex::new(None),
                tasks: TaskManager::new(),
            }),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "forge_context",
        description = "Which repository and forge this server is bound to: forge kind and version, the signed-in user, \
your permissions on the repository, where the credential came from, and every capability with the reason it is or \
isn't available here. Call this first, and whenever a tool you expected is missing. Pass refresh=true to probe the \
forge again (after it comes back up, or after a token change).",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn forge_context(
        &self,
        Parameters(params): Parameters<ForgeContextParams>,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let report = if params.refresh {
            self.probe_now().await
        } else {
            self.report().await
        };
        self.notify_if_tools_changed(&report, &peer).await;
        let value = serde_json::to_value(&report)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::structured(value))
    }

    #[tool(
        name = "ci_status",
        description = "The CI verdict for a commit: success, failure, failing (a job failed, others still running), \
pending, nothing_ran (every job skipped, which is not a pass) or no_runs. Lists every run for the commit and every \
job in it, including jobs still waiting for a runner. Defaults to the checkout's HEAD; or pass one of sha, branch \
(its latest commit on the forge) or run (the run number in the run's URL).",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn ci_status(
        &self,
        Parameters(params): Parameters<CiStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        reply(match self.session().await {
            Ok(session) => tools::ci_status(&session, &params).await,
            Err(e) => Err(e),
        })
    }

    #[tool(
        name = "ci_logs",
        description = "Read CI job logs, cut down: by default the last 80 lines of every failed or cancelled job of \
the checkout's HEAD, without timestamps. Select the commit like ci_status (sha, branch or run). job=<text> picks \
jobs by name; all=true includes jobs that passed; grep=<regex> keeps matching lines; tail=0 reads the whole log \
(capped, keeping the end).",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn ci_logs(
        &self,
        Parameters(params): Parameters<CiLogsParams>,
    ) -> Result<CallToolResult, McpError> {
        reply(match self.session().await {
            Ok(session) => tools::ci_logs(&session, &params).await,
            Err(e) => Err(e),
        })
    }

    #[tool(
        name = "ci_runners",
        description = "CI runner availability: every runner the credential can see (repository, owner and instance \
scopes) as idle, busy or offline with its labels; which scopes it can't see; and the jobs waiting for a runner, \
with how many visible runners could take each. pools groups the jobs running and waiting in every repository \
the credential can list (owner and instance scopes) by the labels they asked for, naming which repositories are \
holding each set of runners and whether runners are taking several jobs at once. Woodpecker's agents are under \
ci_systems. Use it to tell a stuck job from a slow one, and to see which other projects are using shared runners.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn ci_runners(
        &self,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<CallToolResult, McpError> {
        reply(match self.session().await {
            Ok(session) => tools::ci_runners(&session).await,
            Err(e) => Err(e),
        })
    }

    #[tool(
        name = "ci_wait",
        description = "Wait for CI on a commit to finish, then report like ci_status. Polls the forge, with progress \
notifications, until every job has finished, a job fails (fail_fast, default true), or timeout_seconds (default 300, \
at most 1800) pass. Gives up after 90 seconds if no run appears at all. Select the commit like ci_status (default: \
the checkout's HEAD). Prefer this to calling ci_status in a loop. It holds up the conversation while it waits, \
unless your client runs it as an MCP task.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn ci_wait(
        &self,
        Parameters(params): Parameters<CiWaitParams>,
        peer: Peer<RoleServer>,
        meta: RequestMetaObject,
        cancelled: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let session = match self.session().await {
            Ok(s) => s,
            Err(e) => return reply(Err(e)),
        };
        let token = meta.get_progress_token();
        let report =
            move |waited: u64,
                  summary: String|
                  -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                let peer = peer.clone();
                let token = token.clone();
                Box::pin(async move {
                    if let Some(token) = token {
                        let mut p = ProgressNotificationParam::new(token, waited as f64);
                        p.message = Some(format!("waited {waited}s: {summary}"));
                        let _ = peer.notify_progress(p).await;
                    }
                })
            };
        reply(tools::ci_wait(&session, &params, Some(&report), &cancelled).await)
    }

    #[tool(
        name = "ci_estimate",
        description = "How long CI will likely take, before a push: per workflow, the median and 90th-percentile \
time of recent passing runs, the time until a failure usually shows, and the spread by job set (which jobs ran, \
since path filters and conditions change it). Defaults to push runs of the checkout's branch, or of the default \
branch where it has too little history; pass branch, event or workflow to choose. ci_status and ci_wait already \
give an ETA for runs in progress.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn ci_estimate(
        &self,
        Parameters(params): Parameters<CiEstimateParams>,
    ) -> Result<CallToolResult, McpError> {
        reply(match self.session().await {
            Ok(session) => tools::ci_estimate(&session, &params).await,
            Err(e) => Err(e),
        })
    }

    #[tool(
        name = "ci_control",
        description = "Change CI: cancel a run that hasn't finished, rerun a finished run (rerun) or only its failed \
jobs (rerun_failed), or start a workflow_dispatch workflow (dispatch, with workflow, ref and inputs). For cancel and \
the reruns, name the run with run=<number>, or a sha or branch with exactly one run (default: the checkout's HEAD). \
Only the actions this forge's version and your access allow are listed. On Woodpecker, rerun restarts the whole \
pipeline as a new one and dispatch starts a manual pipeline (inputs become variables). These change CI: act only \
when the user asked for it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn ci_control(
        &self,
        Parameters(params): Parameters<CiControlParams>,
    ) -> Result<CallToolResult, McpError> {
        reply(match self.session().await {
            Ok(session) => tools::ci_control(&session, &params).await,
            Err(e) => Err(e),
        })
    }

    #[tool(
        name = "pull_request",
        description = "Pull requests. action=list (default) lists them, most recently updated first, filtered by state \
(open, closed, all). action=get with number returns one in full, including whether it can merge cleanly and the CI \
verdict of its head commit and where its reviews stand. action=reviews with number lists its reviews and their \
comments on lines of the diff (path, line, diff hunk), by default only what is still open (include_resolved=true \
for the rest). action=create opens one (head defaults to the checkout's branch, base to the default \
branch). action=merge merges one, refusing if its head moved from expected_head_sha, it conflicts, or (unless \
require_ci=false) its CI hasn't passed. action=merge_when_ready merges once CI passes: at once if it already has, \
otherwise the forge merges it when the checks its base branch requires pass (refused if the base requires none); \
cancel_auto_merge cancels that. Only the actions your access allows are listed; create and the merges change the \
forge, so use them only when the user asked.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn pull_request(
        &self,
        Parameters(params): Parameters<PullRequestParams>,
    ) -> Result<CallToolResult, McpError> {
        reply(match self.session().await {
            Ok(session) => tools::pull_request(&session, &params).await,
            Err(e) => Err(e),
        })
    }

    #[tool(
        name = "repo_settings",
        description = "A repository's settings, in four sections: general (description, website, default_branch, \
private, has_issues, has_wiki), merge (allowed_styles, default_style, delete_branch_after_merge, allow_auto_merge), \
branch_protection (rules by pattern: require_pull_request, required_approvals, dismiss_stale_approvals, \
required_checks, require_up_to_date, require_signed_commits, push_allowlist, and enforce_admins on GitHub or \
block_on_rejected_reviews on Gitea and Forgejo) and webhooks (by id: url, content_type, events, active, and \
branch_filter and type on Gitea and Forgejo; secret is write-only). action=get (default) reads one section or all. \
To change settings: action=plan with section and changes shows each change (from, to) and a fingerprint; show it \
to the user; then action=apply with the same section, changes and fingerprint makes it, refusing if the section \
changed since the plan, and returns a revert object; action=revert with that object undoes it, refusing if the \
section changed after the apply. Fields already as asked are left alone. A webhook's secret is never shown and \
can't be restored by revert. action=test_webhook sends a webhook a test delivery; webhook_deliveries lists its \
recent ones (GitHub). On GitHub, a rule's pattern is an existing branch's name. Only the actions your access \
allows are listed; apply, revert and test_webhook change the repository, so use them only when the user asked.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn repo_settings(
        &self,
        Parameters(params): Parameters<RepoSettingsParams>,
    ) -> Result<CallToolResult, McpError> {
        reply(match self.session().await {
            Ok(session) => repo_settings::repo_settings(&session, &params).await,
            Err(e) => Err(e),
        })
    }
}

impl GitforgeServer {
    /// ci_wait as an MCP task: the call returns at once with a task id, and
    /// the client polls `tasks/get`, whose status message is the progress
    /// line, until the wait ends. The client's turn isn't held up, so the
    /// timeout defaults to the longest allowed.
    async fn ci_wait_task(
        &self,
        request: CallToolRequestParams,
    ) -> Result<CallToolResponse, McpError> {
        let mut params: CiWaitParams =
            serde_json::from_value(Value::Object(request.arguments.unwrap_or_default()))
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        params.timeout_seconds.get_or_insert(tools::MAX_WAIT);
        let session = match self.session().await {
            Ok(session) => session,
            Err(e) => return reply(Err(e)).map(Into::into),
        };
        let timeout = params.timeout_seconds.unwrap_or(tools::MAX_WAIT);
        let options = TaskOptions::new()
            // Past the wait's own timeout, with room for the last poll.
            .with_ttl_ms((timeout.min(tools::MAX_WAIT) + TASK_TTL_MARGIN_SECONDS) * MS_PER_SECOND)
            .with_poll_interval_ms(TASK_POLL_INTERVAL_MS)
            .with_status_message("waiting for CI");
        let task = self.inner.tasks.spawn(options, move |ctx| {
            Box::pin(async move {
                let report = {
                    let ctx = ctx.clone();
                    move |waited: u64,
                          progress: String|
                          -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                        ctx.set_status_message(format!("waited {waited}s: {progress}"));
                        Box::pin(std::future::ready(()))
                    }
                };
                let cancelled = CancellationToken::new();
                tokio::select! {
                    result = tools::ci_wait(&session, &params, Some(&report), &cancelled) => {
                        reply(result).map_err(TaskExit::Error)
                    }
                    () = ctx.cancelled() => Err(TaskExit::Cancelled),
                }
            })
        });
        Ok(CallToolResponse::Task(CreateTaskResult::new(task)))
    }

    /// The bound repository's session, for commands that aren't tools.
    pub async fn session_for_cli(&self) -> Result<Arc<Session>, String> {
        self.session().await.map_err(|e| e.0)
    }

    /// Runs a tool outside MCP, for the `call` command: same gating, same
    /// results.
    pub async fn call_tool_directly(
        &self,
        name: &str,
        args: serde_json::Value,
        progress: Option<tools::Progress<'_>>,
    ) -> Result<serde_json::Value, String> {
        let report = self.report().await;
        if let Some(action) = requested_action(name, args.as_object())
            && let Some(refusal) = action_refusal(name, &action, report.capabilities.as_ref())
        {
            return Err(refusal);
        }
        if !gate_for(name).is_some_and(|g| g.allows(report.capabilities.as_ref())) {
            return Err(format!(
                "{name} is not available for this repository (offered: {}); see `gitforge-mcp context`",
                report.tools.join(", ")
            ));
        }
        fn parse<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
            serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))
        }
        if name == "forge_context" {
            return serde_json::to_value(report).map_err(|e| e.to_string());
        }
        let session = self.session().await.map_err(|e| e.0)?;
        let result = match name {
            "ci_status" => tools::ci_status(&session, &parse(args)?).await,
            "ci_logs" => tools::ci_logs(&session, &parse(args)?).await,
            "ci_runners" => tools::ci_runners(&session).await,
            "ci_estimate" => tools::ci_estimate(&session, &parse(args)?).await,
            "pull_request" => tools::pull_request(&session, &parse(args)?).await,
            "ci_control" => tools::ci_control(&session, &parse(args)?).await,
            "repo_settings" => repo_settings::repo_settings(&session, &parse(args)?).await,
            "ci_wait" => {
                tools::ci_wait(&session, &parse(args)?, progress, &CancellationToken::new()).await
            }
            other => return Err(format!("no tool named {other}")),
        };
        result.map_err(|e| e.0)
    }

    /// The current report, probing first if there isn't one yet.
    pub async fn report(&self) -> Report {
        let mut state = self.inner.state.lock().await;
        if let Some(report) = state.report.as_ref() {
            return report.clone();
        }
        self.probe_into(&mut state).await
    }

    async fn probe_now(&self) -> Report {
        let mut state = self.inner.state.lock().await;
        self.probe_into(&mut state).await
    }

    async fn probe_into(&self, state: &mut State) -> Report {
        let (report, session) = context::probe(&self.inner.options, &self.inner.cache).await;
        let report = self.with_tools(report);
        state.report = Some(report.clone());
        state.session = session.map(Arc::new);
        report
    }

    /// The live session. Without one (the forge was unreachable), probe
    /// once more in case it is back.
    async fn session(&self) -> Result<Arc<Session>, ToolError> {
        let mut state = self.inner.state.lock().await;
        if state.session.is_none() {
            self.probe_into(&mut state).await;
        }
        match &state.session {
            Some(session) => Ok(session.clone()),
            None => Err(ToolError(format!(
                "the forge can't be used right now: {}",
                state
                    .report
                    .as_ref()
                    .map(|r| r.problems.join("; "))
                    .unwrap_or_default()
            ))),
        }
    }

    fn with_tools(&self, mut report: Report) -> Report {
        report.tools = offered(self.tool_router.list_all(), report.capabilities.as_ref())
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        report
    }

    async fn notify_if_tools_changed(&self, report: &Report, peer: &Peer<RoleServer>) {
        let changed = {
            let advertised = self
                .inner
                .advertised
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            advertised
                .as_ref()
                .is_some_and(|names| *names != report.tools)
        };
        if changed {
            // Clients that ignore this still get a short TTL on the list.
            let _ = peer.notify_tool_list_changed().await;
        }
    }
}

impl ServerHandler for GitforgeServer {
    fn get_info(&self) -> ServerConfig {
        let mut config = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_tasks()
                .build(),
        )
        .with_instructions(instructions(&self.inner.options.dir));
        config.server_info = Implementation::new("gitforge-mcp", env!("CARGO_PKG_VERSION"));
        config
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let caps = match tokio::time::timeout(FIRST_PROBE_TIMEOUT, self.report()).await {
            Ok(report) => report.capabilities,
            Err(_) => None,
        };
        let mut tools = offered(self.tool_router.list_all(), caps.as_ref());
        describe_ci_wait(&mut tools, &self.inner.options.dir);
        *self
            .inner
            .advertised
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(tools.iter().map(|t| t.name.to_string()).collect());
        Ok(ListToolsResult::with_all_items(tools)
            .with_ttl_ms(TOOL_LIST_TTL.as_millis() as u64)
            .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let gate = gate_for(&request.name);
        let caps = match gate {
            Some(Gate::Always) => None,
            Some(_) => self.report().await.capabilities,
            None => None,
        };
        let allowed = match gate {
            Some(Gate::Always) => true,
            Some(g) => g.allows(caps.as_ref()),
            None => false,
        };
        if !allowed {
            return Err(McpError::invalid_params(
                format!(
                    "{} is not available for this repository; call forge_context to see why",
                    request.name
                ),
                None,
            ));
        }
        if let Some(action) = requested_action(&request.name, request.arguments.as_ref())
            && let Some(refusal) = action_refusal(&request.name, &action, caps.as_ref())
        {
            return Ok(CallToolResult::error(vec![ContentBlock::text(refusal)]).into());
        }
        if request.name == "ci_wait"
            && context
                .client_capabilities()
                .is_some_and(|caps| caps.supports_tasks())
        {
            return self.ci_wait_task(request).await;
        }
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        Ok(GetTaskResult::new(
            self.inner.tasks.get_task(&request.task_id)?,
        ))
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.inner
            .tasks
            .update_task(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.inner.tasks.cancel_task(&request.task_id)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_command_quotes_its_paths_for_the_shell() {
        assert_eq!(shell_quote("/srv/my repo"), r#""/srv/my repo""#);
        assert_eq!(
            shell_quote(r#"/srv/a"b\c$HOME`x`"#),
            r#""/srv/a\"b\\c\$HOME\`x\`""#
        );
        assert_eq!(
            powershell_quote(r"C:\my repo\O'Brien`$"),
            r"'C:\my repo\O''Brien`$'"
        );
        let dir = std::env::temp_dir().join("my repo");
        let command = background_wait(&dir);
        let absolute = std::path::absolute(&dir).unwrap();
        let expected = if cfg!(windows) {
            assert!(command.starts_with("& '"), "{command}");
            powershell_quote(&absolute.display().to_string())
        } else {
            shell_quote(&absolute.display().to_string())
        };
        assert!(
            command.ends_with(&format!(" wait --dir {expected}")),
            "{command}"
        );
    }

    #[test]
    fn instructions_and_ci_wait_name_the_background_command() {
        let dir = std::path::Path::new("/srv/widgets");
        let command = background_wait(dir);
        assert!(instructions(dir).contains(&format!("prefer running `{command}`")));

        let mut tools = server().tool_router.list_all();
        let before: Vec<_> = tools.iter().map(|t| t.description.clone()).collect();
        describe_ci_wait(&mut tools, dir);
        for (tool, was) in tools.iter().zip(before) {
            let now = tool.description.as_deref().unwrap_or_default();
            if tool.name == "ci_wait" {
                let was = was.unwrap_or_default();
                assert!(now.starts_with(was.as_ref()), "{now}");
                assert!(now.contains(&format!("run `{command}`")), "{now}");
                assert!(now.ends_with(BACKGROUND_WAIT), "{now}");
            } else {
                assert_eq!(Some(now), was.as_deref(), "{} changed", tool.name);
            }
        }
    }

    #[test]
    fn every_registered_tool_has_a_gate() {
        for tool in server().tool_router.list_all() {
            assert!(gate_for(&tool.name).is_some(), "{} has no gate", tool.name);
        }
    }

    use crate::site::{Permissions, RepoInfo, ServerInfo};
    use crate::version::{ForgeKind, Version};

    fn caps(kind: ForgeKind, version: (u64, u64), admin: bool, push: bool) -> Capabilities {
        Capabilities::evaluate(
            &ServerInfo {
                kind,
                version: Some(Version::new(version.0, version.1, 0)),
                version_raw: None,
                detected_by: "test".into(),
            },
            &RepoInfo {
                full_name: "acme/widgets".into(),
                default_branch: "main".into(),
                private: true,
                archived: false,
                permissions: Permissions {
                    admin,
                    push,
                    pull: true,
                },
                actions_enabled: Some(true),
                auto_merge_allowed: None,
            },
        )
    }

    fn server() -> GitforgeServer {
        GitforgeServer::new(
            Options {
                dir: ".".into(),
                remote: None,
                config_path: None,
                read_only: false,
            },
            Cache::at(std::path::Path::new("/nonexistent/probes.json")),
        )
    }

    /// The action names a tool's schema lists.
    fn schema_actions(tool: &Tool) -> Vec<String> {
        let schema = Value::Object((*tool.input_schema).clone());
        let node = match schema
            .pointer("/properties/action/$ref")
            .and_then(Value::as_str)
        {
            Some(r) => schema.pointer(r.trim_start_matches('#')).cloned(),
            None => schema.pointer("/properties/action").cloned(),
        }
        .expect("an action property");
        let mut names: Vec<String> = ["enum", "oneOf", "anyOf"]
            .iter()
            .filter_map(|k| node.get(*k).and_then(Value::as_array))
            .flatten()
            .filter_map(|v| {
                v.as_str()
                    .or_else(|| v.get("const").and_then(Value::as_str))
                    .or_else(|| v.get("enum").and_then(|e| e[0].as_str()))
                    .map(str::to_owned)
            })
            .collect();
        names.sort();
        names
    }

    fn tool<'a>(tools: &'a [Tool], name: &str) -> Option<&'a Tool> {
        tools.iter().find(|t| t.name == name)
    }

    #[test]
    fn action_lists_follow_the_forge_version() {
        let all = server().tool_router.list_all();
        let fj15 = offered(
            all.clone(),
            Some(&caps(ForgeKind::Forgejo, (15, 0), true, true)),
        );
        assert_eq!(
            schema_actions(tool(&fj15, "ci_control").unwrap()),
            ["dispatch"]
        );
        let fj17 = offered(
            all.clone(),
            Some(&caps(ForgeKind::Forgejo, (17, 0), true, true)),
        );
        assert_eq!(
            schema_actions(tool(&fj17, "ci_control").unwrap()),
            ["cancel", "dispatch", "rerun", "rerun_failed"]
        );
        let gitea126 = offered(
            all.clone(),
            Some(&caps(ForgeKind::Gitea, (1, 26), true, true)),
        );
        assert_eq!(
            schema_actions(tool(&gitea126, "ci_control").unwrap()),
            ["dispatch", "rerun", "rerun_failed"]
        );
        assert_eq!(
            schema_actions(tool(&fj17, "pull_request").unwrap()),
            [
                "cancel_auto_merge",
                "create",
                "get",
                "list",
                "merge",
                "merge_when_ready",
                "reviews"
            ]
        );
    }

    #[test]
    fn readers_get_read_only_pull_requests_and_no_ci_control() {
        let all = server().tool_router.list_all();
        let reader = offered(all, Some(&caps(ForgeKind::Forgejo, (17, 0), false, false)));
        let prs = tool(&reader, "pull_request").unwrap();
        assert_eq!(schema_actions(prs), ["get", "list", "reviews"]);
        let ann = prs.annotations.as_ref().unwrap();
        assert_eq!(ann.read_only_hint, Some(true));
        assert_eq!(ann.destructive_hint, Some(false));
        assert!(tool(&reader, "ci_control").is_none());
        assert!(tool(&reader, "ci_wait").is_some());
    }

    #[test]
    fn read_only_mode_hides_every_write() {
        let all = server().tool_router.list_all();
        let ro = offered(
            all,
            Some(&caps(ForgeKind::Gitea, (28, 3), true, true).read_only()),
        );
        assert_eq!(
            schema_actions(tool(&ro, "pull_request").unwrap()),
            ["get", "list", "reviews"]
        );
        assert!(tool(&ro, "ci_control").is_none());
    }

    #[test]
    fn refusals_say_why_and_what_is_available() {
        let c = caps(ForgeKind::Forgejo, (15, 0), true, true);
        let refusal = action_refusal("ci_control", "cancel", Some(&c)).unwrap();
        assert!(refusal.contains("needs Forgejo 16 or later"), "{refusal}");
        assert!(refusal.ends_with("Available: dispatch"), "{refusal}");
        assert!(action_refusal("ci_control", "dispatch", Some(&c)).is_none());
        let reader = caps(ForgeKind::Forgejo, (17, 0), false, false);
        for action in ["merge", "merge_when_ready", "cancel_auto_merge"] {
            assert!(
                action_refusal("pull_request", action, Some(&reader))
                    .unwrap()
                    .contains("write (push) access"),
                "{action}"
            );
        }
        let ro = caps(ForgeKind::Gitea, (1, 25), true, true).read_only();
        assert!(
            action_refusal("pull_request", "merge_when_ready", Some(&ro))
                .unwrap()
                .contains("read-only mode")
        );
        assert_eq!(
            requested_action("pull_request", None).as_deref(),
            Some("list")
        );
        assert_eq!(requested_action("ci_control", None), None);
    }

    #[test]
    fn ungated_tools_are_never_offered() {
        let tool = Tool::new("not_in_the_table", "x", Arc::new(serde_json::Map::new()));
        assert!(offered(vec![tool], None).is_empty());
    }

    /// The documentation site covers every tool: a page in the table of
    /// contents and the overview, every parameter, every action with a
    /// table row, and the default action marked. The rest of the site is
    /// checked in tests/docs.rs.
    #[test]
    fn every_tool_parameter_and_action_is_documented() {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/src");
        let read = |path: &str| std::fs::read_to_string(docs.join(path)).unwrap_or_default();
        let summary = read("SUMMARY.md");
        let overview = read("tools/index.md");
        let tools = server().tool_router.list_all();
        assert!(tools.len() >= 8);
        // The commit parameters are flattened into each CI tool's schema.
        let status = tools.iter().find(|t| t.name == "ci_status").unwrap();
        assert!(status.input_schema["properties"].get("sha").is_some());
        let mut problems = Vec::new();
        for tool in tools {
            let name = tool.name.to_string();
            let path = format!("tools/{}.md", name.replace('_', "-"));
            let page = read(&path);
            let mut expected: Vec<String> = tool
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.keys().map(|k| format!("`{k}`")).collect())
                .unwrap_or_default();
            if let Some((_, default, actions)) = ACTIONS.iter().find(|(n, _, _)| *n == name) {
                expected.extend(actions.iter().map(|(a, _)| format!("| `{a}`")));
                expected.extend(default.map(|d| format!("| `{d}` *(default)*")));
            }
            problems.extend(
                expected
                    .iter()
                    .filter(|e| !page.contains(e.as_str()))
                    .map(|e| format!("{path}: no {e}")),
            );
            for (what, text, wanted) in [
                ("SUMMARY.md", &summary, format!("]({path})")),
                ("tools/index.md", &overview, format!("[`{name}`]")),
            ] {
                if !text.contains(&wanted) {
                    problems.push(format!("{what}: no {wanted}"));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
