//! The standalone review loop (#142): a human can request an agent review of
//! any ticket the daemon's agent owns while it sits in `review`
//! (`POST /tasks/{id}/agent-review` sets `agentReviewRequested` on the task
//! summaries). The review loop polls for that flag and launches a **review
//! run** on the ticket itself: the dev-agent reviews the ticket's own work —
//! the diff of the ticket's branch against the project's base branch (or the
//! parent ticket's branch when the ticket has a parent) plus the
//! implementation report — and records its verdict via the `set_task_review`
//! MCP tool on the same ticket.
//!
//! When the run finishes, the daemon acts on the verdict:
//! `changes_requested` → the review is posted to the ticket's thread and the
//! ticket bounces `review → implement`; `approve` → nothing further — the
//! verdict is already recorded as the review outcome and the ticket stays in
//! `review` for the human.
//!
//! Starting the run clears the request flag server-side (same transaction), so
//! the flag is the dedup of record across daemon restarts; the in-memory
//! backoff plus the "a review run row is still running" check cover the window
//! between trigger and run start. A 400 from the run-start endpoint means the
//! flag was already consumed (e.g. a human withdrew the request or another
//! daemon started the run) and the trigger is dropped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::{
    client::{ProjectRepoConfig, RemoterClient, TaskDetail, TaskSummary},
    config::Config,
    driver::{AgentDriver, DriverError, RunFailure, RunOutcome, RunSpec},
    logstore::LogStore,
    ports::PortBlock,
    run::{self, RunLogGuard},
    session_log::SessionLogger,
    supervise, workspace,
};

/// The run kind a review trigger starts for the ticket (`RunKind::Review`).
pub const REVIEW_RUN_KIND: &str = "review";

/// Do not re-trigger a review run for the same ticket within this window
/// (mirrors the supervise trigger dedup: repeated polls must not spam runs
/// while the start is deferred on capacity/ports).
const TRIGGER_BACKOFF: Duration = Duration::from_secs(300);

/// Ticket description quoted in the review prompt.
const MAX_DESC_BYTES: usize = 4 * 1024;
/// Implementation report quoted in the review prompt.
const MAX_REPORT_BYTES: usize = 16 * 1024;
/// The diff quoted in the review prompt — diffs can be huge, and the agent can
/// always run `git diff` itself when it needs more.
const MAX_DIFF_BYTES: usize = 64 * 1024;

/// A review run the review loop wants launched for a ticket.
pub struct ReviewTrigger {
    pub task: TaskSummary,
}

/// The review loop state, plugged into the daemon's poll cycle next to the
/// supervision loop.
#[derive(Default)]
pub struct Reviewer {
    /// task_id → last review-trigger attempt.
    last_trigger: HashMap<i32, Instant>,
    /// task_id → trigger attempts (an in-memory metric for tests and log
    /// correlation).
    trigger_count: HashMap<i32, u64>,
}

impl Reviewer {
    pub fn new() -> Self {
        Self::default()
    }

    /// One review cycle over every `review` ticket assigned to this daemon's
    /// agent: tickets with `agentReviewRequested` get a review-run trigger
    /// (deduped — see `trigger_warranted`). Individual failures are logged and
    /// skipped — a backend hiccup must not disturb the claim loop.
    pub async fn poll(&mut self, client: &RemoterClient) -> Vec<ReviewTrigger> {
        let mut triggers = Vec::new();
        let tasks = match client.my_tasks(Some("review")).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "review: could not list review tasks");
                return triggers;
            }
        };
        for task in tasks.into_iter().filter(|t| t.agent_review_requested) {
            if self.trigger_warranted(client, task.id).await {
                triggers.push(ReviewTrigger { task });
            }
        }
        triggers
    }

    /// Whether a review run should be triggered for the ticket: skip when one
    /// was attempted within the backoff window or a review run row is still
    /// `running` (the backend check survives daemon restarts). The backoff
    /// stamp is applied by [`Reviewer::mark_triggered`] only after the daemon
    /// actually launched the run — a capacity/port deferral must not eat the
    /// backoff window (same rule as the supervise trigger, review #110).
    async fn trigger_warranted(&mut self, client: &RemoterClient, task_id: i32) -> bool {
        if self
            .last_trigger
            .get(&task_id)
            .is_some_and(|t| t.elapsed() < TRIGGER_BACKOFF)
        {
            tracing::debug!(task_id, "review: trigger within backoff; skipping");
            return false;
        }
        let runs = match client.list_runs(task_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "review: could not list runs; trigger deferred");
                return false;
            }
        };
        if runs.iter().any(|r| r.kind == REVIEW_RUN_KIND && r.status == "running") {
            tracing::debug!(task_id, "review: review run already active; skipping");
            return false;
        }
        true
    }

    /// Records a successfully launched review trigger: stamps the backoff
    /// window and the attempt counter. Called by the daemon (which owns the
    /// capacity/port checks) only after the run was actually spawned.
    pub fn mark_triggered(&mut self, task_id: i32) {
        self.last_trigger.insert(task_id, Instant::now());
        *self.trigger_count.entry(task_id).or_default() += 1;
    }

    /// Trigger attempts per ticket (test/observability hook).
    pub fn trigger_count(&self, task_id: i32) -> u64 {
        self.trigger_count.get(&task_id).copied().unwrap_or(0)
    }

    /// Whether a trigger attempt was recorded within the backoff window.
    pub fn triggered(&self, task_id: i32) -> bool {
        self.last_trigger.contains_key(&task_id)
    }
}

// ── review run execution ──────────────────────────────────────────────────────

