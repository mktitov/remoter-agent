//! Typed HTTP client of the Remoter backend (mirrors the mcp-server client's
//! shape, extended with the agent-run endpoints). Holds only a base URL and the
//! agent token — no DB credentials (spec §2.2).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// API call failure. `status` is `None` for transport-level errors (connect,
/// timeout, body decode) — those are always transient.
#[derive(Debug)]
pub struct ClientError {
    pub status: Option<reqwest::StatusCode>,
    pub message: String,
}

impl ClientError {
    fn transport(e: reqwest::Error) -> Self {
        Self {
            status: e.status(),
            message: e.to_string(),
        }
    }

    /// The claim race outcome (spec §5.3): someone else moved the ticket first.
    pub fn is_conflict(&self) -> bool {
        self.status == Some(reqwest::StatusCode::CONFLICT)
    }

    /// 403 — e.g. a rollback write rejected because a human moved the ticket to
    /// a state the agent transition set doesn't cover (spec §3.2/§5.7).
    pub fn is_forbidden(&self) -> bool {
        self.status == Some(reqwest::StatusCode::FORBIDDEN)
    }

    /// Transient per spec §5.7: transport errors, 429, 5xx.
    pub fn is_transient(&self) -> bool {
        match self.status {
            None => true,
            Some(s) => s == reqwest::StatusCode::TOO_MANY_REQUESTS || s.is_server_error(),
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "HTTP {s}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ClientError {}

/// A workspace the caller belongs to, as returned by `GET /auth/me`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMembership {
    pub id: i32,
    pub name: String,
    pub role: String,
}

/// The caller's identity from `GET /auth/me`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    pub id: i32,
    pub name: String,
    pub kind: String,
    pub workspaces: Vec<WorkspaceMembership>,
}

/// A project row from `GET /projects` (snake_case — the backend serializes the
/// `Project` entity as-is). Only the fields the daemon needs; the rest are
/// ignored by serde.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectDto {
    pub id: i32,
    pub repo_url: Option<String>,
    pub base_branch: Option<String>,
    #[serde(default)]
    pub staging_auto_start: bool,
}

/// The daemon's view of one agent-managed project's repo config (spec §4.4),
/// fetched over the API each poll cycle. Executability = non-NULL `repo_url`.
#[derive(Debug, Clone)]
pub struct ProjectRepoConfig {
    pub project_id: i32,
    pub repo_url: String,
    /// `None` = remote default branch (HEAD).
    pub base_branch: Option<String>,
    pub staging_auto_start: bool,
}

impl ProjectRepoConfig {
    /// `None` when the project is not agent-managed (no `repo_url`). A blank
    /// `repo_url` is treated as missing too — the backend normalizes blank to
    /// NULL on write, this keeps the invariant local to the daemon as well.
    pub fn from_dto(dto: ProjectDto) -> Option<Self> {
        dto.repo_url.filter(|u| !u.trim().is_empty()).map(|repo_url| Self {
            project_id: dto.id,
            repo_url,
            base_branch: dto.base_branch,
            staging_auto_start: dto.staging_auto_start,
        })
    }
}

/// The gated forge-config payload from `GET /projects/{id}/forge-config`
/// (camelCase, spec §4.4) — the only API response carrying the raw forge token.
/// All-null `forge_kind` = no forge configured; the daemon skips push/PR.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgeConfigDto {
    pub forge_kind: Option<String>,
    pub forge_api_url: Option<String>,
    pub forge_token: Option<String>,
}

/// A task card from `GET /tasks?assignee=me` (camelCase DTO).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSummary {
    pub id: i32,
    pub project_id: i32,
    pub project_name: String,
    pub feature_id: i32,
    pub feature_description: String,
    pub title: String,
    pub description: String,
    pub task_status: String,
    /// snake_case `TaskPriority` token (`low`/`medium`/`high`/`critical`);
    /// `None` only when talking to an older backend.
    #[serde(default)]
    pub task_priority: Option<String>,
    pub actions_total: i32,
    pub actions_completed: i32,
    pub time_spent: i64,
    #[serde(default)]
    pub blocked: bool,
    /// A human asked for an agent review of this ticket while it sits in
    /// `review` (#142). The review poll loop triggers a `review` run on it;
    /// starting the run clears the flag server-side. Absent on older backends.
    #[serde(default)]
    pub agent_review_requested: bool,
}

