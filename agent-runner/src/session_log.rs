//! Per-run structured session logger used by the ACP driver (spec §5.5).
//!
//! The logger is intentionally best-effort: a full queue or a failed disk write
//! drops entries but never blocks ticket work.  A [`SessionLogger`] may be
//! `None` (e.g. for the stub driver), in which case all methods are no-ops.

use crate::logstore::{NewLogEntry, RunLogger};

/// Cloneable handle for streaming structured ACP events to one run log.
#[derive(Clone, Default, Debug)]
pub struct SessionLogger(Option<RunLogger>);

impl SessionLogger {
    /// Creates a logger backed by a concrete [`RunLogger`].
    pub fn new(logger: RunLogger) -> Self {
        Self(Some(logger))
    }

    /// Creates a no-op logger (used by the stub driver and in tests).
    pub fn noop() -> Self {
        Self(None)
    }

    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    pub fn log_prompt(&self, text: impl Into<String>) {
        self.log(NewLogEntry::prompt(text));
    }

    pub fn log_message(&self, text: impl Into<String>) {
        self.log(NewLogEntry::message(text));
    }

    pub fn log_thought(&self, text: impl Into<String>) {
        self.log(NewLogEntry::thought(text));
    }

    pub fn log_plan(&self, text: impl Into<String>) {
        self.log(NewLogEntry::plan(text));
    }

    pub fn log_error(&self, text: impl Into<String>) {
        self.log(NewLogEntry::error(text));
    }

    pub fn log_tool_call(&self, tool_id: impl Into<String>, kind: impl Into<String>, title: impl Into<String>) {
        self.log(NewLogEntry::tool_call(tool_id, kind, title));
    }

    pub fn log_tool_call_update(
        &self,
        tool_id: impl Into<String>,
        kind: Option<String>,
        title: Option<String>,
        status: Option<String>,
    ) {
        self.log(NewLogEntry::tool_call_update(tool_id, kind, title, status));
    }

    fn log(&self, entry: NewLogEntry) {
        if let Some(logger) = &self.0 {
            logger.log(entry);
        }
    }
}

/// Converts a [`ToolKind`] from the ACP SDK into the snake_case string we
/// store in the log entry.
pub fn tool_kind_str(kind: &agent_client_protocol::schema::v1::ToolKind) -> &'static str {
    use agent_client_protocol::schema::v1::ToolKind;
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switchMode",
        ToolKind::Other => "other",
        _ => "other",
    }
}

/// Converts a [`ToolCallStatus`] into the snake_case string we store.
pub fn tool_status_str(status: &agent_client_protocol::schema::v1::ToolCallStatus) -> &'static str {
    use agent_client_protocol::schema::v1::ToolCallStatus;
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "inProgress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "pending",
    }
}

impl From<Option<RunLogger>> for SessionLogger {
    fn from(logger: Option<RunLogger>) -> Self {
        Self(logger)
    }
}
