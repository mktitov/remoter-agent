//! User Q&A runs (`question`): fork the source ACP conversation when the agent
//! supports `session/fork`, or use a fresh, self-contained read-only session.
//! Question runs never move the ticket, write workflow comments/outcomes, push,
//! or create PRs. The only backend writes are the atomic question start and the
//! question-run completion.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{
    client::{
        AgentQuestionDto, AgentRunDto, FinishRunBody, ProjectRepoConfig, RemoterClient, StartedQuestion, TaskDetail,
        TaskSummary,
    },
    config::Config,
    driver::{AgentDriver, DriverError, RunFailure, RunOutcome, RunSpec},
    logstore::LogStore,
    ports::PortBlock,
    run::{self, RunLogGuard},
    session_log::SessionLogger,
    workspace,
};

pub const QUESTION_RUN_KIND: &str = "question";

const MAX_ARTIFACT_BYTES: usize = 16 * 1024;
const MAX_DIFF_BYTES: usize = 64 * 1024;
const MAX_SOURCE_LOG_BYTES: usize = 96 * 1024;
const MAX_HISTORY_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone)]
pub struct QuestionTrigger {
    pub question: AgentQuestionDto,
    pub task: TaskSummary,
}

#[derive(Debug, Default)]
pub struct QuestionPoll {
    pub pending: Vec<QuestionTrigger>,
    pub cancellations: Vec<AgentQuestionDto>,
}

#[derive(Default)]
pub struct Questioner {
    unsupported_backend: bool,
}

impl Questioner {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn poll(&mut self, client: &RemoterClient) -> QuestionPoll {
        if self.unsupported_backend {
            return QuestionPoll::default();
        }
        let pending = match client.agent_question_queue("pending").await {
            Ok(q) => q,
            Err(e) if unsupported_endpoint(&e) => {
                self.unsupported_backend = true;
                tracing::info!("agent questions are not supported by this backend; question polling disabled");
                return QuestionPoll::default();
            }
            Err(e) => {
                tracing::warn!(error = %e, "question: could not list pending queue");
                return QuestionPoll::default();
            }
        };
        let cancellations = match client.agent_question_queue("cancel_requested").await {
            Ok(q) => q,
            Err(e) if unsupported_endpoint(&e) => {
                self.unsupported_backend = true;
                tracing::info!("agent questions are not supported by this backend; question polling disabled");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "question: could not list cancellation queue");
                Vec::new()
            }
        };

        let tasks = match client.my_tasks(None).await {
            Ok(t) => t.into_iter().map(|t| (t.id, t)).collect::<HashMap<i32, TaskSummary>>(),
            Err(e) => {
                tracing::warn!(error = %e, "question: could not list assigned tasks");
                return QuestionPoll {
                    pending: Vec::new(),
                    cancellations,
                };
            }
        };
        let mut pending = pending;
        pending.sort_by_key(|q| q.id);
        let pending = pending
            .into_iter()
            .filter_map(|question| {
                let task = tasks.get(&question.task_id)?.clone();
                matches!(task.task_status.as_str(), "plan_review" | "review")
                    .then_some(QuestionTrigger { question, task })
            })
            .collect();
        QuestionPoll { pending, cancellations }
    }
}

fn unsupported_endpoint(e: &crate::client::ClientError) -> bool {
    matches!(
        e.status,
        Some(
            reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::METHOD_NOT_ALLOWED
                | reqwest::StatusCode::NOT_IMPLEMENTED
        )
    )
}

pub struct QuestionContext {
    pub client: RemoterClient,
    pub config: Arc<Config>,
    pub driver: Arc<dyn AgentDriver>,
    pub trigger: QuestionTrigger,
    pub project: ProjectRepoConfig,
    pub port_block: Option<PortBlock>,
    pub image_locks: crate::image::ImageLocks,
    pub cancel: CancellationToken,
    pub log_store: LogStore,
}

