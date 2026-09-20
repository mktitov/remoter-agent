//! `remoter-mcp` — stdio MCP server for AI agents and humans.
//!
//! An HTTP client of the Remoter backend: speaks MCP/stdio with the MCP host
//! (remoter-agent's dev-agent CLI, Claude Code, Cursor, …), forwards requests
//! to the backend's REST API with the caller's bearer token. Never connects to
//! Postgres directly (spec §2.1).
//!
//! ## Usage
//! Spawn this binary as a child process with env:
//!   - `REMOTER_API_URL` (e.g. `http://localhost:8181`)
//!   - `REMOTER_TOKEN` (a personal access token from `POST /auth/token`, a
//!     login JWT, or an agent's static token; `REMOTER_AGENT_TOKEN` is accepted
//!     as a legacy alias)
//!
//! On startup it calls `GET /auth/me` to validate the token. Transient
//! failures (5xx, 429, network errors) are retried with exponential backoff
//! (see [`validate_with_retry`]); permanent failures (401/403, other 4xx) and
//! exhausted attempts exit with a clear error. On success it serves MCP over
//! stdio.

mod client;
mod error;
mod schema;
mod tools;
// Shared with agent-runner: one version module for the whole workspace.
#[path = "../../version.rs"]
mod version;

use std::env;
use std::time::Duration;

use rmcp::{
    ServiceExt,
    handler::server::ServerHandler,
    model::{
        CallToolRequestParam, CallToolResult, ErrorData, Implementation, ListToolsResult, PaginatedRequestParam,
        ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
};
use tools::RemoterMcp;

use crate::client::RemoterClient;

/// Runtime role that controls which workflow tools are exposed.
///
/// - `Full` (default): every tool is available.
/// - `DevAgentPlan`: hide board-status transitions and action active/complete;
///   the agent plans by adding/updating/deleting/rejecting actions and setting
///   the task report.
/// - `DevAgentImplement`: hide board-status transitions; the daemon owns those
///   while the agent implements and updates the report/comments/attachments.
/// - `DevAgentSupervise`: for a parent agent supervising its child tickets.
///   Board start/complete and self-unassign stay daemon-owned, but
///   `advance_task` stays available: the backend's parent-scoped transitions
///   (`backlog → todo`/`implement`, `review → completed`) are exactly what a
///   supervisor needs. Assignment is done via `assign_task` (see [`tools`]).
/// - `DevAgentReview`: for the ticket's own assignee agent running a
///   human-requested review run. Read-only discovery tools plus
///   `set_task_review` (the backend allows the verdict endpoint for the
///   assignee while a review run is running); all mutating tools stay
///   daemon-owned or out of scope for a reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Full,
    DevAgentPlan,
    DevAgentImplement,
    DevAgentSupervise,
    DevAgentReview,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Full => write!(f, "full"),
            Role::DevAgentPlan => write!(f, "dev-agent-plan"),
            Role::DevAgentImplement => write!(f, "dev-agent-implement"),
            Role::DevAgentSupervise => write!(f, "dev-agent-supervise"),
            Role::DevAgentReview => write!(f, "dev-agent-review"),
        }
    }
}