/// One action inside `TaskDetail`; extra fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct ActionItem {
    pub id: i32,
    pub description: String,
    pub status: String,
    #[serde(default)]
    pub rejection_reason: Option<String>,
}

/// One entry of the ticket thread (`task_comments` row, snake_case entity
/// JSON). Embedded in `TaskDetail` when `comments` is included.
#[derive(Debug, Clone, Deserialize)]
pub struct CommentDto {
    pub id: i32,
    pub task_id: i32,
    pub author_id: i32,
    pub body: String,
    /// RFC 3339 timestamp, kept opaque (the daemon never parses it).
    pub created_at: String,
}

/// One attachment of the task (camelCase DTO, docs/specs/attachments.md).
/// Embedded in `TaskDetail` when `attachments` is included; only the fields
/// the prompt needs — the rest are ignored by serde.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    pub id: i32,
    pub file_name: String,
    pub content_type: String,
    pub size_bytes: i64,
}

/// One task-to-task link as the backend serializes it (camelCase DTO —
/// `TaskLinkRef` in `domain/entities/task_link.rs`, not the `task_links`
/// entity). Embedded in `TaskDetail` when `links` is included; only the fields
/// the daemon needs — the rest are ignored by serde.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkDto {
    pub relation: String,
    pub task_id: i32,
    pub created_by: Option<i32>,
}

/// A task question with its answer options (`include=questions`; entity JSON
/// is snake_case). `status` is `"open" | "answered"`; the answer shows up as
/// `selected` options and/or `answer_text`.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskQuestionDto {
    pub id: i32,
    pub body: String,
    pub multiple: bool,
    pub status: String,
    pub answer_text: Option<String>,
    #[serde(default)]
    pub options: Vec<TaskQuestionOptionDto>,
}

/// One answer option of a task question (entity JSON, snake_case).
#[derive(Debug, Clone, Deserialize)]
pub struct TaskQuestionOptionDto {
    pub body: String,
    pub selected: bool,
}

/// Serde default for bool fields that must stay `true` when an older backend
/// does not send them yet (additive deploys).
fn default_true() -> bool {
    true
}

/// `GET /tasks/{id}?include=actions,runs,comments,attachments,links,questions`
/// — the full prompt context in one call (spec §4.3/§5.6). `runs`/`comments`/
/// `attachments`/`links`/`questions` are absent unless the backend was asked
/// for them (this client always asks).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetail {
    pub id: i32,
    pub project_id: i32,
    pub project_name: String,
    pub feature_id: i32,
    pub feature_description: String,
    pub title: String,
    pub description: String,
    pub task_status: String,
    #[serde(default)]
    pub task_priority: Option<String>,
    /// Unfinished `blocked_by` blockers exist (TaskDetail flattens the
    /// backend's TaskSummary, which carries `blocked`).
    #[serde(default)]
    pub blocked: bool,
    pub assignee_id: Option<i32>,
    pub assignee_name: Option<String>,
    #[serde(default)]
    pub actions: Vec<ActionItem>,
    #[serde(default)]
    pub runs: Option<Vec<AgentRunDto>>,
    #[serde(default)]
    pub comments: Option<Vec<CommentDto>>,
    #[serde(default)]
    pub attachments: Option<Vec<AttachmentDto>>,
    #[serde(default)]
    pub links: Option<Vec<LinkDto>>,
    /// The ticket's open/answered questions with options and answers
    /// (entity JSON, snake_case); feeds the prompt's "## Open questions"
    /// section (spec §5.6).
    #[serde(default)]
    pub questions: Option<Vec<TaskQuestionDto>>,
    /// The task's implementation report body (`task_outcomes` kind=report);
    /// absent until the dev-agent writes it via `set_task_report`. The
    /// post-finish report check reads this (spec §5.6).
    #[serde(default)]
    pub report: Option<String>,
    /// The supervisor's review body (`task_outcomes` kind=review); absent until
    /// a parent-scoped supervisor writes it via `set_task_review` (#116). The
    /// supervision loop reads the verdict after a supervise run (#115).
    #[serde(default)]
    pub review: Option<String>,
    /// The review outcome row's id (`reviewOutcomeId`) — present whenever a
    /// review was written, even with an empty body.
    #[serde(default)]
    pub review_outcome_id: Option<i32>,
    /// When the current review verdict was written (`reviewOutcomeUpdatedAt`).
    /// The supervise run only acts on verdicts written during the run itself;
    /// an older timestamp means the verdict belongs to a previous review
    /// cycle and must not be re-applied (review #110).
    #[serde(default)]
    pub review_outcome_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The review verdict token (`approve`/`changes_requested`), same row.
    #[serde(default)]
    pub review_verdict: Option<String>,
    /// When the task entered `completed` (`completedAt`); the supervision
    /// loop fingerprints the children-completed cycle with it so the final
    /// integration trigger re-arms once a child leaves `completed`.
    #[serde(default)]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Supervision toggle (`supervisionEnabled`): `false` on a parent ticket
    /// turns the whole supervision subtree off — no child kickoff, no
    /// supervise runs, no branch stacking. Absent on backends older than the
    /// flag; default `true` keeps current behavior (additive deploy).
    #[serde(default = "default_true")]
    pub supervision_enabled: bool,
}

