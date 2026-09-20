//! The MCP tool surface: discovery tools (`list_projects`, `list_features`,
//! `search_tasks`, `list_board`, `list_my_tasks`, `get_task`, `whoami`,
//! `list_users`), workflow tools that advance tasks and actions (including
//! supervision: `assign_task` plus the `dev-agent-supervise` role scoping),
//! plus the attachments & comments group (spec §3.5). Each tool makes one HTTP
//! call to the backend via [`RemoterClient`] — except `add_attachment`, which
//! drives the whole presign flow (upload intent → PUT bytes → confirm) itself,
//! and `create_task` with `assigneeId` (create → assign). The MCP layer
//! enforces no permissions — it forwards the caller's bearer token and relies
//! on the backend's RBAC.

use base64ct::{Base64, Encoding};
use rmcp::{
    ErrorData as McpErrorData,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ErrorCode},
    schemars, tool, tool_router,
};

use crate::{
    Role,
    client::{MAX_ATTACHMENT_BYTES, RemoterClient, TaskLinkRef, WhoAmI},
    error::McpError,
};

/// The MCP server. Holds the HTTP client and the cached agent identity.
/// The `tool_router` field is populated by the `#[tool_router]` macro.
#[derive(Clone)]
pub struct RemoterMcp {
    pub client: RemoterClient,
    pub whoami: WhoAmI,
    pub role: Role,
    /// When running inside the ticket-execution daemon, this is the ticket
    /// being worked on. It scopes `create_task`/`update_task` to that ticket.
    pub agent_task_id: Option<i32>,
    pub tool_router: ToolRouter<Self>,
}

impl RemoterMcp {
    pub fn new(client: RemoterClient, whoami: WhoAmI, role: Role, agent_task_id: Option<i32>) -> Self {
        Self {
            client,
            whoami,
            role,
            agent_task_id,
            tool_router: Self::tool_router(),
        }
    }

    /// Return an error if the named tool is gated away in the current role.
    /// This is the same check used by `ServerHandler::call_tool` before dispatch.
    pub(crate) fn gate_tool(&self, name: &str) -> Result<(), McpErrorData> {
        if self.role.is_gated(name) {
            return Err(McpErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("tool `{name}` is not available in {} mode", self.role),
                None,
            ));
        }
        Ok(())
    }

    /// Daemon-mode discovery scoping for the supervise role (spec
    /// remoter-agent.md §5.8). Returns `Some(ids)` when the visible task set
    /// is restricted — the current ticket plus its direct children (from the
    /// ticket's perspective its child links carry the `"parent"` relation,
    /// same convention as [`is_child_of_ticket`]) — or `None` when the tool
    /// must keep its existing behavior (standalone mode, or a role that
    /// pre-dates scoping, so old agents are unaffected).
    async fn supervise_scope_ids(&self) -> Result<Option<std::collections::HashSet<i32>>, McpErrorData> {
        if self.role != Role::DevAgentSupervise {
            return Ok(None);
        }
        let Some(ticket_id) = self.agent_task_id else {
            return Ok(None);
        };
        let ticket = self.client.get_task(ticket_id).await.map_err(McpErrorData::from)?;
        let mut ids = std::collections::HashSet::new();
        ids.insert(ticket.id);
        if let Some(links) = &ticket.links {
            for link in links {
                if link.relation == "parent" {
                    ids.insert(link.task_id);
                }
            }
        }
        Ok(Some(ids))
    }
}

/// Helper: serialize a value to a successful CallToolResult (JSON text block).
fn json_result<T: serde::Serialize>(val: &T) -> Result<CallToolResult, McpErrorData> {
    let text = serde_json::to_string_pretty(val)
        .map_err(|e| McpErrorData::internal_error(format!("serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

/// Daemon-mode `update_task` scoping: the target task counts as a child of
/// the current ticket when its links contain the ticket. From the child's
/// perspective that link carries the `"subtask"` relation (the child is a
/// subtask of the ticket) — the backend resolves relations from the viewing
/// task's side.
fn is_child_of_ticket(links: Option<&[TaskLinkRef]>, ticket_id: i32) -> bool {
    links.is_some_and(|links| {
        links
            .iter()
            .any(|link| link.relation == "subtask" && link.task_id == ticket_id)
    })
}

// ── Parameter structs ─────────────────────────────────────────────────────────

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListMyTasksParams {
    /// Filter by board status: "backlog", "todo", "in_progress", "review", "completed".
    #[serde(rename = "status")]
    pub status: Option<String>,
    /// Filter to one project.
    #[serde(rename = "projectId")]
    pub project_id: Option<i32>,
    /// Max number of tasks (default 50, max 200).
    pub limit: Option<i32>,
}

/// No-parameter tools still need an empty params struct: without a
/// `Parameters<T>` argument the rmcp macro emits `{}` as the input schema,
/// which strict clients reject because it lacks `"type": "object"`.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WhoAmIParams {}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct TaskIdParams {
    /// The task ID.
    #[serde(rename = "taskId")]
    pub task_id: i32,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ActionIdParams {
    /// The action ID (not the task ID).
    #[serde(rename = "actionId")]
    pub action_id: i32,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RejectActionParams {
    /// The action ID.
    #[serde(rename = "actionId")]
    pub action_id: i32,
    /// Why the action is being rejected. This reason must also be documented in the task report.
    /// Plain text, not Markdown.
    pub reason: String,
}

#[derive(Debug, schemars::JsonSchema)]
pub struct UpdateActionParams {
    /// The action ID (not the task ID).
    #[serde(rename = "actionId")]
    pub action_id: i32,
    /// Updated description for the action. Plain text (the UI renders it as
    /// plain text, not Markdown).
    pub description: Option<String>,
    /// Updated priority / order for the action.
    pub priority: Option<i32>,
}

impl<'de> serde::Deserialize<'de> for UpdateActionParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Helper {
            #[serde(rename = "actionId")]
            action_id: i32,
            description: Option<String>,
            priority: Option<i32>,
        }
        let helper = Helper::deserialize(deserializer)?;
        if helper.description.is_none() && helper.priority.is_none() {
            return Err(serde::de::Error::custom(
                "at least one of description or priority is required",
            ));
        }
        Ok(Self {
            action_id: helper.action_id,
            description: helper.description,
            priority: helper.priority,
        })
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteActionParams {
    /// The action ID (not the task ID).
    #[serde(rename = "actionId")]
    pub action_id: i32,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AdvanceTaskParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// Target status: "backlog", "todo", "in_progress", "review", "completed".
    pub to: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddActionParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The action description. Plain text (the UI renders it as plain text,
    /// not Markdown).
    pub description: String,
}

#[derive(Debug, schemars::JsonSchema)]
pub struct AddAttachmentParams {
    /// The task the file attaches to (exactly one of taskId/featureId/outcomeId).
    #[serde(rename = "taskId")]
    pub task_id: Option<i32>,
    /// The feature the file attaches to (exactly one of taskId/featureId/outcomeId).
    #[serde(rename = "featureId")]
    pub feature_id: Option<i32>,
    /// The task outcome the file attaches to — for report artifacts, the
    /// reportOutcomeId from get_task (exactly one of taskId/featureId/outcomeId).
    #[serde(rename = "outcomeId")]
    pub outcome_id: Option<i32>,
    /// Original file name (used for the download's Content-Disposition).
    /// Required with contentBase64; defaults to the file's basename with filePath.
    #[serde(rename = "fileName")]
    pub file_name: Option<String>,
    /// MIME type, e.g. "image/png". image/* links render inline in markdown.
    /// Required with contentBase64; guessed from the extension with filePath
    /// (fallback application/octet-stream).
    #[serde(rename = "contentType")]
    pub content_type: Option<String>,
    /// Base64-encoded file content (max 25 MB decoded). Provide exactly one of
    /// contentBase64 or filePath.
    #[serde(rename = "contentBase64")]
    pub content_base64: Option<String>,
    /// Local path to the file to upload (max 25 MB). Preferred over contentBase64
    /// because the bytes are read from disk by remoter-mcp and never encoded into
    /// the tool call. Path is interpreted relative to remoter-mcp's cwd. Provide
    /// exactly one of contentBase64 or filePath.
    #[serde(rename = "filePath")]
    pub file_path: Option<String>,
}

impl<'de> serde::Deserialize<'de> for AddAttachmentParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Helper {
            #[serde(rename = "taskId")]
            task_id: Option<i32>,
            #[serde(rename = "featureId")]
            feature_id: Option<i32>,
            #[serde(rename = "outcomeId")]
            outcome_id: Option<i32>,
            #[serde(rename = "fileName")]
            file_name: Option<String>,
            #[serde(rename = "contentType")]
            content_type: Option<String>,
            #[serde(rename = "contentBase64")]
            content_base64: Option<String>,
            #[serde(rename = "filePath")]
            file_path: Option<String>,
        }
        let h = Helper::deserialize(deserializer)?;
        match (&h.content_base64, &h.file_path) {
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "provide exactly one of contentBase64 or filePath",
                ));
            }
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "exactly one of contentBase64 or filePath is required",
                ));
            }
            _ => {}
        }
        if h.content_base64.is_some() {
            if h.file_name.is_none() {
                return Err(serde::de::Error::custom("fileName is required with contentBase64"));
            }
            if h.content_type.is_none() {
                return Err(serde::de::Error::custom("contentType is required with contentBase64"));
            }
        }
        Ok(Self {
            task_id: h.task_id,
            feature_id: h.feature_id,
            outcome_id: h.outcome_id,
            file_name: h.file_name,
            content_type: h.content_type,
            content_base64: h.content_base64,
            file_path: h.file_path,
        })
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListAttachmentsParams {
    /// List attachments of this task (exactly one of taskId/featureId/outcomeId).
    #[serde(rename = "taskId")]
    pub task_id: Option<i32>,
    /// List attachments of this feature (exactly one of taskId/featureId/outcomeId).
    #[serde(rename = "featureId")]
    pub feature_id: Option<i32>,
    /// List attachments of this task outcome, e.g. a report's artifacts
    /// (exactly one of taskId/featureId/outcomeId).
    #[serde(rename = "outcomeId")]
    pub outcome_id: Option<i32>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadAttachmentParams {
    /// The attachment ID (from list_attachments or an add_attachment result).
    #[serde(rename = "attachmentId")]
    pub attachment_id: i32,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddTaskCommentParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// Comment body (Markdown; reference attachments via the Markdown links
    /// returned by add_attachment).
    pub body: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListTaskCommentsParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// Only comments with id > afterId — read the new ones since the last
    /// known position.
    #[serde(rename = "afterId")]
    pub after_id: Option<i32>,
    /// Max number of comments (backend default 100, cap 200).
    pub limit: Option<i32>,
    /// The last N comments of the thread, chronological. Takes precedence
    /// over afterId/limit when set.
    pub tail: Option<i32>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListTaskQuestionsParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddTaskQuestionParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The question body. Markdown; rendered as Markdown in the task UI.
    pub body: String,
    /// Concrete answer options the human can pick from. Omit only when the
    /// question is genuinely free-form.
    pub options: Option<Vec<String>>,
    /// `true` when several options may apply; `false` (default) when exactly
    /// one option applies.
    pub multiple: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteTaskQuestionParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The question ID (from get_task's questions or list_task_questions).
    #[serde(rename = "questionId")]
    pub question_id: i32,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetTaskReportParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The report Markdown, or null/absent to clear the report.
    pub report: Option<String>,
}

/// Verdict values for `set_task_review` (sent to the backend verbatim).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdictParam {
    Approve,
    ChangesRequested,
}

/// Task kind values for `create_task`/`update_task` (sent to the backend
/// verbatim; mirrors the backend's domain `TaskKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskKindParam {
    Task,
    Research,
    Bug,
}