impl Role {
    /// Tool names that are not available in this role.
    pub fn gated_tools(&self) -> &'static [&'static str] {
        match self {
            // set_task_review is supervise/review-only: the backend gate
            // requires a parent-scoped supervisor or the ticket's assignee on
            // a review run, so the tool would 403 for everyone else.
            // add/delete_task_question are dev-agent tools: humans ask and
            // answer questions in the UI.
            // set_task_goal is implement/plan-only (remoter#199 question #72);
            // humans link goals in the UI.
            // request_human_action is plan/implement-only (remoter#162): the
            // waiting status exists so a running agent can hand a human-only
            // step to a human; humans move tickets in the UI.
            Role::Full => &[
                "set_task_review",
                "add_task_question",
                "delete_task_question",
                "set_task_goal",
                "request_human_action",
            ],
            Role::DevAgentImplement => &[
                "start_task",
                "advance_task",
                "complete_task",
                "unassign_self",
                "set_task_review",
                // Questions are asked in the plan phase; implement only reads
                // answers and deletes obsolete questions.
                "add_task_question",
            ],
            Role::DevAgentSupervise => &[
                "start_task",
                "complete_task",
                "unassign_self",
                "add_task_question",
                "delete_task_question",
                // Goal linking is implement/plan-only (remoter#199 q#72).
                "set_task_goal",
                // Waiting is for the agent working its own ticket, not the
                // supervisor (remoter#162).
                "request_human_action",
            ],
            // Reviewer: read-only discovery plus set_task_review. Every
            // mutating tool (board transitions, actions, tasks, comments,
            // reports, attachments, links, question edits) is gated away.
            Role::DevAgentReview => &[
                "start_task",
                "advance_task",
                "complete_task",
                "unassign_self",
                "assign_task",
                "create_task",
                "update_task",
                "start_action",
                "complete_action",
                "reject_action",
                "update_action",
                "delete_action",
                "add_action",
                "add_attachment",
                "add_task_comment",
                "set_task_report",
                "add_link",
                "remove_link",
                "add_task_question",
                "delete_task_question",
                "set_task_goal",
                "request_human_action",
            ],
            Role::DevAgentPlan => &[
                "start_task",
                "advance_task",
                "complete_task",
                "unassign_self",
                "start_action",
                "complete_action",
                "create_task",
                "update_task",
                "set_task_review",
            ],
        }
    }

    /// Whether the named tool is gated away in this role.
    pub fn is_gated(&self, name: &str) -> bool {
        self.gated_tools().contains(&name)
    }
}

const USAGE: &str = "usage: remoter-mcp [--version] [--role full|dev-agent-plan|dev-agent-implement|dev-agent-supervise|dev-agent-review]";

