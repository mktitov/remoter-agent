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
const QUESTION_OWNERSHIP_DIR: &str = "question-ownership";
const START_PENDING: &str = "start-pending";

fn ownership_path(config: &Config, question_id: i32) -> PathBuf {
    config
        .workspace_root
        .join(QUESTION_OWNERSHIP_DIR)
        .join(format!("{question_id}.run"))
}

fn write_ownership(config: &Config, question_id: i32, run_id: Option<i32>) -> std::io::Result<()> {
    let path = ownership_path(config, question_id);
    let parent = path.parent().expect("ownership path has a parent");
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    let contents = run_id.map_or_else(|| START_PENDING.to_string(), |id| id.to_string());
    std::fs::write(&temporary, contents)?;
    std::fs::rename(temporary, path)
}

fn record_question_start(config: &Config, question_id: i32) -> std::io::Result<()> {
    write_ownership(config, question_id, None)
}

pub(crate) fn record_question_run(config: &Config, question_id: i32, run_id: i32) -> std::io::Result<()> {
    write_ownership(config, question_id, Some(run_id))
}

fn forget_question_run(config: &Config, question_id: i32) {
    let path = ownership_path(config, question_id);
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(question_id, path = %path.display(), error = %e, "question: could not remove ownership record");
    }
}

fn owns_question_run(config: &Config, question_id: i32, run_id: i32) -> bool {
    let path = ownership_path(config, question_id);
    match std::fs::read_to_string(&path) {
        Ok(contents) => contents.trim().parse::<i32>().ok() == Some(run_id),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!(question_id, path = %path.display(), error = %e, "question: could not read ownership record");
            false
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuestionTrigger {
    pub question: AgentQuestionDto,
    pub task: TaskSummary,
}

#[derive(Debug, Default)]
pub struct QuestionPoll {
    pub pending: Vec<QuestionTrigger>,
    pub cancellations: Vec<AgentQuestionDto>,
    pub running: Vec<AgentQuestionDto>,
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
                Vec::new()
            }
        };
        let cancellations = match client.agent_question_queue("cancel_requested").await {
            Ok(q) => q,
            Err(e) if unsupported_endpoint(&e) => {
                self.unsupported_backend = true;
                tracing::info!("agent questions are not supported by this backend; question polling disabled");
                return QuestionPoll::default();
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
                    running: Vec::new(),
                };
            }
        };
        let mut running = Vec::new();
        let mut task_ids: Vec<i32> = tasks
            .values()
            .filter(|task| matches!(task.task_status.as_str(), "plan_review" | "review"))
            .map(|task| task.id)
            .collect();
        task_ids.sort_unstable();
        for task_id in task_ids {
            let history = match client.task_agent_questions(task_id).await {
                Ok(history) => history,
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "question: could not read task history for reconciliation");
                    continue;
                }
            };
            let candidates: Vec<AgentQuestionDto> = history
                .into_iter()
                .filter(|question| question.status == "running" && question.run_id.is_some())
                .collect();
            if candidates.is_empty() {
                continue;
            }
            let runs = match client.list_runs(task_id).await {
                Ok(runs) => runs,
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "question: could not list runs for reconciliation");
                    continue;
                }
            };
            running.extend(candidates.into_iter().filter(|question| {
                runs.iter().any(|run| {
                    Some(run.id) == question.run_id && run.kind == QUESTION_RUN_KIND && run.status == "running"
                })
            }));
        }

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
        QuestionPoll {
            pending,
            cancellations,
            running,
        }
    }
}

pub async fn reconcile_orphan(client: &RemoterClient, config: &Config, question: &AgentQuestionDto) {
    let Some(run_id) = question.run_id else {
        return;
    };
    if !owns_question_run(config, question.id, run_id) {
        tracing::debug!(
            question_id = question.id,
            run_id,
            "question: skipping run owned by another daemon"
        );
        return;
    }
    let body = if question.status == "cancel_requested" {
        FinishRunBody::failed("cancelled by user".to_string())
    } else {
        FinishRunBody::failed("question run has no active daemon task".to_string())
    };
    match client.finish_run(run_id, &body).await {
        Ok(_) => forget_question_run(config, question.id),
        Err(e) => {
            tracing::warn!(
                question_id = question.id,
                run_id,
                error = %e,
                "question: orphan reconciliation failed; a later poll will retry"
            );
        }
    }
}