/// Everything the review run needs. Deliberately mirrors
/// `supervise::SuperviseContext` (same lifecycle, different prompt and
/// post-run actions).
pub struct ReviewContext {
    pub client: RemoterClient,
    pub config: Arc<Config>,
    pub driver: Arc<dyn AgentDriver>,
    pub task: TaskSummary,
    /// The ticket's project repo config; `None` when the project is not
    /// agent-managed (the run then reviews the report without git context).
    pub project: Option<ProjectRepoConfig>,
    /// The run's port block (`REMOTER_AGENT_PORT_BASE`, spec §5.8); released on
    /// drop. `None` in container mode — sidecars make the port contract
    /// obsolete (containers spec §3.4).
    pub port_block: Option<PortBlock>,
    /// Serializes per-project agent-image builds (container mode).
    pub image_locks: crate::image::ImageLocks,
    /// Human-cancellation signal (the ticket left `review`, spec §5.7).
    pub cancel: CancellationToken,
    pub log_store: LogStore,
}

/// Where a review attempt runs: the agent's working directory, whether devenv
/// services wrap the turn (spec §5.8), and the branch recorded on the run row.
struct AttemptEnv<'a> {
    cwd: &'a std::path::Path,
    devenv: bool,
    branch: &'a str,
}

/// The ticket's diff against the review base, or why it is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TicketDiff {
    /// `git diff <base>...<branch>`, capped to the prompt budget.
    Present(String),
    /// The ticket's branch exists neither locally nor on `origin`.
    BranchMissing,
    /// The diff base (parent branch or project base) exists nowhere.
    BaseMissing,
    /// Both refs exist but `git diff` failed.
    DiffFailed,
}

/// Git facts for the review prompt.
struct GitContext {
    branch: String,
    /// What the diff is computed against — the resolved ref (e.g.
    /// `agent/task-10-parent` or `origin/main`) …
    base: Option<String>,
    /// … and how the prompt describes it (the parent ticket's branch vs the
    /// project base branch).
    base_label: String,
    commits_ahead: Option<u32>,
    diff: TicketDiff,
}

/// Executes the review run to completion (success or recorded failure) and
/// then acts on the verdict. Like `supervise::execute`, this only logs — a
/// failed review run must not disturb the poll loop, and it never moves the
/// ticket (the human decides what happens next). A review run has a single
/// attempt: starting the run consumes the request flag server-side, so a retry
/// could not open a fresh run row anyway — a failure closes the row as failed
/// and the ticket stays in `review` for the human.
pub async fn execute(rc: ReviewContext) {
    let task_id = rc.task.id;
    // The agent works in the ticket's existing worktree when one exists, else
    // in the project clone (reviewing a diff needs no checkout); with neither,
    // the workspace root keeps the session spawnable and the prompt degrades.
    let cwd = review_cwd(&rc);
    let devenv = workspace::uses_devenv(&cwd);
    let branch = ticket_branch(&rc).await.unwrap_or_default();

    if rc.cancel.is_cancelled() {
        tracing::info!(task_id, "review run cancelled before start");
        return;
    }
    let run = match rc.client.start_run(task_id, REVIEW_RUN_KIND).await {
        Ok(r) => r,
        Err(e) if e.status == Some(reqwest::StatusCode::BAD_REQUEST) => {
            // The flag is gone: already consumed by another daemon or
            // withdrawn by a human — nothing to review.
            tracing::info!(task_id, "review: request flag already consumed; skipping");
            return;
        }
        Err(e) => {
            tracing::warn!(task_id, error = %e, "review: could not start run row; skipping");
            return;
        }
    };
    tracing::info!(task_id, run_id = run.id, "review run started");

    let (logger, _log_guard) = match rc.log_store.open_run(task_id, run.id) {
        Ok(run_logger) => {
            let guard = RunLogGuard::new(rc.log_store.clone(), task_id, run.id);
            (SessionLogger::new(run_logger), Some(guard))
        }
        Err(e) => {
            tracing::warn!(task_id, run_id = run.id, error = %e, "review: could not open ACP log file; logging disabled");
            (SessionLogger::noop(), None)
        }
    };

    let prompt = match build_prompt(&rc).await {
        Ok(p) => p,
        Err(e) => {
            run::finish_failed(
                &rc.client,
                task_id,
                run.id,
                format!("prompt context fetch failed: {e}"),
                None,
                None,
            )
            .await;
            return;
        }
    };

    // Posted after build_prompt so the daemon's own note doesn't land in this
    // run's Conversation section (mirroring `run::execute`).
    note(&rc.client, task_id, format!("review run #{} started", run.id)).await;

    let env = AttemptEnv {
        cwd: &cwd,
        devenv,
        branch: &branch,
    };
    let outcome = match run_attempt(&rc, run.id, &logger, &prompt, &env).await {
        Some(result) => match result {
            Ok(outcome) => outcome,
            Err(failure) => {
                run::finish_failed(
                    &rc.client,
                    task_id,
                    run.id,
                    failure.to_string(),
                    failure.input_tokens,
                    failure.output_tokens,
                )
                .await;
                tracing::warn!(task_id, run_id = run.id, error = %failure, "review: run failed");
                return;
            }
        },
        None => {
            // Cancelled mid-attempt (the ticket left `review`).
            run::finish_failed(
                &rc.client,
                task_id,
                run.id,
                "cancelled (human intervention or daemon shutdown)".to_string(),
                None,
                None,
            )
            .await;
            tracing::info!(task_id, run_id = run.id, "review run cancelled");
            return;
        }
    };

    if rc.cancel.is_cancelled() {
        run::finish_failed(
            &rc.client,
            task_id,
            run.id,
            "cancelled (human intervention or daemon shutdown)".to_string(),
            None,
            None,
        )
        .await;
        return;
    }

    // Close the run row before acting on the verdict: unlike supervise (where
    // the run lives on the parent), this run lives on the ticket itself, so
    // the `review → implement` bounce would make housekeeping cancel the
    // still-open row mid-flight and record a successful run as failed.
    let mut body = crate::client::FinishRunBody::succeeded();
    body.session_id = outcome.session_id.clone();
    body.branch = if branch.is_empty() { None } else { Some(branch.clone()) };
    body.summary = Some(outcome.text.clone());
    body.input_tokens = outcome.input_tokens;
    body.output_tokens = outcome.output_tokens;
    body.model = outcome.model.clone();
    body.thinking = outcome.thinking.clone();
    run::finish_reporting(&rc.client, run.id, &body).await;

    process_verdict(&rc, run.created_at).await;
    note(&rc.client, task_id, format!("review run #{} finished", run.id)).await;
    tracing::info!(task_id, run_id = run.id, "review run succeeded");
}

