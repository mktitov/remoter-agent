//! The ACP driver (spec §5.5): drives a dev-agent over the Agent Client
//! Protocol — the only driver protocol; there is deliberately no CLI
//! print-mode fallback (one protocol, one code path).
//!
//! Flow per attempt: spawn the agent (process-group leader, kill-on-drop) →
//! `initialize` → `session/new { cwd, mcpServers }` (or `session/resume` for
//! bounce/re-plan runs, §5.4 — with an in-attempt fallback to a fresh session
//! when the resumed session's first prompt fails) → `session/prompt` with
//! streaming capture. Every
//! `session/request_permission` is auto-approved and every elicitation is
//! declined — this is what guarantees "the agent must not ask questions"
//! (§5.5). End-turn token usage is recorded when the agent reports it (§4.1).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, CreateElicitationRequest, CreateElicitationResponse, ElicitationAction, InitializeRequest, McpServer,
    NewSessionRequest, PermissionOptionKind, Plan, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigId, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionValue, SessionConfigSelectOptions, SessionId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, StopReason, TextContent,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use async_trait::async_trait;

use super::{AgentDriver, DriverError, RunFailure, RunOutcome, RunSpec, kimi_usage, mcp};
use crate::config::DriverConfig;
use crate::run::RunKind;
use crate::session_log::{SessionLogger, tool_kind_str, tool_status_str};
use crate::workspace;

/// Grace between SIGTERM and SIGKILL when a run is cancelled or times out
/// (spec §5.5).
const TERM_GRACE: Duration = Duration::from_secs(3);

/// How often the run polls the agent process for exit while the ACP
/// connection is open. An agent that dies mid-turn without answering the
/// pending request (kimi: a provider-side turn failure can end the turn
/// without a JSON-RPC response) must not hold the attempt hostage until the
/// outer run timeout — the pipe EOF is not always observed by the connection.
const EXIT_POLL: Duration = Duration::from_secs(1);

/// An ACP agent binary driven per run. The same implementation supports Kimi,
/// OpenCode, and other compatible agents configured through `DriverConfig`.
pub struct AcpDriver {
    program: String,
    args: Vec<String>,
    remoter_mcp_command: String,
    api_url: String,
    token: String,
    /// Effective workspace scoped on every backend request. Forwarded to the
    /// spawned `remoter-mcp` child so it can send `X-Workspace-Id`.
    workspace_id: Option<i32>,
    /// Kimi session directory, used only by Kimi's token-usage fallback when
    /// ACP does not report usage directly (§4.1).
    sessions_dir: Option<PathBuf>,
}

impl AcpDriver {
    pub fn new(cfg: &DriverConfig, api_url: &str, token: &str, workspace_id: Option<i32>) -> Self {
        Self {
            program: cfg.agent_program.clone(),
            args: cfg.agent_args.clone(),
            remoter_mcp_command: cfg.remoter_mcp_command.clone(),
            api_url: api_url.to_string(),
            token: token.to_string(),
            workspace_id,
            sessions_dir: cfg.sessions_dir.clone(),
        }
    }
}

