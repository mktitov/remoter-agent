//! The `AgentDriver` abstraction (spec §5.5). A driver executes one run attempt
//! against a prepared worktree and reports the outcome; it knows nothing about
//! the backend — run-row bookkeeping and status transitions live in `run.rs`.
//!
//! v1 drivers:
//! - `stub` — the dry-run driver ([`stub::StubDriver`]): no LLM, simulates a
//!   successful run. Validates the whole orchestration (poll → claim → run →
//!   report) without an agent binary, and powers the PoC integration tests.
//! - `kimi-acp` and `opencode-acp` — the real driver ([`acp::AcpDriver`]): ACP
//!   is the only driver protocol; there is no CLI print-mode fallback.

pub mod acp;
pub mod kimi_usage;
pub mod mcp;
pub mod stub;

use std::path::PathBuf;

use async_trait::async_trait;

use crate::config::DriverConfig;
use crate::session_log::SessionLogger;

/// Everything a driver needs for one attempt.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// The ticket id this run belongs to.
    pub task_id: i32,
    /// The backend `agent_runs` row id for this attempt.
    pub run_id: i32,
    /// The ticket worktree (`<workspace_root>/p<project>/wt-<task>`).
    pub cwd: PathBuf,
    /// The full prompt (ticket context block + instructions, spec §5.6).
    pub prompt: String,
    /// `plan` or `implement`.
    pub kind: &'static str,
    /// The ACP session to resume (only bounce review → implement and re-plan
    /// runs resume — the previous run's session of the same kind, spec §5.4);
    /// `None` = fresh session.
    pub resume_session: Option<String>,
    /// Working branch (`agent/task-<id>-<slug>`) — for drivers that need it.
    pub branch: String,
    /// Extra env for the agent process (and its devenv services):
    /// `CI`, `REMOTER_AGENT_PORT_BASE` (host mode), `REMOTER_CONTAINER`,
    /// `REMOTER_AGENT_TASK_ID` (spec §5.8 + containers spec §3.4).
    pub env: Vec<(String, String)>,
    /// Where the agent process executes: on the host (optionally via
    /// `devenv shell --`) or inside the run container via `docker exec`.
    pub exec: crate::workspace::ExecEnv,
    /// ACP `session/set_config_option` pairs (`config_id` → `value`) to apply
    /// after the session is created/resumed (spec §5.5). Empty for the stub
    /// driver and for any driver when no per-kind model/thinking is configured.
    pub config_options: Vec<(String, String)>,
    /// Structured ACP log writer for this run.  The stub driver ignores it.
    pub logger: SessionLogger,
}

/// A finished attempt's outcome.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The dev-agent session id (resume token for later runs).
    pub session_id: Option<String>,
    /// Plan text (plan runs) or change summary (implement runs).
    pub text: String,
    /// Driver-reported LLM usage; `None` when the driver doesn't report it.
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    /// The model configured for this run kind, if any (recorded on the run
    /// row for cost analysis, spec §4.1).
    pub model: Option<String>,
    /// The thinking level configured for this run kind, if any.
    pub thinking: Option<String>,
}

/// Failure taxonomy (spec §5.7): transient errors are retried (bounded by
/// `max_attempts`); permanent ones escalate the ticket immediately.
#[derive(Debug)]
pub enum DriverError {
    Transient(String),
    Permanent(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::Transient(e) => write!(f, "transient driver error: {e}"),
            DriverError::Permanent(e) => write!(f, "permanent driver error: {e}"),
        }
    }
}

impl std::error::Error for DriverError {}

/// A failed driver attempt, optionally carrying the ACP session id and LLM usage
/// the driver managed to collect before the failure. `source` keeps the original
/// transient/permanent classification used by the retry logic in `run.rs`.
#[derive(Debug)]
pub struct RunFailure {
    pub source: DriverError,
    pub session_id: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

impl RunFailure {
    pub fn new(source: DriverError, session_id: Option<String>) -> Self {
        Self {
            source,
            session_id,
            input_tokens: None,
            output_tokens: None,
        }
    }

    pub fn with_tokens(mut self, input: Option<i64>, output: Option<i64>) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }
}

impl std::fmt::Display for RunFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for RunFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[async_trait]
pub trait AgentDriver: Send + Sync {
    /// Executes one attempt. Implementations must be cancellation-safe: dropping
    /// the returned future must not leave a live dev-agent process behind.
    async fn run(&self, spec: RunSpec) -> Result<RunOutcome, RunFailure>;
}

/// Builds the configured driver. The ACP driver needs the API URL + agent token
/// to inject into the per-session `remoter` MCP server (spec §5.6). The daemon's
/// effective workspace is also forwarded so the spawned `remoter-mcp` does not
/// fail on multi-workspace agents.
pub fn from_config(
    cfg: &DriverConfig,
    api_url: &str,
    token: &str,
    workspace_id: Option<i32>,
) -> anyhow::Result<std::sync::Arc<dyn AgentDriver>> {
    match cfg.kind.as_str() {
        "stub" => Ok(std::sync::Arc::new(stub::StubDriver::new(
            cfg.simulate_delay_ms,
            cfg.simulate_uncommitted_changes,
        ))),
        "kimi-acp" | "opencode-acp" => Ok(std::sync::Arc::new(acp::AcpDriver::new(
            cfg,
            api_url,
            token,
            workspace_id,
        ))),
        other => anyhow::bail!("unknown driver kind {other:?} (expected \"stub\", \"kimi-acp\", or \"opencode-acp\")"),
    }
}