/// Working directory for the review session (see `execute`).
fn review_cwd(rc: &ReviewContext) -> PathBuf {
    let root = &rc.config.workspace_root;
    if let Some(project) = &rc.project {
        let wt = workspace::worktree_dir(root, project.project_id, rc.task.id);
        if wt.exists() {
            return wt;
        }
        let repo = workspace::repo_dir(root, project.project_id);
        if repo.exists() {
            return repo;
        }
    }
    root.clone()
}

/// The ticket's branch: the worktree's branch when one exists, else the name
/// recomputed from the title (same rule as `run::execute`).
async fn ticket_branch(rc: &ReviewContext) -> Option<String> {
    let project = rc.project.as_ref()?;
    let root = &rc.config.workspace_root;
    Some(
        workspace::existing_branch(root, project.project_id, rc.task.id)
            .await
            .unwrap_or_else(|| workspace::branch_name(rc.task.id, &rc.task.title)),
    )
}

/// The env every review turn (and its services/sidecars) gets — the same
/// contract as claim runs (spec §5.8 + containers spec §3.4).
fn review_env(rc: &ReviewContext) -> Vec<(String, String)> {
    if rc.config.execution.is_container() {
        return crate::container::container_env(rc.task.id);
    }
    let mut env = vec![
        ("CI".to_string(), "1".to_string()),
        ("REMOTER_AGENT_TASK_ID".to_string(), rc.task.id.to_string()),
    ];
    if let Some(block) = &rc.port_block {
        env.push(("REMOTER_AGENT_PORT_BASE".to_string(), block.base().to_string()));
    }
    env
}

/// One driver turn with the cancellation/timeout envelope: devenv services up
/// around the turn, always down after. `None` = cancelled mid-attempt.
async fn run_attempt(
    rc: &ReviewContext,
    run_id: i32,
    logger: &SessionLogger,
    prompt: &str,
    env: &AttemptEnv<'_>,
) -> Option<Result<RunOutcome, RunFailure>> {
    let env_vars = review_env(rc);
    // Container mode: the review turn runs in a container like any other run —
    // no silent fallback to host. A ticket whose project is not agent-managed
    // has no repo to build an image from — it keeps the host path (its review
    // is report-only anyway).
    let containers = match &rc.project {
        Some(project) => {
            match run::start_container_runtime(
                &rc.config,
                &rc.image_locks,
                project,
                run_id,
                env.cwd,
                env.devenv,
                &env_vars,
            )
            .await
            {
                Ok(c) => c,
                Err(f) => return Some(Err(f)),
            }
        }
        None => None,
    };
    if containers.is_none()
        && env.devenv
        && let Err(e) = workspace::services_up(env.cwd, &env_vars).await
    {
        return Some(Err(RunFailure::new(
            DriverError::Transient(format!("devenv services up: {e}")),
            None,
        )));
    }
    let exec = match &containers {
        Some(_) => run::exec_env(&rc.config, run_id, env.devenv),
        None => workspace::ExecEnv::host(env.devenv),
    };
    let spec = RunSpec {
        task_id: rc.task.id,
        run_id,
        cwd: env.cwd.to_path_buf(),
        prompt: prompt.to_string(),
        kind: REVIEW_RUN_KIND,
        resume_session: None,
        branch: env.branch.to_string(),
        env: env_vars,
        exec,
        config_options: rc.config.driver.config_options_for(REVIEW_RUN_KIND),
        logger: logger.clone(),
    };
    let timeout = Duration::from_secs(rc.config.run_timeout_minutes * 60);
    let result = tokio::select! {
        _ = rc.cancel.cancelled() => None,
        r = tokio::time::timeout(timeout, rc.driver.run(spec)) => Some(r),
    };
    if containers.is_none() && env.devenv {
        // Best-effort, must not mask the turn's real outcome (spec §5.8).
        workspace::services_down(env.cwd, &review_env(rc)).await;
    }
    result.map(|r| {
        r.unwrap_or_else(|_| {
            Err(RunFailure::new(
                DriverError::Transient(format!("timeout after {} minutes", rc.config.run_timeout_minutes)),
                None,
            ))
        })
    })
}

/// Best-effort daemon note (spec §5.6); failures never affect the run.
async fn note(client: &RemoterClient, task_id: i32, body: String) {
    run::note(client, task_id, body).await
}

// ── prompt ────────────────────────────────────────────────────────────────────