/// An `agent_runs` row as the backend serializes it (entity = snake_case).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentRunDto {
    pub id: i32,
    pub task_id: i32,
    pub agent_user_id: i32,
    pub kind: String,
    pub status: String,
    pub session_id: Option<String>,
    pub branch: Option<String>,
    pub plan: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
    /// The draft PR/MR opened for the branch (spec §4.4); `None` when the
    /// project has no forge configured or push/PR failed.
    pub pr_url: Option<String>,
    /// Why push/PR failed on an otherwise successful run (spec §4.4);
    /// `None` when there was no failure.
    pub pr_error: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub attempt: i16,
    /// When the run row was opened (`created_at` — the entity serializes
    /// snake_case). The supervise run anchors its verdict freshness check on
    /// it.
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// `PATCH /agent-runs/{id}` body (camelCase, mirrors the backend's `FinishRun`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishRunBody {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

impl FinishRunBody {
    pub fn succeeded() -> Self {
        Self {
            status: "succeeded",
            session_id: None,
            branch: None,
            plan: None,
            summary: None,
            error: None,
            pr_url: None,
            pr_error: None,
            input_tokens: None,
            output_tokens: None,
            model: None,
            thinking: None,
        }
    }

    pub fn failed(error: String) -> Self {
        Self {
            status: "failed",
            session_id: None,
            branch: None,
            plan: None,
            summary: None,
            error: Some(error),
            pr_url: None,
            pr_error: None,
            input_tokens: None,
            output_tokens: None,
            model: None,
            thinking: None,
        }
    }
}

#[derive(Clone)]
pub struct RemoterClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    /// If set, every request carries `X-Workspace-Id` to scope calls to one workspace.
    workspace_id: Option<i32>,
}

impl RemoterClient {
    pub fn new(base_url: &str, token: &str, workspace_id: Option<i32>) -> Self {
        Self {
            // nginx closes idle keep-alive connections after ~75s, and the
            // infrequent loops (housekeeping, PR sync) run minutes apart — a
            // pooled connection that outlives the server's timeout gets reused
            // and fails mid-send ("error sending request"). Drop pool entries
            // well before that and bound connect/total time.
            http: reqwest::Client::builder()
                .pool_idle_timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .build()
                .expect("reqwest client builder"),
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            workspace_id,
        }
    }

