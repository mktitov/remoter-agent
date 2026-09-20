//! Typed HTTP client for the Remoter backend. Each method makes one request
//! with the agent's bearer token and maps non-2xx responses to [`McpError`].
//!
//! The client holds no DB credentials — only a base URL and a token (spec §2.1).

use serde::{Deserialize, Serialize};

use crate::error::McpError;

/// Client-side attachment size cap, mirroring the backend's `max_upload_bytes`
/// (docs/specs/attachments.md §3): 25 MiB. Enforced before upload (decoded
/// size) and before returning a downloaded body.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

/// A workspace the caller belongs to, as returned by `GET /auth/me`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMembership {
    pub id: i32,
    pub name: String,
    pub role: String,
}

/// The agent's identity, cached from the startup `GET /auth/me` validation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    pub id: i32,
    pub name: String,
    pub kind: String,
    pub workspaces: Vec<WorkspaceMembership>,
    /// Effective workspace set by the startup resolver. Not part of the backend
    /// response, but included in the `whoami` tool output.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<i32>,
    /// Effective workspace name set by the startup resolver.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// Effective role in the effective workspace.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// A link to another task, from the viewing task's perspective.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskLinkRef {
    pub link_id: i32,
    pub relation: String,
    pub task_id: i32,
    pub title: String,
    pub task_status: String,
    pub feature_id: i32,
    pub project_id: i32,
    #[serde(default)]
    pub created_by: Option<i32>,
}

/// A feature summary returned by `GET /features`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureDto {
    pub id: i32,
    #[serde(alias = "project_id")]
    pub project_id: i32,
    pub description: String,
    #[serde(alias = "tasks_total")]
    pub tasks_total: i32,
    #[serde(alias = "tasks_completed")]
    pub tasks_completed: i32,
}

/// A project summary returned by `GET /projects` (entity JSON; the kept fields
/// are single words, so snake_case/camelCase serialize identically).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectDto {
    pub id: i32,
    pub name: String,
    pub goal: String,
}

/// A compact task search hit from `GET /tasks?search=` (camelCase DTO).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSearchItemDto {
    pub id: i32,
    pub title: String,
    pub task_status: String,
    pub project_id: i32,
    pub feature_id: i32,
}

/// One kanban card from `GET /boards` (subset of the backend's camelCase
/// `BoardCard`; serde ignores the rest).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardCardDto {
    pub id: i32,
    pub project_id: i32,
    pub project_name: String,
    pub feature_id: i32,
    pub feature_description: String,
    pub title: String,
    pub task_status: String,
    pub assignee_id: Option<i32>,
    pub assignee_name: Option<String>,
    pub actions_total: i32,
    pub actions_completed: i32,
    pub completed_at: Option<String>,
}

/// A task summary with denormalized project/feature names (from `GET /tasks?assignee=me`).
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// `"task" | "research" | "bug"` (backend `TaskKind`, snake_case).
    pub task_kind: String,
    /// `"low" | "medium" | "high" | "critical"` (backend `TaskPriority`).
    pub task_priority: String,
    pub actions_total: i32,
    pub actions_completed: i32,
    pub actions_rejected: i32,
    pub time_spent: i64,
    #[serde(default)]
    pub blocked: bool,
    /// Whether a human requested an agent review of this task (always present
    /// on the backend's TaskSummary; defaulted for older backends).
    #[serde(default)]
    pub agent_review_requested: bool,
    /// The business goal the task is linked to (`goalId`). Additive field —
    /// absent on backends older than the goals rollout (remoter#199).
    #[serde(default)]
    pub goal_id: Option<i32>,
}