/// Fetches the ticket detail (and its parent's when linked), computes the git
/// context, and renders the prompt.
async fn build_prompt(rc: &ReviewContext) -> Result<String, crate::client::ClientError> {
    let detail = rc.client.task_detail(rc.task.id).await?;
    // From the ticket's perspective the link to its parent carries the
    // "subtask" relation (the backend resolves relations from the viewing
    // task's side — same rule as `run::child_link_ids`).
    let parent_id = detail
        .links
        .as_deref()
        .and_then(|links| links.iter().find(|l| l.relation == "subtask").map(|l| l.task_id));
    let parent = match parent_id {
        Some(id) => match rc.client.task_detail(id).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(task_id = rc.task.id, parent_id = id, error = %e, "review: could not fetch parent detail; diff base falls back to the project base");
                None
            }
        },
        None => None,
    };
    let git = git_context(rc, parent.as_ref()).await;
    Ok(render_prompt(&rc.task, &detail, git.as_ref()))
}

/// Git facts for the prompt: the ticket's branch and its diff against the
/// review base — the parent ticket's branch when the ticket has a parent,
/// otherwise the project's base branch (or the remote default branch).
/// `None` when the project is not agent-managed or has no local clone yet.
async fn git_context(rc: &ReviewContext, parent: Option<&TaskDetail>) -> Option<GitContext> {
    let project = rc.project.as_ref()?;
    let root = &rc.config.workspace_root;
    let repo = workspace::repo_dir(root, project.project_id);
    if !repo.exists() {
        return None;
    }
    let branch = ticket_branch(rc).await?;
    let (base_name, base_label) = match parent {
        Some(p) => (
            workspace::existing_branch(root, project.project_id, p.id)
                .await
                .unwrap_or_else(|| workspace::branch_name(p.id, &p.title)),
            format!("the parent ticket #{}'s branch", p.id),
        ),
        None => match project.base_branch.clone() {
            Some(base) => (base, "the project base branch".to_string()),
            None => match workspace::remote_default_branch(&repo).await {
                Ok(default) => (default, "the project base branch".to_string()),
                Err(e) => {
                    tracing::warn!(error = %e, "review: could not resolve the remote default branch");
                    return Some(GitContext {
                        branch,
                        base: None,
                        base_label: "the project base branch".to_string(),
                        commits_ahead: None,
                        diff: TicketDiff::BaseMissing,
                    });
                }
            },
        },
    };
    Some(collect_git_context(&repo, &branch, &base_name, &base_label).await)
}

/// Pure git-ref facts for the prompt, split out from `git_context` (which
/// resolves branch names from the workspace) so tests can run it against
/// scratch repositories. Both refs get the same self-heal as the supervise
/// diff base: a ref missing locally is fetched best-effort and used as
/// `origin/<ref>` when only a remote copy survived.
async fn collect_git_context(repo: &std::path::Path, branch: &str, base_name: &str, base_label: &str) -> GitContext {
    let base = resolve_ref(repo, base_name).await;
    let branch_ref = resolve_ref(repo, branch).await;
    let (commits_ahead, diff) = match (&base, &branch_ref) {
        (None, _) => (None, TicketDiff::BaseMissing),
        (Some(_), None) => (None, TicketDiff::BranchMissing),
        (Some(base), Some(branch_ref)) => {
            let ahead = workspace::commit_count(repo, &format!("{base}..{branch_ref}"))
                .await
                .ok();
            let diff = match workspace::diff(repo, &format!("{base}...{branch_ref}")).await {
                Ok(d) => TicketDiff::Present(run::truncate_with_marker(&d, MAX_DIFF_BYTES)),
                Err(e) => {
                    tracing::warn!(error = %e, "review: could not compute the ticket diff");
                    TicketDiff::DiffFailed
                }
            };
            (ahead, diff)
        }
    };
    GitContext {
        branch: branch.to_string(),
        base,
        base_label: base_label.to_string(),
        commits_ahead,
        diff,
    }
}

/// A local ref when it exists, else `origin/<name>` after a best-effort fetch
/// (a wiped/recreated workspace keeps only the remote-tracking refs); `None`
/// when the ref exists nowhere.
async fn resolve_ref(repo: &std::path::Path, name: &str) -> Option<String> {
    if workspace::local_branch_exists(repo, name).await {
        return Some(name.to_string());
    }
    let _ = workspace::fetch_branch(repo, name).await;
    if workspace::remote_tracking_branch_exists(repo, name).await {
        return Some(format!("origin/{name}"));
    }
    None
}

/// Review-run instructions (#142): the dev-agent reviews THIS ticket's own
/// work — the diff against the review base plus the implementation report —
/// and records exactly one verdict on the same ticket. Module-scope so tests
/// can pin the contract.
const REVIEW_INSTRUCTIONS: &str = "## Instructions\nA human asked you to review the work of the ticket above — your own \
     ticket's implementation, produced by an earlier agent run. Study the diff against the review \
     base and the implementation report, then record your verdict with the `set_task_review` MCP \
     tool — exactly one call, on THIS same ticket: `set_task_review(taskId, markdown, verdict)`.\n\n\
     Verdicts:\n\
     - `approve` — the diff fully implements the ticket, the report explains what was done and how \
     to test/deploy it, and the work is complete. The verdict is recorded as the review outcome; \
     the ticket stays in `review` for the human.\n\
     - `changes_requested` — anything is missing, wrong, untested, or the report is absent. The \
     markdown MUST list concrete numbered findings the implementer can act on: the daemon posts it \
     to the ticket's thread and returns the ticket to `implement`.\n\n\
     Rules:\n\
     - READ-ONLY: do not create, edit, or delete files and do not commit anything — you review, \
     the daemon acts on the verdict. Never end your turn with background tasks still running — \
     your turn's end is final: the daemon shuts the agent down and pending tasks are killed, their \
     completion never arrives.\n\
     - Approve only what you would merge into the review base yourself.\n\n";