#[async_trait]
impl AgentDriver for AcpDriver {
    async fn run(&self, spec: RunSpec) -> Result<RunOutcome, RunFailure> {
        let (child, stdin, stdout, stderr) =
            spawn_agent(&self.program, &self.args, &spec).map_err(|e| RunFailure::new(e, None))?;
        // Kill-on-drop: a dropped run future (timeout, human cancel) must never
        // leave a live dev-agent behind (spec §5.5).
        let mut guard = ChildGuard::new(child);
        let stderr_tail = spawn_stderr_drain(stderr);

        let capture = Arc::new(Mutex::new(Capture::default()));
        let kind = match spec.kind {
            "plan" => RunKind::Plan,
            "supervise" => RunKind::Supervise,
            "review" => RunKind::Review,
            _ => RunKind::Implement,
        };
        let mcp_servers = mcp::merged_mcp_servers(
            &spec.cwd,
            &self.remoter_mcp_command,
            // Container mode: remoter-mcp runs inside the container, so the
            // loopback API URL is rewritten to host.docker.internal.
            match &spec.exec {
                workspace::ExecEnv::Container(c) => &c.api_url,
                workspace::ExecEnv::Host { .. } => &self.api_url,
            },
            &self.token,
            self.workspace_id,
            kind,
            spec.task_id,
            &spec.exec,
        );
        let transport = ByteStreams::new(stdin, stdout);

        let updates = capture.clone();
        // Container mode: the ACP session's cwd is the in-container mount
        // point (`/work`); on the host it's the worktree itself.
        let cwd = match &spec.exec {
            workspace::ExecEnv::Container(c) => c.work_dir.clone(),
            workspace::ExecEnv::Host { .. } => spec.cwd.clone(),
        };
        let prompt = spec.prompt.clone();
        let resume = spec.resume_session.clone();
        let logger = spec.logger.clone();
        let config_options = spec.config_options.clone();
        // Container mode: the wire files land in the host-side agent home
        // (mounted at /root in the container), not the host user's home.
        let sessions_dir = match &spec.exec {
            workspace::ExecEnv::Container(c) => Some(c.sessions_dir.clone()),
            workspace::ExecEnv::Host { .. } => self.sessions_dir.clone(),
        };
        let kimi_usage_enabled = self.program == "kimi" || self.program.ends_with("/kimi");
        let known_session: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let baseline_lines: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let known_session_for_conn = known_session.clone();
        let baseline_for_conn = baseline_lines.clone();
        let sessions_dir_after = sessions_dir.clone();

        // Log the outgoing prompt before any session setup can fail.
        spec.logger.log_prompt(&spec.prompt);

        let conn = Client
            .builder()
            .name("remoter-agent")
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    apply_update(&updates, &logger, n.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    // Auto-approve every permission request (spec §5.5) — but only
                    // via an allow option. An agent that offers none gets the
                    // request cancelled; blindly picking the first option could
                    // select a reject and silently stall the run.
                    let option = request
                        .options
                        .iter()
                        .find(|o| o.kind == PermissionOptionKind::AllowAlways)
                        .or_else(|| {
                            request
                                .options
                                .iter()
                                .find(|o| o.kind == PermissionOptionKind::AllowOnce)
                        });
                    match option {
                        Some(o) => responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(o.option_id.clone())),
                        )),
                        None => responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_request: CreateElicitationRequest, responder, _cx| {
                    // The agent must not ask questions (spec §5.5): decline every
                    // elicitation so it proceeds on its own assumptions.
                    responder.respond(CreateElicitationResponse::new(ElicitationAction::Decline))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, move |cx: ConnectionTo<Agent>| async move {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                // Bounce/re-plan runs resume the previous session of the same
                // kind (spec §5.4); a gone session falls back to a fresh one —
                // the prompt already carries the plan text and the thread, so
                // the ticket is self-contained.
                let mut resumed = false;
                let (mut session_id, advertised) = match resume {
                    Some(id) => match resume_session(&cx, &id, &cwd, &mcp_servers).await {
                        Ok(adv) => {
                            tracing::info!(session_id = %id, "resumed previous session");
                            resumed = true;
                            (SessionId::from(id), adv)
                        }
                        Err(e) => {
                            tracing::warn!(session_id = %id, error = %e, "session resume failed; starting fresh");
                            new_session(&cx, &cwd, &mcp_servers).await?
                        }
                    },
                    None => new_session(&cx, &cwd, &mcp_servers).await?,
                };

                // Apply per-kind model/effort config via the standard ACP
                // `session/set_config_option` request. A mismatch with what the
                // agent advertises is a permanent configuration error (the run
                // cannot proceed with the requested model).
                apply_config_options(&cx, &session_id, &advertised, &config_options)
                    .await
                    .map_err(driver_error_to_acp_error)?;

                // Record the session id and a usage baseline right before the
                // LLM request so we can poll the local wire file for token counts
                // when ACP does not report them directly.
                let sid = session_id.0.to_string();
                *known_session_for_conn.lock().expect("known_session poisoned") = Some(sid.clone());
                *baseline_for_conn.lock().expect("baseline poisoned") = sessions_dir
                    .as_deref()
                    .filter(|_| kimi_usage_enabled)
                    .map(|d| kimi_usage::usage_line_count(d, &sid))
                    .unwrap_or(0);

                let prompt_request = |session_id: SessionId| {
                    PromptRequest::new(session_id, vec![ContentBlock::Text(TextContent::new(prompt.clone()))])
                };
                let response = match cx.send_request(prompt_request(session_id.clone())).block_task().await {
                    Ok(response) => response,
                    Err(e) if resumed => {
                        // A resume can succeed while the session is already too
                        // big for the provider: the first prompt then dies on
                        // compaction / context-limit (task 11: a ~60 MB session
                        // made every run fail before doing any work). Retry on
                        // a fresh session within the same attempt — the prompt
                        // is self-contained, so the attempt budget is not spent
                        // on a doomed resume. Only if the fresh prompt doesn't
                        // fit either does the error become Permanent via
                        // classify_error.
                        tracing::warn!(session_id = %session_id.0, error = %e,
                            "prompt on the resumed session failed; retrying on a fresh session");
                        let (fresh_id, fresh_adv) = new_session(&cx, &cwd, &mcp_servers).await?;
                        apply_config_options(&cx, &fresh_id, &fresh_adv, &config_options)
                            .await
                            .map_err(driver_error_to_acp_error)?;
                        session_id = fresh_id;
                        let sid = session_id.0.to_string();
                        *known_session_for_conn.lock().expect("known_session poisoned") = Some(sid.clone());
                        *baseline_for_conn.lock().expect("baseline poisoned") = sessions_dir
                            .as_deref()
                            .filter(|_| kimi_usage_enabled)
                            .map(|d| kimi_usage::usage_line_count(d, &sid))
                            .unwrap_or(0);
                        cx.send_request(prompt_request(session_id.clone())).block_task().await?
                    }
                    Err(e) => return Err(e),
                };
                Ok((session_id, response))
            });