impl TaskKindParam {
    fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Research => "research",
            Self::Bug => "bug",
        }
    }
}

impl ReviewVerdictParam {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::ChangesRequested => "changes_requested",
        }
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetTaskReviewParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The review Markdown (what the supervisor checked and found).
    pub markdown: String,
    /// "approve" or "changes_requested".
    pub verdict: ReviewVerdictParam,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddTaskLinkParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The other task ID.
    #[serde(rename = "otherTaskId")]
    pub other_task_id: i32,
    /// Relation from taskId's perspective: "blocks", "blocked_by", "relates", "parent", "subtask".
    pub relation: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RemoveTaskLinkParams {
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The link ID (linkId from get_task's links list).
    #[serde(rename = "linkId")]
    pub link_id: i32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateTaskParams {
    /// The task title.
    pub title: String,
    /// The task description (Markdown; rendered as Markdown in the task UI,
    /// may embed attachment links returned by add_attachment).
    pub description: String,
    /// The feature ID (required in standalone mode; omit in daemon mode unless
    /// creating a cross-project subtask — see projectId).
    #[serde(rename = "featureId")]
    pub feature_id: Option<i32>,
    /// Target project ID (daemon mode only). Omit (or pass the current
    /// ticket's project) for a same-project subtask. When it differs from the
    /// current ticket's project, featureId is required and must belong to
    /// that project; the subtask is created there with the current ticket as
    /// its cross-project parent.
    #[serde(rename = "projectId")]
    pub project_id: Option<i32>,
    /// Assign the new task to this user immediately (id from list_users).
    /// Default null: the task is created unassigned.
    #[serde(rename = "assigneeId")]
    pub assignee_id: Option<i32>,
    /// The kind of task: "task" (default), "research", or "bug".
    #[serde(rename = "taskKind")]
    pub task_kind: Option<TaskKindParam>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AssignTaskParams {
    /// The task ID.
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The user to assign the task to (id from list_users). Omit or pass null
    /// to unassign the task instead.
    #[serde(rename = "assigneeId")]
    pub assignee_id: Option<i32>,
}

/// No-parameter tools still need an empty params struct (see `WhoAmIParams`).
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListUsersParams {}

#[derive(Debug, schemars::JsonSchema)]
pub struct UpdateTaskParams {
    /// The task ID.
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// Updated title.
    pub title: Option<String>,
    /// Updated description (Markdown; rendered as Markdown in the task UI,
    /// may embed attachment links returned by add_attachment).
    pub description: Option<String>,
    /// Updated task kind: "task", "research", or "bug".
    #[serde(rename = "taskKind")]
    pub task_kind: Option<TaskKindParam>,
}

impl<'de> serde::Deserialize<'de> for UpdateTaskParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Helper {
            #[serde(rename = "taskId")]
            task_id: i32,
            title: Option<String>,
            description: Option<String>,
            #[serde(rename = "taskKind")]
            task_kind: Option<TaskKindParam>,
        }
        let helper = Helper::deserialize(deserializer)?;
        if helper.title.is_none() && helper.description.is_none() && helper.task_kind.is_none() {
            return Err(serde::de::Error::custom(
                "at least one of title, description or taskKind is required",
            ));
        }
        Ok(Self {
            task_id: helper.task_id,
            title: helper.title,
            description: helper.description,
            task_kind: helper.task_kind,
        })
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListFeaturesParams {
    /// The project ID.
    #[serde(rename = "projectId")]
    pub project_id: i32,
}

/// No-parameter tools still need an empty params struct (see `WhoAmIParams`).
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListProjectsParams {}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchTasksParams {
    /// Search query: an exact task id or `#id`, or a case-insensitive title substring.
    pub query: String,
    /// Max number of hits (backend default 20).
    pub limit: Option<i32>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListBoardParams {
    /// Scope the board to one project.
    #[serde(rename = "projectId")]
    pub project_id: Option<i32>,
    /// Completed-column window: "today", "week", "month", or "all"
    /// (the backend default applies when omitted).
    pub completed: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListGoalsParams {
    /// The project ID. In daemon mode (inside a ticket run) omit it to list
    /// the goals of the current ticket's project.
    #[serde(rename = "projectId")]
    pub project_id: Option<i32>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetTaskGoalParams {
    /// The task ID.
    #[serde(rename = "taskId")]
    pub task_id: i32,
    /// The business goal to link the task to (id from list_goals), or
    /// null/absent to unlink the task from its current goal. The goal must
    /// belong to the task's project.
    #[serde(rename = "goalId")]
    pub goal_id: Option<i32>,
}

// ── Attachment helpers (pure, unit-tested) ────────────────────────────────────

/// Exactly one owner (spec §3.5): feature → `owner_kind` 1, task → 2,
/// task outcome (plan/report) → 3.
fn attachment_owner(
    task_id: Option<i32>,
    feature_id: Option<i32>,
    outcome_id: Option<i32>,
) -> Result<(i16, i32), McpError> {
    match (task_id, feature_id, outcome_id) {
        (Some(t), None, None) => Ok((2, t)),
        (None, Some(f), None) => Ok((1, f)),
        (None, None, Some(o)) => Ok((3, o)),
        _ => Err(McpError::invalid_params(
            "exactly one of taskId/featureId/outcomeId must be set",
        )),
    }
}

/// Shared size/empty check for attachment payloads. The backend re-checks the
/// announced size at intent time and the real size on confirm.
fn check_attachment_size(bytes: &[u8]) -> Result<(), McpError> {
    if bytes.is_empty() {
        return Err(McpError::invalid_params("attachment content is empty"));
    }
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(McpError::invalid_params(format!(
            "attachment is {} bytes — over the 25 MB limit",
            bytes.len()
        )));
    }
    Ok(())
}

/// Decodes the base64 payload and enforces the 25 MB limit client-side.
fn decode_content(content_base64: &str) -> Result<Vec<u8>, McpError> {
    let bytes = Base64::decode_vec(content_base64)
        .map_err(|e| McpError::invalid_params(format!("contentBase64 is not valid base64: {e}")))?;
    check_attachment_size(&bytes)?;
    Ok(bytes)
}

/// Reads a file from disk for the `filePath` upload path and returns its bytes,
/// default file name (basename), and guessed MIME type. Errors are mapped to
/// clear MCP invalid_params messages.
async fn read_file_attachment(path: &str) -> Result<(Vec<u8>, String, String), McpError> {
    let meta = tokio::fs::metadata(path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => McpError::invalid_params(format!("file not found: {path}")),
        _ => McpError::invalid_params(format!("could not read file metadata for {path}: {e}")),
    })?;
    if !meta.is_file() {
        let kind = if meta.is_dir() {
            "directory"
        } else {
            "not a regular file"
        };
        return Err(McpError::invalid_params(format!("path is a {kind}: {path}")));
    }
    if meta.len() > MAX_ATTACHMENT_BYTES {
        return Err(McpError::invalid_params(format!(
            "attachment exceeds the 25 MB limit ({} bytes)",
            meta.len()
        )));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| McpError::invalid_params(format!("could not read file {path}: {e}")))?;
    check_attachment_size(&bytes)?;
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment")
        .to_string();
    let content_type = mime_guess::from_path(path).first_or_octet_stream().to_string();
    Ok((bytes, file_name, content_type))
}

/// The stable download URL embedded in markdown (spec §4): it never expires —
/// only the 302 redirect target is presigned.
fn download_url(api_base: &str, attachment_id: i32) -> String {
    format!(
        "{}/api/v1/attachments/{attachment_id}/download",
        api_base.trim_end_matches('/')
    )
}

/// `[name](url)` — prefixed with `!` for `image/*` so images render inline.
fn markdown_link(api_base: &str, file_name: &str, content_type: &str, attachment_id: i32) -> String {
    let url = download_url(api_base, attachment_id);
    let bang = if content_type.starts_with("image/") { "!" } else { "" };
    format!("{bang}[{file_name}]({url})")
}

/// Text vs binary for `read_attachment` (spec §3.5): known binary kinds
/// (image/audio/video/octet-stream) are always binary; anything else is
/// returned as text when the body is valid UTF-8 (covers text/*, JSON, and
/// extension-less text files). SVG is exempt: it is XML text, and vision
/// APIs reject `image/svg+xml` image blocks, so it is served decoded.
fn as_text(content_type: &str, bytes: &[u8]) -> Option<String> {
    let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let binary_hint = (mime.starts_with("image/") && mime != "image/svg+xml")
        || mime.starts_with("audio/")
        || mime.starts_with("video/")
        || mime == "application/octet-stream";
    if binary_hint {
        return None;
    }
    String::from_utf8(bytes.to_vec()).ok()
}

/// Build the MCP tool result for an attachment's bytes based on its declared
/// content type. Text types are returned as decoded text, images as native
/// image content, and everything else as a base64 JSON envelope.
fn attachment_content(content_type: &str, bytes: &[u8]) -> Result<CallToolResult, McpErrorData> {
    match as_text(content_type, bytes) {
        Some(text) => Ok(CallToolResult::success(vec![Content::text(text)])),
        None => {
            let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            if mime.starts_with("image/") {
                Ok(CallToolResult::success(vec![Content::image(
                    Base64::encode_string(bytes),
                    mime,
                )]))
            } else {
                json_result(&serde_json::json!({
                    "contentType": content_type,
                    "sizeBytes": bytes.len(),
                    "contentBase64": Base64::encode_string(bytes),
                }))
            }
        }
    }
}

// ── Tools ─────────────────────────────────────────────────────────────────────

#[tool_router]
impl RemoterMcp {
    // ── Discovery (read) ─────────────────────────────────────────────────

    #[tool(
        name = "list_my_tasks",
        description = "List tasks assigned to the calling agent, with denormalized project/feature names. This is the primary entry point: call this first to see what work you have."
    )]
    async fn list_my_tasks(
        &self,
        Parameters(p): Parameters<ListMyTasksParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let mut tasks = self
            .client
            .list_my_tasks(p.status.as_deref(), p.project_id, p.limit)
            .await
            .map_err(McpErrorData::from)?;
        if let Some(scope) = self.supervise_scope_ids().await? {
            tasks.retain(|t| scope.contains(&t.id));
        }
        if tasks.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No tasks assigned to you.".to_string(),
            )]));
        }
        json_result(&tasks)
    }

    #[tool(
        name = "get_task",
        description = "Get full details of a single task, including its action list and its open/answered questions (with options and answers). Use this to understand what steps (actions) make up a task before starting work."
    )]
    async fn get_task(&self, Parameters(p): Parameters<TaskIdParams>) -> Result<CallToolResult, McpErrorData> {
        let detail = self.client.get_task(p.task_id).await.map_err(McpErrorData::from)?;
        json_result(&detail)
    }

    #[tool(
        name = "whoami",
        description = "Return the caller's identity (id, name, kind, current workspace id/name/role, and the list of all workspaces)."
    )]
    async fn whoami(&self, Parameters(_): Parameters<WhoAmIParams>) -> Result<CallToolResult, McpErrorData> {
        json_result(&self.whoami)
    }

    #[tool(
        name = "list_users",
        description = "List the active members of the current workspace (id + name). Use this to pick an assigneeId for assign_task or create_task."
    )]
    async fn list_users(&self, Parameters(_): Parameters<ListUsersParams>) -> Result<CallToolResult, McpErrorData> {
        let users = self.client.list_assignable().await.map_err(McpErrorData::from)?;
        if users.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No users in this workspace.".to_string(),
            )]));
        }
        json_result(&users)
    }

    #[tool(
        name = "list_features",
        description = "List features in a project, with task totals and completion counts. Use this to discover feature IDs when creating tasks in standalone mode."
    )]
    async fn list_features(
        &self,
        Parameters(p): Parameters<ListFeaturesParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let features = self
            .client
            .list_features(p.project_id)
            .await
            .map_err(McpErrorData::from)?;
        if features.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No features in this project.".to_string(),
            )]));
        }
        json_result(&features)
    }

    #[tool(
        name = "list_projects",
        description = "List all projects. Use this first to discover project IDs for list_features, list_board, and list_my_tasks."
    )]
    async fn list_projects(
        &self,
        Parameters(_): Parameters<ListProjectsParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let projects = self.client.list_projects().await.map_err(McpErrorData::from)?;
        if projects.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text("No projects.".to_string())]));
        }
        json_result(&projects)
    }

    #[tool(
        name = "search_tasks",
        description = "Find tasks by exact #id/id or a case-insensitive title substring. Returns compact hits (id, title, status, project/feature IDs); call get_task with the id for full details."
    )]
    async fn search_tasks(&self, Parameters(p): Parameters<SearchTasksParams>) -> Result<CallToolResult, McpErrorData> {
        let mut items = self
            .client
            .search_tasks(&p.query, p.limit)
            .await
            .map_err(McpErrorData::from)?;
        if let Some(scope) = self.supervise_scope_ids().await? {
            items.retain(|t| scope.contains(&t.id));
        }
        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No tasks match.".to_string(),
            )]));
        }
        json_result(&items)
    }

    #[tool(
        name = "list_board",
        description = "List the kanban board: every task as a card with its column (taskStatus), assignee, and action counts. Optionally scope to one project (projectId) and set the Completed column window (completed: today, week, month, or all)."
    )]
    async fn list_board(&self, Parameters(p): Parameters<ListBoardParams>) -> Result<CallToolResult, McpErrorData> {
        let mut cards = self
            .client
            .list_board(p.project_id, p.completed.as_deref())
            .await
            .map_err(McpErrorData::from)?;
        if let Some(scope) = self.supervise_scope_ids().await? {
            cards.retain(|c| scope.contains(&c.id));
        }
        if cards.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No tasks on the board.".to_string(),
            )]));
        }
        json_result(&cards)
    }

    #[tool(
        name = "list_goals",
        description = "List the business goals of a project (archived goals are excluded). In daemon mode omit projectId to list the goals of the current ticket's project. Use this to discover goal IDs for set_task_goal; a task's current goal shows up as goalId in get_task/list_my_tasks."
    )]
    async fn list_goals(&self, Parameters(p): Parameters<ListGoalsParams>) -> Result<CallToolResult, McpErrorData> {
        let project_id = match p.project_id {
            Some(id) => id,
            None => {
                let Some(ticket_id) = self.agent_task_id else {
                    return Err(McpErrorData::invalid_params(
                        "projectId is required in standalone mode",
                        None,
                    ));
                };
                self.client
                    .get_task(ticket_id)
                    .await
                    .map_err(McpErrorData::from)?
                    .project_id
            }
        };
        let goals = self.client.list_goals(project_id).await.map_err(McpErrorData::from)?;
        json_result(&goals)
    }

    // ── Workflow mutation ────────────────────────────────────────────────

    #[tool(
        name = "start_task",
        description = "Set a task's status to in_progress, signalling that work has begun."
    )]
    async fn start_task(&self, Parameters(p): Parameters<TaskIdParams>) -> Result<CallToolResult, McpErrorData> {
        let task = self
            .client
            .set_task_status(p.task_id, "in_progress")
            .await
            .map_err(McpErrorData::from)?;
        json_result(&task)
    }

    #[tool(
        name = "advance_task",
        description = "Move a task to a specific board status. Allowed values: backlog, todo, in_progress, review, completed. Note: completing a task requires you to be the assignee or hold CompleteTask permission."
    )]
    async fn advance_task(&self, Parameters(p): Parameters<AdvanceTaskParams>) -> Result<CallToolResult, McpErrorData> {
        let task = self
            .client
            .set_task_status(p.task_id, &p.to)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&task)
    }

    #[tool(
        name = "complete_task",
        description = "Mark a task as completed. Only the assignee (or an admin) can do this. All actions should typically be completed first."
    )]
    async fn complete_task(&self, Parameters(p): Parameters<TaskIdParams>) -> Result<CallToolResult, McpErrorData> {
        let task = self
            .client
            .set_task_status(p.task_id, "completed")
            .await
            .map_err(McpErrorData::from)?;
        json_result(&task)
    }

    #[tool(
        name = "unassign_self",
        description = "Unassign yourself from a task. The task returns to the backlog."
    )]
    async fn unassign_self(&self, Parameters(p): Parameters<TaskIdParams>) -> Result<CallToolResult, McpErrorData> {
        let task = self.client.unassign_task(p.task_id).await.map_err(McpErrorData::from)?;
        json_result(&task)
    }

    #[tool(
        name = "assign_task",
        description = "Assign a task to a workspace member (assigneeId from list_users), or unassign it when assigneeId is omitted/null. The backend enforces the permission gates: an agent may only assign children of its own ticket (parent-scoped). Assigning wakes the assignee's daemon; unassigning a task in todo/in_progress returns it to the backlog."
    )]
    async fn assign_task(&self, Parameters(p): Parameters<AssignTaskParams>) -> Result<CallToolResult, McpErrorData> {
        if let Some(ticket_id) = self.agent_task_id {
            let target = self.client.get_task(p.task_id).await.map_err(McpErrorData::from)?;
            if !is_child_of_ticket(target.links.as_deref(), ticket_id) {
                return Err(McpErrorData::invalid_params(
                    "task is not a child of the current ticket",
                    None,
                ));
            }
        }
        let task = match p.assignee_id {
            Some(assignee_id) => self
                .client
                .assign_task(p.task_id, assignee_id)
                .await
                .map_err(McpErrorData::from)?,
            None => self.client.unassign_task(p.task_id).await.map_err(McpErrorData::from)?,
        };
        json_result(&task)
    }

    #[tool(
        name = "create_task",
        description = "Create a new task. In daemon mode (inside a ticket run) the task is created as a subtask of the current ticket; do not pass featureId. In standalone mode, featureId is required and is used to derive the project. Cross-project subtasks (daemon mode): pass projectId of a different project together with a featureId that belongs to it — the subtask is created in that project with the current ticket as its cross-project parent. Pass assigneeId (from list_users) to assign the task immediately; omit it to leave the task unassigned. Pass taskKind (\"task\", \"research\", or \"bug\") to set the task type; omit it to create a regular task. The description field accepts Markdown (rendered as Markdown in the task UI) and may embed attachment links returned by add_attachment."
    )]
    async fn create_task(&self, Parameters(p): Parameters<CreateTaskParams>) -> Result<CallToolResult, McpErrorData> {
        let task_kind = p.task_kind;
        let body = if let Some(agent_task_id) = self.agent_task_id {
            let task = self.client.get_task(agent_task_id).await.map_err(McpErrorData::from)?;
            let cross_project = p.project_id.is_some_and(|id| id != task.project_id);
            if !cross_project {
                if p.feature_id.is_some() {
                    return Err(McpErrorData::invalid_params(
                        "featureId must not be provided in daemon mode; omit it so the subtask is created in the current ticket's project/feature",
                        None,
                    ));
                }
                serde_json::json!({
                    "projectId": task.project_id,
                    "featureId": task.feature_id,
                    "title": p.title,
                    "description": p.description,
                    "parentTaskId": agent_task_id,
                })
            } else {
                let project_id = p.project_id.expect("cross_project implies projectId");
                let Some(feature_id) = p.feature_id else {
                    return Err(McpErrorData::invalid_params(
                        "featureId is required for a cross-project subtask (projectId differs from the current ticket's project)",
                        None,
                    ));
                };
                let feature = self.client.get_feature(feature_id).await.map_err(McpErrorData::from)?;
                if feature.project_id != project_id {
                    return Err(McpErrorData::invalid_params(
                        format!(
                            "featureId {feature_id} belongs to project {}, not to the requested projectId {project_id}",
                            feature.project_id
                        ),
                        None,
                    ));
                }
                serde_json::json!({
                    "projectId": project_id,
                    "featureId": feature_id,
                    "title": p.title,
                    "description": p.description,
                    "parentTaskId": agent_task_id,
                })
            }
        } else {
            let Some(feature_id) = p.feature_id else {
                return Err(McpErrorData::invalid_params(
                    "featureId is required in standalone mode",
                    None,
                ));
            };
            let feature = self.client.get_feature(feature_id).await.map_err(McpErrorData::from)?;
            serde_json::json!({
                "projectId": feature.project_id,
                "featureId": feature_id,
                "title": p.title,
                "description": p.description,
            })
        };
        let mut body = body;
        if let Some(task_kind) = task_kind {
            body["taskKind"] = serde_json::Value::String(task_kind.as_str().to_string());
        }
        let task = self.client.create_task(&body).await.map_err(McpErrorData::from)?;
        if let Some(assignee_id) = p.assignee_id {
            // The backend always returns the created task's id; fail loudly
            // rather than assigning task 0 if that ever changes.
            let Some(task_id) = task["id"].as_i64().map(|id| id as i32) else {
                return Err(McpErrorData::internal_error(
                    format!("backend response has no task id: {task}"),
                    None,
                ));
            };
            self.client.assign_task(task_id, assignee_id).await.map_err(|e| {
                McpErrorData::internal_error(
                    format!(
                        "task {task_id} was created but assigning it to user {assignee_id} failed: {e}; retry with assign_task"
                    ),
                    None,
                )
            })?;
        }
        json_result(&task)
    }

    #[tool(
        name = "update_task",
        description = "Update a task's title, description and/or taskKind (\"task\", \"research\", or \"bug\"). In daemon mode, only subtasks created under the current ticket may be edited. At least one of title, description or taskKind is required. The description field accepts Markdown (rendered as Markdown in the task UI) and may embed attachment links returned by add_attachment."
    )]
    async fn update_task(&self, Parameters(p): Parameters<UpdateTaskParams>) -> Result<CallToolResult, McpErrorData> {
        if p.title.is_none() && p.description.is_none() && p.task_kind.is_none() {
            return Err(McpErrorData::invalid_params(
                "at least one of title, description or taskKind is required",
                None,
            ));
        }
        if let Some(agent_task_id) = self.agent_task_id {
            let task = self.client.get_task(p.task_id).await.map_err(McpErrorData::from)?;
            if !is_child_of_ticket(task.links.as_deref(), agent_task_id) {
                return Err(McpErrorData::invalid_params(
                    "task was not created in this ticket",
                    None,
                ));
            }
        }
        let mut body = serde_json::Map::new();
        if let Some(title) = p.title {
            body.insert("title".to_string(), serde_json::Value::String(title));
        }
        if let Some(description) = p.description {
            body.insert("description".to_string(), serde_json::Value::String(description));
        }
        if let Some(task_kind) = p.task_kind {
            body.insert(
                "taskKind".to_string(),
                serde_json::Value::String(task_kind.as_str().to_string()),
            );
        }
        let task = self
            .client
            .update_task(p.task_id, &serde_json::Value::Object(body))
            .await
            .map_err(McpErrorData::from)?;
        json_result(&task)
    }

    #[tool(
        name = "set_task_goal",
        description = "Link a task to a business goal (goalId from list_goals) or unlink it (goalId null/absent). The goal must belong to the task's project; an unknown goal is rejected by the backend with a 404. In daemon mode only the current ticket or its subtasks may be re-linked."
    )]
    async fn set_task_goal(
        &self,
        Parameters(p): Parameters<SetTaskGoalParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        if let Some(ticket_id) = self.agent_task_id
            && p.task_id != ticket_id
        {
            let target = self.client.get_task(p.task_id).await.map_err(McpErrorData::from)?;
            if !is_child_of_ticket(target.links.as_deref(), ticket_id) {
                return Err(McpErrorData::invalid_params(
                    "task is not the current ticket or its subtask",
                    None,
                ));
            }
        }
        let task = self
            .client
            .set_task_goal(p.task_id, p.goal_id)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&task)
    }

    // ── Action mutation ──────────────────────────────────────────────────

    #[tool(
        name = "start_action",
        description = "Activate an action (the single-active invariant deactivates siblings in the same task). (human callers; for agents the daemon owns board status)"
    )]
    async fn start_action(&self, Parameters(p): Parameters<ActionIdParams>) -> Result<CallToolResult, McpErrorData> {
        let body = serde_json::json!({ "status": "active" });
        let action = self
            .client
            .update_action(p.action_id, &body)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&action)
    }

    #[tool(
        name = "complete_action",
        description = "Mark an action as completed. (human callers; for agents the daemon owns board status)"
    )]
    async fn complete_action(&self, Parameters(p): Parameters<ActionIdParams>) -> Result<CallToolResult, McpErrorData> {
        let body = serde_json::json!({ "status": "completed" });
        let action = self
            .client
            .update_action(p.action_id, &body)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&action)
    }

    #[tool(
        name = "reject_action",
        description = "Reject an action as no longer needed or incorrect. The reason is recorded on the action and must also be documented in the task report. Rejection is a final state: the action counts toward task completion. If the step never saw work and is pure planning noise, use `delete_action` instead."
    )]
    async fn reject_action(
        &self,
        Parameters(p): Parameters<RejectActionParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let action = self
            .client
            .reject_action(p.action_id, &p.reason)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&action)
    }

    #[tool(
        name = "update_action",
        description = "Update an action's description and/or priority (order). At least one field is required; one action per call."
    )]
    async fn update_action(
        &self,
        Parameters(p): Parameters<UpdateActionParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        if p.description.is_none() && p.priority.is_none() {
            return Err(McpErrorData::invalid_params(
                "at least one of description or priority is required",
                None,
            ));
        }
        let mut body = serde_json::Map::new();
        if let Some(d) = p.description {
            body.insert("description".to_string(), serde_json::Value::String(d));
        }
        if let Some(priority) = p.priority {
            body.insert(
                "priority".to_string(),
                serde_json::Value::Number(serde_json::Number::from(priority)),
            );
        }
        let action = self
            .client
            .update_action(p.action_id, &serde_json::Value::Object(body))
            .await
            .map_err(McpErrorData::from)?;
        json_result(&action)
    }

    #[tool(
        name = "delete_action",
        description = "Delete an action. Deleted actions are removed from task counts and prompts — use this for stale planning steps."
    )]
    async fn delete_action(
        &self,
        Parameters(p): Parameters<DeleteActionParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        self.client
            .delete_action(p.action_id)
            .await
            .map_err(McpErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            "Action deleted.".to_string(),
        )]))
    }

    #[tool(name = "add_action", description = "Add a new action (step) to a task.")]
    async fn add_action(&self, Parameters(p): Parameters<AddActionParams>) -> Result<CallToolResult, McpErrorData> {
        // The backend's POST /actions requires project_id + feature_id + task_id.
        // We don't have project/feature here — fetch the task first to get them.
        let task = self.client.get_task(p.task_id).await.map_err(McpErrorData::from)?;
        let body = serde_json::json!({
            "projectId": task.project_id,
            "featureId": task.feature_id,
            "taskId": p.task_id,
            "description": p.description,
        });
        let action = self.client.create_action(&body).await.map_err(McpErrorData::from)?;
        json_result(&action)
    }

    // ── Attachments & comments (spec §3.5) ───────────────────────────────

    #[tool(
        name = "add_attachment",
        description = "Attach a file to a task, feature, or task outcome: provide exactly one of taskId/featureId/outcomeId and exactly one of filePath (preferred) or contentBase64. filePath reads the file from the local machine where remoter-mcp runs; fileName defaults to the file's basename and contentType is guessed from the extension. contentBase64 requires fileName and contentType. Max 25 MB. Report artifacts attach to the outcome (outcomeId = get_task's reportOutcomeId); task-bound attachments serve descriptions and comments. Runs the whole upload flow and returns a ready-to-paste markdown link."
    )]
    async fn add_attachment(
        &self,
        Parameters(p): Parameters<AddAttachmentParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let (owner_kind, owner_id) =
            attachment_owner(p.task_id, p.feature_id, p.outcome_id).map_err(McpErrorData::from)?;

        let (bytes, file_name, content_type) = match (&p.content_base64, &p.file_path) {
            (Some(b64), None) => {
                // fileName and contentType are required by deserialization for this branch.
                let bytes = decode_content(b64).map_err(McpErrorData::from)?;
                (
                    bytes,
                    p.file_name.clone().expect("fileName validated by deserialize"),
                    p.content_type.clone().expect("contentType validated by deserialize"),
                )
            }
            (None, Some(path)) => {
                let (bytes, default_name, default_mime) =
                    read_file_attachment(path).await.map_err(McpErrorData::from)?;
                (
                    bytes,
                    p.file_name.clone().unwrap_or(default_name),
                    p.content_type.clone().unwrap_or(default_mime),
                )
            }
            _ => unreachable!("exactly one of contentBase64/filePath is enforced by deserialize"),
        };

        let upload = self
            .client
            .create_upload(owner_kind, owner_id, &file_name, &content_type, bytes.len() as i64)
            .await
            .map_err(McpErrorData::from)?;
        self.client
            .put_presigned(&upload.upload_url, &content_type, bytes)
            .await
            .map_err(McpErrorData::from)?;
        self.client
            .confirm_upload(upload.attachment_id)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&serde_json::json!({
            "attachmentId": upload.attachment_id,
            "markdownLink": markdown_link(self.client.base_url(), &file_name, &content_type, upload.attachment_id),
        }))
    }

    #[tool(
        name = "list_attachments",
        description = "List the confirmed attachments of a task, feature, or task outcome (exactly one of taskId/featureId/outcomeId; outcomeId = get_task's reportOutcomeId for report artifacts), each with a stable download URL."
    )]
    async fn list_attachments(
        &self,
        Parameters(p): Parameters<ListAttachmentsParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let (owner_kind, owner_id) =
            attachment_owner(p.task_id, p.feature_id, p.outcome_id).map_err(McpErrorData::from)?;
        let items = self
            .client
            .list_attachments(owner_kind, owner_id)
            .await
            .map_err(McpErrorData::from)?;
        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No attachments.".to_string(),
            )]));
        }
        let out: Vec<serde_json::Value> = items
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "fileName": a.file_name,
                    "contentType": a.content_type,
                    "sizeBytes": a.size_bytes,
                    "downloadUrl": download_url(self.client.base_url(), a.id),
                })
            })
            .collect();
        json_result(&out)
    }

    #[tool(
        name = "read_attachment",
        description = "Read an attachment's content by ID. Text content is returned decoded; images are returned as native image content; other binary content is returned base64-encoded with its content type."
    )]
    async fn read_attachment(
        &self,
        Parameters(p): Parameters<ReadAttachmentParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let dl = self
            .client
            .download_attachment(p.attachment_id)
            .await
            .map_err(McpErrorData::from)?;
        attachment_content(&dl.content_type, &dl.bytes)
    }

    #[tool(
        name = "add_task_comment",
        description = "Add a comment to a task's thread. Reference attachments by embedding their markdown links (from add_attachment) in the body. Note: there is no delete_attachment tool — agents cannot delete attachments."
    )]
    async fn add_task_comment(
        &self,
        Parameters(p): Parameters<AddTaskCommentParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let comment = self
            .client
            .add_task_comment(p.task_id, &p.body)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&comment)
    }

    #[tool(
        name = "list_task_comments",
        description = "Read a task's comment thread, chronological. Use afterId to fetch only comments newer than the last one you saw (e.g. human replies added mid-run), limit to page through the thread, or tail to read just the last N. get_task's commentsCount tells whether there is anything to read. Attachment references in comment bodies carry the attachment id in their URL — read_attachment accepts it as attachmentId."
    )]
    async fn list_task_comments(
        &self,
        Parameters(p): Parameters<ListTaskCommentsParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let comments = self
            .client
            .list_task_comments(p.task_id, p.after_id, p.limit, p.tail)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&comments)
    }

    #[tool(
        name = "add_task_question",
        description = "Register an open question on a task as a structured question. In the plan phase, use this for EVERY open question instead of writing an \"Open questions\" text section in the plan — humans answer these in the UI and the answers come back via list_task_questions/get_task. Whenever possible propose concrete answer `options`; set `multiple` to true when several options may apply, false (default) when exactly one applies; omit `options` only when the question is genuinely free-form."
    )]
    async fn add_task_question(
        &self,
        Parameters(p): Parameters<AddTaskQuestionParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let question = self
            .client
            .add_task_question(
                p.task_id,
                &p.body,
                p.options.unwrap_or_default(),
                p.multiple.unwrap_or(false),
            )
            .await
            .map_err(McpErrorData::from)?;
        json_result(&question)
    }

    #[tool(
        name = "list_task_questions",
        description = "List a task's questions with their options and answers (status open/answered, selected options, answer_text). Available in every role — use it to read the human's answers before re-planning or implementing."
    )]
    async fn list_task_questions(
        &self,
        Parameters(p): Parameters<ListTaskQuestionsParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let questions = self
            .client
            .list_task_questions(p.task_id)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&questions)
    }

    #[tool(
        name = "delete_task_question",
        description = "Delete a question that is obsolete or already answered and no longer needs context space. Available in plan and implement roles."
    )]
    async fn delete_task_question(
        &self,
        Parameters(p): Parameters<DeleteTaskQuestionParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        self.client
            .delete_task_question(p.task_id, p.question_id)
            .await
            .map_err(McpErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            "Question deleted.".to_string(),
        )]))
    }

    #[tool(
        name = "set_task_report",
        description = "Set or clear the task's markdown implementation report: what was done, how to test it, how to deploy it to production. The current report is read via get_task (field report). Attach file artifacts to the report's outcome via add_attachment (outcomeId = get_task's reportOutcomeId) and reference their markdown links in the report text. Do not link the pull request — the daemon creates it after the run and attaches it as a separate artifact (get_task's prUrl). Pass null/absent report to clear."
    )]
    async fn set_task_report(
        &self,
        Parameters(p): Parameters<SetTaskReportParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let task = self
            .client
            .set_task_report(p.task_id, p.report)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&task)
    }

    #[tool(
        name = "set_task_review",
        description = "Write a review of a ticket: the markdown assessment plus the verdict ('approve' or 'changes_requested'). Usable by a parent-scoped agent supervisor reviewing a child ticket (the task's parent ticket must be assigned to you) or by the ticket's own assignee agent while it has a running review run on that ticket; the backend returns 403 otherwise. The review lives until the next set_task_review overwrites it and is read via get_task (fields review, reviewVerdict, reviewOutcomeId)."
    )]
    async fn set_task_review(
        &self,
        Parameters(p): Parameters<SetTaskReviewParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        let outcome = self
            .client
            .set_task_review(p.task_id, &p.markdown, p.verdict.as_str())
            .await
            .map_err(McpErrorData::from)?;
        json_result(&outcome)
    }

    // ── Task links ──────────────────────────────────────────────────────

    #[tool(
        name = "add_link",
        description = "Create a link between two tasks. Relation is from taskId's perspective: 'blocks' means taskId blocks otherTaskId; 'blocked_by' means taskId is blocked by otherTaskId; 'relates' is symmetric; 'parent' means taskId is the parent of otherTaskId; 'subtask' means taskId is a subtask (child) of otherTaskId. 'blocks'/'blocked_by' between tasks related in the subtask tree (parent/child, any depth) are rejected by the backend — wire blocking dependencies only between sibling tasks."
    )]
    async fn add_link(&self, Parameters(p): Parameters<AddTaskLinkParams>) -> Result<CallToolResult, McpErrorData> {
        let link = self
            .client
            .add_task_link(p.task_id, p.other_task_id, &p.relation)
            .await
            .map_err(McpErrorData::from)?;
        json_result(&link)
    }

    #[tool(
        name = "remove_link",
        description = "Remove a task link by its linkId (found in get_task's links list)."
    )]
    async fn remove_link(
        &self,
        Parameters(p): Parameters<RemoveTaskLinkParams>,
    ) -> Result<CallToolResult, McpErrorData> {
        self.client
            .remove_task_link(p.task_id, p.link_id)
            .await
            .map_err(McpErrorData::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            "Link removed.".to_string(),
        )]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::WorkspaceMembership;

    /// Strict MCP clients (e.g. Zod-based validators) require every tool's
    /// inputSchema to be a JSON Schema object with `"type": "object"`.
    #[test]
    fn all_tool_input_schemas_are_object_type() {
        let router = RemoterMcp::tool_router();
        let tools = router.list_all();
        assert!(!tools.is_empty());
        for tool in tools {
            let schema_type = tool.input_schema.get("type").and_then(|v| v.as_str());
            assert_eq!(
                schema_type,
                Some("object"),
                "tool {:?} has a non-object inputSchema: {}",
                tool.name,
                serde_json::to_string_pretty(&*tool.input_schema).unwrap()
            );
        }
    }

    #[test]
    fn attachment_owner_requires_exactly_one_owner() {
        assert_eq!(attachment_owner(Some(7), None, None).unwrap(), (2, 7));
        assert_eq!(attachment_owner(None, Some(3), None).unwrap(), (1, 3));
        // outcomeId → owner_kind 3 (task_outcomes row, e.g. the report).
        assert_eq!(attachment_owner(None, None, Some(9)).unwrap(), (3, 9));
        // Two or more owners set → error.
        assert!(attachment_owner(Some(1), Some(2), None).is_err());
        assert!(attachment_owner(Some(1), None, Some(3)).is_err());
        assert!(attachment_owner(None, Some(2), Some(3)).is_err());
        assert!(attachment_owner(Some(1), Some(2), Some(3)).is_err());
        // No owner → error.
        assert!(attachment_owner(None, None, None).is_err());
    }

    /// `add_attachment`/`list_attachments` params: camelCase `outcomeId` is the
    /// third owner alternative.
    #[test]
    fn attachment_params_map_camel_case_outcome_id() {
        let p: AddAttachmentParams = serde_json::from_value(serde_json::json!({
            "outcomeId": 9,
            "fileName": "shot.png",
            "contentType": "image/png",
            "contentBase64": "aGk=",
        }))
        .unwrap();
        assert_eq!(p.outcome_id, Some(9));
        assert_eq!(p.task_id, None);
        assert_eq!(p.feature_id, None);
        assert_eq!(p.file_name.as_deref(), Some("shot.png"));
        assert_eq!(p.content_type.as_deref(), Some("image/png"));
        assert_eq!(p.content_base64.as_deref(), Some("aGk="));
        assert_eq!(p.file_path, None);

        let p: AddAttachmentParams = serde_json::from_value(serde_json::json!({
            "taskId": 7,
            "filePath": "/tmp/shot.png",
        }))
        .unwrap();
        assert_eq!(p.task_id, Some(7));
        assert_eq!(p.file_path.as_deref(), Some("/tmp/shot.png"));
        assert_eq!(p.content_base64, None);
        assert_eq!(p.file_name, None);
        assert_eq!(p.content_type, None);

        let p: ListAttachmentsParams = serde_json::from_value(serde_json::json!({"outcomeId": 9})).unwrap();
        assert_eq!(p.outcome_id, Some(9));
        // snake_case outcome_id is not mapped — the MCP surface is camelCase
        // (all owner fields are optional, so the unknown key is ignored).
        let p: ListAttachmentsParams = serde_json::from_value(serde_json::json!({"outcome_id": 9})).unwrap();
        assert_eq!(p.outcome_id, None);
    }

    #[test]
    fn add_attachment_requires_exactly_one_content_source() {
        // Neither provided.
        let err = serde_json::from_value::<AddAttachmentParams>(serde_json::json!({
            "taskId": 1,
            "fileName": "x.png",
            "contentType": "image/png",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("contentBase64 or filePath"), "{err}");

        // Both provided.
        let err = serde_json::from_value::<AddAttachmentParams>(serde_json::json!({
            "taskId": 1,
            "fileName": "x.png",
            "contentType": "image/png",
            "contentBase64": "aGk=",
            "filePath": "/tmp/x.png",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("contentBase64 or filePath"), "{err}");

        // contentBase64 without fileName / contentType.
        let err = serde_json::from_value::<AddAttachmentParams>(serde_json::json!({
            "taskId": 1,
            "contentBase64": "aGk=",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("fileName"), "{err}");

        let err = serde_json::from_value::<AddAttachmentParams>(serde_json::json!({
            "taskId": 1,
            "fileName": "x.png",
            "contentBase64": "aGk=",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("contentType"), "{err}");
    }

    #[tokio::test]
    async fn read_file_attachment_reads_defaults_and_mime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        tokio::fs::write(&path, b"png-bytes").await.unwrap();

        let (bytes, name, mime) = read_file_attachment(path.to_str().unwrap()).await.unwrap();
        assert_eq!(bytes, b"png-bytes");
        assert_eq!(name, "shot.png");
        assert_eq!(mime, "image/png");

        // Unknown extension falls back to octet-stream.
        let unknown = dir.path().join("data.unknown-ext");
        tokio::fs::write(&unknown, b"bytes").await.unwrap();
        let (_, _, mime) = read_file_attachment(unknown.to_str().unwrap()).await.unwrap();
        assert_eq!(mime, "application/octet-stream");
    }

    #[tokio::test]
    async fn read_file_attachment_errors_on_missing_directory_empty_and_oversized() {
        // Missing path.
        let err = read_file_attachment("/definitely/not/a/file.txt").await.unwrap_err();
        assert!(format!("{err}").contains("file not found"), "{err}");

        // Directory.
        let dir = tempfile::tempdir().unwrap();
        let err = read_file_attachment(dir.path().to_str().unwrap()).await.unwrap_err();
        assert!(format!("{err}").contains("directory"), "{err}");

        // Empty file.
        let empty = dir.path().join("empty.txt");
        tokio::fs::write(&empty, b"").await.unwrap();
        let err = read_file_attachment(empty.to_str().unwrap()).await.unwrap_err();
        assert!(format!("{err}").contains("empty"), "{err}");

        // Just over the limit (sparse file, so it is cheap).
        let huge = dir.path().join("huge.bin");
        let huge_file = std::fs::File::create(&huge).unwrap();
        huge_file.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        drop(huge_file);
        let err = read_file_attachment(huge.to_str().unwrap()).await.unwrap_err();
        assert!(format!("{err}").contains("25 MB"), "{err}");
    }

    /// `set_task_report` params: camelCase `taskId`; `report` absent or null
    /// clears the report (maps to `None`).
    #[test]
    fn set_task_report_params_map_camel_case_and_nullable_report() {
        let p: SetTaskReportParams = serde_json::from_value(serde_json::json!({
            "taskId": 7,
            "report": "did the thing"
        }))
        .unwrap();
        assert_eq!(p.task_id, 7);
        assert_eq!(p.report.as_deref(), Some("did the thing"));

        for body in [
            serde_json::json!({"taskId": 7}),
            serde_json::json!({"taskId": 7, "report": null}),
        ] {
            let p: SetTaskReportParams = serde_json::from_value(body).unwrap();
            assert_eq!(p.task_id, 7);
            assert_eq!(p.report, None, "absent/null report must clear");
        }

        // snake_case task_id is not accepted — the MCP surface is camelCase.
        assert!(serde_json::from_value::<SetTaskReportParams>(serde_json::json!({"task_id": 7})).is_err());
    }

    /// `add_link`'s description must warn that blocking tasks related in the
    /// subtask tree (parent/child, any depth) is rejected by the backend (#205).
    #[test]
    fn add_link_description_warns_against_relative_blocks() {
        let router = RemoterMcp::tool_router();
        let tool = router
            .list_all()
            .into_iter()
            .find(|t| t.name == "add_link")
            .expect("add_link tool must be registered");
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(desc.contains("subtask tree"), "{desc}");
        assert!(desc.contains("rejected by the backend"), "{desc}");
    }

    /// The human-facing discovery tools are registered and available in every
    /// role (nothing about them is daemon-owned).
    #[test]
    fn discovery_tools_are_registered_and_ungated() {
        let router = RemoterMcp::tool_router();
        let tools = router.list_all();
        for name in ["list_projects", "search_tasks", "list_board", "list_goals"] {
            assert!(tools.iter().any(|t| t.name == name), "missing tool `{name}`");
            for role in [
                Role::Full,
                Role::DevAgentPlan,
                Role::DevAgentImplement,
                Role::DevAgentSupervise,
                Role::DevAgentReview,
            ] {
                assert!(!role.is_gated(name), "`{name}` must be available in {role} mode");
            }
        }
    }

    /// `list_board`/`search_tasks` params: camelCase `projectId`, optional fields.
    #[test]
    fn discovery_params_map_camel_case() {
        let p: ListBoardParams =
            serde_json::from_value(serde_json::json!({"projectId": 3, "completed": "week"})).unwrap();
        assert_eq!(p.project_id, Some(3));
        assert_eq!(p.completed.as_deref(), Some("week"));

        let p: ListBoardParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p.project_id, None);
        assert_eq!(p.completed, None);

        let p: SearchTasksParams = serde_json::from_value(serde_json::json!({"query": "#42", "limit": 5})).unwrap();
        assert_eq!(p.query, "#42");
        assert_eq!(p.limit, Some(5));

        // `query` is required.
        assert!(serde_json::from_value::<SearchTasksParams>(serde_json::json!({})).is_err());
    }

    #[test]
    fn markdown_link_prefixes_images_with_bang() {
        let link = markdown_link("http://api:8181", "shot.png", "image/png", 42);
        assert_eq!(link, "![shot.png](http://api:8181/api/v1/attachments/42/download)");
        let link = markdown_link("http://api:8181/", "spec.pdf", "application/pdf", 7);
        assert_eq!(link, "[spec.pdf](http://api:8181/api/v1/attachments/7/download)");
    }

    #[test]
    fn decode_content_rejects_bad_base64_empty_and_oversized() {
        let ok = Base64::encode_string(b"hello");
        assert_eq!(decode_content(&ok).unwrap(), b"hello");

        assert!(decode_content("!!! not base64 !!!").is_err());
        assert!(decode_content("").is_err(), "empty content must be rejected");

        let too_big = Base64::encode_string(&vec![0u8; (MAX_ATTACHMENT_BYTES + 1) as usize]);
        let err = decode_content(&too_big).unwrap_err();
        assert!(format!("{err}").contains("25 MB"), "unexpected message: {err}");
        let at_limit = Base64::encode_string(&vec![0u8; MAX_ATTACHMENT_BYTES as usize]);
        assert!(decode_content(&at_limit).is_ok(), "exactly 25 MB is allowed");
    }

    #[test]
    fn as_text_decodes_utf8_unless_binary_type() {
        assert_eq!(as_text("text/plain", b"hi").as_deref(), Some("hi"));
        assert_eq!(as_text("application/json", br#"{"a":1}"#).as_deref(), Some("{\"a\":1}"));
        // Valid UTF-8 with an unknown type is still treated as text.
        assert_eq!(as_text("application/x-custom", b"hi").as_deref(), Some("hi"));
        // Known binary kinds stay binary even when the bytes happen to be UTF-8.
        assert!(as_text("image/png", b"hi").is_none());
        assert!(as_text("application/octet-stream", b"hi").is_none());
        // SVG is XML text: served decoded, not as an image block.
        assert_eq!(as_text("image/svg+xml", b"<svg/>").as_deref(), Some("<svg/>"));
        // Declared text that is not valid UTF-8 falls back to base64.
        assert!(as_text("text/plain", &[0xff, 0xfe]).is_none());
    }

    #[test]
    fn attachment_content_splits_by_mime_type() {
        // Text content is returned as a decoded text block.
        let text_result = attachment_content("text/plain", b"hello").unwrap();
        assert_eq!(text_result.content.len(), 1);
        assert!(text_result.content[0].as_text().is_some());

        // Images are returned as native image content.
        let img_result = attachment_content("image/png", b"\x89PNG").unwrap();
        assert_eq!(img_result.content.len(), 1);
        let img = img_result.content[0].as_image().expect("expected image content block");
        assert_eq!(img.mime_type, "image/png");
        assert_eq!(img.data, Base64::encode_string(b"\x89PNG"));

        // The image block carries the normalized mime type, not the raw header.
        let img_result = attachment_content("Image/PNG; charset=binary", b"\x89PNG").unwrap();
        let img = img_result.content[0].as_image().expect("expected image content block");
        assert_eq!(img.mime_type, "image/png");

        // SVG is text: returned decoded, never as an image block.
        let svg_result = attachment_content("image/svg+xml", b"<svg/>").unwrap();
        assert_eq!(svg_result.content.len(), 1);
        assert_eq!(svg_result.content[0].as_text().unwrap().text, "<svg/>");

        // Other binary stays in the base64 JSON envelope.
        let bin = b"\x00\x01\x02\x03";
        let binary_result = attachment_content("application/octet-stream", bin).unwrap();
        assert_eq!(binary_result.content.len(), 1);
        let text = binary_result.content[0].as_text().unwrap().text.clone();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["contentType"], "application/octet-stream");
        assert_eq!(json["contentBase64"], Base64::encode_string(bin));
    }

    fn dummy_mcp(role: Role) -> RemoterMcp {
        RemoterMcp::new(
            RemoterClient::new("http://localhost:9999".into(), "dummy".into(), None),
            WhoAmI {
                id: 1,
                name: "Test".into(),
                kind: "human".into(),
                workspaces: vec![WorkspaceMembership {
                    id: 1,
                    name: "Personal".into(),
                    role: "owner".into(),
                }],
                workspace_id: Some(1),
                workspace_name: Some("Personal".into()),
                role: Some("owner".into()),
            },
            role,
            None,
        )
    }

    fn tool_names(mcp: &RemoterMcp) -> Vec<String> {
        mcp.tool_router
            .list_all()
            .into_iter()
            .filter(|tool| !mcp.role.is_gated(&tool.name))
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    #[test]
    fn role_full_lists_all_tools() {
        let mcp = dummy_mcp(Role::Full);
        let names = tool_names(&mcp);
        // 35 tools total; set_task_review is supervise-only,
        // add/delete_task_question are dev-agent tools, and set_task_goal is
        // implement/plan-only, so the Full count is 31.
        assert_eq!(names.len(), 31, "unexpected tools: {names:?}");
        for gated in [
            "set_task_review",
            "add_task_question",
            "delete_task_question",
            "set_task_goal",
        ] {
            assert!(
                !names.contains(&gated.to_string()),
                "{gated} should be gated in full role"
            );
        }
        assert!(
            names.contains(&"list_task_questions".to_string()),
            "list_task_questions should be available in full role"
        );
    }

    #[test]
    fn role_implement_hides_four_board_tools() {
        let mcp = dummy_mcp(Role::DevAgentImplement);
        let names = tool_names(&mcp);
        // 35 tools - 4 board tools - set_task_review (supervise-only) -
        // add_task_question (plan-only) = 29; create_task/update_task/
        // list_features/delete_task_question/list_goals/set_task_goal stay
        // available.
        assert_eq!(names.len(), 29, "unexpected tools: {names:?}");
        for gated in [
            "start_task",
            "advance_task",
            "complete_task",
            "unassign_self",
            "set_task_review",
            "add_task_question",
        ] {
            assert!(
                !names.contains(&gated.to_string()),
                "{gated} should be gated in implement role"
            );
        }
        for available in [
            "create_task",
            "update_task",
            "list_features",
            "list_task_questions",
            "delete_task_question",
            "list_goals",
            "set_task_goal",
        ] {
            assert!(
                names.contains(&available.to_string()),
                "{available} should be available in implement role"
            );
        }
    }

    #[test]
    fn role_plan_hides_eight_tools() {
        let mcp = dummy_mcp(Role::DevAgentPlan);
        let names = tool_names(&mcp);
        // 35 tools - 9 gated (6 board/action + create_task/update_task + set_task_review) = 26.
        assert_eq!(names.len(), 26, "unexpected tools: {names:?}");
        for gated in [
            "start_task",
            "advance_task",
            "complete_task",
            "unassign_self",
            "start_action",
            "complete_action",
            "create_task",
            "update_task",
            "set_task_review",
        ] {
            assert!(
                !names.contains(&gated.to_string()),
                "{gated} should be gated in plan role"
            );
        }
        for available in [
            "add_task_question",
            "list_task_questions",
            "delete_task_question",
            "list_goals",
            "set_task_goal",
        ] {
            assert!(
                names.contains(&available.to_string()),
                "{available} should be available in plan role"
            );
        }
    }

    #[test]
    fn role_supervise_keeps_supervision_tools() {
        let mcp = dummy_mcp(Role::DevAgentSupervise);
        let names = tool_names(&mcp);
        // 35 tools - 3 daemon-owned board tools (start/complete/unassign_self)
        // - add/delete_task_question (not the supervisor's job)
        // - set_task_goal (implement/plan-only) = 29.
        // advance_task and set_task_review stay: parent-scoped transitions and
        // child reviews are the supervisor's job.
        assert_eq!(names.len(), 29, "unexpected tools: {names:?}");
        for gated in [
            "start_task",
            "complete_task",
            "unassign_self",
            "add_task_question",
            "delete_task_question",
            "set_task_goal",
        ] {
            assert!(
                !names.contains(&gated.to_string()),
                "{gated} should be gated in supervise role"
            );
        }
        for available in [
            "assign_task",
            "list_users",
            "advance_task",
            "create_task",
            "add_task_comment",
            "set_task_review",
            "list_task_questions",
        ] {
            assert!(
                names.contains(&available.to_string()),
                "{available} should be available in supervise role"
            );
        }
    }

    #[test]
    fn role_review_keeps_only_read_tools_and_set_task_review() {
        let mcp = dummy_mcp(Role::DevAgentReview);
        let names = tool_names(&mcp);
        // 35 tools - 21 mutating tools = 14: read-only discovery plus
        // set_task_review for the review verdict.
        assert_eq!(names.len(), 14, "unexpected tools: {names:?}");
        for available in [
            "list_my_tasks",
            "get_task",
            "whoami",
            "list_users",
            "list_features",
            "list_projects",
            "search_tasks",
            "list_board",
            "list_goals",
            "list_attachments",
            "read_attachment",
            "list_task_comments",
            "list_task_questions",
            "set_task_review",
        ] {
            assert!(
                names.contains(&available.to_string()),
                "{available} should be available in review role"
            );
        }
        for gated in [
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
        ] {
            assert!(
                !names.contains(&gated.to_string()),
                "{gated} should be gated in review role"
            );
        }
    }

    #[test]
    fn gated_tool_returns_method_not_found() {
        let mcp = dummy_mcp(Role::DevAgentImplement);
        let err = mcp.gate_tool("start_task").unwrap_err();
        assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
        assert_eq!(err.code.0, -32601);
        let msg = err.message.as_ref();
        assert!(msg.contains("start_task"), "message missing tool name: {msg}");
        assert!(msg.contains("not available"), "message missing 'not available': {msg}");
    }

    #[test]
    fn parse_role_mapping() {
        use crate::parse_role;

        assert_eq!(parse_role(std::iter::empty()), Role::Full);
        assert_eq!(parse_role(["--role", "full"].map(String::from).into_iter()), Role::Full);
        assert_eq!(parse_role(["--role=full"].map(String::from).into_iter()), Role::Full);
        assert_eq!(
            parse_role(["--role", "dev-agent-plan"].map(String::from).into_iter()),
            Role::DevAgentPlan
        );
        assert_eq!(
            parse_role(["--role=dev-agent-plan"].map(String::from).into_iter()),
            Role::DevAgentPlan
        );
        assert_eq!(
            parse_role(["--role", "dev-agent-implement"].map(String::from).into_iter()),
            Role::DevAgentImplement
        );
        assert_eq!(
            parse_role(["--role=dev-agent-implement"].map(String::from).into_iter()),
            Role::DevAgentImplement
        );
        assert_eq!(
            parse_role(["--role", "dev-agent-supervise"].map(String::from).into_iter()),
            Role::DevAgentSupervise
        );
        assert_eq!(
            parse_role(["--role=dev-agent-supervise"].map(String::from).into_iter()),
            Role::DevAgentSupervise
        );
        assert_eq!(
            parse_role(["--role", "dev-agent-review"].map(String::from).into_iter()),
            Role::DevAgentReview
        );
        assert_eq!(
            parse_role(["--role=dev-agent-review"].map(String::from).into_iter()),
            Role::DevAgentReview
        );
    }

    #[test]
    fn update_action_requires_at_least_one_field() {
        let err = serde_json::from_value::<UpdateActionParams>(serde_json::json!({"actionId": 1})).unwrap_err();
        assert!(err.to_string().contains("at least one"), "unexpected error: {err}");
    }

    #[test]
    fn delete_action_params_map_action_id() {
        let p: DeleteActionParams = serde_json::from_value(serde_json::json!({"actionId": 42})).unwrap();
        assert_eq!(p.action_id, 42);
    }

    #[test]
    fn create_task_params_map_title_description_and_feature_id() {
        let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
            "title": "Subtask",
            "description": "do the thing",
            "featureId": 7,
        }))
        .unwrap();
        assert_eq!(p.title, "Subtask");
        assert_eq!(p.description, "do the thing");
        assert_eq!(p.feature_id, Some(7));
        assert_eq!(p.assignee_id, None, "assigneeId defaults to null");
        assert_eq!(p.task_kind, None, "taskKind defaults to null");

        let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
            "title": "Daemon subtask",
            "description": "no featureId in daemon mode",
            "assigneeId": 3,
            "taskKind": "bug",
        }))
        .unwrap();
        assert_eq!(p.feature_id, None);
        assert_eq!(p.assignee_id, Some(3));
        assert_eq!(p.task_kind, Some(TaskKindParam::Bug));

        // snake_case feature_id/assignee_id are not mapped — the MCP surface
        // is camelCase (both fields are optional, so unknown keys are ignored).
        let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
            "title": "x",
            "description": "y",
            "feature_id": 7,
            "assignee_id": 3,
        }))
        .unwrap();
        assert_eq!(p.feature_id, None);
        assert_eq!(p.assignee_id, None);
    }

    /// Unknown taskKind values are rejected at param parsing, before anything
    /// is forwarded to the backend (PR review on task #103).
    #[test]
    fn task_kind_param_rejects_unknown_values() {
        let err = serde_json::from_value::<CreateTaskParams>(serde_json::json!({
            "title": "x",
            "description": "y",
            "featureId": 7,
            "taskKind": "epic",
        }))
        .unwrap_err();
        assert!(
            err.to_string().contains("taskKind") || err.to_string().contains("unknown"),
            "unexpected error: {err}"
        );

        assert!(
            serde_json::from_value::<UpdateTaskParams>(serde_json::json!({
                "taskId": 1,
                "taskKind": "feature",
            }))
            .is_err()
        );
    }

    /// `assign_task` params: camelCase ids; `assigneeId` absent/null means unassign.
    #[test]
    fn assign_task_params_map_camel_case_and_nullable_assignee() {
        let p: AssignTaskParams = serde_json::from_value(serde_json::json!({
            "taskId": 7,
            "assigneeId": 3,
        }))
        .unwrap();
        assert_eq!(p.task_id, 7);
        assert_eq!(p.assignee_id, Some(3));

        for body in [
            serde_json::json!({"taskId": 7}),
            serde_json::json!({"taskId": 7, "assigneeId": null}),
        ] {
            let p: AssignTaskParams = serde_json::from_value(body).unwrap();
            assert_eq!(p.task_id, 7);
            assert_eq!(p.assignee_id, None, "absent/null assigneeId must unassign");
        }

        // snake_case task_id is not accepted — the MCP surface is camelCase.
        assert!(serde_json::from_value::<AssignTaskParams>(serde_json::json!({"task_id": 7})).is_err());
    }

    /// `set_task_goal` params: camelCase ids; `goalId` absent/null unlinks.
    #[test]
    fn set_task_goal_params_map_camel_case_and_nullable_goal() {
        let p: SetTaskGoalParams = serde_json::from_value(serde_json::json!({
            "taskId": 7,
            "goalId": 3,
        }))
        .unwrap();
        assert_eq!(p.task_id, 7);
        assert_eq!(p.goal_id, Some(3));

        for body in [
            serde_json::json!({"taskId": 7}),
            serde_json::json!({"taskId": 7, "goalId": null}),
        ] {
            let p: SetTaskGoalParams = serde_json::from_value(body).unwrap();
            assert_eq!(p.task_id, 7);
            assert_eq!(p.goal_id, None, "absent/null goalId must unlink");
        }

        // snake_case is not accepted — the MCP surface is camelCase.
        assert!(serde_json::from_value::<SetTaskGoalParams>(serde_json::json!({"task_id": 7})).is_err());
    }

    /// `list_goals` params: optional camelCase `projectId`.
    #[test]
    fn list_goals_params_map_optional_project_id() {
        let p: ListGoalsParams = serde_json::from_value(serde_json::json!({"projectId": 42})).unwrap();
        assert_eq!(p.project_id, Some(42));
        let p: ListGoalsParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p.project_id, None);
        // snake_case project_id is not mapped — the MCP surface is camelCase.
        let p: ListGoalsParams = serde_json::from_value(serde_json::json!({"project_id": 42})).unwrap();
        assert_eq!(p.project_id, None);
    }

    #[test]
    fn update_task_requires_at_least_one_field() {
        let err = serde_json::from_value::<UpdateTaskParams>(serde_json::json!({"taskId": 1})).unwrap_err();
        assert!(err.to_string().contains("at least one"), "unexpected error: {err}");

        for body in [
            serde_json::json!({"taskId": 1, "title": "New title"}),
            serde_json::json!({"taskId": 1, "description": "New description"}),
            serde_json::json!({"taskId": 1, "taskKind": "bug"}),
            serde_json::json!({"taskId": 1, "title": "T", "description": "D"}),
        ] {
            let p: UpdateTaskParams = serde_json::from_value(body).unwrap();
            assert_eq!(p.task_id, 1);
        }

        // taskKind alone satisfies the at-least-one-field requirement.
        let p: UpdateTaskParams =
            serde_json::from_value(serde_json::json!({"taskId": 1, "taskKind": "research"})).unwrap();
        assert_eq!(p.task_kind, Some(TaskKindParam::Research));
        assert!(p.title.is_none() && p.description.is_none());
    }

    #[test]
    fn list_features_params_map_project_id() {
        let p: ListFeaturesParams = serde_json::from_value(serde_json::json!({"projectId": 42})).unwrap();
        assert_eq!(p.project_id, 42);
        // snake_case project_id is not accepted — the MCP surface is camelCase.
        assert!(serde_json::from_value::<ListFeaturesParams>(serde_json::json!({"project_id": 42})).is_err());
    }

    fn link_ref(relation: &str, task_id: i32) -> TaskLinkRef {
        TaskLinkRef {
            link_id: 1,
            relation: relation.to_string(),
            task_id,
            title: "t".to_string(),
            task_status: "backlog".to_string(),
            feature_id: 1,
            project_id: 1,
            created_by: None,
        }
    }

    /// Regression (task #71): when viewing the child, its link to the ticket
    /// carries the `"subtask"` relation — the backend resolves relations from
    /// the viewing task's side. Matching `"parent"` here rejected every
    /// daemon-mode `update_task` with "task was not created in this ticket".
    #[test]
    fn is_child_of_ticket_matches_subtask_relation_from_child_perspective() {
        let links = vec![link_ref("subtask", 71), link_ref("relates", 42)];
        assert!(is_child_of_ticket(Some(&links), 71));
    }

    /// A `"parent"` relation on the child's link list points at the child's
    /// own children, not at the ticket — it must not authorize the edit.
    #[test]
    fn is_child_of_ticket_rejects_parent_relation_and_missing_links() {
        assert!(!is_child_of_ticket(Some(&[link_ref("parent", 71)]), 71));
        assert!(!is_child_of_ticket(Some(&[link_ref("subtask", 72)]), 71));
        assert!(!is_child_of_ticket(Some(&[]), 71));
        assert!(!is_child_of_ticket(None, 71));
    }
}

/// HTTP-level tool tests against a mock backend (see `http_tests.rs`).
#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;