/// Pure prompt assembly, split out for tests. `git` is `None` when no local
/// clone exists — the prompt then degrades to a report-only review.
fn render_prompt(task: &TaskSummary, detail: &TaskDetail, git: Option<&GitContext>) -> String {
    let mut p = String::new();
    p.push_str("## Ticket\n");
    p.push_str(&format!("Ticket #{}\n\n", task.id));
    p.push_str(&format!("{}\n\n", task.title));
    p.push_str(&format!(
        "{}\n\n",
        run::truncate_with_marker(&task.description, MAX_DESC_BYTES)
    ));
    p.push_str(&format!(
        "Project: {} — Feature: {}\n\n",
        task.project_name, task.feature_description
    ));
    supervise::conversation_section(&mut p, detail.comments.as_deref().unwrap_or(&[]));

    p.push_str("## Work under review\n\n");
    match git {
        None => p.push_str(
            "_No local clone of the project repository — review the implementation report and \
             thread instead of a diff._\n\n",
        ),
        Some(g) => {
            let ahead = g
                .commits_ahead
                .map(|n| format!("{n} commit(s)"))
                .unwrap_or_else(|| "unknown".to_string());
            match (&g.diff, &g.base) {
                (TicketDiff::Present(diff), Some(base)) => {
                    p.push_str(&format!(
                        "Branch: `{}` ({} ahead of `{}`, {})\n\n",
                        g.branch, ahead, base, g.base_label
                    ));
                    p.push_str(&format!("### Diff (`{base}`...`{}`)\n\n", g.branch));
                    p.push_str(&format!("```diff\n{diff}\n```\n\n"));
                }
                (TicketDiff::BranchMissing, _) => p.push_str(&format!(
                    "_The ticket's branch `{}` was not found locally or on origin — the diff is \
                     unavailable; review the report and thread._\n\n",
                    g.branch
                )),
                (TicketDiff::BaseMissing, _) => p.push_str(&format!(
                    "_The review base ({}) was not found locally or on origin — there is no diff \
                     base; review the report and thread._\n\n",
                    g.base_label
                )),
                (TicketDiff::DiffFailed, base) => p.push_str(&format!(
                    "_The diff against `{}` could not be computed — review the report and thread._\n\n",
                    base.as_deref().unwrap_or(&g.base_label)
                )),
                (TicketDiff::Present(_), None) => {
                    unreachable!("a diff cannot exist without a diff base")
                }
            }
        }
    }

    p.push_str("### Implementation report\n\n");
    match detail.report.as_deref().filter(|r| !r.trim().is_empty()) {
        Some(report) => p.push_str(&format!("{}\n\n", run::truncate_with_marker(report, MAX_REPORT_BYTES))),
        None => {
            p.push_str("_The ticket has no implementation report — that alone is worth a `changes_requested`._\n\n")
        }
    }

    // Language policy (same as the other run kinds): reasoning in English is
    // cheaper and the strongest for the model, but everything the human reads
    // mirrors the ticket's language.
    p.push_str(
        "## Language\nThink and reason in English — it is more token-efficient. Write everything the \
         human reads in the ticket's language: the review and comments. Code, commit messages, and \
         tool calls stay in English.\n\n",
    );
    p.push_str(REVIEW_INSTRUCTIONS);
    p
}

// ── verdict ───────────────────────────────────────────────────────────────────

/// Acts on the review verdict the agent wrote during the run: bounces
/// `changes_requested` to `implement` with the review on the thread; `approve`
/// needs no action — the verdict is already recorded as the review outcome and
/// the ticket stays in `review` for the human.
///
/// A verdict counts only when it was written at or after this run's start: the
/// outcome row survives status changes, so without the freshness check a
/// crashed run would re-apply the verdict of a previous review cycle (same
/// rule as the supervise run, review #110).
async fn process_verdict(rc: &ReviewContext, run_started_at: Option<chrono::DateTime<chrono::Utc>>) {
    let task_id = rc.task.id;
    let detail = match rc.client.task_detail(task_id).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "review: could not fetch ticket for verdict processing");
            return;
        }
    };
    // The ticket moved under us (a human took it out of `review`) — the human
    // owns the status now, so the verdict is not applied.
    if detail.task_status != "review" {
        tracing::info!(task_id, "review: ticket left `review`; verdict not applied");
        return;
    }
    let verdict_fresh = match (&detail.review_outcome_updated_at, run_started_at) {
        (Some(written_at), Some(started_at)) => written_at >= &started_at,
        // Unknown timestamps (an older backend that does not serialize them):
        // keep the legacy behavior and act on the verdict.
        _ => true,
    };
    match detail.review_verdict.as_deref() {
        Some(verdict) if !verdict_fresh => {
            note(
                &rc.client,
                task_id,
                format!(
                    "review run: the ticket still carries the `{verdict}` verdict from a previous \
                     review cycle — not re-applied"
                ),
            )
            .await;
        }
        Some("approve") => {
            tracing::info!(
                task_id,
                "review: approved — verdict recorded; ticket stays in review for the human"
            );
        }
        Some("changes_requested") => bounce(rc, &detail).await,
        _ => {
            note(
                &rc.client,
                task_id,
                "review run: no review verdict was written — left in review for a human".to_string(),
            )
            .await;
        }
    }
}