/// Parse the optional `--role` CLI argument from an iterator (testable).
pub(crate) fn parse_role<I>(mut args: I) -> Role
where
    I: Iterator<Item = String>,
{
    let mut role = Role::Full;
    while let Some(arg) = args.next() {
        if arg == "--role" {
            role = match args.next() {
                Some(v) if v == "full" => Role::Full,
                Some(v) if v == "dev-agent-plan" => Role::DevAgentPlan,
                Some(v) if v == "dev-agent-implement" => Role::DevAgentImplement,
                Some(v) if v == "dev-agent-supervise" => Role::DevAgentSupervise,
                Some(v) if v == "dev-agent-review" => Role::DevAgentReview,
                Some(v) => {
                    eprintln!("error: unknown role `{v}`");
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                }
                None => {
                    eprintln!("error: --role requires a value");
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                }
            };
        } else if let Some(v) = arg.strip_prefix("--role=") {
            role = match v {
                "full" => Role::Full,
                "dev-agent-plan" => Role::DevAgentPlan,
                "dev-agent-implement" => Role::DevAgentImplement,
                "dev-agent-supervise" => Role::DevAgentSupervise,
                "dev-agent-review" => Role::DevAgentReview,
                _ => {
                    eprintln!("error: unknown role `{v}`");
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                }
            };
        } else {
            eprintln!("error: unknown argument `{arg}`");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
    role
}

/// Parse `--role` from `std::env::args()`.
fn parse_role_args() -> Role {
    parse_role(env::args().skip(1))
}

/// Read a required env var or exit with an error.
fn require_env(key: &str) -> String {
    env::var(key).unwrap_or_else(|_| {
        eprintln!("error: {key} environment variable is required");
        std::process::exit(1);
    })
}

/// Read the bearer token: `REMOTER_TOKEN` preferred, `REMOTER_AGENT_TOKEN` as
/// the legacy alias the remoter-agent daemon still sets.
fn read_token() -> String {
    match (env::var("REMOTER_TOKEN"), env::var("REMOTER_AGENT_TOKEN")) {
        (Ok(t), _) if !t.is_empty() => t,
        (_, Ok(t)) if !t.is_empty() => t,
        _ => {
            eprintln!(
                "error: REMOTER_TOKEN environment variable is required (REMOTER_AGENT_TOKEN accepted as a legacy alias)"
            );
            std::process::exit(1);
        }
    }
}

/// Read the optional `REMOTER_AGENT_TASK_ID` env var and parse it as an i32.
fn parse_agent_task_id() -> Option<i32> {
    env::var("REMOTER_AGENT_TASK_ID").ok().and_then(|s| s.parse().ok())
}

/// Read the optional `REMOTER_WORKSPACE_ID` env var and parse it as an i32.
/// A non-empty, malformed value fails at startup so the caller gets a clear
/// message instead of the ambiguous "set REMOTER_WORKSPACE_ID" error.
fn read_workspace_id() -> anyhow::Result<Option<i32>> {
    match env::var("REMOTER_WORKSPACE_ID") {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => s
            .parse::<i32>()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("REMOTER_WORKSPACE_ID must be an integer workspace id, got {s:?}")),
        Err(_) => Ok(None),
    }
}

/// Picks the effective workspace. Required when the caller belongs to more than
/// one workspace; optional (single-membership fallback) otherwise.
fn resolve_workspace(whoami: &mut client::WhoAmI, configured: Option<i32>) -> Result<(), String> {
    if let Some(id) = configured {
        let found = whoami.workspaces.iter().find(|w| w.id == id).cloned().ok_or_else(|| {
            format!(
                "REMOTER_WORKSPACE_ID={id} is not one of this user's workspaces ({}) — fix the env var",
                whoami
                    .workspaces
                    .iter()
                    .map(|w| w.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        whoami.workspace_id = Some(found.id);
        whoami.workspace_name = Some(found.name);
        whoami.role = Some(found.role);
        return Ok(());
    }
    match whoami.workspaces.len() {
        0 => Err("user has no workspace memberships".to_string()),
        1 => {
            let w = &whoami.workspaces[0];
            whoami.workspace_id = Some(w.id);
            whoami.workspace_name = Some(w.name.clone());
            whoami.role = Some(w.role.clone());
            Ok(())
        }
        _ => Err(format!(
            "user belongs to multiple workspaces ({}); set REMOTER_WORKSPACE_ID",
            whoami
                .workspaces
                .iter()
                .map(|w| format!("{}={}", w.id, w.name))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Startup token validation: retry transient failures (5xx, 429, network
/// errors) with exponential backoff + jitter so a brief backend/proxy blip
/// (e.g. an nginx 503 while several runs start in parallel) does not kill the
/// MCP server and leave the agent session silently without tools. 401/403 and
/// other 4xx are permanent and fail on the first attempt.
const MAX_VALIDATE_ATTEMPTS: u32 = 5;
const VALIDATE_BASE_DELAY: Duration = Duration::from_secs(1);

async fn validate_with_retry(
    client: &RemoterClient,
    max_attempts: u32,
    base_delay: Duration,
) -> Result<client::WhoAmI, error::McpError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match client.validate().await {
            Ok(whoami) => return Ok(whoami),
            Err(e) if e.is_retryable() && attempt < max_attempts => {
                let delay = retry_delay(base_delay, attempt);
                eprintln!(
                    "remoter-mcp: token validation attempt {attempt}/{max_attempts} failed ({e}), retrying in {} ms",
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                if attempt > 1 {
                    eprintln!("remoter-mcp: token validation failed after {attempt}/{max_attempts} attempts");
                }
                return Err(e);
            }
        }
    }
}

/// Delay before retry number `attempt` (1-based): `base * 2^(attempt-1)` plus
/// up to +50% jitter derived from the system clock (no RNG dependency).
/// With the defaults this waits 1s/2s/4s/8s (+jitter) between 5 attempts —
/// ≤ ~23 s total.
fn retry_delay(base: Duration, attempt: u32) -> Duration {
    let scaled = base.saturating_mul(1u32 << attempt.saturating_sub(1).min(10));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter_frac = f64::from(nanos % 500) / 1000.0; // [0, 0.5)
    scaled + scaled.mul_f64(jitter_frac)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if env::args().any(|arg| arg == "--version") {
        println!("{}", version::line("remoter-mcp"));
        return Ok(());
    }

    let role = parse_role_args();

    // Logs go to stderr: stdout is the MCP transport.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    // First log line of the process: which master commit this binary is.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        git_rev = version::GIT_REV,
        git_commit_date = version::GIT_COMMIT_DATE,
        "remoter-mcp starting"
    );

    let base_url = require_env("REMOTER_API_URL");
    let token = read_token();
    let workspace_id = read_workspace_id()?;

    let client = RemoterClient::new(base_url, token, workspace_id);

    // Validate the token before serving (clear failure mode on bad/revoked token).
    let mut whoami = match validate_with_retry(&client, MAX_VALIDATE_ATTEMPTS, VALIDATE_BASE_DELAY).await {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: token validation failed: {e}");
            eprintln!("hint: check REMOTER_API_URL and REMOTER_TOKEN");
            std::process::exit(1);
        }
    };
    if let Err(e) = resolve_workspace(&mut whoami, workspace_id) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    eprintln!(
        "remoter-mcp: authenticated as {} (id={}, kind={}, workspace_id={}, workspace_name={}, role={}, mcp_role={})",
        whoami.name,
        whoami.id,
        whoami.kind,
        whoami.workspace_id.unwrap_or_default(),
        whoami.workspace_name.as_deref().unwrap_or("?"),
        whoami.role.as_deref().unwrap_or("?"),
        role
    );

    let server = RemoterMcp::new(client, whoami, role, parse_agent_task_id());
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ── ServerHandler ─────────────────────────────────────────────────────────────
//
// The `#[tool_router]` macro generates `tool_router()` and populates
// `ToolRouter<Self>`. Here we wire it into the ServerHandler trait: list_tools
// delegates to the router and rewrites the advertised input schemas to
// spec-conformant JSON Schema (`schema` module — schemars 0.8 emits the
// non-standard OpenAPI `nullable` keyword, which strict MCP clients reject),
// call_tool delegates to the router, and get_info reports the server identity.

impl ServerHandler for RemoterMcp {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        let tools = schema::sanitize_tools(
            self.tool_router
                .list_all()
                .into_iter()
                .filter(|tool| !self.role.is_gated(&tool.name))
                .collect(),
        );
        std::future::ready(Ok(ListToolsResult {
            tools,
            next_cursor: None,
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send + '_ {
        let router = self.tool_router.clone();
        async move {
            self.gate_tool(&request.name)?;
            let tcx = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
            router.call(tcx).await
        }
    }

    fn get_info(&self) -> ServerInfo {
        let instructions = match self.role {
            Role::Full => "Remoter task management over MCP, for AI agents and humans alike. \
                 Discover IDs first: list_projects, then list_features for a project; search_tasks finds tasks by #id or title, \
                 list_board shows the kanban, and list_my_tasks shows work assigned to you. \
                 get_task gives full details; start_task/advance_task/complete_task move a task along the board. \
                 Use add_action to create steps, update_action to edit them, delete_action to remove stale ones, \
                 and start_action/complete_action to track progress. reject_action with a reason when a step is no longer needed. \
                 create_task creates a new task (featureId required; inside an agent ticket run it becomes a subtask of the current ticket instead); update_task edits it. \
                 Attachments: add_attachment (returns a markdown link) prefers filePath (local file read by remoter-mcp), \
                 with contentBase64 as a fallback; list_attachments, read_attachment; add_task_comment posts to the task thread."
                .into(),
            Role::DevAgentImplement => "Remoter task implementation mode. Board status is daemon-owned: do NOT call \
                 start_task, advance_task, complete_task, or unassign_self. Use list_my_tasks to see your assigned work, \
                 get_task for details, then implement. Use add_action/update_action/delete_action/reject_action to keep \
                 the plan accurate, set_task_report to write the implementation report, add_task_comment for thread updates, \
                 create_task for subtasks of the current ticket (pass a different projectId plus a featureId \
                 belonging to it for a cross-project subtask), update_task to edit them, and add_attachment/list_attachments/read_attachment for artifacts. \
                 Read the human's answers to the ticket's open questions via list_task_questions (also embedded in get_task); \
                 delete questions that are obsolete or already answered with delete_task_question to keep the context clean. \
                 If the next step can only be performed by a human (external permissions, credentials, a manual environment \
                 change), call request_human_action with a precise, actionable description and end your turn — the ticket \
                 waits in the `waiting` status until the human confirms. \
                 add_attachment prefers filePath (local file read by remoter-mcp), with contentBase64 as a fallback. \
                 Agents cannot delete attachments."
                .into(),
            Role::DevAgentPlan => "Remoter planning mode. Board status, action active/complete, and task creation/editing are daemon-owned: \
                 do NOT call start_task, advance_task, complete_task, unassign_self, start_action, complete_action, create_task, or update_task. \
                 Use list_my_tasks to see assigned work, get_task for details. Plan by adding/updating/deleting actions \
                 (add_action, update_action, delete_action), reject_action for steps that are wrong, and set_task_report \
                 to capture the approved plan. list_features lists features in a project. \
                 Register every open question as a structured question via add_task_question (with concrete answer \
                 options whenever possible) instead of writing an \"Open questions\" text section in the plan; \
                 read the human's answers via list_task_questions (also embedded in get_task) and clean up with \
                 delete_task_question. \
                 If the work cannot proceed without a step only a human can perform (external permissions, credentials, \
                 manual environment changes), call request_human_action with a precise, actionable description — the \
                 ticket waits in the `waiting` status until the human confirms. \
                 Attachments: add_attachment (returns a markdown link) prefers filePath (local file read by remoter-mcp), \
                 with contentBase64 as a fallback; list_attachments, read_attachment; add_task_comment posts to the task thread. \
                 Agents cannot delete attachments."
                .into(),
            Role::DevAgentSupervise => "Remoter supervision mode. You manage the child tickets of the current ticket: \
                 list_board/list_my_tasks/search_tasks show only the current ticket and its children. \
                 Create children with create_task (no featureId — they become subtasks of the current ticket; \
                 for a subtask in a linked project pass its projectId plus a featureId belonging to that project), \
                 staff them with assign_task (assigneeId from list_users; omit assigneeId to unassign), \
                 and move them along with advance_task — the backend allows a parent-scoped agent the transitions \
                 backlog → todo, backlog → implement, and review → completed on its children. \
                 When a child is done, review it: set_task_review(taskId, markdown, verdict) writes the review \
                 with the verdict 'approve' or 'changes_requested'; the child agent reads it via get_task \
                 (fields review, reviewVerdict, reviewOutcomeId). \
                 Do NOT call start_task, complete_task, or unassign_self — the daemon owns those. \
                 Use get_task for details (including the ticket's questions and answers; list_task_questions reads them too), \
                 add_task_comment for thread updates, and add_attachment/list_attachments/read_attachment for artifacts. \
                 add_attachment prefers filePath (local file read by remoter-mcp), with contentBase64 as a fallback. \
                 Agents cannot delete attachments."
                .into(),
            Role::DevAgentReview => "Remoter review mode. A human requested your review of the current ticket: \
                 read the ticket with get_task (the implementation report is the report field; comments and links \
                 come from include=comments/links), browse the board with list_board, list_my_tasks, and search_tasks, \
                 and inspect artifacts with list_attachments/read_attachment. \
                 Then write your verdict: set_task_review(taskId, markdown, verdict) with verdict 'approve' or \
                 'changes_requested'. Everything else is read-only for you — do NOT call any mutating tool \
                 (board transitions, actions, comments, reports, attachments, links)."
                .into(),
        };
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "remoter".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: Some("Remoter MCP Server".into()),
                icons: None,
                website_url: None,
            },
            instructions: Some(instructions),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::client::{WhoAmI, WorkspaceMembership};
    use crate::resolve_workspace;

    fn whoami(workspaces: Vec<WorkspaceMembership>) -> WhoAmI {
        WhoAmI {
            id: 1,
            name: "test".into(),
            kind: "agent".into(),
            workspaces,
            workspace_id: None,
            workspace_name: None,
            role: None,
        }
    }

    fn membership(id: i32, name: &str, role: &str) -> WorkspaceMembership {
        WorkspaceMembership {
            id,
            name: name.into(),
            role: role.into(),
        }
    }

    #[test]
    fn resolve_workspace_uses_configured_id_when_valid() {
        let mut me = whoami(vec![
            membership(1, "Personal", "owner"),
            membership(2, "Team", "member"),
        ]);
        resolve_workspace(&mut me, Some(2)).unwrap();
        assert_eq!(me.workspace_id, Some(2));
        assert_eq!(me.workspace_name, Some("Team".into()));
        assert_eq!(me.role, Some("member".into()));
    }

    #[test]
    fn resolve_workspace_rejects_unknown_configured_id() {
        let mut me = whoami(vec![membership(1, "Personal", "owner")]);
        let err = resolve_workspace(&mut me, Some(7)).unwrap_err();
        assert!(err.contains("REMOTER_WORKSPACE_ID=7"), "{err}");
    }

    #[test]
    fn resolve_workspace_falls_back_to_single_membership() {
        let mut me = whoami(vec![membership(1, "Personal", "owner")]);
        resolve_workspace(&mut me, None).unwrap();
        assert_eq!(me.workspace_id, Some(1));
        assert_eq!(me.workspace_name, Some("Personal".into()));
        assert_eq!(me.role, Some("owner".into()));
    }

    #[test]
    fn resolve_workspace_errors_when_multiple_memberships_without_config() {
        let mut me = whoami(vec![
            membership(1, "Personal", "owner"),
            membership(2, "Team", "member"),
        ]);
        let err = resolve_workspace(&mut me, None).unwrap_err();
        assert!(err.contains("multiple workspaces"), "{err}");
    }

    #[test]
    fn retry_delay_grows_exponentially_within_jitter_bounds() {
        let base = std::time::Duration::from_secs(1);
        for (attempt, expected_secs) in [(1, 1u64), (2, 2), (3, 4), (4, 8)] {
            for _ in 0..50 {
                let d = crate::retry_delay(base, attempt);
                let min = std::time::Duration::from_secs(expected_secs);
                let max = min + min / 2;
                assert!(
                    d >= min && d < max,
                    "attempt {attempt}: {d:?} not in [{min:?}, {max:?})"
                );
            }
        }
    }

    #[test]
    fn retry_delay_stays_within_startup_budget() {
        // 5 attempts with the production defaults: 4 waits of 1/2/4/8 s plus
        // up to +50% jitter must stay under the 30 s startup budget.
        let total: std::time::Duration = (1..crate::MAX_VALIDATE_ATTEMPTS)
            .map(|a| crate::retry_delay(crate::VALIDATE_BASE_DELAY, a))
            .sum();
        assert!(total < std::time::Duration::from_secs(30), "{total:?}");
    }
}