        // Race the protocol exchange against the child process itself: an
        // agent that exits mid-turn (e.g. after a provider-side failure that
        // never produces a JSON-RPC response) is detected here instead of
        // hanging the attempt until the outer run timeout. Transient: the
        // retry budget bounds the damage (spec §5.7).
        let result = tokio::select! {
            result = conn => result,
            status = guard.wait_exited() => {
                let stderr = stderr_tail.lock().expect("stderr poisoned").clone();
                tracing::warn!(%status, "agent process exited with an unanswered ACP request");
                return Err(RunFailure::new(
                    DriverError::Transient(format!(
                        "agent process exited ({status}) before answering the request; agent stderr tail: {stderr}"
                    )),
                    None,
                ));
            }
        };

        let (session_id, response) = match result {
            Ok(ok) => ok,
            Err(e) => {
                let stderr = stderr_tail.lock().expect("stderr poisoned").clone();
                let source = classify_error(e, &stderr);
                let sid = known_session.lock().expect("known_session poisoned").clone();
                let baseline = *baseline_lines.lock().expect("baseline poisoned");
                let (input_tokens, output_tokens) = if kimi_usage_enabled {
                    if let (Some(dir), Some(sid)) = (sessions_dir_after.as_deref(), sid.as_ref()) {
                        kimi_usage::poll_session_usage(dir, sid, baseline).await
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };
                return Err(RunFailure {
                    source,
                    session_id: sid,
                    input_tokens,
                    output_tokens,
                });
            }
        };

        // Normal end: the agent should exit on its own once the dropped
        // transport closed its stdin; give it a grace window, then escalate.
        guard.shutdown_gracefully().await;

        let captured = capture.lock().expect("capture poisoned").finish();
        // The final message was never "completed" by a successor chunk, so the
        // notification handler hasn't logged it yet; log it now.
        if !captured.final_message.trim().is_empty() {
            spec.logger.log_message(&captured.final_message);
        }
        let text = match spec.kind {
            // Plan runs prefer the structured ACP plan over the final chat
            // message (spec §5.4 step 5).
            "plan" => captured.plan.unwrap_or(captured.final_message),
            _ => captured.final_message,
        };

        let baseline = *baseline_lines.lock().expect("baseline poisoned");
        match response.stop_reason {
            // A zero-output end turn is the kimi #1485 class of bug (spec §5.7):
            // fail loudly as permanent instead of reporting an empty plan as a
            // success and parking the ticket in front of a human.
            StopReason::EndTurn if text.trim().is_empty() => {
                let sid = session_id.0.to_string();
                let (input_tokens, output_tokens) =
                    usage_from_wire(kimi_usage_enabled, sessions_dir_after.as_deref(), &sid, baseline).await;
                Err(RunFailure {
                    source: DriverError::Permanent("agent ended the turn with no output".to_string()),
                    session_id: Some(sid),
                    input_tokens,
                    output_tokens,
                })
            }
            StopReason::EndTurn => {
                let (model, thinking) = outcome_model_thinking(&spec.config_options);
                let sid = session_id.0.to_string();
                let (input_tokens, output_tokens) = if let Some(u) = response.usage.as_ref() {
                    (Some(u.input_tokens as i64), Some(u.output_tokens as i64))
                } else {
                    usage_from_wire(kimi_usage_enabled, sessions_dir_after.as_deref(), &sid, baseline).await
                };
                Ok(RunOutcome {
                    session_id: Some(sid),
                    text,
                    input_tokens,
                    output_tokens,
                    model,
                    thinking,
                })
            }
            StopReason::Cancelled => Err(RunFailure::new(
                DriverError::Transient("agent cancelled the turn".to_string()),
                Some(session_id.0.to_string()),
            )),
            other => Err(RunFailure::new(
                DriverError::Permanent(format!(
                    "agent stopped with reason {other:?}; partial final message: {}",
                    truncate(&text, 500)
                )),
                Some(session_id.0.to_string()),
            )),
        }
    }
}