/// `changes_requested`: return the ticket to `implement` (a regular agent
/// transition — the daemon is the assignee), then post the review to the
/// ticket's thread (the bounced agent reads its prompt from the thread). The
/// note reflects the actual outcome: it is only posted once the status move
/// landed (same rule as `supervise::bounce_child`, review #110).
async fn bounce(rc: &ReviewContext, detail: &TaskDetail) {
    let body = detail
        .review
        .as_deref()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or("(agent review requested changes but wrote no review body)");
    match rc.client.set_task_status(detail.id, "implement").await {
        Ok(()) => {
            note(
                &rc.client,
                detail.id,
                format!("## Agent review — changes requested\n\n{body}\n\n— bounced to implement by the agent review."),
            )
            .await;
            tracing::info!(task_id = detail.id, "review: changes requested → implement");
        }
        Err(e) if e.is_conflict() || e.is_forbidden() => {
            tracing::debug!(task_id = detail.id, error = %e, "review: ticket moved under us; bounce skipped");
        }
        Err(e) => {
            note(
                &rc.client,
                detail.id,
                format!("## Agent review — changes requested\n\n{body}\n\n— the agent review tried to return the ticket to `implement`, but the status write failed ({e})."),
            )
            .await;
            tracing::warn!(task_id = detail.id, error = %e, "review: bounce failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::CommentDto;

    fn summary() -> TaskSummary {
        TaskSummary {
            id: 10,
            project_id: 1,
            project_name: "proj".into(),
            feature_id: 1,
            feature_description: "feat".into(),
            title: "ticket".into(),
            description: "ticket body".into(),
            task_status: "review".into(),
            task_priority: None,
            actions_total: 0,
            actions_completed: 0,
            time_spent: 0,
            blocked: false,
            agent_review_requested: true,
        }
    }

    fn detail(id: i32, status: &str) -> TaskDetail {
        TaskDetail {
            id,
            project_id: 1,
            project_name: "proj".into(),
            feature_id: 1,
            feature_description: "feat".into(),
            title: format!("ticket {id}"),
            description: String::new(),
            task_status: status.into(),
            task_priority: None,
            blocked: false,
            assignee_id: Some(7),
            assignee_name: None,
            actions: Vec::new(),
            runs: None,
            comments: None,
            attachments: None,
            links: None,
            questions: None,
            report: None,
            review: None,
            review_outcome_id: None,
            review_outcome_updated_at: None,
            review_verdict: None,
            completed_at: None,
            supervision_enabled: true,
        }
    }

    fn git_context() -> GitContext {
        GitContext {
            branch: "agent/task-10-ticket".into(),
            base: Some("origin/main".into()),
            base_label: "the project base branch".into(),
            commits_ahead: Some(2),
            diff: TicketDiff::Present("diff --git a/x b/x".into()),
        }
    }

    /// The review prompt carries the ticket, its branch, the diff against the
    /// review base, the implementation report, and the verdict instructions —
    /// with set_task_review targeting THIS same ticket.
    #[test]
    fn render_prompt_carries_diff_report_and_verdict_instructions() {
        let mut d = detail(10, "review");
        d.report = Some("did the thing".into());

        let p = render_prompt(&summary(), &d, Some(&git_context()));

        assert!(
            p.starts_with("## Ticket\nTicket #10\n\nticket\n\nticket body\n\n"),
            "{p}"
        );
        assert!(
            p.contains("Branch: `agent/task-10-ticket` (2 commit(s) ahead of `origin/main`, the project base branch)"),
            "{p}"
        );
        assert!(p.contains("### Diff (`origin/main`...`agent/task-10-ticket`)"), "{p}");
        assert!(p.contains("diff --git a/x b/x"), "{p}");
        assert!(p.contains("### Implementation report"), "{p}");
        assert!(p.contains("did the thing"), "{p}");
        assert!(p.contains("`set_task_review(taskId, markdown, verdict)`"), "{p}");
        assert!(p.contains("on THIS same ticket"), "{p}");
        assert!(p.contains("changes_requested"), "{p}");
        assert!(p.contains("stays in `review` for the human"), "{p}");
        assert!(p.contains("READ-ONLY"), "{p}");
        assert!(p.contains("## Language\n"), "{p}");
    }

    /// A ticket without an implementation report is called out — a missing
    /// report alone justifies a bounce.
    #[test]
    fn render_prompt_flags_missing_report() {
        let p = render_prompt(&summary(), &detail(10, "review"), None);
        assert!(p.contains("no implementation report"), "{p}");
        assert!(p.contains("changes_requested"), "{p}");
    }

    /// Without a local clone the prompt degrades to a report-only review.
    #[test]
    fn render_prompt_without_git_context() {
        let p = render_prompt(&summary(), &detail(10, "review"), None);
        assert!(p.contains("No local clone of the project repository"), "{p}");
        assert!(!p.contains("### Diff"), "{p}");
    }

    /// A missing ticket branch or diff base gets an explicit note, not a
    /// silent omission.
    #[test]
    fn render_prompt_missing_refs_are_named() {
        let mut g = git_context();
        g.diff = TicketDiff::BranchMissing;
        g.base = Some("origin/main".into());
        let p = render_prompt(&summary(), &detail(10, "review"), Some(&g));
        assert!(
            p.contains("The ticket's branch `agent/task-10-ticket` was not found"),
            "{p}"
        );

        let mut g = git_context();
        g.diff = TicketDiff::BaseMissing;
        g.base = None;
        g.base_label = "the parent ticket #10's branch".into();
        let p = render_prompt(&summary(), &detail(11, "review"), Some(&g));
        assert!(
            p.contains("The review base (the parent ticket #10's branch) was not found"),
            "{p}"
        );

        let mut g = git_context();
        g.diff = TicketDiff::DiffFailed;
        let p = render_prompt(&summary(), &detail(10, "review"), Some(&g));
        assert!(
            p.contains("The diff against `origin/main` could not be computed"),
            "{p}"
        );
    }

    /// The conversation is quoted (bounded) — human steering belongs in the
    /// reviewer's context.
    #[test]
    fn render_prompt_quotes_conversation() {
        let mut d = detail(10, "review");
        d.comments = Some(vec![CommentDto {
            id: 1,
            task_id: 10,
            author_id: 1,
            body: "please double-check the migration".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }]);
        let p = render_prompt(&summary(), &d, None);
        assert!(p.contains("## Conversation\n"), "{p}");
        assert!(p.contains("- please double-check the migration"), "{p}");
    }

    // ── collect_git_context (scratch git repos) ──────────────────────────

    /// A scratch git repo with one commit on `master`, unique per test name +
    /// process (same pattern as the supervise.rs tests).
    fn scratch_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("remoter-review-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_in(&dir, &["init", "-b", "master"]);
        git_in(&dir, &["commit", "--allow-empty", "-m", "init"]);
        dir
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with the ticket branch ahead of `master` by a single commit
    /// adding `work.txt`.
    fn scratch_repo_with_branch(tag: &str) -> PathBuf {
        let repo = scratch_repo(tag);
        git_in(&repo, &["checkout", "-b", "agent/task-10-ticket"]);
        std::fs::write(repo.join("work.txt"), "work").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "ticket work"]);
        repo
    }

    #[tokio::test]
    async fn collect_git_context_diffs_against_the_base() {
        let repo = scratch_repo_with_branch("base-diff");
        let ctx = collect_git_context(&repo, "agent/task-10-ticket", "master", "the project base branch").await;
        assert_eq!(ctx.base.as_deref(), Some("master"));
        assert_eq!(ctx.commits_ahead, Some(1));
        match &ctx.diff {
            TicketDiff::Present(d) => assert!(d.contains("work.txt"), "{d}"),
            other => panic!("expected a present diff, got {other:?}"),
        }
    }

    /// The base survives only as a remote-tracking ref — the diff base falls
    /// back to `origin/<base>` and the diff is still computed.
    #[tokio::test]
    async fn collect_git_context_falls_back_to_origin_base() {
        let repo = scratch_repo_with_branch("origin-base");
        git_in(&repo, &["update-ref", "refs/remotes/origin/master", "master"]);
        git_in(&repo, &["branch", "-D", "master"]);
        let ctx = collect_git_context(&repo, "agent/task-10-ticket", "master", "the project base branch").await;
        assert_eq!(ctx.base.as_deref(), Some("origin/master"));
        match &ctx.diff {
            TicketDiff::Present(d) => assert!(d.contains("work.txt"), "{d}"),
            other => panic!("expected a present diff, got {other:?}"),
        }
    }

    /// A base that exists nowhere degrades to `BaseMissing`.
    #[tokio::test]
    async fn collect_git_context_missing_base() {
        let repo = scratch_repo_with_branch("missing-base");
        let ctx = collect_git_context(&repo, "agent/task-10-ticket", "no-such-base", "the project base branch").await;
        assert_eq!(ctx.base, None);
        assert_eq!(ctx.diff, TicketDiff::BaseMissing);
        assert_eq!(ctx.commits_ahead, None);
    }

    /// A ticket branch that exists nowhere degrades to `BranchMissing`.
    #[tokio::test]
    async fn collect_git_context_missing_branch() {
        let repo = scratch_repo_with_branch("missing-branch");
        let ctx = collect_git_context(&repo, "agent/task-99-gone", "master", "the project base branch").await;
        assert_eq!(ctx.diff, TicketDiff::BranchMissing);
    }

    // ── trigger / dedup / verdict (mock backend) ─────────────────────────

    /// Minimal HTTP/1.1 mock backend (same shape as the supervise.rs tests):
    /// serves the queued `(status, body)` responses in request order, one
    /// connection each, and counts accepted requests.
    async fn spawn_counting_backend(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
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
                let response = format!(
                    "HTTP/1.1 {status} Reason\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{addr}"), hits)
    }

    const REVIEW_TASKS_JSON: &str = r#"[{
        "id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
        "featureDescription": "feat", "title": "ticket", "description": "body",
        "taskStatus": "review", "actionsTotal": 0, "actionsCompleted": 0,
        "timeSpent": 0, "agentReviewRequested": true
    }]"#;

    /// A ticket with `agentReviewRequested` triggers a review run; one
    /// without the flag does not (no run-list fetch either).
    #[tokio::test]
    async fn trigger_fires_only_on_agent_review_requested() {
        let (url, hits) = spawn_counting_backend(vec![
            (200, REVIEW_TASKS_JSON),
            (200, "[]"),
            // Second poll: the flag is absent → no trigger, no run list.
            (
                200,
                r#"[{"id": 11, "projectId": 1, "projectName": "proj", "featureId": 1,
                    "featureDescription": "feat", "title": "other", "description": "body",
                    "taskStatus": "review", "actionsTotal": 0, "actionsCompleted": 0, "timeSpent": 0}]"#,
            ),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let mut reviewer = Reviewer::new();

        let triggers = reviewer.poll(&client).await;
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].task.id, 10);

        let triggers = reviewer.poll(&client).await;
        assert!(triggers.is_empty(), "no flag → no trigger");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "task list + run list + task list (no run list for the unflagged ticket)"
        );
    }

    /// Dedup: a second poll within the backoff window (after `mark_triggered`)
    /// must not trigger again, and a still-`running` review run row blocks
    /// re-triggering even without a local backoff stamp.
    #[tokio::test]
    async fn trigger_dedup_backoff_and_running_row() {
        let (url, _hits) = spawn_counting_backend(vec![
            // Poll 1: flag set, no runs → trigger.
            (200, REVIEW_TASKS_JSON),
            (200, "[]"),
            // Poll 2: backoff stamp blocks before any run-list fetch.
            (200, REVIEW_TASKS_JSON),
            // Poll 3 (fresh reviewer, no stamp): a running review row blocks.
            (200, REVIEW_TASKS_JSON),
            (
                200,
                r#"[{"id":5,"task_id":10,"agent_user_id":7,"kind":"review","status":"running","session_id":null,"branch":null,"plan":null,"summary":null,"error":null,"pr_url":null,"pr_error":null,"input_tokens":null,"output_tokens":null,"attempt":1}]"#,
            ),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let mut reviewer = Reviewer::new();

        assert_eq!(reviewer.poll(&client).await.len(), 1);
        reviewer.mark_triggered(10);
        assert!(reviewer.triggered(10));
        assert_eq!(reviewer.trigger_count(10), 1);
        assert!(reviewer.poll(&client).await.is_empty(), "backoff window must dedup");

        let mut fresh = Reviewer::new();
        assert!(fresh.poll(&client).await.is_empty(), "a running review row must dedup");
    }

    /// A 400 from the run-start endpoint means the request flag was already
    /// consumed — the run is skipped without an error and without retries.
    #[tokio::test]
    async fn start_run_400_means_already_consumed() {
        let (url, hits) = spawn_counting_backend(vec![(400, r#"{"error":"flag not set"}"#)]).await;
        let client = RemoterClient::new(&url, "tok", None);
        let rc = test_context(client);
        execute(rc).await;
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the consumed flag stops the run after the start attempt"
        );
    }

    /// `changes_requested` bounces the ticket to `implement` and posts the
    /// review body to the thread; `approve` leaves the ticket untouched.
    #[tokio::test]
    async fn verdict_changes_requested_bounces_approve_does_nothing() {
        // changes_requested: detail fetch → status PATCH → comment POST.
        let (url, hits) = spawn_counting_backend(vec![
            (
                200,
                r#"{"id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                    "featureDescription": "feat", "title": "ticket", "description": "body",
                    "taskStatus": "review", "assigneeId": 7,
                    "review": "1. missing tests", "reviewVerdict": "changes_requested",
                    "reviewOutcomeUpdatedAt": "2026-09-07T10:00:00Z"}"#,
            ),
            (200, "{}"),
            (200, "{}"),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let rc = test_context(client.clone());
        process_verdict(&rc, Some("2026-09-07T09:00:00Z".parse().unwrap())).await;
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "detail + status + comment"
        );

        // approve: detail fetch only — the ticket stays in `review`.
        let (url, hits) = spawn_counting_backend(vec![(
            200,
            r#"{"id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "ticket", "description": "body",
                "taskStatus": "review", "assigneeId": 7,
                "review": "looks good", "reviewVerdict": "approve",
                "reviewOutcomeUpdatedAt": "2026-09-07T10:00:00Z"}"#,
        )])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let rc = test_context(client);
        process_verdict(&rc, Some("2026-09-07T09:00:00Z".parse().unwrap())).await;
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "approve must not touch the ticket"
        );
    }

    /// Stale-verdict protection: a verdict written BEFORE the run started
    /// belongs to a previous review cycle and is not re-applied (no bounce).
    #[tokio::test]
    async fn stale_verdict_is_not_reapplied() {
        let (url, hits) = spawn_counting_backend(vec![
            (
                200,
                r#"{"id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                    "featureDescription": "feat", "title": "ticket", "description": "body",
                    "taskStatus": "review", "assigneeId": 7,
                    "review": "old findings", "reviewVerdict": "changes_requested",
                    "reviewOutcomeUpdatedAt": "2026-09-07T08:00:00Z"}"#,
            ),
            // The stale-verdict note.
            (200, "{}"),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let rc = test_context(client);
        process_verdict(&rc, Some("2026-09-07T09:00:00Z".parse().unwrap())).await;
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "detail + note only — no status move for a stale verdict"
        );
    }

    /// A ticket the human moved out of `review` while the run was in flight is
    /// left alone — the verdict is not applied.
    #[tokio::test]
    async fn verdict_skipped_when_ticket_left_review() {
        let (url, hits) = spawn_counting_backend(vec![(
            200,
            r#"{"id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "ticket", "description": "body",
                "taskStatus": "completed", "assigneeId": 7,
                "reviewVerdict": "changes_requested",
                "reviewOutcomeUpdatedAt": "2026-09-07T10:00:00Z"}"#,
        )])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let rc = test_context(client);
        process_verdict(&rc, Some("2026-09-07T09:00:00Z".parse().unwrap())).await;
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "detail only — no writes"
        );
    }

    /// A minimal ReviewContext against the mock backend (stub driver, temp
    /// workspace) for verdict/start tests — the driver never runs there.
    fn test_context(client: RemoterClient) -> ReviewContext {
        // SAFETY: test process env tweak; the env override would otherwise win.
        unsafe { std::env::remove_var("REMOTER_API_URL") };
        let toml = format!(
            r#"
api_url = "{}"
workspace_root = "/tmp/remoter-agent-review-test"
retry_backoff_secs = 1

[driver]
kind = "stub"
"#,
            client.base_url()
        );
        let config = Arc::new(Config::from_toml_str(&toml, Some("tok".to_string())).unwrap());
        let log_store = LogStore::new(&config.logs).unwrap();
        ReviewContext {
            client,
            config,
            driver: Arc::new(crate::driver::stub::StubDriver::new(0, false)),
            task: summary(),
            project: None,
            port_block: None,
            image_locks: crate::image::ImageLocks::default(),
            cancel: CancellationToken::new(),
            log_store,
        }
    }
}