pub async fn execute(qc: QuestionContext) {
    let task_id = qc.trigger.task.id;
    let question_id = qc.trigger.question.id;
    let started = match qc.client.start_agent_question(&qc.trigger.question).await {
        Ok(started) => started,
        Err(e) if e.is_conflict() => {
            tracing::info!(task_id, question_id, "question already started; skipping duplicate");
            return;
        }
        Err(e) => {
            tracing::warn!(task_id, question_id, error = %e, "question: could not start run");
            return;
        }
    };
    let run_id = started.run.id;
    tracing::info!(task_id, question_id, run_id, "question run started");
    let (logger, _log_guard) = match qc.log_store.open_run(task_id, run_id) {
        Ok(run_logger) => {
            let guard = RunLogGuard::new(qc.log_store.clone(), task_id, run_id);
            (SessionLogger::new(run_logger), Some(guard))
        }
        Err(e) => {
            tracing::warn!(task_id, run_id, error = %e, "question: could not open ACP log; logging disabled");
            (SessionLogger::noop(), None)
        }
    };

    let cwd = question_cwd(&qc);
    let before = match WorktreeSnapshot::capture(&cwd).await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            finish_failure(&qc, &started, format!("worktree snapshot before question: {e}"), None).await;
            return;
        }
    };
    let prepared = match prepare_prompts(&qc, &started).await {
        Ok(prepared) => prepared,
        Err(e) => {
            finish_failure(&qc, &started, format!("question context: {e}"), None).await;
            return;
        }
    };

    let result = run_attempt(&qc, &started, &prepared, &cwd, &logger).await;
    let after = WorktreeSnapshot::capture(&cwd).await;
    if qc.cancel.is_cancelled() {
        finish_cancelled(&qc, &started, result.err()).await;
        return;
    }
    if !after.as_ref().is_ok_and(|after| before == *after) {
        let detail = match after {
            Ok(_) => "HEAD or worktree changed".to_string(),
            Err(e) => format!("worktree snapshot after question failed: {e}"),
        };
        let failure = RunFailure::new(
            DriverError::Permanent(format!("question run modified the worktree unexpectedly: {detail}")),
            result.as_ref().ok().and_then(|o| o.session_id.clone()),
        );
        finish_failure(&qc, &started, failure.to_string(), Some(&failure)).await;
        return;
    }

    match result {
        Ok(outcome) => {
            let mut body = FinishRunBody::succeeded();
            body.session_id = outcome.session_id;
            body.summary = Some(outcome.text);
            body.input_tokens = outcome.input_tokens;
            body.output_tokens = outcome.output_tokens;
            body.model = outcome.model;
            body.thinking = outcome.thinking;
            if let Err(e) = qc.client.finish_run(run_id, &body).await {
                tracing::warn!(task_id, question_id, run_id, error = %e, "question: could not record answer");
            } else {
                tracing::info!(task_id, question_id, run_id, "question run succeeded");
            }
        }
        Err(failure) => finish_failure(&qc, &started, failure.to_string(), Some(&failure)).await,
    }
}

fn question_cwd(qc: &QuestionContext) -> PathBuf {
    let root = &qc.config.workspace_root;
    let worktree = workspace::worktree_dir(root, qc.project.project_id, qc.trigger.task.id);
    if worktree.exists() {
        worktree
    } else {
        workspace::repo_dir(root, qc.project.project_id)
    }
}

struct PreparedPrompt {
    prompt: String,
    fallback_prompt: String,
    fork_session: Option<String>,
}

async fn prepare_prompts(qc: &QuestionContext, started: &StartedQuestion) -> Result<PreparedPrompt, String> {
    let detail = qc
        .client
        .task_detail(qc.trigger.task.id)
        .await
        .map_err(|e| e.to_string())?;
    let history = qc
        .client
        .task_agent_questions(qc.trigger.task.id)
        .await
        .map_err(|e| e.to_string())?;
    let runs = qc
        .client
        .list_runs(qc.trigger.task.id)
        .await
        .map_err(|e| e.to_string())?;
    let source = followup_source(&history, &runs).or_else(|| started.source_run.clone());
    let artifacts = current_artifacts(qc, &detail, &runs).await;
    let prompt = render_answer_prompt(&qc.trigger.question, &artifacts);
    let fallback_prompt = render_fallback_prompt(qc, &detail, &history, source.as_ref(), &artifacts);
    Ok(PreparedPrompt {
        prompt,
        fallback_prompt,
        fork_session: source.and_then(|r| r.session_id),
    })
}