/// A task detail with actions and assignment info (from `GET /tasks/{id}?include=actions`).
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// `"task" | "research" | "bug"` (backend `TaskKind`, snake_case).
    pub task_kind: String,
    /// `"low" | "medium" | "high" | "critical"` (backend `TaskPriority`).
    pub task_priority: String,
    pub actions_total: i32,
    pub actions_completed: i32,
    pub actions_rejected: i32,
    pub time_spent: i64,
    #[serde(default)]
    pub blocked: bool,
    pub assignee_id: Option<i32>,
    pub assignee_name: Option<String>,
    pub completed_at: Option<String>,
    pub actions: Vec<serde_json::Value>,
    /// The markdown implementation report (`task_outcomes` kind=report), if any.
    pub report: Option<String>,
    /// The report outcome row id — attachments for report artifacts use it as
    /// `ownerId` with `ownerKind=3`. Present even for an empty report.
    pub report_outcome_id: Option<i32>,
    /// The approved plan (`task_outcomes` kind=plan), if any.
    pub plan: Option<String>,
    /// The plan outcome row id.
    pub plan_outcome_id: Option<i32>,
    /// The ticket's PR/MR URL — a separate artifact written by the daemon
    /// (`agent_runs.pr_url`); absent until the first successful push/PR step.
    pub pr_url: Option<String>,
    /// Total number of comments in the ticket's thread.
    #[serde(default)]
    pub comments_count: i64,
    /// Task-to-task links (blocks / blocked-by / relates / parent / subtask).
    /// Absent unless `get_task` is updated to request `include=links`.
    #[serde(default)]
    pub links: Option<Vec<TaskLinkRef>>,
    /// The ticket's open/answered questions with their options. Absent unless
    /// `include=questions` was requested.
    #[serde(default)]
    pub questions: Option<Vec<TaskQuestionWithOptionsDto>>,
    /// Whether the caller may start an agent review run on this task right now
    /// (always present on the backend's TaskDetail; defaulted for older backends).
    #[serde(default)]
    pub agent_review_available: bool,
    /// The business goal the task is linked to (`goalId`). Additive field —
    /// absent on backends older than the goals rollout (remoter#199).
    #[serde(default)]
    pub goal_id: Option<i32>,
}

/// One comment in a ticket's thread (`GET /tasks/{id}/comments`; entity JSON
/// is snake_case). Attachment links in `body` carry the attachment id in the
/// URL — `read_attachment` takes it directly.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskCommentDto {
    pub id: i32,
    pub task_id: i32,
    pub author_id: i32,
    pub body: String,
    pub created_at: String,
}

/// One structured question on a ticket (`GET /tasks/{id}/questions`; entity
/// JSON is snake_case) with its answer options.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskQuestionWithOptionsDto {
    pub id: i32,
    pub task_id: i32,
    pub author_id: i32,
    pub body: String,
    /// `true` when several options may apply, `false` when exactly one.
    pub multiple: bool,
    /// `"open" | "answered"` (backend `QuestionStatus`, lowercase).
    pub status: String,
    pub answer_text: Option<String>,
    pub answerer_id: Option<i32>,
    pub answered_at: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub options: Vec<TaskQuestionOptionDto>,
}

/// One answer option of a task question (entity JSON, snake_case).
/// `selected` flips to true when the human's answer picks it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskQuestionOptionDto {
    pub id: i32,
    pub question_id: i32,
    pub body: String,
    pub position: i32,
    pub selected: bool,
}

/// One attachment row from `GET /attachments` (camelCase DTO, spec §3.3).
/// Only the fields the tools need; the rest are ignored by serde.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: i32,
    pub file_name: String,
    pub content_type: String,
    pub size_bytes: i64,
}

/// Response of `POST /attachments/uploads`: the row id plus the presigned PUT
/// URL the bytes are uploaded to (spec §3.1).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedUpload {
    pub attachment_id: i32,
    pub upload_url: String,
}

/// A downloaded attachment's body plus its content type (`read_attachment`).
#[derive(Debug)]
pub struct DownloadedAttachment {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// One active workspace member from `GET /users/assignable` (assignee picker).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssignableUserDto {
    pub id: i32,
    pub name: String,
}

/// HTTP client wrapping [`reqwest::Client`] with the agent's token pre-set.
#[derive(Clone)]
pub struct RemoterClient {
    http: reqwest::Client,
    /// Plain client with no auth headers, used for presigned storage URLs where
    /// the URL itself carries the signature (spec §3.1).
    plain_http: reqwest::Client,
    base_url: String,
}