pub async fn reconcile_run(client: &RemoterClient, config: &Config, task_id: i32, run_id: i32) {
    let question = match client.task_agent_questions(task_id).await {
        Ok(questions) => questions.into_iter().find(|q| q.run_id == Some(run_id)),
        Err(e) => {
            tracing::warn!(task_id, run_id, error = %e, "question: could not resolve run ownership");
            None
        }
    };
    if let Some(question) = question {
        reconcile_orphan(client, config, &question).await;
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
    if let Err(e) = record_question_start(&qc.config, question_id) {
        tracing::warn!(task_id, question_id, error = %e, "question: could not persist run ownership; start deferred");
        return;
    }
    let ownership_config = qc.config.clone();
    let mut confirmed_run_id = None;
    let started = match qc
        .client
        .start_agent_question(&qc.trigger.question, |run_id| {
            confirmed_run_id = Some(run_id);
            if let Err(e) = record_question_run(&ownership_config, question_id, run_id) {
                tracing::warn!(task_id, question_id, run_id, error = %e,
                    "question: could not persist started run ID; recovery disabled for this attempt");
            }
        })
        .await
    {
        Ok(started) => started,
        Err(e) if e.is_conflict() => {
            forget_question_run(&qc.config, question_id);
            tracing::info!(task_id, question_id, "question already started; skipping duplicate");
            return;
        }
        Err(e) => {
            tracing::warn!(task_id, question_id, error = %e, "question: could not start run");
            if confirmed_run_id.is_none() && e.status.is_some_and(|status| status.is_client_error()) {
                forget_question_run(&qc.config, question_id);
                return;
            }
            if let Some(started) = recover_committed_start(&qc, question_id).await {
                finish_failure(&qc, &started, format!("question start resolution failed: {e}"), None).await;
            }
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
            match finish_run_with_retry(&qc, run_id, &body).await {
                Ok(()) => tracing::info!(task_id, question_id, run_id, "question run succeeded"),
                Err(e) => {
                    tracing::warn!(task_id, question_id, run_id, error = %e, "question: could not record answer")
                }
            }
        }
        Err(failure) => finish_failure(&qc, &started, failure.to_string(), Some(&failure)).await,
    }
}

async fn recover_committed_start(qc: &QuestionContext, question_id: i32) -> Option<StartedQuestion> {
    let task_id = qc.trigger.task.id;
    let question = qc
        .client
        .task_agent_questions(task_id)
        .await
        .ok()?
        .into_iter()
        .find(|q| {
            q.id == question_id && q.run_id.is_some() && matches!(q.status.as_str(), "running" | "cancel_requested")
        })?;
    if !owns_question_run(&qc.config, question_id, question.run_id?) {
        return None;
    }
    let runs = qc.client.list_runs(task_id).await.ok()?;
    let run = runs
        .iter()
        .find(|r| Some(r.id) == question.run_id && r.kind == QUESTION_RUN_KIND && r.status == "running")
        .cloned()?;
    let source_run = question
        .source_run_id
        .and_then(|source_id| runs.iter().find(|r| r.id == source_id).cloned());
    tracing::info!(
        task_id,
        question_id,
        run_id = run.id,
        "recovered committed question run after start resolution failure"
    );
    Some(StartedQuestion {
        question,
        run,
        source_run,
    })
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
    let containers = match run::start_container_runtime(
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
    };
    let exec = match &containers {
        Some(_) => run::exec_env(&qc.config, started.run.id, false),
        None => workspace::ExecEnv::host(false),
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
            None,
        )),
        result = tokio::time::timeout(timeout, qc.driver.run(spec)) => {
            result.unwrap_or_else(|_| Err(RunFailure::new(
                DriverError::Transient(format!("timeout after {} minutes", qc.config.run_timeout_minutes)),
                None,
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

async fn finish_run_with_retry(
    qc: &QuestionContext,
    run_id: i32,
    body: &FinishRunBody,
) -> Result<(), crate::client::ClientError> {
    let mut backoff = Duration::from_secs(qc.config.retry_backoff_secs.max(1));
    loop {
        match qc.client.finish_run(run_id, body).await {
            Ok(_) => {
                forget_question_run(&qc.config, qc.trigger.question.id);
                return Ok(());
            }
            Err(e) if e.is_transient() => {
                tracing::warn!(run_id, error = %e, backoff_secs = backoff.as_secs(),
                    "question: transient finish failure; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
            Err(e) => return Err(e),
        }
    }
}

async fn finish_failure(qc: &QuestionContext, started: &StartedQuestion, error: String, failure: Option<&RunFailure>) {
    let mut body = FinishRunBody::failed(error);
    if let Some(failure) = failure {
        body.session_id = failure.session_id.clone();
        body.input_tokens = failure.input_tokens;
        body.output_tokens = failure.output_tokens;
    }
    if let Err(e) = finish_run_with_retry(qc, started.run.id, &body).await {
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
    if let Err(e) = finish_run_with_retry(qc, started.run.id, &body).await {
        tracing::warn!(run_id = started.run.id, error = %e, "question: could not record cancellation");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    head: String,
    symbolic_head: Vec<u8>,
    status: Vec<u8>,
    diff: Vec<u8>,
    untracked: Vec<(Vec<u8>, [u8; 32])>,
}

impl WorktreeSnapshot {
    pub async fn capture(cwd: &Path) -> Result<Self, String> {
        use sha2::{Digest, Sha256};

        let head = git_output(cwd, &["rev-parse", "HEAD"]).await?;
        let symbolic_head = git_output(cwd, &["symbolic-ref", "--quiet", "HEAD"])
            .await
            .unwrap_or_default();
        let status = git_output(cwd, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?;
        let diff = git_output(cwd, &["diff", "--binary", "HEAD"]).await?;
        let names = git_output(cwd, &["ls-files", "--others", "-z"]).await?;
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
            symbolic_head,
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
        spawn_backend_impl(responses, false).await
    }

    async fn spawn_full_backend(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        spawn_backend_impl(responses, true).await
    }

    async fn spawn_backend_impl(
        responses: Vec<(u16, String)>,
        full_requests: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
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
                    let header_end = request.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
                    let content_length = header_end
                        .and_then(|end| {
                            String::from_utf8_lossy(&request[..end]).lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                        })
                        .unwrap_or(0);
                    if header_end.is_some_and(|end| request.len() >= end + content_length) {
                        break;
                    }
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let unsupported_queue = request.lines().next().is_some_and(|line| {
                    line.starts_with("GET /api/v1/agent-questions?assignee=me&status=")
                        && !line.starts_with("GET /api/v1/agent-questions?assignee=me&status=pending ")
                        && !line.starts_with("GET /api/v1/agent-questions?assignee=me&status=cancel_requested ")
                });
                let entry = if full_requests {
                    request
                } else {
                    request.lines().next().unwrap_or_default().to_string()
                };
                recorded.lock().unwrap().push(entry);
                let (status, body) = if unsupported_queue {
                    (400, "{}".to_string())
                } else {
                    (status, body)
                };
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
            (200, "[]".to_string()),
            (200, "[]".to_string()),
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

    #[tokio::test]
    async fn snapshot_detects_branch_change_at_the_same_commit() {
        let dir = std::env::temp_dir().join(format!("remoter-question-branch-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_repo(&dir);
        let before = WorktreeSnapshot::capture(&dir).await.unwrap();
        let status = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["checkout", "-q", "-b", "question-side-effect"])
            .status()
            .unwrap();
        assert!(status.success());
        let after = WorktreeSnapshot::capture(&dir).await.unwrap();
        assert_eq!(before.head, after.head);
        assert_ne!(before, after);
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

    #[derive(Default)]
    struct CaptureDriver {
        spec: Mutex<Option<RunSpec>>,
    }

    #[async_trait::async_trait]
    impl AgentDriver for CaptureDriver {
        async fn run(&self, spec: RunSpec) -> Result<RunOutcome, RunFailure> {
            self.spec.lock().unwrap().replace(spec);
            Ok(RunOutcome {
                session_id: Some("question-fork".to_string()),
                text: "answer".to_string(),
                input_tokens: None,
                output_tokens: None,
                model: None,
                thinking: None,
            })
        }
    }

    struct PendingDriver;

    #[async_trait::async_trait]
    impl AgentDriver for PendingDriver {
        async fn run(&self, _spec: RunSpec) -> Result<RunOutcome, RunFailure> {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    #[derive(Default)]
    struct GatedDriver {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl AgentDriver for GatedDriver {
        async fn run(&self, _spec: RunSpec) -> Result<RunOutcome, RunFailure> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(RunOutcome {
                session_id: Some("question-fork".to_string()),
                text: "answer from daemon A".to_string(),
                input_tokens: None,
                output_tokens: None,
                model: None,
                thinking: None,
            })
        }
    }

    #[tokio::test]
    async fn bwrap_question_run_uses_bwrap_exec_instead_of_host() {
        let root = std::env::temp_dir().join(format!("remoter-question-bwrap-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let worktree = workspace::worktree_dir(&root, 1, 7);
        init_repo(&worktree);
        let mut qc = test_context("http://127.0.0.1:9", &root, question(4, "pending", None));
        let toml = format!(
            "api_url = \"http://127.0.0.1:9\"\nworkspace_root = \"{}\"\n[execution]\nmode = \"bwrap\"\n[driver]\nkind = \"stub\"\n",
            root.display()
        );
        qc.config = Arc::new(Config::from_toml_str(&toml, Some("tok".to_string())).unwrap());
        let driver = Arc::new(CaptureDriver::default());
        qc.driver = driver.clone();
        let started = StartedQuestion {
            question: question(4, "running", Some(40)),
            run: run(40, QUESTION_RUN_KIND, None),
            source_run: None,
        };
        let prepared = PreparedPrompt {
            prompt: "question".to_string(),
            fallback_prompt: "fallback".to_string(),
            fork_session: None,
        };

        run_attempt(&qc, &started, &prepared, &worktree, &SessionLogger::noop())
            .await
            .unwrap();

        let spec = driver.spec.lock().unwrap().take().unwrap();
        assert!(matches!(&spec.exec, workspace::ExecEnv::Bwrap(_)), "{:?}", spec.exec);
        let _ = std::fs::remove_dir_all(&root);
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
            (500, "{}".to_string()),
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
        assert_eq!(calls.len(), 8, "{calls:?}");
        assert!(calls[0].starts_with("POST /api/v1/agent-questions/4/run "));
        assert_eq!(calls[1], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert_eq!(calls[2], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert!(calls[3].starts_with("GET /api/v1/tasks/7?include="));
        assert_eq!(calls[4], "GET /api/v1/tasks/7/agent-questions HTTP/1.1");
        assert_eq!(calls[5], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert_eq!(calls[6], "PATCH /api/v1/agent-runs/40 HTTP/1.1");
        assert_eq!(calls[7], "PATCH /api/v1/agent-runs/40 HTTP/1.1");
        assert!(
            calls
                .iter()
                .all(|c| !c.contains("/status") && !c.contains("/comments") && !c.contains("forge"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    async fn assert_terminal_patch_omits_source_session(cancel: bool) {
        let mode = if cancel { "cancel" } else { "timeout" };
        let root = std::env::temp_dir().join(format!("remoter-question-{mode}-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let worktree = workspace::worktree_dir(&root, 1, 7);
        init_repo(&worktree);
        let current = question_json(4, "running", Some(40));
        let detail = serde_json::json!({
            "id":7,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f",
            "title":"seven","description":"d","taskStatus":"review"
        });
        let runs = serde_json::json!([
            run_json(40, "question", "running", None),
            run_json(10, "implement", "succeeded", Some("source-session"))
        ]);
        let mut responses = vec![
            (200, current.to_string()),
            (200, runs.to_string()),
            (200, runs.to_string()),
            (200, detail.to_string()),
            (200, serde_json::json!([current]).to_string()),
            (200, runs.to_string()),
        ];
        if cancel {
            responses.push((
                200,
                serde_json::json!([question_json(4, "cancel_requested", Some(40))]).to_string(),
            ));
        }
        responses.push((200, run_json(40, "question", "failed", None).to_string()));
        let (url, requests) = spawn_full_backend(responses).await;
        let mut pending = question(4, "pending", None);
        pending.source_run_id = Some(10);
        let mut qc = test_context(&url, &root, pending);
        qc.driver = Arc::new(PendingDriver);
        if cancel {
            qc.cancel.cancel();
        } else {
            let toml = format!(
                "api_url = \"{url}\"\nworkspace_root = \"{}\"\nrun_timeout_minutes = 0\n[driver]\nkind = \"stub\"\n",
                root.display()
            );
            qc.config = Arc::new(Config::from_toml_str(&toml, Some("tok".to_string())).unwrap());
        }

        execute(qc).await;

        let calls = requests.lock().unwrap().clone();
        assert_eq!(calls.len(), if cancel { 8 } else { 7 }, "{calls:?}");
        let patch = calls.last().unwrap();
        assert!(patch.starts_with("PATCH /api/v1/agent-runs/40 "), "{patch}");
        let body = patch.split_once("\r\n\r\n").unwrap().1;
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["status"], "failed");
        assert!(json.get("sessionId").is_none(), "{json}");
        assert!(!body.contains("source-session"), "{body}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cancelled_question_patch_omits_source_session_id() {
        assert_terminal_patch_omits_source_session(true).await;
    }

    #[tokio::test]
    async fn timed_out_question_patch_omits_source_session_id() {
        assert_terminal_patch_omits_source_session(false).await;
    }

    #[tokio::test]
    async fn other_daemon_start_failure_and_poll_do_not_finish_active_question_and_owner_succeeds() {
        let root_a = std::env::temp_dir().join(format!("remoter-question-daemon-a-{}", std::process::id()));
        let root_b = std::env::temp_dir().join(format!("remoter-question-daemon-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root_a);
        let _ = std::fs::remove_dir_all(&root_b);
        init_repo(&workspace::worktree_dir(&root_a, 1, 7));
        let current = question_json(4, "running", Some(40));
        let runs = serde_json::json!([
            run_json(40, "question", "running", None),
            run_json(10, "implement", "succeeded", Some("source-session"))
        ]);
        let detail = serde_json::json!({
            "id":7,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f",
            "title":"seven","description":"d","taskStatus":"review"
        });
        let task = serde_json::json!([{
            "id":7,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f",
            "title":"seven","description":"d","taskStatus":"review","actionsTotal":0,
            "actionsCompleted":0,"timeSpent":0,"blocked":false
        }]);
        let (url, requests) = spawn_full_backend(vec![
            (200, current.to_string()),
            (200, runs.to_string()),
            (200, runs.to_string()),
            (200, detail.to_string()),
            (200, serde_json::json!([current]).to_string()),
            (200, runs.to_string()),
            (500, "{}".to_string()),
            (200, serde_json::json!([current]).to_string()),
            (200, "[]".to_string()),
            (200, "[]".to_string()),
            (200, task.to_string()),
            (200, serde_json::json!([current]).to_string()),
            (200, runs.to_string()),
            (
                200,
                run_json(40, "question", "succeeded", Some("question-fork")).to_string(),
            ),
        ])
        .await;
        let driver = Arc::new(GatedDriver::default());
        let mut qc_a = test_context(&url, &root_a, question(4, "pending", None));
        qc_a.driver = driver.clone();
        let config_a = qc_a.config.clone();
        let daemon_a = tokio::spawn(execute(qc_a));
        tokio::time::timeout(Duration::from_secs(5), driver.started.notified())
            .await
            .expect("daemon A must claim and start its driver");

        execute(test_context(&url, &root_b, question(4, "pending", None))).await;
        let daemon_b = test_context(&url, &root_b, question(4, "pending", None));
        assert!(!owns_question_run(&daemon_b.config, 4, 40));
        let poll = Questioner::new().poll(&daemon_b.client).await;
        assert_eq!(poll.running.len(), 1);
        reconcile_orphan(&daemon_b.client, &daemon_b.config, &poll.running[0]).await;
        let calls = requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 13, "{calls:?}");
        assert!(calls[6].starts_with("POST /api/v1/agent-questions/4/run "), "{calls:?}");
        assert_eq!(
            calls[7].lines().next().unwrap(),
            "GET /api/v1/tasks/7/agent-questions HTTP/1.1"
        );
        assert!(calls.iter().all(|call| !call.starts_with("PATCH ")), "{calls:?}");

        driver.release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), daemon_a)
            .await
            .expect("daemon A must finish")
            .unwrap();
        let calls = requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 14, "{calls:?}");
        let patch = calls.last().unwrap();
        assert!(patch.starts_with("PATCH /api/v1/agent-runs/40 "), "{patch}");
        let body: serde_json::Value = serde_json::from_str(patch.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["status"], "succeeded");
        assert_eq!(body["summary"], "answer from daemon A");
        assert!(!ownership_path(&config_a, 4).exists());
        let _ = std::fs::remove_dir_all(&root_a);
        let _ = std::fs::remove_dir_all(&root_b);
    }

    #[tokio::test]
    async fn committed_start_resolution_failure_finishes_recovered_run() {
        let root = std::env::temp_dir().join(format!("remoter-question-start-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let current = question_json(4, "running", Some(40));
        let runs = serde_json::json!([
            run_json(40, "question", "running", None),
            run_json(10, "implement", "succeeded", Some("source-session"))
        ]);
        let (url, requests) = spawn_backend(vec![
            (200, current.to_string()),
            (500, "{}".to_string()),
            (200, serde_json::json!([current]).to_string()),
            (200, runs.to_string()),
            (200, run_json(40, "question", "failed", None).to_string()),
        ])
        .await;
        let mut pending = question(4, "pending", None);
        pending.source_run_id = Some(10);
        execute(test_context(&url, &root, pending)).await;

        let calls = requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 5, "{calls:?}");
        assert!(calls[0].starts_with("POST /api/v1/agent-questions/4/run "));
        assert_eq!(calls[1], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert_eq!(calls[2], "GET /api/v1/tasks/7/agent-questions HTTP/1.1");
        assert_eq!(calls[3], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert_eq!(calls[4], "PATCH /api/v1/agent-runs/40 HTTP/1.1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn committed_start_is_recovered_after_restart_when_initial_recovery_fails() {
        let root = std::env::temp_dir().join(format!("remoter-question-start-repoll-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let current = question_json(4, "running", Some(40));
        let task = serde_json::json!([{
            "id":7,"projectId":1,"projectName":"p","featureId":1,"featureDescription":"f",
            "title":"seven","description":"d","taskStatus":"review","actionsTotal":0,
            "actionsCompleted":0,"timeSpent":0,"blocked":false
        }]);
        let running_runs = serde_json::json!([run_json(40, "question", "running", None)]);
        let (url, requests) = spawn_backend(vec![
            (200, current.to_string()),
            (500, "{}".to_string()),
            (500, "{}".to_string()),
            (200, "[]".to_string()),
            (200, "[]".to_string()),
            (200, task.to_string()),
            (200, serde_json::json!([current]).to_string()),
            (200, running_runs.to_string()),
            (200, run_json(40, "question", "failed", None).to_string()),
        ])
        .await;
        let mut pending = question(4, "pending", None);
        pending.source_run_id = Some(10);
        execute(test_context(&url, &root, pending)).await;

        let restarted = test_context(&url, &root, question(4, "pending", None));
        let client = restarted.client.clone();
        let poll = Questioner::new().poll(&client).await;
        assert!(poll.pending.is_empty());
        assert_eq!(poll.running.len(), 1);
        assert_eq!(poll.running[0].run_id, Some(40));
        reconcile_orphan(&client, &restarted.config, &poll.running[0]).await;

        let calls = requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 9, "{calls:?}");
        assert!(calls[0].starts_with("POST /api/v1/agent-questions/4/run "));
        assert_eq!(calls[1], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert_eq!(calls[2], "GET /api/v1/tasks/7/agent-questions HTTP/1.1");
        assert!(calls[3].starts_with("GET /api/v1/agent-questions?assignee=me&status=pending "));
        assert!(calls[4].starts_with("GET /api/v1/agent-questions?assignee=me&status=cancel_requested "));
        assert_eq!(calls[6], "GET /api/v1/tasks/7/agent-questions HTTP/1.1");
        assert_eq!(calls[7], "GET /api/v1/tasks/7/agent-runs HTTP/1.1");
        assert!(calls[8].starts_with("PATCH /api/v1/agent-runs/40 HTTP/1.1"));
        assert!(calls.iter().all(|call| !call.contains("status=running")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn snapshot_detects_ignored_file_creation_and_modification() {
        let dir = std::env::temp_dir().join(format!("remoter-question-ignored-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_repo(&dir);
        std::fs::write(dir.join(".gitignore"), ".cache/\n").unwrap();
        for args in [
            vec!["add", ".gitignore"],
            vec![
                "-c",
                "user.email=test@remoter",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "ignore cache",
            ],
        ] {
            let status = std::process::Command::new("git")
                .current_dir(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let before = WorktreeSnapshot::capture(&dir).await.unwrap();
        std::fs::create_dir(dir.join(".cache")).unwrap();
        std::fs::write(dir.join(".cache/answer.tmp"), "created\n").unwrap();
        let created = WorktreeSnapshot::capture(&dir).await.unwrap();
        assert_ne!(before, created);
        std::fs::write(dir.join(".cache/answer.tmp"), "modified\n").unwrap();
        assert_ne!(created, WorktreeSnapshot::capture(&dir).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
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