/// Follow-ups fork the latest successful Q&A fork, not the original source.
/// The first question uses the backend-selected plan/implement source run.
fn followup_source(history: &[AgentQuestionDto], runs: &[AgentRunDto]) -> Option<AgentRunDto> {
    let mut succeeded: Vec<&AgentQuestionDto> = history.iter().filter(|q| q.status == "succeeded").collect();
    succeeded.sort_by_key(|q| q.id);
    succeeded.into_iter().rev().find_map(|question| {
        let run_id = question.run_id?;
        runs.iter()
            .find(|r| r.id == run_id && r.kind == QUESTION_RUN_KIND && r.session_id.is_some())
            .cloned()
    })
}

struct ArtifactContext {
    plan: Option<String>,
    report: Option<String>,
    diff: String,
}

async fn current_artifacts(qc: &QuestionContext, detail: &TaskDetail, runs: &[AgentRunDto]) -> ArtifactContext {
    let plan = runs
        .iter()
        .find(|r| r.kind == "plan" && r.status == "succeeded")
        .and_then(|r| r.plan.clone())
        .map(|p| run::truncate_with_marker(&p, MAX_ARTIFACT_BYTES));
    let report = detail
        .report
        .as_deref()
        .map(|r| run::truncate_with_marker(r, MAX_ARTIFACT_BYTES));
    let cwd = question_cwd(qc);
    let configured_base = match qc.project.base_branch.clone() {
        Some(base) => Some(base),
        None => match workspace::remote_default_branch(&cwd).await {
            Ok(base) => Some(base),
            Err(e) => {
                tracing::warn!(error = %e, "question: could not resolve the remote default branch");
                None
            }
        },
    };
    let base = match configured_base.as_deref() {
        Some(base) if workspace::local_branch_exists(&cwd, base).await => Some(base.to_string()),
        Some(base) if workspace::remote_tracking_branch_exists(&cwd, base).await => Some(format!("origin/{base}")),
        Some(_) | None => None,
    };
    let diff = match base {
        Some(base) => workspace::diff(&cwd, &format!("{base}...HEAD"))
            .await
            .unwrap_or_else(|e| format!("_Could not compute diff against {base}: {e}_")),
        None => workspace::diff(&cwd, "HEAD")
            .await
            .unwrap_or_else(|e| format!("_Could not compute worktree diff: {e}_")),
    };
    ArtifactContext {
        plan,
        report,
        diff: run::truncate_with_marker(&diff, MAX_DIFF_BYTES),
    }
}

fn render_answer_prompt(question: &AgentQuestionDto, artifacts: &ArtifactContext) -> String {
    let mut prompt = format!("## Current user question\n{}\n\n", question.body);
    push_current_artifacts(&mut prompt, artifacts);
    prompt.push_str(ANSWER_ONLY_INSTRUCTIONS);
    prompt
}