/// Reads token usage from the local session wire file when ACP does not report
/// it directly. Returns `(None, None)` if no usage file is found or polling times out.
async fn usage_from_wire(
    enabled: bool,
    sessions_dir: Option<&std::path::Path>,
    session_id: &str,
    baseline: usize,
) -> (Option<i64>, Option<i64>) {
    if !enabled {
        return (None, None);
    }
    match sessions_dir {
        Some(dir) => kimi_usage::poll_session_usage(dir, session_id, baseline).await,
        None => (None, None),
    }
}

fn driver_error_to_acp_error(e: DriverError) -> agent_client_protocol::Error {
    let msg = match e {
        DriverError::Permanent(m) => format!("permanent driver error: {m}"),
        DriverError::Transient(m) => m,
    };
    agent_client_protocol::Error::new(-32603, msg)
}

async fn new_session(
    cx: &ConnectionTo<Agent>,
    cwd: &std::path::Path,
    mcp_servers: &[McpServer],
) -> Result<(SessionId, Vec<SessionConfigOption>), agent_client_protocol::Error> {
    let resp = cx
        .send_request(NewSessionRequest::new(cwd.to_path_buf()).mcp_servers(mcp_servers.to_vec()))
        .block_task()
        .await?;
    Ok((resp.session_id, resp.config_options.unwrap_or_default()))
}

async fn resume_session(
    cx: &ConnectionTo<Agent>,
    session_id: &str,
    cwd: &std::path::Path,
    mcp_servers: &[McpServer],
) -> Result<Vec<SessionConfigOption>, agent_client_protocol::Error> {
    let resp = cx
        .send_request(
            ResumeSessionRequest::new(session_id.to_string(), cwd.to_path_buf()).mcp_servers(mcp_servers.to_vec()),
        )
        .block_task()
        .await?;
    Ok(resp.config_options.unwrap_or_default())
}