    /// Attaches the bearer token and the optional workspace header to a request builder.
    fn auth_request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut builder = builder.bearer_auth(&self.token);
        if let Some(id) = self.workspace_id {
            builder = builder.header("X-Workspace-Id", id);
        }
        builder
    }

    /// Startup token validation (spec §5.1): `GET /auth/me`.
    pub async fn whoami(&self) -> Result<WhoAmI, ClientError> {
        self.get("/api/v1/auth/me").await
    }

    /// `GET /tasks?assignee=me[&status=…]` — tasks assigned to this agent.
    pub async fn my_tasks(&self, status: Option<&str>) -> Result<Vec<TaskSummary>, ClientError> {
        let mut url = format!("{}/api/v1/tasks?assignee=me", self.base_url);
        if let Some(s) = status {
            url.push_str(&format!("&status={s}"));
        }
        self.get(&url).await
    }

    /// `GET /projects` — all projects; the daemon filters to agent-managed
    /// ones (non-NULL `repo_url`) itself (spec §4.4/§5.1).
    pub async fn projects(&self) -> Result<Vec<ProjectDto>, ClientError> {
        self.get(&format!("{}/api/v1/projects", self.base_url)).await
    }

    /// `GET /projects/{id}/forge-config` — the gated forge payload incl. the
    /// raw token (spec §4.4). Fetched lazily, once per successful implement
    /// run, never per poll.
    pub async fn forge_config(&self, project_id: i32) -> Result<ForgeConfigDto, ClientError> {
        self.get(&format!("{}/api/v1/projects/{project_id}/forge-config", self.base_url))
            .await
    }

    /// The shared HTTP client — forge API calls (spec §4.4) authenticate with
    /// the project's forge token, not the daemon's backend token.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// `GET /tasks/{id}?include=actions,runs,comments,attachments,links,questions`
    /// — the full prompt context in one call (spec §5.6).
    pub async fn task_detail(&self, task_id: i32) -> Result<TaskDetail, ClientError> {
        self.get(&format!(
            "{}/api/v1/tasks/{task_id}?include=actions,runs,comments,attachments,links,questions",
            self.base_url
        ))
        .await
    }

    /// The backend base URL — `build_prompt` builds stable attachment download
    /// links from it (they never expire; only the 302 target is presigned).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `POST /tasks/{id}/comments` — append a daemon note to the ticket thread
    /// (spec §5.6). Best-effort: callers log failures, never fail the run.
    pub async fn add_comment(&self, task_id: i32, body: &str) -> Result<(), ClientError> {
        let resp = self
            .auth_request(
                self.http
                    .post(format!("{}/api/v1/tasks/{task_id}/comments", self.base_url)),
            )
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode_unit(resp).await
    }

    /// `POST /actions/{id}/reject` — reject an action with a reason.
    pub async fn reject_action(&self, action_id: i32, reason: &str) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .auth_request(
                self.http
                    .post(format!("{}/api/v1/actions/{action_id}/reject", self.base_url)),
            )
            .json(&serde_json::json!({ "reason": reason }))
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode(resp).await
    }

    /// `PATCH /tasks/{id}/status`. A 409 here is the claim-race loss (spec §5.3).
    pub async fn set_task_status(&self, task_id: i32, status: &str) -> Result<(), ClientError> {
        let resp = self
            .auth_request(
                self.http
                    .patch(format!("{}/api/v1/tasks/{task_id}/status", self.base_url)),
            )
            .json(&serde_json::json!({ "taskStatus": status }))
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode_unit(resp).await
    }

    /// `POST /tasks/{id}/assign` — set the assignee. The supervision loop
    /// uses this to staff an unassigned child ticket with this daemon's agent
    /// before kicking it off (parent-scoped rights, spec §4.3).
    pub async fn assign_task(&self, task_id: i32, assignee_id: i32) -> Result<(), ClientError> {
        let resp = self
            .auth_request(
                self.http
                    .post(format!("{}/api/v1/tasks/{task_id}/assign", self.base_url)),
            )
            .json(&serde_json::json!({ "assigneeId": assignee_id }))
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode_unit(resp).await
    }

    /// `POST /tasks/{id}/agent-runs { kind }` → the new (running) run row.
    pub async fn start_run(&self, task_id: i32, kind: &str) -> Result<AgentRunDto, ClientError> {
        let resp = self
            .auth_request(
                self.http
                    .post(format!("{}/api/v1/tasks/{task_id}/agent-runs", self.base_url)),
            )
            .json(&serde_json::json!({ "kind": kind }))
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode(resp).await
    }

    /// `PATCH /agent-runs/{id}` — finish a run with its outcome.
    pub async fn finish_run(&self, run_id: i32, body: &FinishRunBody) -> Result<AgentRunDto, ClientError> {
        let resp = self
            .auth_request(self.http.patch(format!("{}/api/v1/agent-runs/{run_id}", self.base_url)))
            .json(body)
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode(resp).await
    }

    /// `POST /agent-logs` — ship a batch of log lines (daemon + ACP
    /// conversation, ticket: agent logs in the Users tab). Best-effort:
    /// callers log failures and drop the batch.
    pub async fn post_agent_logs(&self, entries: &[crate::logs::LogLine]) -> Result<(), ClientError> {
        let resp = self
            .auth_request(self.http.post(format!("{}/api/v1/agent-logs", self.base_url)))
            .json(&serde_json::json!({ "entries": entries }))
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode_unit(resp).await
    }

    /// `GET /tasks/{id}/agent-runs` — run history, newest first.
    pub async fn list_runs(&self, task_id: i32) -> Result<Vec<AgentRunDto>, ClientError> {
        self.get(&format!("{}/api/v1/tasks/{task_id}/agent-runs", self.base_url))
            .await
    }

    // ── helpers ─────────────────────────────────────────────────────────

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, ClientError> {
        let url = if url.starts_with("http") {
            url.to_string()
        } else {
            format!("{}{url}", self.base_url)
        };
        let resp = self
            .auth_request(self.http.get(url))
            .send()
            .await
            .map_err(ClientError::transport)?;
        Self::decode(resp).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, ClientError> {
        let status = resp.status();
        if status.is_success() {
            resp.json().await.map_err(ClientError::transport)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(ClientError {
                status: Some(status),
                message: body,
            })
        }
    }

    async fn decode_unit(resp: reqwest::Response) -> Result<(), ClientError> {
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(ClientError {
                status: Some(status),
                message: body,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the backend serializes embedded links as the camelCase
    /// `TaskLinkRef` read-model (`taskId`/`createdBy`), not the snake_case
    /// `task_links` entity — a snake_case `LinkDto` fails the whole
    /// `TaskDetail` decode with reqwest's opaque "error decoding response
    /// body" for any task that has at least one link.
    #[test]
    fn task_detail_decodes_camel_case_links() {
        let json = serde_json::json!({
            "id": 71,
            "projectId": 1,
            "projectName": "remoter",
            "featureId": 2,
            "featureDescription": "agent",
            "title": "ticket",
            "description": "do the thing",
            "taskStatus": "implement",
            "assigneeId": 7,
            "assigneeName": "bot",
            "actions": [],
            "links": [
                {
                    "linkId": 3,
                    "relation": "parent",
                    "taskId": 72,
                    "title": "child ticket",
                    "taskStatus": "todo",
                    "featureId": 2,
                    "projectId": 1,
                    "createdBy": 7
                },
                {
                    "linkId": 4,
                    "relation": "relates",
                    "taskId": 73,
                    "title": "sibling",
                    "taskStatus": "done",
                    "featureId": 2,
                    "projectId": 1,
                    "createdBy": null
                }
            ]
        });
        let detail: TaskDetail = serde_json::from_value(json).expect("TaskDetail decodes");
        let links = detail.links.expect("links present");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].relation, "parent");
        assert_eq!(links[0].task_id, 72);
        assert_eq!(links[0].created_by, Some(7));
        assert_eq!(links[1].created_by, None);
    }

    /// `supervisionEnabled` defaults to `true` when absent — an older backend
    /// that does not send the field yet must keep supervision on (additive
    /// deploy); an explicit `false` turns it off.
    #[test]
    fn task_detail_supervision_enabled_defaults_true() {
        let base = serde_json::json!({
            "id": 71,
            "projectId": 1,
            "projectName": "remoter",
            "featureId": 2,
            "featureDescription": "agent",
            "title": "ticket",
            "description": "do the thing",
            "taskStatus": "review"
        });
        let detail: TaskDetail = serde_json::from_value(base.clone()).expect("TaskDetail decodes");
        assert!(detail.supervision_enabled, "absent field must default to true");

        let mut disabled = base;
        disabled["supervisionEnabled"] = serde_json::json!(false);
        let detail: TaskDetail = serde_json::from_value(disabled).expect("TaskDetail decodes");
        assert!(!detail.supervision_enabled);
    }

    /// `agentReviewRequested` defaults to `false` when absent — an older
    /// backend that does not send the flag yet must never trigger review runs
    /// (additive deploy); an explicit `true` does.
    #[test]
    fn task_summary_agent_review_requested_defaults_false() {
        let base = serde_json::json!({
            "id": 71,
            "projectId": 1,
            "projectName": "remoter",
            "featureId": 2,
            "featureDescription": "agent",
            "title": "ticket",
            "description": "do the thing",
            "taskStatus": "review",
            "actionsTotal": 0,
            "actionsCompleted": 0,
            "timeSpent": 0
        });
        let summary: TaskSummary = serde_json::from_value(base.clone()).expect("TaskSummary decodes");
        assert!(!summary.agent_review_requested, "absent field must default to false");

        let mut requested = base;
        requested["agentReviewRequested"] = serde_json::json!(true);
        let summary: TaskSummary = serde_json::from_value(requested).expect("TaskSummary decodes");
        assert!(summary.agent_review_requested);
    }
}