fn render_fallback_prompt(
    qc: &QuestionContext,
    detail: &TaskDetail,
    history: &[AgentQuestionDto],
    source: Option<&AgentRunDto>,
    artifacts: &ArtifactContext,
) -> String {
    let mut prompt = format!(
        "## Ticket\nTicket #{}\n\n{}\n\n{}\n\n",
        detail.id,
        detail.title,
        run::truncate_with_marker(&detail.description, MAX_ARTIFACT_BYTES)
    );
    prompt.push_str("## Source run\n");
    match source {
        Some(run) => prompt.push_str(&format!("Run #{} ({})\n\n", run.id, run.kind)),
        None => prompt.push_str("_No source session/run is available._\n\n"),
    }
    prompt.push_str("### Original prompt and conversation log\n\n");
    if let Some(run) = source {
        match qc.log_store.read(detail.id, run.id, None, None, Some(1000)) {
            Ok((entries, _)) => {
                let mut used = 0usize;
                for entry in entries {
                    let text = entry.text.unwrap_or_else(|| {
                        entry
                            .tool_title
                            .map(|title| format!("{}: {title}", entry.kind.as_str()))
                            .unwrap_or_else(|| entry.kind.as_str().to_string())
                    });
                    let line = format!("\n[{}]\n{}\n", entry.kind.as_str(), text);
                    if used + line.len() > MAX_SOURCE_LOG_BYTES {
                        prompt.push_str("\n_[source log truncated at byte budget]_\n");
                        break;
                    }
                    used += line.len();
                    prompt.push_str(&line);
                }
            }
            Err(e) => prompt.push_str(&format!("_Source log unavailable: {e}_\n")),
        }
    }
    prompt.push_str("\n## Previous Q&A\n");
    let mut used = 0usize;
    for question in history.iter().filter(|q| q.id != qc.trigger.question.id) {
        let item = format!(
            "\n### Question #{}\n{}\n\nStatus: {}\nAnswer: {}\nError: {}\n",
            question.id,
            question.body,
            question.status,
            question.answer.as_deref().unwrap_or(""),
            question.error.as_deref().unwrap_or("")
        );
        if used + item.len() > MAX_HISTORY_BYTES {
            prompt.push_str("\n_[Q&A history truncated at byte budget]_\n");
            break;
        }
        used += item.len();
        prompt.push_str(&item);
    }
    prompt.push_str("\n## Current user question\n");
    prompt.push_str(&qc.trigger.question.body);
    prompt.push_str("\n\n");
    push_current_artifacts(&mut prompt, artifacts);
    prompt.push_str(ANSWER_ONLY_INSTRUCTIONS);
    prompt
}

fn push_current_artifacts(prompt: &mut String, artifacts: &ArtifactContext) {
    prompt.push_str("## Current artifacts\nThese are authoritative when they differ from the source session.\n\n");
    prompt.push_str("### Plan\n");
    prompt.push_str(artifacts.plan.as_deref().unwrap_or("_No successful plan run._"));
    prompt.push_str("\n\n### Implementation report\n");
    prompt.push_str(artifacts.report.as_deref().unwrap_or("_No implementation report._"));
    prompt.push_str("\n\n### Current diff\n```diff\n");
    prompt.push_str(&artifacts.diff);
    prompt.push_str("\n```\n\n");
}

const ANSWER_ONLY_INSTRUCTIONS: &str = "## Instructions\nAnswer only the current user question. Use the source \
conversation and current artifacts as context. This is strictly READ-ONLY: do not create, edit, delete, or move \
files; do not run git mutations; do not change the ticket, plan, report, review, actions, comments, attachments, \
PR, branch, or worktree. Do not call mutating MCP tools. Do not create commits or push. Return the complete answer \
as your final message and nothing else.\n";

async fn run_attempt(
    qc: &QuestionContext,
    started: &StartedQuestion,
    prepared: &PreparedPrompt,
    cwd: &Path,
    logger: &SessionLogger,
) -> Result<RunOutcome, RunFailure> {
    let env = question_env(qc);
    let branch = workspace::existing_branch(&qc.config.workspace_root, qc.project.project_id, qc.trigger.task.id)
        .await
        .unwrap_or_default();
    let containers = if qc.config.execution.is_container() {
        match run::start_container_runtime(
            &qc.config,
            &qc.image_locks,
            &qc.project,
            qc.trigger.task.id,
            started.run.id,
            cwd,
            false,
            &env,
            &[],
        )
        .await
        {
            Ok(runtime) => runtime,
            Err(failure) => return Err(failure),
        }
    } else {
        None
    };
    let exec = if containers.is_some() {
        run::exec_env(&qc.config, started.run.id, false)
    } else {
        workspace::ExecEnv::host(false)
    };
    let spec = RunSpec {
        task_id: qc.trigger.task.id,
        run_id: started.run.id,
        cwd: cwd.to_path_buf(),
        prompt: prepared.prompt.clone(),
        kind: QUESTION_RUN_KIND,
        resume_session: None,
        fork_session: prepared.fork_session.clone(),
        fallback_prompt: Some(prepared.fallback_prompt.clone()),
        branch,
        env,
        exec,
        config_options: qc.config.driver.config_options_for(QUESTION_RUN_KIND),
        logger: logger.clone(),
        phase_tx: None,
    };
    let timeout = Duration::from_secs(qc.config.run_timeout_minutes * 60);
    tokio::select! {
        _ = qc.cancel.cancelled() => Err(RunFailure::new(
            DriverError::Transient("question cancelled".to_string()),
            prepared.fork_session.clone(),
        )),
        result = tokio::time::timeout(timeout, qc.driver.run(spec)) => {
            result.unwrap_or_else(|_| Err(RunFailure::new(
                DriverError::Transient(format!("timeout after {} minutes", qc.config.run_timeout_minutes)),
                prepared.fork_session.clone(),
            )))
        }
    }
}