/// Applies per-kind ACP config options (`model`, `thinking`/`effort`, …) after a
/// session is created or resumed. `advertised` is what the agent returned in
/// `session/new`/`session/resume`; if it is empty we silently skip the options
/// so older agents keep working. If the agent advertises config options but
/// does not offer a configured id or value, the run fails permanently — the
/// daemon is misconfigured relative to the agent CLI version.
///
/// Option choices can depend on options set earlier in the same loop (kimi:
/// the `thinking` select's values change with the selected `model` — e.g.
/// `kimi-code/kimi-for-coding` accepts only `on`). Each successful
/// `set_config_option` response carries the full refreshed option set, so
/// subsequent options are validated against that, not the initial snapshot.
async fn apply_config_options(
    cx: &ConnectionTo<Agent>,
    session_id: &SessionId,
    advertised: &[SessionConfigOption],
    options: &[(String, String)],
) -> Result<(), DriverError> {
    if options.is_empty() {
        return Ok(());
    }
    if advertised.is_empty() {
        tracing::warn!(
            ?options,
            "agent advertises no session config options; skipping per-kind model/thinking configuration"
        );
        return Ok(());
    }

    let mut current: Vec<SessionConfigOption> = advertised.to_vec();
    for (id, value) in options {
        let Some(opt) = current.iter().find(|o| o.id.0.as_ref() == id.as_str()) else {
            let advertised_ids: Vec<&str> = current.iter().map(|o| o.id.0.as_ref()).collect();
            // OpenCode models may not expose an effort control at all. The
            // model selection is still valid, so treat this optional tuning
            // setting as a no-op rather than failing the ticket.
            if id == "effort" {
                tracing::warn!(
                    config_id = id,
                    value,
                    ?advertised_ids,
                    "agent does not advertise optional ACP config option; skipping"
                );
                continue;
            }
            return Err(DriverError::Permanent(format!(
                "agent does not advertise configured config option {id:?} (value {value:?}); \
                     advertised options: {advertised_ids:?}; \
                     check driver.plan_model/plan_thinking/implement_model/implement_thinking"
            )));
        };
        if !is_valid_config_value(opt, value) {
            return Err(DriverError::Permanent(format!(
                "configured value {value:?} is not a valid choice for config option {id:?}; \
                     valid choices: {:?}",
                config_option_values(opt)
            )));
        }
        let req = SetSessionConfigOptionRequest::new(
            session_id.clone(),
            SessionConfigId::new(id.clone()),
            SessionConfigOptionValue::value_id(value.clone()),
        );
        match cx.send_request(req).block_task().await {
            Ok(resp) => {
                tracing::info!(session_id = %session_id.0, config_id = %id, value, "set session config option");
                if !resp.config_options.is_empty() {
                    current = resp.config_options;
                }
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id.0,
                    config_id = %id,
                    value,
                    error = %e,
                    "failed to set session config option; continuing"
                );
            }
        }
    }
    Ok(())
}

fn config_option_values(opt: &SessionConfigOption) -> Vec<String> {
    match &opt.kind {
        SessionConfigKind::Select(select) => match &select.options {
            SessionConfigSelectOptions::Ungrouped(opts) => {
                opts.iter().map(|o| o.value.0.as_ref().to_string()).collect()
            }
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|g| g.options.iter().map(|o| o.value.0.as_ref().to_string()))
                .collect(),
            _ => Vec::new(),
        },
        SessionConfigKind::Boolean(_) => vec!["true".to_string(), "false".to_string()],
        _ => Vec::new(),
    }
}

fn is_valid_config_value(opt: &SessionConfigOption, value: &str) -> bool {
    config_option_values(opt).iter().any(|v| v == value)
}

fn outcome_model_thinking(options: &[(String, String)]) -> (Option<String>, Option<String>) {
    let mut model = None;
    let mut thinking = None;
    for (id, value) in options {
        match id.as_str() {
            "model" => model = Some(value.clone()),
            "thinking" => thinking = Some(value.clone()),
            _ => {}
        }
    }
    (model, thinking)
}

/// Spawns the agent as its own process-group leader (spec §5.5) with piped
/// stdio. A spawn failure (binary missing) is permanent — retrying won't help.
fn spawn_agent(
    program: &str,
    args: &[String],
    spec: &RunSpec,
) -> Result<
    (
        async_process::Child,
        async_process::ChildStdin,
        async_process::ChildStdout,
        async_process::ChildStderr,
    ),
    DriverError,
> {
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = workspace::wrap_command(&spec.cwd, &spec.exec, program, &args_ref, &spec.env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut cmd = async_process::Command::from(cmd);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| DriverError::Permanent(format!("spawn {program} {}: {e}", args.join(" "))))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    match (stdin, stdout, stderr) {
        (Some(i), Some(o), Some(e)) => Ok((child, i, o, e)),
        _ => Err(DriverError::Permanent(format!(
            "spawn {program}: could not open stdio pipes"
        ))),
    }
}

/// Protocol/transport failures are transient (spec §5.7): the retry budget
/// bounds the damage when the cause turns out to be persistent. Provider
/// context/size-limit rejections (4xx like "supports only 256K context") and
/// explicit permanent driver errors (e.g. config option mismatch) are permanent
/// instead — retrying only burns the attempt budget.
fn classify_error(e: agent_client_protocol::Error, stderr_tail: &str) -> DriverError {
    let raw = e.to_string();
    if raw.contains("permanent driver error:") {
        let msg = raw.strip_prefix("permanent driver error: ").unwrap_or(&raw).to_string();
        return DriverError::Permanent(msg);
    }
    let mut msg = format!("ACP error: {e}");
    if !stderr_tail.is_empty() {
        msg.push_str(&format!("; agent stderr tail: {stderr_tail}"));
    }
    if looks_like_context_limit(&msg) {
        return DriverError::Permanent(format!("provider context/size limit rejection (not retryable): {msg}"));
    }
    DriverError::Transient(msg)
}