impl RemoterClient {
    pub fn new(base_url: String, token: String, workspace_id: Option<i32>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse().expect("valid header value"),
        );
        if let Some(id) = workspace_id {
            headers.insert("X-Workspace-Id", id.to_string().parse().expect("valid header value"));
        }
        Self {
            http: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .expect("reqwest client builder"),
            plain_http: reqwest::Client::builder().build().expect("reqwest client builder"),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Validates the token at startup via `GET /auth/me`. Returns the agent's identity.
    pub async fn validate(&self) -> Result<WhoAmI, McpError> {
        self.get("/api/v1/auth/me").await
    }

    // ── Discovery ───────────────────────────────────────────────────────

    /// `GET /tasks?assignee=me` — tasks assigned to the calling agent.
    pub async fn list_my_tasks(
        &self,
        status: Option<&str>,
        project_id: Option<i32>,
        limit: Option<i32>,
    ) -> Result<Vec<TaskSummary>, McpError> {
        let mut url = reqwest::Url::parse(&format!("{}{}", self.base_url, "/api/v1/tasks"))
            .map_err(|e| McpError::internal(format!("bad base url: {e}")))?;
        url.query_pairs_mut().append_pair("assignee", "me");
        if let Some(s) = status {
            url.query_pairs_mut().append_pair("status", s);
        }
        if let Some(pid) = project_id {
            url.query_pairs_mut().append_pair("projectId", &pid.to_string());
        }
        if let Some(l) = limit {
            url.query_pairs_mut().append_pair("limit", &l.to_string());
        }
        let resp = self.http.get(url).send().await?;
        Self::decode(resp).await
    }

    /// `GET /tasks/{id}?include=actions,links,questions` — task detail with
    /// actions, links, and questions.
    pub async fn get_task(&self, task_id: i32) -> Result<TaskDetail, McpError> {
        self.get(&format!("/api/v1/tasks/{task_id}?include=actions,links,questions"))
            .await
    }

    /// `GET /features?project_id=` — list features in a project.
    pub async fn list_features(&self, project_id: i32) -> Result<Vec<FeatureDto>, McpError> {
        self.get(&format!("/api/v1/features?project_id={project_id}")).await
    }

    /// `GET /features/{id}` — get a single feature.
    pub async fn get_feature(&self, feature_id: i32) -> Result<FeatureDto, McpError> {
        self.get(&format!("/api/v1/features/{feature_id}")).await
    }

    /// `GET /projects` — list all projects.
    pub async fn list_projects(&self) -> Result<Vec<ProjectDto>, McpError> {
        self.get("/api/v1/projects").await
    }

    /// `GET /projects/{id}/goals` — the project's business goals (archived
    /// goals are excluded by the backend). Forwarded as raw JSON: the goal
    /// read-model is owned by the backend (remoter docs/specs/business-goals.md),
    /// and the MCP layer does not mirror its field set.
    pub async fn list_goals(&self, project_id: i32) -> Result<serde_json::Value, McpError> {
        self.get(&format!("/api/v1/projects/{project_id}/goals")).await
    }

    /// `PATCH /tasks/{id}/set-goal` — link the task to a business goal
    /// (`Some`) or unlink it (`None` → `"goalId": null`). The backend
    /// validates that the goal belongs to the task's project (404 on an
    /// unknown goal).
    pub async fn set_task_goal(&self, task_id: i32, goal_id: Option<i32>) -> Result<serde_json::Value, McpError> {
        let body = serde_json::json!({ "goalId": goal_id });
        let resp = self
            .http
            .patch(format!("{}/api/v1/tasks/{task_id}/set-goal", self.base_url))
            .json(&body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `GET /tasks?search=` — compact task search by `#id`/id/title substring.
    pub async fn search_tasks(&self, query: &str, limit: Option<i32>) -> Result<Vec<TaskSearchItemDto>, McpError> {
        let mut url = reqwest::Url::parse(&format!("{}{}", self.base_url, "/api/v1/tasks"))
            .map_err(|e| McpError::internal(format!("bad base url: {e}")))?;
        url.query_pairs_mut().append_pair("search", query);
        if let Some(l) = limit {
            url.query_pairs_mut().append_pair("limit", &l.to_string());
        }
        let resp = self.http.get(url).send().await?;
        Self::decode(resp).await
    }

    /// `GET /boards` — kanban cards, optionally scoped to one project and with
    /// a Completed-column window (`today|week|month|all`).
    pub async fn list_board(
        &self,
        project_id: Option<i32>,
        completed: Option<&str>,
    ) -> Result<Vec<BoardCardDto>, McpError> {
        let mut url = reqwest::Url::parse(&format!("{}{}", self.base_url, "/api/v1/boards"))
            .map_err(|e| McpError::internal(format!("bad base url: {e}")))?;
        if let Some(pid) = project_id {
            url.query_pairs_mut().append_pair("scope", "project");
            url.query_pairs_mut().append_pair("projectId", &pid.to_string());
        }
        if let Some(c) = completed {
            url.query_pairs_mut().append_pair("completed", c);
        }
        let resp = self.http.get(url).send().await?;
        Self::decode(resp).await
    }

    // ── Workflow ────────────────────────────────────────────────────────

    /// `PATCH /tasks/{id}/status` — set the board workflow status.
    pub async fn set_task_status(&self, task_id: i32, status: &str) -> Result<serde_json::Value, McpError> {
        let body = serde_json::json!({ "taskStatus": status });
        let resp = self
            .http
            .patch(format!("{}/api/v1/tasks/{task_id}/status", self.base_url))
            .json(&body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `PUT /tasks/{id}/report` — set (`Some`) or clear (`None` → empty body)
    /// the task's markdown implementation report. Returns the `TaskOutcome`
    /// row (`kind: "report"`).
    pub async fn set_task_report(&self, task_id: i32, report: Option<String>) -> Result<serde_json::Value, McpError> {
        let body = serde_json::json!({ "body": report.unwrap_or_default() });
        let resp = self
            .http
            .put(format!("{}/api/v1/tasks/{task_id}/report", self.base_url))
            .json(&body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `PUT /tasks/{id}/review` — write the supervisor's review of a child
    /// ticket. `verdict` is `"approve"` or `"changes_requested"`; the backend
    /// only accepts this from a parent-scoped agent supervisor (403 otherwise).
    /// Returns the `TaskOutcome` row (`kind: "review"`).
    pub async fn set_task_review(
        &self,
        task_id: i32,
        markdown: &str,
        verdict: &str,
    ) -> Result<serde_json::Value, McpError> {
        let body = serde_json::json!({ "body": markdown, "verdict": verdict });
        let resp = self
            .http
            .put(format!("{}/api/v1/tasks/{task_id}/review", self.base_url))
            .json(&body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `POST /tasks/{id}/unassign` — unassign the agent from the task.
    pub async fn unassign_task(&self, task_id: i32) -> Result<serde_json::Value, McpError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/tasks/{task_id}/unassign", self.base_url))
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `POST /tasks/{id}/assign` — assign the task to a workspace member
    /// (`assigneeId` body field). The backend enforces `Contribute` plus the
    /// parent-scoped gate for agent callers (docs/specs/task-links.md).
    pub async fn assign_task(&self, task_id: i32, assignee_id: i32) -> Result<serde_json::Value, McpError> {
        let body = serde_json::json!({ "assigneeId": assignee_id });
        let resp = self
            .http
            .post(format!("{}/api/v1/tasks/{task_id}/assign", self.base_url))
            .json(&body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `GET /users/assignable` — active workspace members as the minimal
    /// assignee-picker projection (`id` + `name`, no PII). Open to any
    /// authenticated workspace member.
    pub async fn list_assignable(&self) -> Result<Vec<AssignableUserDto>, McpError> {
        self.get("/api/v1/users/assignable").await
    }

    /// `POST /tasks` — create a new task.
    pub async fn create_task(&self, body: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/tasks", self.base_url))
            .json(body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `PUT /tasks/{id}` — update a task.
    pub async fn update_task(&self, task_id: i32, body: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let resp = self
            .http
            .put(format!("{}/api/v1/tasks/{task_id}", self.base_url))
            .json(body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    // ── Actions ─────────────────────────────────────────────────────────

    /// `PUT /actions/{id}` — update an action (activate/complete).
    pub async fn update_action(&self, action_id: i32, body: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let resp = self
            .http
            .put(format!("{}/api/v1/actions/{action_id}", self.base_url))
            .json(body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `POST /actions/{id}/reject` — reject an action with a reason.
    pub async fn reject_action(&self, action_id: i32, reason: &str) -> Result<serde_json::Value, McpError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/actions/{action_id}/reject", self.base_url))
            .json(&serde_json::json!({ "reason": reason }))
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `POST /actions` — create a new action on a task.
    pub async fn create_action(&self, body: &serde_json::Value) -> Result<serde_json::Value, McpError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/actions", self.base_url))
            .json(body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `DELETE /actions/{id}` — delete an action.
    pub async fn delete_action(&self, action_id: i32) -> Result<(), McpError> {
        let resp = self
            .http
            .delete(format!("{}/api/v1/actions/{action_id}", self.base_url))
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(McpError::from_http(status, body))
        }
    }

    // ── Attachments & comments (spec §3.5) ───────────────────────────────

    /// The backend base URL — the tools build stable attachment download links
    /// from it (the URL never expires; only the 302 redirect target is presigned).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `POST /attachments/uploads` — step 1 of the upload flow: register the
    /// intent and get the presigned PUT URL.
    pub async fn create_upload(
        &self,
        owner_kind: i16,
        owner_id: i32,
        file_name: &str,
        content_type: &str,
        size_bytes: i64,
    ) -> Result<CreatedUpload, McpError> {
        let body = serde_json::json!({
            "ownerKind": owner_kind,
            "ownerId": owner_id,
            "fileName": file_name,
            "contentType": content_type,
            "sizeBytes": size_bytes,
        });
        let resp = self
            .http
            .post(format!("{}/api/v1/attachments/uploads", self.base_url))
            .json(&body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// PUT the bytes directly to the presigned S3 URL (step 2). **No** bearer
    /// token: the URL carries its own signature, and leaking the agent token
    /// to the storage host would be a credential leak.
    pub async fn put_presigned(&self, upload_url: &str, content_type: &str, bytes: Vec<u8>) -> Result<(), McpError> {
        let resp = self
            .plain_http
            .put(upload_url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(McpError::from_http(status, body))
        }
    }

    /// `POST /attachments/{id}/confirm` — step 3: mark the upload finished.
    pub async fn confirm_upload(&self, attachment_id: i32) -> Result<Attachment, McpError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/attachments/{attachment_id}/confirm", self.base_url))
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `GET /attachments?owner_kind=&owner_id=` — confirmed attachments of one
    /// feature (`owner_kind=1`), task (`2`), or task outcome (`3`, e.g. the
    /// report's `reportOutcomeId`).
    pub async fn list_attachments(&self, owner_kind: i16, owner_id: i32) -> Result<Vec<Attachment>, McpError> {
        self.get(&format!(
            "/api/v1/attachments?owner_kind={owner_kind}&owner_id={owner_id}"
        ))
        .await
    }

    /// `GET /attachments/{id}/download` — follows the 302 to the presigned GET
    /// (reqwest's default redirect policy; it strips the Authorization header
    /// on the cross-host hop, where the presign in the URL authenticates).
    /// Bodies over the size cap are refused before being returned.
    pub async fn download_attachment(&self, attachment_id: i32) -> Result<DownloadedAttachment, McpError> {
        let resp = self
            .http
            .get(format!("{}/api/v1/attachments/{attachment_id}/download", self.base_url))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(McpError::from_http(status, body));
        }
        if let Some(len) = resp.content_length()
            && len > MAX_ATTACHMENT_BYTES
        {
            return Err(McpError::invalid_params(format!(
                "attachment {attachment_id} is {len} bytes — over the 25 MB read cap"
            )));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp.bytes().await?;
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(McpError::invalid_params(format!(
                "attachment {attachment_id} is {} bytes — over the 25 MB read cap",
                bytes.len()
            )));
        }
        Ok(DownloadedAttachment {
            content_type,
            bytes: bytes.to_vec(),
        })
    }

    /// `POST /tasks/{id}/links` — create a task link from the perspective of `task_id`.
    pub async fn add_task_link(
        &self,
        task_id: i32,
        other_task_id: i32,
        relation: &str,
    ) -> Result<TaskLinkRef, McpError> {
        let body = serde_json::json!({
            "otherTaskId": other_task_id,
            "relation": relation,
        });
        let resp = self
            .http
            .post(format!("{}/api/v1/tasks/{task_id}/links", self.base_url))
            .json(&body)
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `DELETE /tasks/{id}/links/{link_id}` — remove a task link.
    pub async fn remove_task_link(&self, task_id: i32, link_id: i32) -> Result<(), McpError> {
        let resp = self
            .http
            .delete(format!("{}/api/v1/tasks/{task_id}/links/{link_id}", self.base_url))
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(McpError::from_http(status, body))
        }
    }

    /// `POST /tasks/{id}/comments` — append a comment to the ticket thread.
    pub async fn add_task_comment(&self, task_id: i32, body: &str) -> Result<serde_json::Value, McpError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/tasks/{task_id}/comments", self.base_url))
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `GET /tasks/{id}/comments` — read the ticket thread with pagination
    /// (`after_id` = only newer, `limit` = max rows, `tail` = the last N,
    /// chronological).
    pub async fn list_task_comments(
        &self,
        task_id: i32,
        after_id: Option<i32>,
        limit: Option<i32>,
        tail: Option<i32>,
    ) -> Result<Vec<TaskCommentDto>, McpError> {
        let mut url = reqwest::Url::parse(&format!("{}/api/v1/tasks/{task_id}/comments", self.base_url))
            .map_err(|e| McpError::internal(format!("bad base url: {e}")))?;
        if let Some(a) = after_id {
            url.query_pairs_mut().append_pair("afterId", &a.to_string());
        }
        if let Some(l) = limit {
            url.query_pairs_mut().append_pair("limit", &l.to_string());
        }
        if let Some(t) = tail {
            url.query_pairs_mut().append_pair("tail", &t.to_string());
        }
        let resp = self.http.get(url).send().await?;
        Self::decode(resp).await
    }

    /// `POST /tasks/{id}/questions` — register a structured question (with
    /// optional answer options) on the ticket.
    pub async fn add_task_question(
        &self,
        task_id: i32,
        body: &str,
        options: Vec<String>,
        multiple: bool,
    ) -> Result<TaskQuestionWithOptionsDto, McpError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/tasks/{task_id}/questions", self.base_url))
            .json(&serde_json::json!({
                "body": body,
                "multiple": multiple,
                "options": options,
            }))
            .send()
            .await?;
        Self::decode(resp).await
    }

    /// `GET /tasks/{id}/questions` — the ticket's questions with options and
    /// answers, chronological.
    pub async fn list_task_questions(&self, task_id: i32) -> Result<Vec<TaskQuestionWithOptionsDto>, McpError> {
        self.get(&format!("/api/v1/tasks/{task_id}/questions")).await
    }

    /// `DELETE /tasks/{id}/questions/{question_id}` — hard-delete a question.
    pub async fn delete_task_question(&self, task_id: i32, question_id: i32) -> Result<(), McpError> {
        let resp = self
            .http
            .delete(format!(
                "{}/api/v1/tasks/{task_id}/questions/{question_id}",
                self.base_url
            ))
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(McpError::from_http(status, body))
        }
    }

    // ── Internal helpers ────────────────────────────────────────────────

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, McpError> {
        let resp = self.http.get(format!("{}{path}", self.base_url)).send().await?;
        Self::decode(resp).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, McpError> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp.json().await?)
        } else {
            // Try to read the backend's error body for a better message.
            let body = resp.text().await.unwrap_or_default();
            Err(McpError::from_http(status, body))
        }
    }
}