fn question_env(qc: &QuestionContext) -> Vec<(String, String)> {
    if qc.config.execution.is_container() {
        return crate::container::container_env(qc.trigger.task.id);
    }
    if qc.config.execution.is_bwrap() {
        return crate::bwrap::run_env(qc.trigger.task.id);
    }
    run::host_run_env(&qc.config, qc.trigger.task.id, qc.port_block.as_ref())
}

async fn finish_failure(qc: &QuestionContext, started: &StartedQuestion, error: String, failure: Option<&RunFailure>) {
    let mut body = FinishRunBody::failed(error);
    if let Some(failure) = failure {
        body.session_id = failure.session_id.clone();
        body.input_tokens = failure.input_tokens;
        body.output_tokens = failure.output_tokens;
    }
    if let Err(e) = qc.client.finish_run(started.run.id, &body).await {
        tracing::warn!(run_id = started.run.id, error = %e, "question: could not record failure");
    }
}

async fn finish_cancelled(qc: &QuestionContext, started: &StartedQuestion, failure: Option<RunFailure>) {
    let requested = qc
        .client
        .task_agent_questions(qc.trigger.task.id)
        .await
        .ok()
        .and_then(|questions| questions.into_iter().find(|q| q.id == started.question.id))
        .is_some_and(|q| q.status == "cancel_requested");
    let mut body = if requested {
        FinishRunBody::failed("cancelled by user".to_string())
    } else {
        FinishRunBody::failed("cancelled (human intervention or daemon shutdown)".to_string())
    };
    if let Some(failure) = failure {
        body.session_id = failure.session_id;
        body.input_tokens = failure.input_tokens;
        body.output_tokens = failure.output_tokens;
    }
    if let Err(e) = qc.client.finish_run(started.run.id, &body).await {
        tracing::warn!(run_id = started.run.id, error = %e, "question: could not record cancellation");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    head: String,
    status: Vec<u8>,
    diff: Vec<u8>,
    untracked: Vec<(Vec<u8>, [u8; 32])>,
}

impl WorktreeSnapshot {
    pub async fn capture(cwd: &Path) -> Result<Self, String> {
        use sha2::{Digest, Sha256};

        let head = git_output(cwd, &["rev-parse", "HEAD"]).await?;
        let status = git_output(cwd, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?;
        let diff = git_output(cwd, &["diff", "--binary", "HEAD"]).await?;
        let names = git_output(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
        let mut untracked = Vec::new();
        for name in names.split(|b| *b == 0).filter(|name| !name.is_empty()) {
            let path = cwd.join(path_from_git_name(name));
            let metadata =
                std::fs::symlink_metadata(&path).map_err(|e| format!("stat untracked file {}: {e}", path.display()))?;
            let contents = if metadata.file_type().is_symlink() {
                std::fs::read_link(&path)
                    .map_err(|e| format!("read untracked symlink {}: {e}", path.display()))?
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec()
            } else {
                tokio::fs::read(&path)
                    .await
                    .map_err(|e| format!("read untracked file {}: {e}", path.display()))?
            };
            untracked.push((name.to_vec(), Sha256::digest(contents).into()));
        }
        Ok(Self {
            head: String::from_utf8_lossy(&head).trim().to_string(),
            status,
            diff,
            untracked,
        })
    }
}

#[cfg(unix)]
fn path_from_git_name(name: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(name.to_vec()))
}

#[cfg(not(unix))]
fn path_from_git_name(name: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(name).into_owned())
}

async fn git_output(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = tokio::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: i32, kind: &str, session: Option<&str>) -> AgentRunDto {
        AgentRunDto {
            id,
            task_id: 7,
            agent_user_id: 1,
            kind: kind.to_string(),
            status: "succeeded".to_string(),
            session_id: session.map(str::to_string),
            branch: None,
            plan: None,
            summary: None,
            error: None,
            pr_url: None,
            pr_error: None,
            input_tokens: None,
            output_tokens: None,
            attempt: 1,
            phase: None,
            created_at: None,
        }
    }

    fn question(id: i32, status: &str, run_id: Option<i32>) -> AgentQuestionDto {
        AgentQuestionDto {
            id,
            task_id: 7,
            body: format!("question {id}"),
            status: status.to_string(),
            answer: None,
            error: None,
            run_id,
            source_run_id: None,
            created_at: None,
        }
    }

    #[test]
    fn followup_forks_latest_successful_question() {
        let history = vec![
            question(1, "succeeded", Some(10)),
            question(2, "failed", Some(11)),
            question(3, "succeeded", Some(12)),
            question(4, "pending", None),
        ];
        let runs = vec![
            run(12, "question", Some("fork-two")),
            run(11, "question", Some("failed-fork")),
            run(10, "question", Some("fork-one")),
            run(1, "implement", Some("source")),
        ];
        assert_eq!(
            followup_source(&history, &runs).and_then(|r| r.session_id),
            Some("fork-two".to_string())
        );
    }

    #[test]
    fn first_question_has_no_followup_source() {
        assert!(followup_source(&[question(1, "pending", None)], &[run(1, "implement", Some("source"))]).is_none());
    }

    #[test]
    fn answer_prompt_is_read_only_and_carries_current_artifacts() {
        let prompt = render_answer_prompt(
            &question(9, "pending", None),
            &ArtifactContext {
                plan: Some("current plan".to_string()),
                report: Some("current report".to_string()),
                diff: "current diff".to_string(),
            },
        );
        assert!(prompt.contains("question 9"));
        assert!(prompt.contains("current plan"));
        assert!(prompt.contains("current report"));
        assert!(prompt.contains("current diff"));
        assert!(prompt.contains("strictly READ-ONLY"));
        assert!(!prompt.contains("set_task_status"));
    }

    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn spawn_backend(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            request.extend_from_slice(&chunk[..n]);
                            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let first_line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                recorded.lock().unwrap().push(first_line);
                let response = format!(
                    "HTTP/1.1 {status} Reason\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{addr}"), requests)
    }

    fn run_json(id: i32, kind: &str, status: &str, session: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "task_id": 7,
            "agent_user_id": 1,
            "kind": kind,
            "status": status,
            "session_id": session,
            "branch": null,
            "plan": null,
            "summary": null,
            "error": null,
            "pr_url": null,
            "pr_error": null,
            "input_tokens": null,
            "output_tokens": null,
            "attempt": 1
        })
    }

    fn question_json(id: i32, status: &str, run_id: Option<i32>) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "taskId": 7,
            "body": format!("question {id}"),
            "status": status,
            "answer": null,
            "error": null,
            "runId": run_id,
            "sourceRunId": 10
        })
    }

    fn task_summary(id: i32, status: &str) -> TaskSummary {
        TaskSummary {
            id,
            project_id: 1,
            project_name: "project".to_string(),
            feature_id: 1,
            feature_description: "feature".to_string(),
            title: format!("task {id}"),
            description: "description".to_string(),
            task_status: status.to_string(),
            task_priority: None,
            actions_total: 0,
            actions_completed: 0,
            time_spent: 0,
            blocked: false,
            agent_review_requested: false,
            goal_id: None,
        }
    }

    #[tokio::test]
    async fn poll_is_fifo_and_only_launches_in_qa_statuses() {
        let pending = serde_json::json!([
            {"id":9,"taskId":8,"body":"new","status":"pending"},
            {"id":5,"taskId":6,"body":"wrong task state","status":"pending"},
            {"id":2,"taskId":7,"body":"old","status":"pending"}
        ]);
        let tasks = serde_json::json!([
            {"id":7,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f","title":"seven","description":"d","taskStatus":"plan_review","actionsTotal":0,"actionsCompleted":0,"timeSpent":0},
            {"id":8,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f","title":"eight","description":"d","taskStatus":"review","actionsTotal":0,"actionsCompleted":0,"timeSpent":0},
            {"id":6,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f","title":"six","description":"d","taskStatus":"implement","actionsTotal":0,"actionsCompleted":0,"timeSpent":0}
        ]);
        let (url, _) = spawn_backend(vec![
            (200, pending.to_string()),
            (200, "[]".to_string()),
            (200, tasks.to_string()),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let result = Questioner::new().poll(&client).await;
        let ids: Vec<i32> = result.pending.iter().map(|t| t.question.id).collect();
        assert_eq!(ids, [2, 9]);
    }

    #[tokio::test]
    async fn old_backend_disables_question_polling_without_repeated_requests() {
        let (url, requests) = spawn_backend(vec![(404, "{}".to_string())]).await;
        let client = RemoterClient::new(&url, "tok", None);
        let mut questioner = Questioner::new();
        assert!(questioner.poll(&client).await.pending.is_empty());
        assert!(questioner.poll(&client).await.pending.is_empty());
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("tracked.txt"), "before\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "tracked.txt"],
            vec![
                "-c",
                "user.email=test@remoter",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "initial",
            ],
        ] {
            let status = std::process::Command::new("git")
                .current_dir(path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
    }

    #[tokio::test]
    async fn snapshot_detects_head_and_worktree_changes() {
        let dir = std::env::temp_dir().join(format!("remoter-question-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_repo(&dir);
        let before = WorktreeSnapshot::capture(&dir).await.unwrap();
        assert_eq!(before, WorktreeSnapshot::capture(&dir).await.unwrap());
        std::fs::write(dir.join("tracked.txt"), "after\n").unwrap();
        assert_ne!(before, WorktreeSnapshot::capture(&dir).await.unwrap());
        std::fs::write(dir.join("tracked.txt"), "before\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "new\n").unwrap();
        let with_untracked = WorktreeSnapshot::capture(&dir).await.unwrap();
        assert_ne!(before, with_untracked);
        std::fs::write(dir.join("untracked.txt"), "changed\n").unwrap();
        assert_ne!(with_untracked, WorktreeSnapshot::capture(&dir).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_context(api_url: &str, root: &Path, question: AgentQuestionDto) -> QuestionContext {
        let toml = format!(
            "api_url = \"{api_url}\"\nworkspace_root = \"{}\"\n[driver]\nkind = \"stub\"\n",
            root.display()
        );
        let config = Arc::new(Config::from_toml_str(&toml, Some("tok".to_string())).unwrap());
        let log_store = LogStore::new(&config.logs).unwrap();
        QuestionContext {
            client: RemoterClient::new(api_url, "tok", None),
            config,
            driver: Arc::new(crate::driver::stub::StubDriver::new(0, false)),
            trigger: QuestionTrigger {
                question,
                task: task_summary(7, "review"),
            },
            project: ProjectRepoConfig {
                project_id: 1,
                repo_url: "https://example.invalid/repo.git".to_string(),
                base_branch: None,
                staging_auto_start: false,
                reference_repos: Vec::new(),
            },
            port_block: None,
            image_locks: crate::image::ImageLocks::default(),
            cancel: CancellationToken::new(),
            log_store,
        }
    }

    #[test]
    fn host_question_uses_the_same_kimi_home_as_main_runs() {
        let root = std::env::temp_dir().join(format!("remoter-question-kimi-home-{}", std::process::id()));
        let mut qc = test_context("http://127.0.0.1:9", &root, question(4, "pending", None));
        let toml = format!(
            "api_url = \"http://127.0.0.1:9\"\nworkspace_root = \"{}\"\n[execution]\nagent_home = \"{}\"\n[driver]\nkind = \"kimi-acp\"\n",
            root.display(),
            root.join("daemon-home").display()
        );
        qc.config = Arc::new(Config::from_toml_str(&toml, Some("tok".to_string())).unwrap());

        let env = question_env(&qc);
        let expected_home = root.join("daemon-home/.kimi-code").to_string_lossy().into_owned();
        assert_eq!(
            env.iter().find(|(key, _)| key == "KIMI_CODE_HOME"),
            Some(&("KIMI_CODE_HOME".to_string(), expected_home))
        );
        assert_eq!(env, run::host_run_env(&qc.config, 7, None));
    }

    #[tokio::test]
    async fn default_base_diff_includes_committed_pr_changes() {
        let root = std::env::temp_dir().join(format!("remoter-question-default-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let worktree = workspace::worktree_dir(&root, 1, 7);
        init_repo(&worktree);
        let origin = root.join("origin.git");
        let git = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&root, &["init", "--bare", "-q", origin.to_str().unwrap()]);
        git(&worktree, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&worktree, &["push", "-q", "origin", "HEAD:main"]);
        git(&worktree, &["remote", "set-head", "origin", "main"]);
        git(&worktree, &["checkout", "-q", "-b", "agent/task-7"]);
        std::fs::write(worktree.join("tracked.txt"), "committed after\n").unwrap();
        git(&worktree, &["add", "tracked.txt"]);
        git(
            &worktree,
            &[
                "-c",
                "user.email=test@remoter",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "PR change",
            ],
        );
        assert!(workspace::diff(&worktree, "HEAD").await.unwrap().is_empty());

        let qc = test_context("http://127.0.0.1:9", &root, question(4, "pending", None));
        let detail: TaskDetail = serde_json::from_value(serde_json::json!({
            "id":7,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f",
            "title":"seven","description":"d","taskStatus":"review"
        }))
        .unwrap();
        let artifacts = current_artifacts(&qc, &detail, &[]).await;
        assert!(artifacts.diff.contains("tracked.txt"), "{}", artifacts.diff);
        assert!(artifacts.diff.contains("+committed after"), "{}", artifacts.diff);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn stub_question_finishes_without_side_effect_calls_or_worktree_changes() {
        let root = std::env::temp_dir().join(format!("remoter-question-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let worktree = workspace::worktree_dir(&root, 1, 7);
        init_repo(&worktree);
        let before = WorktreeSnapshot::capture(&worktree).await.unwrap();
        let current = question_json(4, "running", Some(40));
        let detail = serde_json::json!({
            "id":7,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f",
            "title":"seven","description":"d","taskStatus":"review","report":"implementation report"
        });
        let runs = serde_json::json!([
            run_json(40, "question", "running", None),
            run_json(10, "implement", "succeeded", Some("source-session"))
        ]);
        let (url, requests) = spawn_backend(vec![
            (200, current.to_string()),
            (200, runs.to_string()),
            (200, runs.to_string()),
            (200, detail.to_string()),
            (200, serde_json::json!([current]).to_string()),
            (200, runs.to_string()),
            (
                200,
                run_json(40, "question", "succeeded", Some("stub-question")).to_string(),
            ),
        ])
        .await;
        let mut pending = question(4, "pending", None);
        pending.source_run_id = Some(10);
        execute(test_context(&url, &root, pending)).await;

        assert_eq!(before, WorktreeSnapshot::capture(&worktree).await.unwrap());
        let calls = requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 7, "{calls:?}");
        assert!(calls[0].starts_with("POST /api/v1/agent-questions/4/run "));
        assert_eq!(calls[1], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert_eq!(calls[2], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert!(calls[3].starts_with("GET /api/v1/tasks/7?include="));
        assert_eq!(calls[4], "GET /api/v1/tasks/7/agent-questions HTTP/1.1");
        assert_eq!(calls[5], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert_eq!(calls[6], "PATCH /api/v1/agent-runs/40 HTTP/1.1");
        assert!(
            calls
                .iter()
                .all(|c| !c.contains("/status") && !c.contains("/comments") && !c.contains("forge"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn duplicate_question_start_conflict_does_not_run_driver_or_finish_run() {
        let root = std::env::temp_dir().join(format!("remoter-question-duplicate-{}", std::process::id()));
        let (url, requests) = spawn_backend(vec![(409, "{}".to_string())]).await;
        execute(test_context(&url, &root, question(4, "pending", None))).await;
        let calls = requests.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("POST /api/v1/agent-questions/4/run "));
        let _ = std::fs::remove_dir_all(&root);
    }
}