/// Provider 4xx "prompt too large" rejections surface only as message text
/// (no structured code to rely on) — match the known wordings,
/// case-insensitively. "Total message size exceeds limit" is the task-11
/// wording: the provider rejected the resumed session's compaction payload.
fn looks_like_context_limit(msg: &str) -> bool {
    let m = msg.to_lowercase();
    const PHRASES: &[&str] = &[
        "context length",
        "context window",
        "context limit",
        "context size",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "request too large",
        "payload too large",
        "message size",
    ];
    PHRASES.iter().any(|p| m.contains(p)) || (m.contains("supports only") && m.contains("context"))
}

/// Kill-on-drop guard for the agent process tree (spec §5.5: SIGTERM, grace,
/// SIGKILL; always reap). Held by the run future: dropping the future (timeout,
/// human cancel) drops the guard.
struct ChildGuard {
    child: Option<async_process::Child>,
}

impl ChildGuard {
    fn new(child: async_process::Child) -> Self {
        Self { child: Some(child) }
    }

    /// The turn ended normally — the dropped transport closed the agent's
    /// stdin, so it should exit on its own. Give it a grace window; if it
    /// lingers, hand the child back to Drop for TERM → KILL escalation.
    async fn shutdown_gracefully(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        if tokio::time::timeout(TERM_GRACE, child.status()).await.is_err() {
            self.child = Some(child);
        }
    }

    /// Polls until the agent process exits, reaping it via `try_status`
    /// (signal-0 existence checks can't tell a zombie from a live process).
    /// The child stays in the guard so Drop still TERM → KILLs the process
    /// group: the agent may leave grandchildren (MCP servers, tool subprocesses)
    /// behind when it dies mid-turn.
    async fn wait_exited(&mut self) -> std::process::ExitStatus {
        loop {
            let exited = match self.child.as_mut() {
                Some(child) => child.try_status().ok().flatten(),
                // Only reachable if shutdown_gracefully ran concurrently —
                // it doesn't; pend rather than end the select! arm spuriously.
                None => std::future::pending().await,
            };
            if let Some(status) = exited {
                return status;
            }
            tokio::time::sleep(EXIT_POLL).await;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        #[cfg(unix)]
        // SAFETY: plain signal send; the pgid belongs to the spawned child,
        // which stays a group member (as a zombie) until we reap it below.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGTERM);
        }
        // Escalate and reap off the runtime: Drop must not block.
        std::thread::spawn(move || {
            std::thread::sleep(TERM_GRACE);
            #[cfg(unix)]
            // SAFETY: see above.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = futures::executor::block_on(child.status());
        });
    }
}

/// Drains the agent's stderr into a bounded tail buffer (for error reports)
/// plus trace logs. Ends when the child exits and closes the pipe.
fn spawn_stderr_drain(stderr: async_process::ChildStderr) -> Arc<Mutex<String>> {
    let tail = Arc::new(Mutex::new(String::new()));
    let sink = tail.clone();
    tokio::spawn(async move {
        use futures::AsyncReadExt;
        let mut stderr = stderr;
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    tracing::debug!(target: "remoter_agent::acp_stderr", "{chunk}");
                    let mut tail = sink.lock().expect("stderr poisoned");
                    tail.push_str(&chunk);
                    // Keep only the last 8 KiB.
                    let excess = tail.len().saturating_sub(8 * 1024);
                    if excess > 0 {
                        let mut start = excess;
                        while !tail.is_char_boundary(start) {
                            start += 1;
                        }
                        tail.drain(..start);
                    }
                }
            }
        }
    });
    tail
}

/// What the streamed `session/update` notifications accumulate into.
#[derive(Default)]
struct Capture {
    /// Text of the message currently streaming.
    current: String,
    /// `messageId` of the message currently streaming (multi-message turns).
    current_id: Option<String>,
    /// Text of the last completed message.
    final_message: String,
    /// The latest structured plan update, rendered as markdown.
    plan: Option<String>,
}

impl Capture {
    /// Applies one streamed update; returns the text of a message that just
    /// completed (superseded by a new `messageId`), so the caller can ship it
    /// to the AI log. The final message completes in [Capture::finish].
    fn apply(&mut self, update: SessionUpdate) -> Option<String> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let id = chunk.message_id.map(|id| id.0.to_string());
                let mut completed = None;
                if id != self.current_id {
                    let done = std::mem::take(&mut self.current);
                    if !done.is_empty() {
                        completed = Some(done.clone());
                    }
                    self.final_message = done;
                    self.current_id = id;
                }
                if let ContentBlock::Text(t) = chunk.content {
                    self.current.push_str(&t.text);
                }
                completed
            }
            SessionUpdate::Plan(plan) => {
                self.plan = Some(render_plan(&plan));
                None
            }
            _ => None,
        }
    }

    fn finish(&mut self) -> Captured {
        if !self.current.is_empty() {
            self.final_message = std::mem::take(&mut self.current);
        }
        Captured {
            final_message: std::mem::take(&mut self.final_message),
            plan: self.plan.take(),
        }
    }
}

struct Captured {
    final_message: String,
    plan: Option<String>,
}

fn apply_update(capture: &Arc<Mutex<Capture>>, logger: &SessionLogger, update: SessionUpdate) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let Some(done) = capture
                .lock()
                .expect("capture poisoned")
                .apply(SessionUpdate::AgentMessageChunk(chunk))
            {
                logger.log_message(done);
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let Some(text) = text_from_chunk(&chunk.content) {
                logger.log_thought(text);
            }
        }
        SessionUpdate::ToolCall(t) => {
            logger.log_tool_call(t.tool_call_id.0.to_string(), tool_kind_str(&t.kind), t.title);
        }
        SessionUpdate::ToolCallUpdate(u) => {
            logger.log_tool_call_update(
                u.tool_call_id.0.to_string(),
                u.fields.kind.as_ref().map(|k| tool_kind_str(k).to_string()),
                u.fields.title,
                u.fields.status.as_ref().map(|s| tool_status_str(s).to_string()),
            );
        }
        SessionUpdate::Plan(plan) => {
            let rendered = render_plan(&plan);
            capture
                .lock()
                .expect("capture poisoned")
                .apply(SessionUpdate::Plan(plan));
            logger.log_plan(rendered);
        }
        _ => {}
    }
}

fn text_from_chunk(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

fn render_plan(plan: &Plan) -> String {
    plan.entries
        .iter()
        .map(|e| format!("- [{}] {}", status_mark(&e.status), e.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn status_mark(status: &agent_client_protocol::schema::v1::PlanEntryStatus) -> &'static str {
    use agent_client_protocol::schema::v1::PlanEntryStatus as S;
    match status {
        S::Pending => " ",
        S::InProgress => "~",
        S::Completed => "x",
        _ => "?",
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The task_id=11 failure mode: the provider rejects the oversized prompt
    /// ("supports only 256K context") — permanent, not worth a retry (§5.7).
    #[test]
    fn context_limit_rejection_is_permanent() {
        let e = agent_client_protocol::Error::new(
            -32603,
            "HTTP 401: this model supports only 256K context, but the prompt has 500K tokens",
        );
        let err = classify_error(e, "");
        assert!(matches!(err, DriverError::Permanent(_)), "{err}");
    }

    /// The same rejection reported via the agent's stderr tail is permanent too.
    #[test]
    fn context_limit_in_stderr_tail_is_permanent() {
        let e = agent_client_protocol::Error::internal_error();
        let err = classify_error(e, "error: maximum context length is 256000 tokens");
        assert!(matches!(err, DriverError::Permanent(_)), "{err}");
    }

    #[test]
    fn transport_error_stays_transient() {
        let e = agent_client_protocol::Error::new(-32603, "connection reset by peer");
        let err = classify_error(e, "");
        assert!(matches!(err, DriverError::Transient(_)), "{err}");
    }

    #[test]
    fn context_limit_phrases() {
        for msg in [
            "maximum context length is 256000 tokens",
            "context window exceeded",
            "context limit reached",
            "prompt is too long",
            "too many tokens in request",
            "request too large",
            "payload too large",
            "this model supports only 256K context",
            "400 total message size exceeds limit",
        ] {
            assert!(looks_like_context_limit(msg), "{msg}");
        }
        for msg in ["permission denied", "rate limited, retry later", "internal error"] {
            assert!(!looks_like_context_limit(msg), "{msg}");
        }
    }
}
