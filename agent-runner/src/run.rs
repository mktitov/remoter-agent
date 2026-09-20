//! One claimed ticket's run lifecycle (spec §5.4): start the run row, prepare
//! the worktree, build the prompt, execute the driver (bounded by
//! `run_timeout_minutes`), record the outcome, and drive the board status.
//! Retries, escalation, and rollback follow §5.7.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{
    client::{AttachmentDto, FinishRunBody, LinkDto, ProjectRepoConfig, RemoterClient, TaskDetail, TaskSummary},
    config::Config,
    container,
    driver::{AgentDriver, DriverError, DriverPhase, RunFailure, RunOutcome, RunSpec},
    forge, image,
    image::ImageLocks,
    logstore::LogStore,
    ports::PortBlock,
    session_log::SessionLogger,
    workspace::{self, ExecEnv},
};

/// `plan`, `implement`, `supervise`, or `review` — which phase this run
/// executes. Supervise runs are started by the supervision loop on a parent
/// ticket in `review` (spec §5.4, #115), and review runs by the review loop on
/// a ticket whose human requested an agent review (#142); neither goes through
/// [`execute`] — their lifecycles live in `supervise.rs` / `review.rs` — but
/// they share the kind type for the MCP role mapping, the running-registry,
/// and the per-kind model config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Plan,
    Implement,
    Supervise,
    Review,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Plan => "plan",
            RunKind::Implement => "implement",
            RunKind::Supervise => "supervise",
            RunKind::Review => "review",
        }
    }

    /// The status a successful run moves the ticket to. Only meaningful for
    /// claim-loop runs (`execute`): supervise runs leave the parent in
    /// `review` and are driven by `supervise.rs`.
    fn success_status(self) -> &'static str {
        match self {
            RunKind::Plan => "plan_review",
            RunKind::Implement => "review",
            RunKind::Supervise => "review",
            RunKind::Review => "review",
        }
    }

    /// The status a run rolls the ticket back to (spec §5.7) — only for
    /// cancellations (human cancel or daemon shutdown), which must put the
    /// ticket back in the claimable queue; a failure escalates instead.
    /// Supervise runs never roll the parent back — the supervision loop
    /// re-triggers on its own cadence.
    fn rollback_status(self) -> &'static str {
        match self {
            RunKind::Plan => "todo",
            RunKind::Implement => "implement",
            RunKind::Supervise => "review",
            RunKind::Review => "review",
        }
    }

    /// The status a failed run escalates the ticket to when the failure itself
    /// (not a cancellation) ends the claim: the ticket must leave the claimable
    /// queue so the poll loop cannot re-claim it in an infinite retry loop
    /// (spec §5.7) — the same transitions as a successful run. Only meaningful
    /// for claim-loop runs (`execute`): supervise runs handle their own
    /// failures in `supervise.rs` (the parent stays in `review`).
    fn escalation_status(self) -> &'static str {
        match self {
            RunKind::Plan => "plan_review",
            RunKind::Implement => "review",
            RunKind::Supervise => "review",
            RunKind::Review => "review",
        }
    }
}

pub struct RunContext {
    pub client: RemoterClient,
    pub config: Arc<Config>,
    pub driver: Arc<dyn AgentDriver>,
    pub task: TaskSummary,
    pub project: ProjectRepoConfig,
    pub kind: RunKind,
    /// The run's port block (`REMOTER_AGENT_PORT_BASE`, spec §5.8); released on
    /// drop. `None` in container mode — sidecars make the port contract
    /// obsolete (containers spec §3.4).
    pub port_block: Option<PortBlock>,
    /// Serializes per-project agent-image builds (container mode).
    pub image_locks: ImageLocks,
    /// Human-cancellation signal (unassign / manual status move, spec §5.7).
    pub cancel: CancellationToken,
    /// Persistent JSONL store for this run's ACP log (spec §7.1).
    pub log_store: LogStore,
}

/// Ensures the per-run broadcast channel is closed when the attempt finishes,
/// even on early returns or panics.
pub(crate) struct RunLogGuard {
    store: LogStore,
    task_id: i32,
    run_id: i32,
}

impl RunLogGuard {
    pub(crate) fn new(store: LogStore, task_id: i32, run_id: i32) -> Self {
        Self { store, task_id, run_id }
    }
}

impl Drop for RunLogGuard {
    fn drop(&mut self) {
        self.store.close_run(self.task_id, self.run_id);
    }
}

/// After a successful driver turn, check whether the run was cancelled before
/// doing any further work. If so, record the failure, roll back, and stop.
async fn cancel_after_attempt(rc: &RunContext, task_id: i32, run_id: i32) -> bool {
    if rc.cancel.is_cancelled() {
        set_phase(rc, run_id, "cancelling").await;
        finish_failed(
            &rc.client,
            task_id,
            run_id,
            "cancelled (human intervention or daemon shutdown)".to_string(),
            None,
            None,
        )
        .await;
        tracing::info!(task_id, run_id, "run cancelled");
        rollback(rc).await;
        true
    } else {
        false
    }
}

/// Marker of the daemon's stacking note on the ticket thread — the dedupe key
/// so a bounce/retry run never posts it twice (review #106).
const STACKING_NOTE_MARKER: &str = "stacked on parent";

/// A supervised child's stacking target: the parent ticket and the branch the
/// child's worktree branches off (branch stacking, spec §5.4). Same-project:
/// the parent's local branch, and the child's commits ride the parent's PR.
/// Cross-project (`cross_project`): the parent's integration branch in the
/// child's own project clone — the child's PR targets that branch, never the
/// base (docs/specs/cross-repo-projects.md).
struct SupervisedParent {
    parent_id: i32,
    branch: String,
    cross_project: bool,
}

/// Supervised-child detection (branch stacking): a child ticket whose parent
/// is assigned to this same agent, has supervision enabled, is not
/// `completed`, and whose branch is still alive in the local clone. All gates
/// must hold; any failure — including API errors — degrades to the normal
/// standalone-ticket behavior, so a fetch hiccup never changes how a ticket is
/// built. The child's `TaskDetail` (already fetched) rides along for the
/// caller's use.
async fn supervised_parent(rc: &RunContext) -> Option<(SupervisedParent, TaskDetail)> {
    let me = rc.client.whoami().await.ok()?;
    let detail = rc.client.task_detail(rc.task.id).await.ok()?;
    // From the child's perspective the link to its parent carries the
    // "subtask" relation (the backend resolves relations from the viewing
    // task's side — see `child_link_ids`).
    let parent_id = detail
        .links
        .as_deref()?
        .iter()
        .find(|l| l.relation == "subtask")?
        .task_id;
    let parent = rc.client.task_detail(parent_id).await.ok()?;
    if !parent.supervision_enabled {
        return None;
    }
    if parent.assignee_id != Some(me.id) {
        return None;
    }
    if parent.task_status == "completed" {
        return None;
    }
    let root = &rc.config.workspace_root;
    if parent.project_id != rc.project.project_id {
        // Cross-project child: the parent's branch lives in the parent's
        // project clone, which the child never touches — stack on the
        // parent's integration branch in the child's own project clone
        // instead (created and pushed on first use).
        let repo = workspace::ensure_repo_clone(root, rc.project.project_id, &rc.project.repo_url)
            .await
            .ok()?;
        let branch =
            workspace::ensure_integration_branch(&repo, parent_id, &parent.title, rc.project.base_branch.as_deref())
                .await
                .ok()?;
        return Some((
            SupervisedParent {
                parent_id,
                branch,
                cross_project: true,
            },
            detail,
        ));
    }
    let repo = workspace::repo_dir(root, rc.project.project_id);
    // The parent's branch: its worktree's branch when one exists, else the
    // name recomputed from the title (the completion cleanup removes the
    // worktree but keeps the branch until the human accepts the parent).
    let branch = workspace::existing_branch(root, rc.project.project_id, parent_id)
        .await
        .unwrap_or_else(|| workspace::branch_name(parent_id, &parent.title));
    if !workspace::local_branch_exists(&repo, &branch).await {
        return None;
    }
    Some((
        SupervisedParent {
            parent_id,
            branch,
            cross_project: false,
        },
        detail,
    ))
}

/// Executes the run to completion (success, escalation, rollback, or
/// cancellation). All
/// outcomes are recorded on the run row; this function only logs, never errors
/// upward — a failed ticket must not kill the poll loop.
pub async fn execute(rc: RunContext) {
    let task_id = rc.task.id;
    // A ticket renamed between runs keeps the branch its worktree was created
    // with — recomputing from the current title would name a branch that
    // doesn't exist locally, and the push at finish time would fail.
    let branch = match workspace::existing_branch(&rc.config.workspace_root, rc.project.project_id, task_id).await {
        Some(branch) => branch,
        None => workspace::branch_name(task_id, &rc.task.title),
    };
    let timeout = Duration::from_secs(rc.config.run_timeout_minutes * 60);

    // Hybrid resume strategy (spec §5.4): resume only where the agent's
    // working memory is worth the most and the session is still young — a
    // review → implement bounce resumes the succeeded implement run's session
    // (the agent remembers what it wrote and why, and the human's correction
    // lands in live context), and a re-plan resumes the succeeded plan run's
    // session (a fresh, amnesiac session just reports the already-touched
    // worktree as "satisfied" and the human's correction never lands in the
    // plan). Plan → implement and any retry after a failure start fresh: the
    // prompt is self-contained (plan text + thread), while long-lived sessions
    // only grow until compaction no longer fits the provider limit (task 11:
    // a ~60 MB session made every resumed run die before doing any work).
    let runs = match rc.client.list_runs(task_id).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "could not fetch run history; starting fresh session");
            Vec::new()
        }
    };
    // The approved plan text seeds the implement prompt (a fresh session then
    // still has everything the human approved); the run history is
    // newest-first, so the first succeeded plan run is the latest one.
    let plan = runs
        .iter()
        .find(|r| r.kind == "plan" && r.status == "succeeded")
        .and_then(|r| r.plan.as_deref());
    let resume_session = resume_session_for(rc.kind, &runs);

    // Supervised child (branch stacking, spec §5.4): the child's worktree
    // branches off the parent's branch tip, its commits ride the parent's PR
    // (whose base stays the project base branch), so the child run skips the
    // commit guard, and skips push/PR while its branch has no remote copy.
    // The stacking link is registered on the ticket thread once — implement
    // runs only; plan runs and bounce/retry runs must not re-post it (review
    // #106).
    let supervised = supervised_parent(&rc).await;
    if let Some((parent, detail)) = &supervised {
        tracing::info!(
            task_id,
            parent_id = parent.parent_id,
            parent_branch = %parent.branch,
            "supervised child: stacking on the parent's branch"
        );
        let already_noted = detail
            .comments
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|c| c.body.contains(STACKING_NOTE_MARKER));
        if rc.kind == RunKind::Implement && !already_noted {
            let text = if parent.cross_project {
                format!(
                    "branch `{branch}` {STACKING_NOTE_MARKER} #{}'s integration branch `{}` — supervised \
                     cross-project run: this branch's PR targets the integration branch, never the base; \
                     the work reaches this repo's base only through the human-merged integration PR",
                    parent.parent_id, parent.branch
                )
            } else {
                format!(
                    "branch `{branch}` {STACKING_NOTE_MARKER} #{}'s branch `{}` — supervised run: \
                     commits ride the parent's PR; this branch is not pushed (unless a remote copy \
                     already exists) and no new PR is opened",
                    parent.parent_id, parent.branch
                )
            };
            note(&rc.client, task_id, text).await;
        }
    }

    for attempt in 1..=rc.config.max_attempts {
        if rc.cancel.is_cancelled() {
            tracing::info!(task_id, "run cancelled before attempt {attempt}");
            rollback(&rc).await;
            return;
        }

        // Open the run row. A rejection here means the ticket moved under us
        // (human intervention) — the human owns the status now, so just stop.
        let run = match rc.client.start_run(task_id, rc.kind.as_str()).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "could not start run row; abandoning claim");
                return;
            }
        };
        set_phase(&rc, run.id, "active").await;
        // Live driver-phase forwarder (#154): the driver reports `stalled`
        // the moment its idle watchdog fires; relay it to the backend so the
        // journal shows the stall during the cancel/grace window.
        let (phase_tx, mut phase_rx) = tokio::sync::mpsc::unbounded_channel::<DriverPhase>();
        let phase_forwarder = {
            let client = rc.client.clone();
            let run_id = run.id;
            tokio::spawn(async move {
                while let Some(p) = phase_rx.recv().await {
                    let name = match p {
                        DriverPhase::Stalled => "stalled",
                    };
                    if let Err(e) = client.set_run_phase(run_id, Some(name)).await {
                        tracing::warn!(run_id, phase = name, error = %e, "could not set run phase");
                    }
                }
            })
        };
        tracing::info!(
            task_id,
            run_id = run.id,
            attempt,
            kind = rc.kind.as_str(),
            "run started"
        );

        let (logger, _log_guard) = match rc.log_store.open_run(task_id, run.id) {
            Ok(run_logger) => {
                let guard = RunLogGuard::new(rc.log_store.clone(), task_id, run.id);
                (SessionLogger::new(run_logger), Some(guard))
            }
            Err(e) => {
                tracing::warn!(
                    task_id,
                    run_id = run.id,
                    error = %e,
                    "could not open ACP log file; logging disabled for this run"
                );
                (SessionLogger::noop(), None)
            }
        };

        // A transient fetch failure (429/5xx, transport) consumes an attempt
        // like any other transient failure (spec §5.7) — only permanent errors
        // or an exhausted budget escalate the ticket immediately.
        let prompt = match build_prompt(&rc, plan, run.id).await {
            Ok(p) => p,
            Err(e) => {
                finish_failed(
                    &rc.client,
                    task_id,
                    run.id,
                    format!("prompt context fetch failed: {e}"),
                    None,
                    None,
                )
                .await;
                if e.is_transient() && attempt < rc.config.max_attempts {
                    tracing::warn!(task_id, run_id = run.id, attempt, error = %e, "transient context-fetch failure; retrying");
                    backoff(&rc, attempt).await;
                    continue;
                }
                escalate(&rc, run.id, &format!("prompt context fetch failed: {e}"), attempt).await;
                return;
            }
        };

        // Posted after build_prompt so the daemon's own note doesn't land in
        // this run's Conversation section (it will be in the next run's).
        note(
            &rc.client,
            task_id,
            format!("{} run #{} started (attempt {attempt})", rc.kind.as_str(), run.id),
        )
        .await;

        // Cancellation wins over the attempt: dropping the future mid-attempt
        // may leave a short-lived git child running (reaped by the OS soon
        // after); the dev-agent process itself gets kill-on-drop semantics from
        // the driver (spec §5.5). The ticket is rolled back unless a human owns
        // its status now (then the rollback write conflicts and is skipped).
        let result = tokio::select! {
            _ = rc.cancel.cancelled() => {
                set_phase(&rc, run.id, "cancelling").await;
                finish_failed(&rc.client, task_id, run.id, "cancelled (human intervention or daemon shutdown)".to_string(), None, None).await;
                tracing::info!(task_id, run_id = run.id, "run cancelled");
                rollback(&rc).await;
                return;
            }
            // Resume only on attempt 1 (spec §5.4): a retry after a failure
            // always starts fresh — failures are the main source of poisoned
            // sessions, and resuming one would just replay the same crash.
            r = tokio::time::timeout(timeout, attempt_run(&rc, run.id, &logger, &prompt, if attempt == 1 { &resume_session } else { &None }, &branch, supervised.as_ref().map(|(p, _)| p.branch.as_str()), Some(phase_tx))) => r,
        };
        phase_forwarder.abort();

        match result {
            Ok(Ok(mut outcome)) => {
                if cancel_after_attempt(&rc, task_id, run.id).await {
                    return;
                }
                // Commit guard (spec §5.4 step 5): an implement run the agent
                // left uncommitted is no success — pushing the empty branch
                // makes the forge 422 the PR create ("No commits between base
                // and branch") and the human gets nothing to review. Fail the
                // attempt instead; the retry's prompt carries this error under
                // Prior runs, so the agent knows it must commit.
                let mut created_child_tickets = false;
                if rc.kind == RunKind::Implement
                    && supervised.is_none()
                    && let Err(e) = ensure_committed(&rc, &branch).await
                {
                    // Ticket-creation run (spec §5.4): an implement run may
                    // produce child tickets instead of commits. If this ticket
                    // has fresh subtask links created by its assignee, treat the
                    // run as succeeded and skip the empty-branch failure.
                    if let Some(ids) = child_ticket_ids(&rc.client, task_id).await {
                        let joined = ids.iter().map(|id| format!("#{id}")).collect::<Vec<_>>().join(", ");
                        note(
                            &rc.client,
                            task_id,
                            format!(
                                "implement run produced {} new ticket(s) instead of commits: {joined}",
                                ids.len()
                            ),
                        )
                        .await;
                        created_child_tickets = true;
                    } else {
                        tracing::warn!(task_id, run_id = run.id, attempt, error = %e, "implement run left no commits on the branch");
                        let failure = e;
                        finish_failed(&rc.client, task_id, run.id, failure.clone(), None, None).await;
                        if attempt < rc.config.max_attempts {
                            backoff(&rc, attempt).await;
                            continue;
                        }
                        escalate(&rc, run.id, &failure, attempt).await;
                        return;
                    }
                }
                let mut body = FinishRunBody::succeeded();
                body.session_id = outcome.session_id.clone();
                body.branch = Some(branch.clone());
                body.input_tokens = outcome.input_tokens;
                body.output_tokens = outcome.output_tokens;
                body.model = outcome.model.clone();
                body.thinking = outcome.thinking.clone();
                match rc.kind {
                    RunKind::Plan => body.plan = Some(outcome.text.clone()),
                    RunKind::Implement => {
                        body.summary = Some(outcome.text.clone());
                        if cancel_after_attempt(&rc, task_id, run.id).await {
                            return;
                        }
                        // PROD-2 (§4.4): push the branch and open/reuse the
                        // draft PR before finishing, so `pr_url` rides the same
                        // PATCH. Any failure rides the run row as `pr_error` and
                        // is noted on the thread, and the run still succeeds —
                        // the code is committed and reviewable in the workspace
                        // clone. Ticket-creation runs have child tickets as the
                        // outcome and deliberately skip push/PR; supervised
                        // children skip push/PR only while their branch has no
                        // remote copy — a branch pushed by an earlier run (a
                        // gate was down) keeps being updated, or its open PR
                        // would silently go stale (review #106).
                        let skip_push_pr = match &supervised {
                            // Cross-project child: always push and open the PR
                            // against the parent's integration branch (never the
                            // base) — create-or-reuse keeps it idempotent.
                            Some((p, _)) if p.cross_project => false,
                            Some(_) => {
                                let wt =
                                    workspace::worktree_dir(&rc.config.workspace_root, rc.project.project_id, task_id);
                                !workspace::remote_branch_exists(&wt, &branch).await
                            }
                            None => false,
                        };
                        if !created_child_tickets && !skip_push_pr {
                            let pr_base = supervised
                                .as_ref()
                                .and_then(|(p, _)| p.cross_project.then_some(p.branch.as_str()));
                            match push_and_create_pr(&rc, &branch, pr_base).await {
                                Ok(Some(url)) => {
                                    body.pr_url = Some(url.clone());
                                    // PROD-9: if the PR is not mergeable into its
                                    // target, ask the agent to rebase. Never fail
                                    // the run over this — the code is still
                                    // reviewable.
                                    ensure_pr_mergeable(
                                        &rc,
                                        run.id,
                                        &logger,
                                        &branch,
                                        &url,
                                        &mut outcome,
                                        timeout,
                                        pr_base,
                                    )
                                    .await;
                                }
                                Ok(None) => {} // no forge configured — skip
                                Err(e) => {
                                    // Forge API errors can be long HTML pages —
                                    // bound them like any other failure text.
                                    let e = truncate_with_marker(&e, MAX_ERROR_BYTES);
                                    // The failure also rides the run row (§4.4) so
                                    // the UI can show it on the run, not only as a
                                    // thread note.
                                    body.pr_error = Some(e.clone());
                                    note(&rc.client, task_id, format!("push/PR failed: {e}")).await;
                                }
                            }
                        }
                    }
                    // Supervise and review runs never reach run::execute — they
                    // have their own lifecycles in supervise.rs / review.rs.
                    RunKind::Supervise | RunKind::Review => {
                        unreachable!("supervise/review runs use supervise::execute / review::execute")
                    }
                }
                if cancel_after_attempt(&rc, task_id, run.id).await {
                    return;
                }
                // The mergeability nudges above may have advanced the session or
                // accumulated extra tokens — reflect the final outcome on the run row.
                body.session_id = outcome.session_id.clone();
                body.input_tokens = outcome.input_tokens;
                body.output_tokens = outcome.output_tokens;

                // Final read-only action check: if actions are still not final,
                // move to review anyway but note the remaining items on the thread.
                if rc.kind == RunKind::Implement
                    && let Ok(detail) = rc.client.task_detail(task_id).await
                {
                    let pending: Vec<_> = detail.actions.iter().filter(|a| !is_action_final(&a.status)).collect();
                    if !pending.is_empty() {
                        let ids = pending
                            .iter()
                            .map(|a| format!("#{} {}", a.id, a.description))
                            .collect::<Vec<_>>()
                            .join("; ");
                        note(
                            &rc.client,
                            task_id,
                            format!(
                                "implement run #{} finished with {} non-final action(s): {}",
                                run.id,
                                pending.len(),
                                ids
                            ),
                        )
                        .await;
                    }
                }

                finish_reporting(&rc.client, run.id, &body).await;
                match rc.client.set_task_status(task_id, rc.kind.success_status()).await {
                    Ok(()) => {
                        note(
                            &rc.client,
                            task_id,
                            format!(
                                "{} run #{} finished → {}",
                                rc.kind.as_str(),
                                run.id,
                                rc.kind.success_status()
                            ),
                        )
                        .await;
                    }
                    // The dev-agent may have advanced the ticket itself via the
                    // remoter MCP tools (spec §5.6) — then this write is a no-op
                    // or conflicts; a forbidden means a human took the ticket
                    // somewhere the agent set doesn't cover. Either way the
                    // ticket is already where a human decided it should be, so
                    // no "finished →" note (it would claim a transition that
                    // didn't happen).
                    Err(e) if e.is_conflict() || e.is_forbidden() => {
                        tracing::info!(
                            task_id,
                            "ticket already advanced (agent MCP write or human move); status write skipped"
                        )
                    }
                    Err(e) => tracing::error!(task_id, error = %e, "run succeeded but status transition failed"),
                }
                // Report reminder (ticket decision: the agent writes the
                // report text itself — no auto-copy of the summary — but an
                // implement run that finished leaving the report empty gives
                // the reviewer nothing, so the daemon nudges on the thread).
                if rc.kind == RunKind::Implement {
                    match rc.client.task_detail(task_id).await {
                        Ok(d) if d.report.as_deref().is_none_or(|r| r.trim().is_empty()) => {
                            note(
                                &rc.client,
                                task_id,
                                format!(
                                    "implement run #{} finished without writing the task report \
                                     (set_task_report) — the reviewer has no summary",
                                    run.id
                                ),
                            )
                            .await;
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(task_id, error = %e, "post-finish report check failed"),
                    }
                }
                tracing::info!(task_id, run_id = run.id, "run succeeded → {}", rc.kind.success_status());
                return;
            }
            Ok(Err(RunFailure {
                source: DriverError::Stalled(e),
                input_tokens,
                output_tokens,
                ..
            })) if attempt < rc.config.max_attempts => {
                tracing::warn!(task_id, run_id = run.id, attempt, error = %e, "run stalled; retrying");
                logger.log_error(format!("attempt {attempt} stalled — retrying attempt {}", attempt + 1));
                let failure = format!("attempt {attempt}: stalled after idle limit: {e} — retrying");
                // The live `stalled` phase was already PATCHed when the
                // watchdog fired; mark the row `retrying` now so the phase
                // survives finish_failed and stays visible across the backoff
                // (#154).
                set_phase(&rc, run.id, "retrying").await;
                finish_failed(
                    &rc.client,
                    task_id,
                    run.id,
                    failure.clone(),
                    input_tokens,
                    output_tokens,
                )
                .await;
                note(
                    &rc.client,
                    task_id,
                    format!(
                        "{} run #{} stalled after the idle limit ({e}) — retrying (attempt {}/{})",
                        rc.kind.as_str(),
                        run.id,
                        attempt + 1,
                        rc.config.max_attempts
                    ),
                )
                .await;
                backoff(&rc, attempt).await;
            }
            Ok(Err(RunFailure {
                source: DriverError::Transient(e),
                input_tokens,
                output_tokens,
                ..
            })) if attempt < rc.config.max_attempts => {
                tracing::warn!(task_id, run_id = run.id, attempt, error = %e, "transient failure; retrying");
                logger.log_error(format!(
                    "attempt {attempt} failed: {e} — retrying attempt {}",
                    attempt + 1
                ));
                set_phase(&rc, run.id, "retrying").await;
                finish_failed(
                    &rc.client,
                    task_id,
                    run.id,
                    format!("attempt {attempt}: {e}"),
                    input_tokens,
                    output_tokens,
                )
                .await;
                backoff(&rc, attempt).await;
            }
            Ok(Err(failure)) => {
                tracing::warn!(task_id, run_id = run.id, attempt, error = %failure.source, "run failed permanently");
                finish_failed(
                    &rc.client,
                    task_id,
                    run.id,
                    failure.source.to_string(),
                    failure.input_tokens,
                    failure.output_tokens,
                )
                .await;
                escalate(&rc, run.id, &failure.source.to_string(), attempt).await;
                return;
            }
            Err(_elapsed) => {
                let reason = format!("timeout after {} minutes", rc.config.run_timeout_minutes);
                tracing::warn!(task_id, run_id = run.id, attempt, "run timed out");
                finish_failed(&rc.client, task_id, run.id, reason.clone(), None, None).await;
                escalate(&rc, run.id, &reason, attempt).await;
                return;
            }
        }
    }
}

/// One attempt: prepare the worktree (spec §5.4 step 1), bring the runtime up
/// (container mode: agent image + run container + sidecars; host mode: devenv
/// services when the project uses devenv, §5.8), run the driver (with
/// dirty-worktree nudges), always tear the runtime down. Workspace/devenv
/// failures are transient (network, nix cache); a missing project Dockerfile
/// is permanent (container mode fails closed).
#[allow(clippy::too_many_arguments)]
async fn attempt_run(
    rc: &RunContext,
    run_id: i32,
    logger: &SessionLogger,
    prompt: &str,
    resume_session: &Option<String>,
    branch: &str,
    stack_base: Option<&str>,
    phase_tx: Option<tokio::sync::mpsc::UnboundedSender<DriverPhase>>,
) -> Result<RunOutcome, RunFailure> {
    let task_id = rc.task.id;
    let prepared = workspace::prepare_with_stack(
        &rc.config.workspace_root,
        rc.project.project_id,
        task_id,
        &rc.task.title,
        &rc.project.repo_url,
        rc.project.base_branch.as_deref(),
        stack_base,
    )
    .await
    .map_err(|e| RunFailure::new(DriverError::Transient(format!("workspace prepare: {e}")), None))?;

    // Reference repos (docs/specs/cross-repo-projects.md): mounted read-only
    // into the ticket worktree as `.refs/<mount>` — fail-open per ref; an
    // unavailable ref is noted on the thread and the run continues without it
    // (same fail-open policy as the image freshness check).
    let refs = workspace::prepare_reference_repos(
        &rc.config.workspace_root,
        rc.project.project_id,
        task_id,
        &prepared.dir,
        &rc.project.reference_repos,
    )
    .await;
    for (mount, error) in &refs.failed {
        note(
            &rc.client,
            task_id,
            format!("reference repo `{mount}` is unavailable: {error} — the run continues without it"),
        )
        .await;
    }

    let env = run_env(rc);
    // Container mode: the guard's drop (`docker rm -f` + worktree unlock) is
    // the teardown on every exit path below, including cancellation.
    let containers = start_container_runtime(
        &rc.config,
        &rc.image_locks,
        &rc.project,
        run_id,
        &prepared.dir,
        prepared.devenv,
        &env,
        &refs.mounted,
    )
    .await?;
    if containers.is_none() {
        // Host mode surfaces refs as symlinks; container mode bind-mounts them.
        for (mount, error) in workspace::link_refs_into(&prepared.dir, &refs.mounted).await {
            note(
                &rc.client,
                task_id,
                format!("reference repo `{mount}` could not be linked into the worktree: {error} — the run continues without it"),
            )
            .await;
        }
    }
    if containers.is_none() && prepared.devenv {
        workspace::services_up(&prepared.dir, &env)
            .await
            .map_err(|e| RunFailure::new(DriverError::Transient(format!("devenv services up: {e}")), None))?;
    }

    let spec = RunSpec {
        task_id,
        run_id,
        cwd: prepared.dir.clone(),
        prompt: prompt.to_string(),
        kind: rc.kind.as_str(),
        resume_session: resume_session.clone(),
        branch: branch.to_string(),
        env: env.clone(),
        exec: exec_env(&rc.config, run_id, prepared.devenv),
        config_options: rc.config.driver.config_options_for(rc.kind.as_str()),
        logger: logger.clone(),
        phase_tx,
    };
    let result = run_with_dirty_nudges(rc, run_id, spec, branch, &prepared.dir, logger).await;

    // Implement runs: gate on task actions being final before any commit/PR work.
    let result = match result {
        Ok(outcome) if rc.kind == RunKind::Implement => {
            ensure_final_actions(rc, run_id, outcome, branch, &prepared.dir, logger).await
        }
        other => other,
    };

    if containers.is_none() && prepared.devenv {
        // Best-effort, must not mask the run's real outcome (spec §5.8).
        workspace::services_down(&prepared.dir, &env).await;
    }
    result
}

/// Runs the driver, then ensures the worktree is clean (spec §5.4 step 5).
async fn run_with_dirty_nudges(
    rc: &RunContext,
    run_id: i32,
    spec: RunSpec,
    branch: &str,
    dir: &Path,
    logger: &SessionLogger,
) -> Result<RunOutcome, RunFailure> {
    let outcome = rc.driver.run(spec).await?;
    ensure_clean_worktree(rc, run_id, outcome, branch, dir, logger).await
}

/// Checks `git status --porcelain`. A dirty worktree sends the agent back with
/// a nudge prompt on the resumed session, up to 2 times; if it stays dirty, the
/// attempt fails as transient. Extracted so it can be reused after the action
/// finalization gate (spec §5.4 step 5 extended).
async fn ensure_clean_worktree(
    rc: &RunContext,
    run_id: i32,
    outcome: RunOutcome,
    branch: &str,
    dir: &Path,
    logger: &SessionLogger,
) -> Result<RunOutcome, RunFailure> {
    let task_id = rc.task.id;
    let devenv = workspace::uses_devenv(dir);
    let env = run_env(rc);
    let config_options = rc.config.driver.config_options_for(rc.kind.as_str());

    let mut merged = outcome;

    let status = match workspace::dirty_status(dir).await {
        Ok(s) => s,
        Err(e) => {
            return Err(RunFailure::new(
                DriverError::Transient(format!("dirty status check failed: {e}")),
                merged.session_id.clone(),
            )
            .with_tokens(merged.input_tokens, merged.output_tokens));
        }
    };
    if status.is_empty() {
        return Ok(merged);
    }

    let nudge_prompt = dirty_nudge_prompt(rc.kind, &status);
    for nudge in 1..=2 {
        let nudge_spec = RunSpec {
            task_id,
            run_id,
            cwd: dir.to_path_buf(),
            prompt: nudge_prompt.clone(),
            kind: rc.kind.as_str(),
            resume_session: merged.session_id.clone(),
            branch: branch.to_string(),
            env: env.clone(),
            exec: exec_env(&rc.config, run_id, devenv),
            config_options: config_options.clone(),
            logger: logger.clone(),
            phase_tx: None,
        };
        let nudge_outcome = rc.driver.run(nudge_spec).await?;
        merged.session_id = nudge_outcome.session_id.clone().or(merged.session_id);
        if !nudge_outcome.text.trim().is_empty() {
            merged.text = nudge_outcome.text.clone();
        }
        merged.input_tokens = sum_tokens(merged.input_tokens, nudge_outcome.input_tokens);
        merged.output_tokens = sum_tokens(merged.output_tokens, nudge_outcome.output_tokens);

        match workspace::dirty_status(dir).await {
            Ok(s) if s.is_empty() => return Ok(merged),
            Ok(s) if nudge == 2 => {
                return Err(RunFailure {
                    source: DriverError::Transient(format!(
                        "agent left the worktree dirty after 2 nudges: {}",
                        truncate_with_marker(&s, 200)
                    )),
                    session_id: merged.session_id.clone(),
                    input_tokens: merged.input_tokens,
                    output_tokens: merged.output_tokens,
                });
            }
            Ok(_) => continue,
            Err(e) => {
                return Err(RunFailure::new(
                    DriverError::Transient(format!("dirty status check failed: {e}")),
                    merged.session_id.clone(),
                )
                .with_tokens(merged.input_tokens, merged.output_tokens));
            }
        }
    }
    unreachable!("nudge loop exits after at most 2 iterations")
}

/// Prompt sent back to the agent when the worktree is still dirty after the
/// initial driver run (spec §5.4 step 5). Plan runs must restore pristine;
/// implement runs must commit/revert the leftovers.
fn dirty_nudge_prompt(kind: RunKind, status: &str) -> String {
    let truncated = truncate_with_marker(status, 500);
    match kind {
        RunKind::Plan => format!(
            "Your previous turn ended with a dirty worktree even though you were in PLAN-ONLY mode:\n\n```\n{truncated}\n```\n\nRestore the worktree to pristine (`git checkout -- .` and remove any untracked files) so that `git status --porcelain` is empty, then end your turn."
        ),
        RunKind::Implement => format!(
            "Your previous turn ended with a dirty worktree:\n\n```\n{truncated}\n```\n\nFinish the work: commit everything intended (or revert/remove the rest) so that `git status --porcelain` is empty, then end your turn."
        ),
        // Supervise/review runs never nudge dirty worktrees (their lifecycles
        // live outside run::execute).
        RunKind::Supervise | RunKind::Review => unreachable!("supervise/review runs never nudge dirty worktrees"),
    }
}

/// How many action-finalization nudges the daemon issues before moving to
/// review anyway (spec §5.4). Each nudge runs on the resumed session.
const MAX_ACTION_NUDGES: u32 = 2;

/// Prompt sent back to the agent when one or more task actions are still
/// not final before the daemon would move the ticket to review. The agent must
/// complete or reject every listed action via the remoter MCP tools.
fn action_nudge_prompt(actions: &[&crate::client::ActionItem]) -> String {
    let mut s = String::from(
        "The implement run is about to finish, but the following task actions are not in a final state yet. \
         Each action must be completed (use start_action while you work on it, then complete_action) \
         or rejected (reject_action with a reason). Use the remoter MCP tools to finalize every action \
         listed below, then end your turn. Rejected actions must include a reason; that reason must \
         also be documented in the task report.\n\n",
    );
    for a in actions {
        s.push_str(&format!("- #{}: {}\n", a.id, a.description));
    }
    s
}

/// Returns true when the action status is final (completed or rejected) or
/// soft-deleted. Non-final statuses are inactive and active.
fn is_action_final(status: &str) -> bool {
    matches!(status, "completed" | "rejected" | "deleted")
}

/// Implement-run gate: before the daemon moves the ticket to review, every
/// non-deleted task action must be final (completed or rejected). Pending
/// actions get up to `MAX_ACTION_NUDGES` resumed nudges; after that the worktree
/// is re-checked for cleanliness and the gate returns the merged outcome. The
/// caller is responsible for the final read-only check and thread note.
async fn ensure_final_actions(
    rc: &RunContext,
    run_id: i32,
    outcome: RunOutcome,
    branch: &str,
    dir: &Path,
    logger: &SessionLogger,
) -> Result<RunOutcome, RunFailure> {
    if rc.kind != RunKind::Implement {
        return Ok(outcome);
    }

    let task_id = rc.task.id;
    let devenv = workspace::uses_devenv(dir);
    let env = run_env(rc);
    let config_options = rc.config.driver.config_options_for(rc.kind.as_str());
    let mut merged = outcome;

    for _nudge in 1..=MAX_ACTION_NUDGES {
        let detail = match rc.client.task_detail(task_id).await {
            Ok(d) => d,
            Err(e) => {
                return Err(RunFailure::new(
                    DriverError::Transient(format!("final-actions gate could not fetch task detail: {e}")),
                    merged.session_id.clone(),
                )
                .with_tokens(merged.input_tokens, merged.output_tokens));
            }
        };

        let pending: Vec<&crate::client::ActionItem> =
            detail.actions.iter().filter(|a| !is_action_final(&a.status)).collect();

        if pending.is_empty() {
            return Ok(merged);
        }

        let prompt = action_nudge_prompt(&pending);
        let nudge_spec = RunSpec {
            task_id,
            run_id,
            cwd: dir.to_path_buf(),
            prompt,
            kind: rc.kind.as_str(),
            resume_session: merged.session_id.clone(),
            branch: branch.to_string(),
            env: env.clone(),
            exec: exec_env(&rc.config, run_id, devenv),
            config_options: config_options.clone(),
            logger: logger.clone(),
            phase_tx: None,
        };
        let nudge_outcome = rc.driver.run(nudge_spec).await?;
        merged.session_id = nudge_outcome.session_id.clone().or(merged.session_id);
        if !nudge_outcome.text.trim().is_empty() {
            merged.text = nudge_outcome.text.clone();
        }
        merged.input_tokens = sum_tokens(merged.input_tokens, nudge_outcome.input_tokens);
        merged.output_tokens = sum_tokens(merged.output_tokens, nudge_outcome.output_tokens);
    }

    // After action nudges, re-run the full dirty-worktree check + nudges: the
    // agent may have left the worktree dirty while finalizing actions.
    ensure_clean_worktree(rc, run_id, merged, branch, dir, logger).await
}

fn sum_tokens(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (Some(x), Some(y)) => Some(x + y),
    }
}

/// The env every run (and its services/sidecars) gets (spec §5.8 + containers
/// spec §3.4). Host mode exports the run's port block; container mode exports
/// `REMOTER_CONTAINER=1` and loopback DB URLs instead (the postgres sidecar
/// shares the run container's network namespace).
fn run_env(rc: &RunContext) -> Vec<(String, String)> {
    if rc.config.execution.is_container() {
        return container::container_env(rc.task.id);
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

/// The `RunSpec.exec` for a driver turn: host (optionally devenv-wrapped) or
/// `docker exec` into the run container.
pub(crate) fn exec_env(config: &Config, run_id: i32, devenv: bool) -> ExecEnv {
    if !config.execution.is_container() {
        return ExecEnv::host(devenv);
    }
    ExecEnv::Container(workspace::ContainerExec {
        docker: config.execution.docker_binary.clone(),
        container: container::run_container_name(run_id),
        work_dir: std::path::PathBuf::from(container::WORK_DIR),
        devenv,
        sessions_dir: config.driver.sessions_dir.clone().unwrap_or_else(|| {
            config
                .execution
                .agent_home(&config.workspace_root)
                .join(if config.driver.kind == "kimi-acp" {
                    ".kimi-code/sessions"
                } else {
                    "sessions"
                })
        }),
        api_url: container::container_api_url(&config.api_url),
    })
}

/// Container-mode runtime for one driver turn (shared by `attempt_run`, the
/// rebase-nudge loop, and `supervise::run_attempt`): ensure the project's
/// agent image, start the run container + sidecars, and — when the worktree
/// carries a justfile — run the project's `just db-up` inside the container
/// (its `REMOTER_CONTAINER=1` branch only waits for the sidecars; projects
/// without a justfile have no services to wait for and skip the step). Host
/// mode returns `None` and the caller falls back to
/// `workspace::services_up`/`services_down`. No silent fallback: a
/// container-mode failure propagates as a run failure, never a host run.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_container_runtime(
    config: &Config,
    image_locks: &ImageLocks,
    project: &ProjectRepoConfig,
    run_id: i32,
    worktree: &Path,
    devenv: bool,
    env: &[(String, String)],
    refs: &[workspace::RefMount],
) -> Result<Option<container::RunContainers>, RunFailure> {
    if !config.execution.is_container() {
        return Ok(None);
    }
    let repo = workspace::repo_dir(&config.workspace_root, project.project_id);
    let image = image::ensure_project_image(&config.execution, image_locks, &repo, project)
        .await
        .map_err(|e| RunFailure::new(e, None))?;
    let containers = container::start(container::ContainerSpec {
        cfg: &config.execution,
        driver_kind: &config.driver.kind,
        run_id,
        project_id: project.project_id,
        image: &image,
        worktree,
        repo: &repo,
        agent_home: &config.execution.agent_home(&config.workspace_root),
        env,
        refs,
    })
    .await
    .map_err(|e| RunFailure::new(e, None))?;
    if devenv && worktree_has_justfile(worktree) {
        container_services_up(config, run_id, env).await?;
    } else if devenv {
        tracing::info!(run_id, "no justfile in worktree — skipping db-up (no sidecar wait)");
    }
    Ok(Some(containers))
}

/// Whether the worktree carries a justfile — the `db-up` contract below is
/// optional: projects without a justfile have no services to wait for (their
/// tests don't use the sidecars), so the step is skipped for them instead of
/// dying on `just: command not found` (exit 127) inside the run container.
fn worktree_has_justfile(worktree: &Path) -> bool {
    ["justfile", "Justfile", ".justfile"]
        .iter()
        .any(|f| worktree.join(f).exists())
}

/// `just db-up` inside the run container — under `REMOTER_CONTAINER=1` the
/// justfile only waits for the already-running sidecars (no devenv services).
/// Called only when the worktree carries a justfile (see
/// `worktree_has_justfile`); a justfile without a `db-up` recipe stays a hard
/// error — that signals a broken project contract, not a missing one.
async fn container_services_up(config: &Config, run_id: i32, env: &[(String, String)]) -> Result<(), RunFailure> {
    let exec = exec_env(config, run_id, true);
    let cmd = workspace::wrap_command(Path::new("/"), &exec, "just", &["db-up"], env);
    let out = tokio::process::Command::from(cmd)
        .output()
        .await
        .map_err(|e| RunFailure::new(DriverError::Transient(format!("container services up: {e}")), None))?;
    if !out.status.success() {
        // pg_isready and npm report failures on stdout, just on stderr — an
        // error built from stderr alone comes out empty (run #944).
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(RunFailure::new(
            DriverError::Transient(format!(
                "container services up (just db-up, {}): {}{}",
                out.status,
                stderr.trim(),
                if stderr.trim().is_empty() { stdout.trim() } else { "" }
            )),
            None,
        ));
    }
    Ok(())
}

/// Exponential backoff between attempts (spec §5.7): `retry_backoff_secs`
/// doubled per attempt, interruptible by cancellation — the next loop
/// iteration's cancel check then rolls the ticket back.
async fn backoff(rc: &RunContext, attempt: u32) {
    let shift = (attempt - 1).min(10);
    let delay = rc.config.retry_backoff_secs.saturating_mul(1u64 << shift);
    tokio::select! {
        _ = rc.cancel.cancelled() => {}
        _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
    }
}

/// Moves the ticket back to its pre-claim status so a human sees it in the
/// queue rather than in a zombie state (spec §5.7). A conflict or a forbidden
/// here means a human touched the ticket in the meantime (their move is outside
/// the agent transition set) — leave it alone. Used only for cancellations
/// (human cancel, daemon shutdown) — a failure escalates instead, so the poll
/// loop cannot re-claim the ticket in an infinite retry loop.
async fn rollback(rc: &RunContext) {
    match rc.client.set_task_status(rc.task.id, rc.kind.rollback_status()).await {
        Ok(()) => tracing::info!(task_id = rc.task.id, "rolled back → {}", rc.kind.rollback_status()),
        Err(e) if e.is_conflict() || e.is_forbidden() => {
            tracing::info!(task_id = rc.task.id, "rollback skipped: ticket moved by a human")
        }
        Err(e) => tracing::warn!(task_id = rc.task.id, error = %e, "rollback failed"),
    }
}

/// A failed run (attempts exhausted, permanent error, or timeout) does not roll
/// the ticket back into the claimable queue (spec §5.7) — `todo`/`implement`
/// are exactly what the poll loop claims, so a rollback with an unhealed cause
/// turns into an infinite claim/fail loop spamming runs and push
/// notifications. Instead the ticket escalates to `review`/`plan_review` (the
/// successful run's targets) with an explanatory thread note, and a human
/// bounces it back (`review → implement`) once the cause is fixed. A conflict
/// or forbidden means a human already moved the ticket — leave it there.
async fn escalate(rc: &RunContext, run_id: i32, reason: &str, attempt: u32) {
    let task_id = rc.task.id;
    let reason = truncate_with_marker(reason, MAX_ERROR_BYTES);
    note(
        &rc.client,
        task_id,
        format!(
            "{} run #{} failed after {attempt} attempt(s): {reason} — moving to {}; needs a human",
            rc.kind.as_str(),
            run_id,
            rc.kind.escalation_status()
        ),
    )
    .await;
    match rc.client.set_task_status(task_id, rc.kind.escalation_status()).await {
        Ok(()) => tracing::info!(task_id, run_id, "escalated → {}", rc.kind.escalation_status()),
        Err(e) if e.is_conflict() || e.is_forbidden() => {
            tracing::info!(task_id, run_id, "escalation skipped: ticket moved by a human")
        }
        Err(e) => tracing::warn!(task_id, run_id, error = %e, "escalation status transition failed"),
    }
}

/// The resume token for this run (spec §5.4): the session of the NEWEST run
/// in the ticket's history (`runs` is newest-first), but only when that run
/// succeeded and has the same kind — i.e. a review → implement bounce resumes
/// the succeeded implement session, and a re-plan resumes the succeeded plan
/// session (never an implement one). Everything else starts fresh: plan →
/// implement (the newest run is the succeeded plan run), any retry after a
/// failure (the newest run is failed — resuming a poisoned session would just
/// replay the same crash), and a plan run whose newest run is an implement
/// run. Checking the newest run rather than "the latest succeeded run of the
/// same kind" is what keeps a failure after a succeeded run from resuming.
fn resume_session_for(kind: RunKind, runs: &[crate::client::AgentRunDto]) -> Option<String> {
    runs.first()
        .filter(|r| r.status == "succeeded" && r.kind == kind.as_str())
        .and_then(|r| r.session_id.clone())
}

/// Plan-run instructions (spec §5.6). Module-scope so tests can pin the
/// contract (open questions are registered via `add_task_question`, never
/// written as a text section in the plan).
const PLAN_INSTRUCTIONS: &str = "## Instructions\nYou are in PLAN-ONLY mode: explore the repository, but do NOT create, edit or \
     delete any files — the worktree must stay exactly as you found it. Write a step-by-step \
     implementation plan and make it your ENTIRE final message (Markdown — do not ask questions \
     interactively). The plan must contain a \
     \"Plan\" section that describes what the implementation run will do and the expected outcome of \
     the implementation step: code changes on this ticket's branch or a set of child tickets. \
     Register every open question or assumption the human must clarify as a structured question via \
     the `add_task_question` MCP tool (one call per question) — never write an \"Open questions\" \
     section into the plan text. Propose concrete answer `options` whenever possible; set `multiple` \
     to true when several options may apply. The human answers in the UI; the answers come back in \
     the \"## Open questions\" prompt section and via `list_task_questions`/`get_task`. If the ticket or the \
     conversation changed since your previous plan, produce the UPDATED plan in full, not a status \
     report; human corrections in the Conversation section supersede the original ticket text. \
     Honor AGENTS.md files in the repo. Never end your turn with background tasks still running — \
     your turn's end is final: the daemon shuts the agent down and pending tasks are killed, their \
     completion never arrives. Run long commands in the foreground or wait for them to finish \
     before your final message. A dirty worktree after the turn fails the run: restore the worktree \
     to pristine before ending your turn.\n\n\
     Register every distinct step of your plan as a task action via the `add_action` MCP tool \
     (call `get_task` first to read the existing actions and avoid duplicates). Actions are the \
     progress bar the human sees on the board card. Edit a step's description or priority with \
     `update_action`. Delete superseded planning steps with `delete_action` so they vanish from counts \
     and prompts; use `reject_action` only for steps that already saw real work (started, time/tokens \
     spent). Do not use `start_action` or `complete_action` in plan mode — those tools are unavailable \
     in plan mode and activating an action would not move the ticket for agents anyway. If the right \
     outcome for this ticket is a set of new tickets rather than code changes, structure the plan as one \
     action per future ticket; ticket-creation tools are unavailable in plan mode, and the implement run \
     creates the child tickets after the plan is approved. Each future ticket must be holistic: \
     implementing it on its own must leave the project building, all tests passing, and the result \
     safe to deploy to production — never slice the work so that a single ticket breaks the build \
     or ships a half-finished feature. Order the future tickets by dependency and state which ticket \
     blocks which — the implement run wires these as `blocks`/`blocked_by` links via the `add_link` \
     MCP tool, so humans and agents can see which child ticket is ready for implementation and which \
     are blocked.\n";

/// Cap for error text stored on the run row and posted to the thread — the
/// full output stays in the daemon logs (spec §5.6).
pub(crate) const MAX_ERROR_BYTES: usize = 2 * 1024;
/// Cap per run outcome quoted under "## Prior runs" (spec §5.6).
const MAX_PRIOR_OUTCOME_BYTES: usize = 2 * 1024;
/// Cap per single comment body quoted under "## Conversation" (spec §5.6).
pub(crate) const MAX_COMMENT_BYTES: usize = 8 * 1024;
/// Only the newest N comments are quoted under "## Conversation" (spec §5.6).
pub(crate) const MAX_CONVERSATION_COMMENTS: usize = 50;
/// Total cap for the "## Conversation" section, enforced from newest to oldest
/// so the most recent context is preserved (spec §5.6).
pub(crate) const MAX_CONVERSATION_BYTES: usize = 64 * 1024;
/// How many rebase nudges the daemon issues when a PR is not mergeable into
/// its target (PROD-9). Each nudge runs on the resumed session and is followed
/// by a daemon push + mergeability recheck.
const MAX_REBASE_NUDGES: u32 = 2;

/// Bounds text that goes into the run row, the thread, or the next prompt:
/// a char-boundary-safe cut with a marker, so both the human and the next
/// agent see the truncation happened (the full text stays in the daemon logs).
pub(crate) fn truncate_with_marker(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated, {} bytes total]", &text[..end], text.len())
}

/// The implement prompt's `## Attachments` section: the task's attachments as
/// markdown links against the stable download URLs (docs/specs/attachments.md
/// §4), plus the MCP-tool rules. Omitted entirely when the task has none —
/// the same empty-section style as Actions / Conversation.
fn attachments_section(base_url: &str, attachments: &[AttachmentDto]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let mut s = String::from("## Attachments\n");
    for a in attachments {
        s.push_str(&format!(
            "- [{}]({}/api/v1/attachments/{}/download)\n",
            a.file_name,
            base_url.trim_end_matches('/'),
            a.id
        ));
    }
    s.push_str(
        "\nRead a file's contents with the `read_attachment` MCP tool; add files with `add_attachment` and \
         embed the returned markdown link in comments or descriptions. Prefer `filePath` (absolute path to the \
         local file) over `contentBase64` — remoter-mcp reads the bytes from disk, so you do not need to encode \
         screenshots or other artifacts. You cannot delete attachments.\n\n",
    );
    s
}

/// Builds the prompt: the ticket context block (ticket, prior runs,
/// conversation) plus per-kind instructions (spec §5.6). The conversation
/// thread is what makes a `review → implement` bounce steer the next run,
/// and — with the approved plan text — fully seeds a fresh session when no
/// resume is possible (§5.4). `current_run_id` is excluded from Prior runs:
/// the live run row (status `running`, no outcome) would be prompt noise.
async fn build_prompt(
    rc: &RunContext,
    plan: Option<&str>,
    current_run_id: i32,
) -> Result<String, crate::client::ClientError> {
    let detail = rc.client.task_detail(rc.task.id).await?;
    // The configured mount names — the `.refs/` block appears only when the
    // project has reference repos, whether or not every one of them mounted
    // (a failed ref is noted on the thread separately).
    let refs: Vec<String> = rc
        .project
        .reference_repos
        .iter()
        .map(|r| r.mount_name.clone())
        .collect();
    let deps = dependency_repos_section(rc, &detail).await;
    Ok(render_prompt(
        rc.kind,
        &detail,
        plan,
        current_run_id,
        rc.client.base_url(),
        &refs,
        &deps,
    ))
}

/// The static intro of the "## Dependency repos" block — module scope so
/// tests can pin the contract.
const DEPENDENCY_REPOS_INTRO: &str = "## Dependency repos\nThis feature spans into other projects: their child tickets' work is collected \
     on this ticket's integration branch in each repo (it reaches the repo's base branch only through a \
     human-merged integration PR — never merge it yourself). Before finishing, pin each listed dependency \
     to the integration branch tip below (e.g. `cargo update -p <dep> --precise <rev>` for a git \
     dependency, or the rev of the corresponding flake input) and run the integration tests that \
     exercise the pairing.\n\n";

/// The "## Dependency repos" block (docs/specs/cross-repo-projects.md): only
/// for the final integration run of a parent with cross-project supervised
/// children. Each child project's repo collects the children's work on this
/// ticket's integration branch (never merged to the repo's base automatically)
/// — the block hands the agent each repo's URL, the branch, its current tip
/// (via `git ls-remote`), and the pin/integration-test instructions. Empty when
/// there are no cross-project children. Fail-open per repo: an unmanaged
/// project or an unreachable remote degrades its entry, never the prompt.
async fn dependency_repos_section(rc: &RunContext, detail: &TaskDetail) -> String {
    if rc.kind != RunKind::Implement {
        return String::new();
    }
    let child_ids: Vec<i32> = detail
        .links
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|l| l.relation == "parent")
        .map(|l| l.task_id)
        .collect();
    if child_ids.is_empty() {
        return String::new();
    }
    let mut projects: Vec<ProjectRepoConfig> = Vec::new();
    for id in child_ids {
        let child = match rc.client.task_detail(id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(task_id = id, error = %e, "dependency repos: could not fetch child");
                continue;
            }
        };
        if child.project_id == detail.project_id || projects.iter().any(|p| p.project_id == child.project_id) {
            continue;
        }
        if let Some(cfg) = crate::client::project_repo_config(&rc.client, child.project_id).await {
            projects.push(cfg);
        }
    }
    if projects.is_empty() {
        return String::new();
    }
    let branch = workspace::integration_branch_name(detail.id, &detail.title);
    let mut s = String::from(DEPENDENCY_REPOS_INTRO);
    for project in &projects {
        let tip = workspace::ls_remote_tip(&rc.config.workspace_root, &project.repo_url, &branch)
            .await
            .unwrap_or_else(|| "unknown (ls-remote failed — fetch it yourself)".to_string());
        s.push_str(&format!(
            "- {} (project #{}), branch `{branch}`, tip `{tip}`\n",
            project.repo_url, project.project_id
        ));
    }
    s.push('\n');
    s
}

/// The `.refs/` block (docs/specs/cross-repo-projects.md): rendered only when
/// the project has reference repos configured. Both run kinds get the
/// read-only rule; the plan run additionally gets the cross-repo splitting
/// convention.
fn refs_section(kind: RunKind, refs: &[String]) -> String {
    let names = refs
        .iter()
        .map(|r| format!("`.refs/{r}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!(
        "## Reference repositories\nRead-only copies of the project's dependent repositories are mounted at \
         {names} — read them freely, but never create, edit, or delete anything in them: they are detached \
         checkouts that land in no PR, and any change there is lost. If the task requires changes in a \
         reference repository, that is a separate ticket in that repository's project, not this one.\n\n"
    );
    if kind == RunKind::Plan {
        s.push_str(
            "Cross-repo splitting convention: a task that spans several repositories is planned as a chain of \
             per-repo tickets linked with `blocks`: (1) the contract/API in the provider repo, (2) the \
             implementation in the consumer repo, (3) the pin-bump plus integration tests in the first repo. \
             Every such ticket must be holistic (builds, tests green, deployable) and self-contained (the \
             contract is quoted in its description). Never create cross-project tickets yourself — propose \
             their text in the plan or a comment for the human.\n\n",
        );
    }
    s
}

/// Pure prompt assembly, split out from `build_prompt` for tests. Every
/// section fed from history is bounded (spec §5.6), so the prompt size is
/// independent of the number of failed runs and the thread length — an
/// unbounded thread once grew the prompt past the model's context limit,
/// making every retry fail before the agent even started.
fn render_prompt(
    kind: RunKind,
    detail: &TaskDetail,
    plan: Option<&str>,
    current_run_id: i32,
    base_url: &str,
    refs: &[String],
    deps_section: &str,
) -> String {
    let mut p = String::new();
    p.push_str("## Ticket\n");
    // The id lets the agent reference its own ticket (comments, MCP calls)
    // without a list_my_tasks round-trip.
    p.push_str(&format!("Ticket #{}\n\n", detail.id));
    // The title is the short summary (branch names and PR titles derive from
    // it); the description remains the full body.
    p.push_str(&format!("{}\n\n", detail.title));
    p.push_str(&format!("{}\n\n", detail.description));
    p.push_str(&format!(
        "Project: {} — Feature: {}\n\n",
        detail.project_name, detail.feature_description
    ));
    if !detail.actions.is_empty() {
        p.push_str("## Actions\n");
        let rejected: Vec<&crate::client::ActionItem> =
            detail.actions.iter().filter(|a| a.status == "rejected").collect();
        for a in &detail.actions {
            if a.status == "rejected" {
                continue;
            }
            let mark = match a.status.as_str() {
                "completed" => "x",
                _ => " ",
            };
            p.push_str(&format!("- [{mark}] #{}: {}\n", a.id, a.description));
        }
        if !rejected.is_empty() {
            match kind {
                RunKind::Plan => {
                    for a in rejected {
                        let reason = a
                            .rejection_reason
                            .as_deref()
                            .filter(|r| !r.trim().is_empty())
                            .unwrap_or("rejected");
                        p.push_str(&format!("- [~] #{}: {} (rejected: {})\n", a.id, a.description, reason));
                    }
                }
                RunKind::Implement => {
                    let ids = rejected
                        .iter()
                        .map(|a| format!("#{}", a.id))
                        .collect::<Vec<_>>()
                        .join(", ");
                    p.push_str(&format!(
                        "_… {} rejected action(s) omitted ({})._\n",
                        rejected.len(),
                        ids
                    ));
                }
                // Supervise/review runs never render through this prompt builder.
                RunKind::Supervise | RunKind::Review => {}
            }
        }
        p.push('\n');
    }
    // The ticket's structured questions with the human's answers (spec §5.6):
    // the re-plan run reads the answers here, and the implement run is nudged
    // to delete questions that are obsolete or already answered.
    if let Some(questions) = detail.questions.as_ref().filter(|q| !q.is_empty()) {
        p.push_str("## Open questions\n");
        for q in questions {
            let kind = if q.multiple { "multi-choice" } else { "single-choice" };
            p.push_str(&format!("- #{} [{}] {} ({kind})\n", q.id, q.status, q.body));
            if !q.options.is_empty() {
                let opts = q
                    .options
                    .iter()
                    .map(|o| {
                        if o.selected {
                            format!("**{}** (selected)", o.body)
                        } else {
                            o.body.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                p.push_str(&format!("  options: {opts}\n"));
            }
            if let Some(text) = q.answer_text.as_deref().filter(|t| !t.trim().is_empty()) {
                p.push_str(&format!("  answer: {}\n", text.trim()));
            }
        }
        match kind {
            RunKind::Plan => p.push_str(
                "\nUnanswered questions above are still waiting on the human — register new ones with \
                 `add_task_question` and delete resolved ones with `delete_task_question`.\n\n",
            ),
            RunKind::Implement => p.push_str(
                "\nThe answers above are final. Delete questions that are obsolete or already answered \
                 via `delete_task_question` (taskId, questionId) to keep the context clean.\n\n",
            ),
            // Review runs are read-only like supervise: questions are shown
            // for context but the reviewer must not be nudged to edit them.
            RunKind::Supervise | RunKind::Review => {}
        }
    }
    // Earlier plans/summaries/errors, newest first, capped (spec §5.6). The
    // live run's own row is filtered out (see the doc comment above). Each
    // outcome is quoted truncated: run rows written before the write-side cap
    // (or by hand) may carry unbounded error text.
    if let Some(runs) = detail.runs.as_ref().filter(|r| !r.is_empty()) {
        let prior: Vec<_> = runs.iter().filter(|r| r.id != current_run_id).take(5).collect();
        if !prior.is_empty() {
            p.push_str("## Prior runs\n");
            for r in prior {
                let outcome = match (r.plan.as_deref(), r.summary.as_deref(), r.error.as_deref()) {
                    (Some(plan), _, _) => format!("plan:\n{}", truncate_with_marker(plan, MAX_PRIOR_OUTCOME_BYTES)),
                    (_, Some(summary), _) => {
                        format!("summary:\n{}", truncate_with_marker(summary, MAX_PRIOR_OUTCOME_BYTES))
                    }
                    (_, _, Some(error)) => format!("error: {}", truncate_with_marker(error, MAX_PRIOR_OUTCOME_BYTES)),
                    _ => "no recorded outcome".to_string(),
                };
                p.push_str(&format!(
                    "### {} run #{} (attempt {}) — {}\n{}\n\n",
                    r.kind, r.id, r.attempt, r.status, outcome
                ));
            }
        }
    }
    // The newest N comments, chronological (spec §5.6): human corrections,
    // open-question answers, daemon notes. Older comments are dropped with a
    // marker rather than letting the thread grow the prompt without bound.
    if let Some(comments) = detail.comments.as_ref().filter(|c| !c.is_empty()) {
        p.push_str("## Conversation\n");
        let skipped_by_count = comments.len().saturating_sub(MAX_CONVERSATION_COMMENTS);
        let candidates = &comments[skipped_by_count..];
        // Pick comments from newest to oldest until the section budget is used.
        // The effective byte cost is the untruncated body length, capped at the
        // per-comment limit, because that's what each entry contributes.
        let mut remaining = MAX_CONVERSATION_BYTES;
        let mut selected_start = candidates.len();
        for (i, c) in candidates.iter().enumerate().rev() {
            let cost = c.body.len().min(MAX_COMMENT_BYTES);
            if cost > remaining {
                break;
            }
            remaining -= cost;
            selected_start = i;
        }
        let skipped = skipped_by_count + selected_start;
        if skipped > 0 {
            p.push_str(&format!(
                "_… {skipped} earlier comment(s) omitted (thread truncated to fit the prompt budget)._\n"
            ));
        }
        for c in &candidates[selected_start..] {
            p.push_str(&format!("- {}\n", truncate_with_marker(&c.body, MAX_COMMENT_BYTES)));
        }
        p.push('\n');
    }
    // Language policy (both run kinds): reasoning in English is cheaper
    // (Cyrillic tokenizes ~2-3x worse) and the strongest for the model, but
    // everything the human reads mirrors the ticket's language.
    p.push_str(
        "## Language\nThink and reason in English — it is more token-efficient. Write everything the \
         human reads in the ticket's language: the plan, the report, and comments. \
         Code, commit messages, and tool calls stay in English.\n\n",
    );
    if !refs.is_empty() {
        p.push_str(&refs_section(kind, refs));
    }
    if !deps_section.is_empty() {
        p.push_str(deps_section);
    }
    match kind {
        RunKind::Plan => p.push_str(PLAN_INSTRUCTIONS),
        RunKind::Implement => {
            if let Some(plan) = plan {
                p.push_str(&format!("## Approved plan\n{plan}\n\n"));
            }
            p.push_str(&attachments_section(
                base_url,
                detail.attachments.as_deref().unwrap_or(&[]),
            ));
            p.push_str(
                "## Instructions\nImplement the approved plan. Run the project's checks (e.g. `just lint test` or \
                 the repo's equivalent), keep the ticket's actions up to date via the remoter MCP tools, and \
                 **commit your changes to the current branch** (`git add` + `git commit` with a clear message) \
                 before finishing — the human reviews the branch. Leave the worktree clean — no uncommitted \
                 or untracked leftovers; the daemon checks `git status` after your turn and sends you back to \
                 finish if it is dirty. Never end your turn with background tasks still running — your \
                 turn's end is final: the daemon shuts the agent down and pending tasks are killed, their \
                 completion never arrives. Run long checks in the foreground or wait for them to finish \
                 before your final message. End with a summary of changes.\n\n\
                 Action choreography: use `start_action` when you begin work on an action, \
                 `complete_action` when it is done, and `reject_action` with a reason for any step that \
                 is no longer needed or incorrect. Only one action per task may be active at a time. By the \
                 end of your turn every action must be in a final state (completed or rejected); the daemon \
                 checks this before moving the ticket to review. Add new actions if the plan needs extra steps. \
                 Use `update_action` to correct a step's description or priority, and `delete_action` only \
                 for steps that are pure planning noise and never saw work.\n\n\
                 If the approved plan is a set of tickets, create each one via `create_task` as a child of \
                 this ticket in `backlog`. Keep every created ticket holistic, exactly as planned: \
                 implementing it on its own must leave the project building, all tests passing, and the \
                 result safe to deploy to production. Wire the dependency order between the created \
                 tickets with the `add_link` MCP tool: whenever ticket A must land before ticket B, call \
                 `add_link` with taskId A, relation `blocks`, otherTaskId B (equivalently `blocked_by` \
                 from B's perspective) — these blocker links are how humans and agents see which child \
                 ticket is ready for implementation and which are blocked. Complete the matching action for every created ticket, do not commit \
                 any code, and end your turn — the created child tickets are the run outcome and no PR is opened.\n\n",
            );
            p.push_str(
                "## Task report\nBefore finishing, write the ticket's implementation report via the \
                 `set_task_report` MCP tool (the current report is `get_task`'s `report` field): a short \
                 summary of what was done, what is needed to test it, and what is needed to deploy it to \
                 production. Attach file artifacts via \
                 `add_attachment` with `outcomeId` (from `get_task`'s `reportOutcomeId`) and link them from \
                 the report text as markdown links. Pass the artifact's local path as `filePath` \
                 (preferred) instead of base64-encoding it. Do not link the pull request in the report — it \
                 is created only after you finish, and the daemon attaches it as a separate artifact \
                 (`get_task`'s `prUrl`). For ticket-creation runs, summarize the created child tickets \
                 instead. If you hit tooling or environment problems during the run (missing tools, \
                 network/TLS failures, permission errors, broken project checks), end the report with a \
                 short \"Problems with tools\" bullet list describing each problem in one line; omit the \
                 section entirely if there were none.\n",
            );
        }
        // Supervise/review runs never render through this prompt builder —
        // their prompts are built in supervise.rs / review.rs.
        RunKind::Supervise | RunKind::Review => {}
    }
    p
}

pub(crate) async fn finish_failed(
    client: &RemoterClient,
    task_id: i32,
    run_id: i32,
    error: String,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) {
    // The full error stays in the daemon logs; the run row and the thread get
    // a bounded copy — an unbounded one inflates every next prompt (spec §5.6).
    let error = truncate_with_marker(&error, MAX_ERROR_BYTES);
    note(client, task_id, format!("run #{run_id} failed: {error}")).await;
    let mut body = FinishRunBody::failed(error);
    body.input_tokens = input_tokens;
    body.output_tokens = output_tokens;
    finish_reporting(client, run_id, &body).await;
}

/// Implement-success guard (spec §5.4 step 5): the prompt tells the agent to
/// commit, but nothing enforced it — a run that ends with zero commits on the
/// ticket branch would push an empty branch and the forge rejects the PR
/// create with 422 ("No commits between base and branch"). Count commits ahead
/// of the base ref; zero means the agent never committed, so the attempt
/// failed.
async fn ensure_committed(rc: &RunContext, branch: &str) -> Result<(), String> {
    let wt = workspace::worktree_dir(&rc.config.workspace_root, rc.project.project_id, rc.task.id);
    let base = match base_branch(rc).await {
        Ok(b) => format!("origin/{b}"),
        Err(e) => return Err(format!("commit check failed: {e}")),
    };
    match workspace::commits_ahead(&wt, &base).await {
        Ok(0) => Err(format!(
            "implement run left no commits on {branch} ahead of {base} — the agent must commit its changes before finishing"
        )),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("commit check failed: {e}")),
    }
}

/// Pure core of `child_ticket_ids`: from the ticket's perspective, links to
/// its children carry the `"parent"` relation (the ticket is the parent of
/// the child); only links created by the ticket's assignee count.
fn child_link_ids(assignee_id: i32, links: &[LinkDto]) -> Vec<i32> {
    links
        .iter()
        .filter(|l| l.relation == "parent" && l.created_by == Some(assignee_id))
        .map(|l| l.task_id)
        .collect()
}

/// Links to child tickets created by this ticket's assignee identify a
/// ticket-creation run: the implement run produced child tickets instead of
/// commits. Returns the child ticket ids, or `None` when there are no such
/// links.
async fn child_ticket_ids(client: &RemoterClient, task_id: i32) -> Option<Vec<i32>> {
    let detail = client.task_detail(task_id).await.ok()?;
    let assignee_id = detail.assignee_id?;
    let ids = child_link_ids(assignee_id, detail.links.as_deref().unwrap_or(&[]));
    if ids.is_empty() { None } else { Some(ids) }
}

/// PROD-2 (§4.4), implement success path only: fetch the project's forge
/// config, push the ticket branch to origin, then create-or-reuse the draft
/// PR/MR. `base_override` replaces the project's base branch as the PR target —
/// a cross-project supervised child's PR targets the parent's integration
/// branch, never the base (docs/specs/cross-repo-projects.md). `Ok(None)` = no
/// forge configured (push/PR skipped). Every failure is returned as a one-line
/// message for the thread note and the run row's `pr_error` — it must never
/// fail the run itself.
async fn push_and_create_pr(
    rc: &RunContext,
    branch: &str,
    base_override: Option<&str>,
) -> Result<Option<String>, String> {
    let cfg = rc
        .client
        .forge_config(rc.project.project_id)
        .await
        .map_err(|e| format!("forge config fetch: {e}"))?;
    let Some(kind) = cfg.forge_kind.as_deref() else {
        return Ok(None);
    };
    let kind = forge::ForgeKind::from_token(kind).map_err(|e| e.to_string())?;

    let wt = workspace::worktree_dir(&rc.config.workspace_root, rc.project.project_id, rc.task.id);
    let push_outcome = workspace::push(&wt, branch)
        .await
        .map_err(|e| format!("git push: {e}"))?;
    if push_outcome == workspace::PushOutcome::ForcePushed {
        note(
            &rc.client,
            rc.task.id,
            "branch history was rewritten (agent rebase); force-pushed with `--force-with-lease` \
             — no remote-only commits were lost"
                .to_string(),
        )
        .await;
    }

    // Create-or-reuse: an earlier run of this ticket (a review → implement
    // bounce) may already have opened the PR — record the same URL instead of
    // opening a second one.
    let runs = rc
        .client
        .list_runs(rc.task.id)
        .await
        .map_err(|e| format!("run history fetch: {e}"))?;
    if let Some(url) = runs.iter().find_map(|r| r.pr_url.clone()) {
        return Ok(Some(url));
    }

    let token = cfg
        .forge_token
        .as_deref()
        .ok_or_else(|| "forge token is not configured".to_string())?;
    let base = match base_override {
        Some(b) => b.to_string(),
        None => base_branch(rc).await.map_err(|e| format!("default branch: {e}"))?,
    };
    let url = forge::create_pr(
        rc.client.http(),
        kind,
        cfg.forge_api_url.as_deref(),
        token,
        &rc.project.repo_url,
        &forge::DraftPr {
            branch,
            base: &base,
            task_id: rc.task.id,
            title: &rc.task.title,
            body: &rc.task.description,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(Some(url))
}

/// Resolves the PR target base branch for this project: the configured
/// `base_branch` or the remote's default branch (`origin/HEAD`).
async fn base_branch(rc: &RunContext) -> Result<String, String> {
    let wt = workspace::worktree_dir(&rc.config.workspace_root, rc.project.project_id, rc.task.id);
    match rc.project.base_branch.as_deref() {
        Some(b) => Ok(b.to_string()),
        None => workspace::remote_default_branch(&wt)
            .await
            .map_err(|e| format!("default branch: {e}")),
    }
}

/// Prompt sent back to the agent when the freshly-created/updated PR is not
/// mergeable into its target (PROD-9). The agent must rebase onto origin/<base>,
/// resolve conflicts, run checks, and leave the worktree clean — the daemon
/// pushes after the run.
fn rebase_nudge_prompt(base: &str, branch: &str) -> String {
    format!(
        "The draft PR for branch `{branch}` is not mergeable into `{base}` because of conflicts.
\
\
Fetch the latest `origin/{base}`, then `git rebase origin/{base}` (or an equivalent merge if the project prefers). Resolve all conflicts, run the project's checks (`just lint` and `just test` or the repo's equivalent), commit everything, and leave the worktree clean (`git status --porcelain` empty). **Do not push** — the daemon pushes the branch after your turn ends."
    )
}

/// PROD-9: after a successful implement run created or reused a PR, poll its
/// mergeability. If the forge reports a conflict, send the agent up to
/// `MAX_REBASE_NUDGES` rebase prompts on the resumed session, push after each
/// successful nudge, and recheck. Errors and unresolved conflicts are noted on
/// the thread but never fail the run.
#[allow(clippy::too_many_arguments)]
async fn ensure_pr_mergeable(
    rc: &RunContext,
    run_id: i32,
    logger: &SessionLogger,
    branch: &str,
    pr_url: &str,
    outcome: &mut RunOutcome,
    _timeout: Duration,
    base_override: Option<&str>,
) {
    let task_id = rc.task.id;
    let cfg = match rc.client.forge_config(rc.project.project_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "mergeability check: could not fetch forge config");
            return;
        }
    };
    let Some(kind) = cfg.forge_kind.as_deref() else {
        return;
    };
    let kind = match forge::ForgeKind::from_token(kind) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "mergeability check: unknown forge kind");
            return;
        }
    };
    let Some(token) = cfg.forge_token.as_deref() else {
        return;
    };
    let pr_number = match forge::pr_number_from_url(pr_url) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(task_id, pr_url, error = %e, "mergeability check: unparseable PR URL");
            return;
        }
    };
    let base = match base_override {
        Some(b) => Ok(b.to_string()),
        None => base_branch(rc).await,
    };
    let base = match base {
        Ok(b) => b,
        Err(e) => {
            note(&rc.client, task_id, format!("mergeability check failed: {e}")).await;
            return;
        }
    };
    let state = match forge::wait_for_pr_mergeable(
        rc.client.http(),
        kind,
        cfg.forge_api_url.as_deref(),
        token,
        &rc.project.repo_url,
        pr_number,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            note(&rc.client, task_id, format!("mergeability check failed: {e}")).await;
            return;
        }
    };

    if state.mergeable == forge::Mergeable::Yes {
        tracing::info!(task_id, pr_url, "PR is mergeable");
        return;
    }
    if state.mergeable == forge::Mergeable::Unknown {
        tracing::info!(
            task_id,
            pr_url,
            "PR mergeability still unknown after polling; leaving for review sync"
        );
        return;
    }

    // The forge reports a conflict — ask the agent to rebase. Services come
    // up once for the whole nudge loop (the worktree and runtime are identical
    // across iterations) and down once after, on every exit path (spec §5.8):
    // container mode starts the run container + sidecars (the guard's drop is
    // the down), host mode wraps the loop in devenv up/down. The loop lives in
    // `rebase_nudge_loop` so its early returns cannot skip the down.
    let wt = workspace::worktree_dir(&rc.config.workspace_root, rc.project.project_id, rc.task.id);
    let env = run_env(rc);
    let devenv = workspace::uses_devenv(&wt);
    let containers =
        match start_container_runtime(&rc.config, &rc.image_locks, &rc.project, run_id, &wt, devenv, &env, &[]).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "rebase nudge: container runtime start failed");
                note(
                    &rc.client,
                    task_id,
                    format!("PR is not mergeable into `{base}`: auto-rebase setup failed ({e}) — human help needed"),
                )
                .await;
                return;
            }
        };
    if containers.is_none()
        && devenv
        && let Err(e) = workspace::services_up(&wt, &env).await
    {
        tracing::warn!(task_id, error = %e, "rebase nudge: devenv services up failed");
        note(
            &rc.client,
            task_id,
            format!("PR is not mergeable into `{base}`: auto-rebase setup failed ({e}) — human help needed"),
        )
        .await;
        return;
    }
    rebase_nudge_loop(
        rc,
        run_id,
        logger,
        branch,
        kind,
        cfg.forge_api_url.as_deref(),
        token,
        pr_number,
        &base,
        &wt,
        &env,
        devenv,
        outcome,
    )
    .await;
    if containers.is_none() && devenv {
        workspace::services_down(&wt, &env).await;
    }
}

/// The nudge loop of `ensure_pr_mergeable`: up to `MAX_REBASE_NUDGES` rebase
/// prompts on the resumed session, pushing and rechecking mergeability after
/// each. The caller brackets it with `services_up`/`services_down`; every
/// early return here is covered by that trailing down.
#[allow(clippy::too_many_arguments)]
async fn rebase_nudge_loop(
    rc: &RunContext,
    run_id: i32,
    logger: &SessionLogger,
    branch: &str,
    kind: forge::ForgeKind,
    forge_api_url: Option<&str>,
    token: &str,
    pr_number: u64,
    base: &str,
    wt: &Path,
    env: &[(String, String)],
    devenv: bool,
    outcome: &mut RunOutcome,
) {
    let task_id = rc.task.id;
    let nudge_timeout = Duration::from_secs(rc.config.run_timeout_minutes * 60);

    for _nudge in 1..=MAX_REBASE_NUDGES {
        let prompt = rebase_nudge_prompt(base, branch);
        let nudge_spec = RunSpec {
            task_id,
            run_id,
            cwd: wt.to_path_buf(),
            prompt,
            kind: rc.kind.as_str(),
            resume_session: outcome.session_id.clone(),
            branch: branch.to_string(),
            env: env.to_vec(),
            exec: exec_env(&rc.config, run_id, devenv),
            config_options: rc.config.driver.config_options_for(rc.kind.as_str()),
            logger: logger.clone(),
            phase_tx: None,
        };
        let nudge_outcome = tokio::select! {
            biased;
            _ = rc.cancel.cancelled() => {
                tracing::info!(task_id, "rebase nudge cancelled");
                return;
            }
            r = tokio::time::timeout(nudge_timeout, rc.driver.run(nudge_spec)) => match r {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    tracing::warn!(task_id, error = %e, "rebase nudge: driver failed");
                    note(
                        &rc.client,
                        task_id,
                        format!("PR is not mergeable into `{base}`: auto-rebase failed ({e}) — human help needed"),
                    )
                    .await;
                    return;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        task_id,
                        "rebase nudge timed out after {} minutes",
                        rc.config.run_timeout_minutes
                    );
                    note(
                        &rc.client,
                        task_id,
                        format!("PR is not mergeable into `{base}`: auto-rebase timed out — human help needed"),
                    )
                    .await;
                    return;
                }
            }
        };

        match workspace::dirty_status(wt).await {
            Ok(s) if !s.is_empty() => {
                tracing::warn!(task_id, "rebase nudge: worktree still dirty");
                note(
                    &rc.client,
                    task_id,
                    format!("PR is not mergeable into `{base}`: auto-rebase left a dirty worktree — human help needed"),
                )
                .await;
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(task_id, error = %e, "rebase nudge: dirty status check failed");
                note(
                    &rc.client,
                    task_id,
                    format!(
                        "PR is not mergeable into `{base}`: auto-rebase verification failed ({e}) — human help needed"
                    ),
                )
                .await;
                return;
            }
        }

        outcome.session_id = nudge_outcome.session_id.clone().or(outcome.session_id.clone());
        if !nudge_outcome.text.trim().is_empty() {
            outcome.text = nudge_outcome.text.clone();
        }
        outcome.input_tokens = sum_tokens(outcome.input_tokens, nudge_outcome.input_tokens);
        outcome.output_tokens = sum_tokens(outcome.output_tokens, nudge_outcome.output_tokens);

        if let Err(e) = workspace::push(wt, branch).await {
            tracing::warn!(task_id, error = %e, "rebase nudge: push failed");
            note(
                &rc.client,
                task_id,
                format!("PR is not mergeable into `{base}`: auto-rebase push failed ({e}) — human help needed"),
            )
            .await;
            return;
        }

        let state = match forge::pr_mergeable(
            rc.client.http(),
            kind,
            forge_api_url,
            token,
            &rc.project.repo_url,
            pr_number,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                note(&rc.client, task_id, format!("mergeability check failed: {e}")).await;
                return;
            }
        };
        if state.mergeable == forge::Mergeable::Yes {
            note(
                &rc.client,
                task_id,
                format!("PR conflicted with `{base}`; the agent rebased, the branch was force-pushed, and the PR is now mergeable"),
            )
            .await;
            return;
        }
        if state.mergeable == forge::Mergeable::Unknown {
            tracing::info!(task_id, "rebase nudge: mergeability still unknown");
            return;
        }
        // `No` → try another nudge, up to the limit.
    }

    note(
        &rc.client,
        task_id,
        format!("PR is not mergeable into `{base}`: auto-rebase did not resolve conflicts after {MAX_REBASE_NUDGES} attempts — human help needed"),
    )
    .await;
}

/// Best-effort live phase update on the run row (#154): `active` / `stalled` /
/// `cancelling` / `retrying`. Finishing clears the phase server-side, except
/// `retrying`, which survives so the inter-attempt backoff stays visible. Like
/// `note`, a failure here must never affect the run itself.
async fn set_phase(rc: &RunContext, run_id: i32, phase: &str) {
    if let Err(e) = rc.client.set_run_phase(run_id, Some(phase)).await {
        tracing::warn!(run_id, phase, error = %e, "could not set run phase");
    }
}

/// Best-effort daemon note on the ticket thread (spec §5.6): the conversation
/// a human reads — and the next prompt is built from — stays complete. A
/// failure here must never affect the run itself.
pub(crate) async fn note(client: &RemoterClient, task_id: i32, body: String) {
    if let Err(e) = client.add_comment(task_id, &body).await {
        tracing::warn!(task_id, error = %e, "could not write run note to the ticket thread");
    }
}

pub(crate) async fn finish_reporting(client: &RemoterClient, run_id: i32, body: &FinishRunBody) {
    if let Err(e) = client.finish_run(run_id, body).await {
        // Losing the run-row update must not crash the loop, but it leaves the
        // row `running` — the next daemon start reconciles it (spec §5.7).
        tracing::error!(run_id, error = %e, "failed to report run outcome");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{AgentRunDto, CommentDto};

    /// The `db-up` contract is keyed on the worktree carrying a justfile:
    /// without one the container-mode `just db-up` step is skipped (it would
    /// die on `just: command not found`, exit 127, inside the run container);
    /// with one it runs and any failure (e.g. a missing `db-up` recipe) stays
    /// a hard error.
    #[test]
    fn worktree_has_justfile_detects_contract() {
        let dir = std::env::temp_dir().join(format!("remoter-run-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!worktree_has_justfile(&dir));
        std::fs::write(dir.join("justfile"), "").unwrap();
        assert!(worktree_has_justfile(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The plan prompt must NOT ask for a text "Open questions" section —
    /// open questions are registered as structured questions via
    /// `add_task_question` (spec §5.6) and answered in the UI.
    #[test]
    fn plan_instructions_register_open_questions_via_mcp_tool() {
        assert!(PLAN_INSTRUCTIONS.contains("PLAN-ONLY mode"));
        assert!(PLAN_INSTRUCTIONS.contains("`add_task_question`"));
        assert!(
            PLAN_INSTRUCTIONS.contains("never write an \"Open questions\" section into the plan text"),
            "the plan text must not carry an Open questions section anymore"
        );
    }

    /// The plan must spell out the implementation-step outcome in a "Plan"
    /// section (spec §5.6): code changes on the branch or a set of child
    /// tickets — so the human approves the outcome shape, not just the steps.
    #[test]
    fn plan_instructions_require_plan_section_with_implementation_outcome() {
        assert!(PLAN_INSTRUCTIONS.contains("\"Plan\" section"));
        assert!(PLAN_INSTRUCTIONS.contains("expected outcome"));
        assert!(PLAN_INSTRUCTIONS.contains("code changes on this ticket's branch or a set of child tickets"));
    }

    /// Ticket-creation outcomes must be holistic (spec §5.6): each child
    /// ticket alone leaves the project building, tests green, and the result
    /// safe to deploy. Both prompts carry the rule — the plan shapes the
    /// tickets, the implement run creates them.
    #[test]
    fn ticket_creation_outcome_must_be_holistic() {
        for kind in [RunKind::Plan, RunKind::Implement] {
            let p = render_prompt(kind, &detail(vec![], vec![]), None, 999, "http://api", &[], "");
            assert!(p.contains("holistic"), "{kind:?}: {p}");
            assert!(p.contains("all tests passing"), "{kind:?}: {p}");
            assert!(p.contains("safe to deploy to production"), "{kind:?}: {p}");
        }
    }

    /// Child tickets must be wired with blocker links (spec §5.6): the plan
    /// states the dependency order, the implement run creates `blocks` /
    /// `blocked_by` links via `add_link` so humans and agents can tell which
    /// child ticket is ready for implementation and which are blocked.
    #[test]
    fn ticket_creation_outcome_wires_blocker_links() {
        let p = render_prompt(RunKind::Plan, &detail(vec![], vec![]), None, 999, "http://api", &[], "");
        assert!(p.contains("which ticket blocks which"), "{p}");

        for kind in [RunKind::Plan, RunKind::Implement] {
            let p = render_prompt(kind, &detail(vec![], vec![]), None, 999, "http://api", &[], "");
            assert!(p.contains("`add_link`"), "{kind:?}: {p}");
            assert!(p.contains("ready for implementation"), "{kind:?}: {p}");
        }
        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("relation `blocks`"), "{p}");
        assert!(p.contains("`blocked_by`"), "{p}");
    }

    /// The `.refs/` block (docs/specs/cross-repo-projects.md): with reference
    /// repos configured, both prompts name the mounts and forbid changes in
    /// them (they land in no PR); changes in a reference repo are a separate
    /// ticket of that repo's project.
    #[test]
    fn refs_block_marks_reference_repos_read_only() {
        let refs = vec!["contracts".to_string(), "backend".to_string()];
        for kind in [RunKind::Plan, RunKind::Implement] {
            let p = render_prompt(kind, &detail(vec![], vec![]), None, 999, "http://api", &refs, "");
            assert!(p.contains("## Reference repositories"), "{kind:?}: {p}");
            assert!(p.contains("`.refs/contracts`"), "{kind:?}: {p}");
            assert!(p.contains("`.refs/backend`"), "{kind:?}: {p}");
            assert!(
                p.contains("never create, edit, or delete anything in them"),
                "{kind:?}: {p}"
            );
            assert!(p.contains("land in no PR"), "{kind:?}: {p}");
            assert!(
                p.contains("a separate ticket in that repository's project"),
                "{kind:?}: {p}"
            );
        }
    }

    /// The cross-repo splitting convention rides the plan prompt only: a
    /// multi-repo task is a chain of per-repo tickets linked with `blocks`
    /// (contract → consumer → pin-bump), each holistic and self-contained,
    /// and the agent never creates cross-project tickets itself.
    #[test]
    fn refs_block_carries_cross_repo_splitting_convention_in_plan_only() {
        let refs = vec!["contracts".to_string()];
        let p = render_prompt(
            RunKind::Plan,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &refs,
            "",
        );
        assert!(p.contains("Cross-repo splitting convention"), "{p}");
        assert!(p.contains("linked with `blocks`"), "{p}");
        assert!(p.contains("contract/API in the provider repo"), "{p}");
        assert!(p.contains("consumer repo"), "{p}");
        assert!(p.contains("pin-bump"), "{p}");
        assert!(p.contains("self-contained"), "{p}");
        assert!(p.contains("Never create cross-project tickets yourself"), "{p}");

        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &refs,
            "",
        );
        assert!(!p.contains("Cross-repo splitting convention"), "{p}");
    }

    /// No configured reference repos → no `.refs/` block at all: projects
    /// without refs see the same prompts as before the feature.
    #[test]
    fn refs_block_absent_without_configured_refs() {
        for kind in [RunKind::Plan, RunKind::Implement] {
            let p = render_prompt(kind, &detail(vec![], vec![]), None, 999, "http://api", &[], "");
            assert!(!p.contains("## Reference repositories"), "{kind:?}: {p}");
            assert!(!p.contains(".refs/"), "{kind:?}: {p}");
        }
    }

    /// The "## Dependency repos" block (cross-project children, #184): the
    /// intro states the isolation invariant and the pin/integration-test
    /// instructions; the block renders only when the section is non-empty.
    #[test]
    fn dependency_repos_block_contract() {
        assert!(
            DEPENDENCY_REPOS_INTRO.starts_with("## Dependency repos\n"),
            "{DEPENDENCY_REPOS_INTRO}"
        );
        assert!(
            DEPENDENCY_REPOS_INTRO.contains("integration branch"),
            "{DEPENDENCY_REPOS_INTRO}"
        );
        assert!(
            DEPENDENCY_REPOS_INTRO.contains("human-merged integration PR"),
            "{DEPENDENCY_REPOS_INTRO}"
        );
        assert!(
            DEPENDENCY_REPOS_INTRO.contains("`cargo update -p <dep> --precise <rev>`"),
            "{DEPENDENCY_REPOS_INTRO}"
        );
        assert!(
            DEPENDENCY_REPOS_INTRO.contains("flake input"),
            "{DEPENDENCY_REPOS_INTRO}"
        );
        assert!(
            DEPENDENCY_REPOS_INTRO.contains("integration tests"),
            "{DEPENDENCY_REPOS_INTRO}"
        );

        let deps = format!(
            "{DEPENDENCY_REPOS_INTRO}- https://forge/child (project #2), branch `agent/feature-1-x`, tip `abc`\n\n"
        );
        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &[],
            &deps,
        );
        assert!(p.contains("## Dependency repos"), "{p}");
        assert!(p.contains("https://forge/child (project #2)"), "{p}");

        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(!p.contains("Dependency repos"), "{p}");
    }

    fn comment(id: i32, body: String) -> CommentDto {
        CommentDto {
            id,
            task_id: 1,
            author_id: 1,
            body,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn run_row(id: i32, kind: &str, status: &str, session_id: Option<String>, error: Option<String>) -> AgentRunDto {
        AgentRunDto {
            id,
            task_id: 1,
            agent_user_id: 1,
            kind: kind.to_string(),
            status: status.to_string(),
            session_id,
            branch: None,
            plan: None,
            summary: None,
            error,
            pr_url: None,
            pr_error: None,
            input_tokens: None,
            output_tokens: None,
            attempt: 1,
            phase: None,
            created_at: None,
        }
    }

    fn detail(comments: Vec<CommentDto>, runs: Vec<AgentRunDto>) -> TaskDetail {
        TaskDetail {
            id: 1,
            project_id: 1,
            project_name: "proj".to_string(),
            feature_id: 1,
            feature_description: "feat".to_string(),
            title: "ticket title".to_string(),
            description: "ticket".to_string(),
            task_status: "implement".to_string(),
            task_priority: None,
            blocked: false,
            assignee_id: None,
            assignee_name: None,
            actions: vec![],
            runs: Some(runs),
            comments: Some(comments),
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
            goal_id: None,
        }
    }

    /// The Ticket section leads with the task title (the short summary the
    /// branch name and PR title derive from), then the full description body.
    #[test]
    fn ticket_section_leads_with_title() {
        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(
            p.starts_with("## Ticket\nTicket #1\n\nticket title\n\nticket\n\n"),
            "{p}"
        );
    }

    /// The Language section steers both run kinds: reasoning in English
    /// (token efficiency), ticket-facing prose in the ticket's language.
    #[test]
    fn language_section_is_in_both_prompts() {
        for kind in [RunKind::Plan, RunKind::Implement] {
            let p = render_prompt(kind, &detail(vec![], vec![]), None, 999, "http://api", &[], "");
            assert!(p.contains("## Language\n"), "{kind:?}: {p}");
            assert!(p.contains("Think and reason in English"), "{kind:?}: {p}");
            assert!(p.contains("ticket's language"), "{kind:?}: {p}");
        }
    }

    /// Both prompts forbid ending the turn with pending background tasks:
    /// the daemon treats turn end as final and shuts the agent down, so a
    /// backgrounded check's completion would never arrive (task 26 incident:
    /// an implement run ended mid-`just test`, leaving the worktree dirty).
    #[test]
    fn no_pending_background_tasks_rule_is_in_both_prompts() {
        for kind in [RunKind::Plan, RunKind::Implement] {
            let p = render_prompt(kind, &detail(vec![], vec![]), None, 999, "http://api", &[], "");
            assert!(
                p.contains("Never end your turn with background tasks still running"),
                "{kind:?}: {p}"
            );
        }
    }

    /// Implement runs must finalize the ticket's report via the
    /// `set_task_report` MCP tool; plan runs stay read-only (no report write).
    #[test]
    fn task_report_section_is_implement_only() {
        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("## Task report\n"), "{p}");
        assert!(p.contains("`set_task_report`"), "{p}");
        // Report artifacts attach to the outcome, not the task.
        assert!(p.contains("`outcomeId`"), "{p}");
        assert!(p.contains("`reportOutcomeId`"), "{p}");
        // Agents must surface tooling/environment problems in the report,
        // but omit the section entirely when there were none.
        assert!(p.contains("\"Problems with tools\" bullet list"), "{p}");
        assert!(p.contains("omit the section entirely if there were none"), "{p}");

        let p = render_prompt(RunKind::Plan, &detail(vec![], vec![]), None, 999, "http://api", &[], "");
        assert!(!p.contains("## Task report"), "{p}");
        assert!(!p.contains("set_task_report"), "{p}");
        assert!(!p.contains("Problems with tools"), "{p}");
    }

    /// The task_id=11 failure mode (spec §5.6): a long failure history must not
    /// grow the prompt — the Conversation section keeps only the newest
    /// comments, with a marker, no matter how long the thread gets.
    #[test]
    fn conversation_is_capped_to_last_n_comments() {
        let comments: Vec<_> = (1..=(MAX_CONVERSATION_COMMENTS as i32 + 10))
            .map(|i| comment(i, format!("note {i}")))
            .collect();
        let p = render_prompt(
            RunKind::Implement,
            &detail(comments, vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("earlier comment(s) omitted"), "{p}");
        assert!(!p.contains("- note 1\n"), "oldest comments must be dropped: {p}");
        assert!(
            p.contains(&format!("- note {}", MAX_CONVERSATION_COMMENTS as i32 + 10)),
            "newest comments must be kept: {p}"
        );
    }

    #[test]
    fn short_thread_is_not_marked_truncated() {
        let comments = vec![comment(1, "hello".to_string())];
        let p = render_prompt(
            RunKind::Implement,
            &detail(comments, vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("- hello\n"));
        assert!(!p.contains("omitted"), "{p}");
    }

    #[test]
    fn long_comment_body_is_truncated() {
        let comments = vec![comment(1, "x".repeat(10 * 1024))];
        let p = render_prompt(
            RunKind::Implement,
            &detail(comments, vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("… [truncated, 10240 bytes total]"), "{p}");
        // The new per-comment cap is 8 KiB, so the prefix kept in the prompt is
        // larger than the old 2 KiB limit.
        assert!(p.contains(&"x".repeat(8 * 1024)), "{p}");
        assert!(!p.contains(&"x".repeat(8 * 1024 + 1)), "{p}");
    }

    /// The conversation section also has a total budget, enforced from newest to
    /// oldest. Bodies over the per-comment cap count at their cap for budgeting,
    /// so 10 comments of 8 KiB effective size (80 KiB total) must drop the oldest.
    #[test]
    fn conversation_respects_total_budget_from_newest_to_oldest() {
        let comments: Vec<_> = (1..=10)
            .map(|i| comment(i, format!("comment {i}\n{}", "x".repeat(8 * 1024))))
            .collect();
        let p = render_prompt(
            RunKind::Implement,
            &detail(comments, vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("earlier comment(s) omitted"), "{p}");
        assert!(
            !p.contains("- comment 1\n"),
            "oldest comments must be dropped by the budget: {p}"
        );
        assert!(
            !p.contains("- comment 2\n"),
            "oldest comments must be dropped by the budget: {p}"
        );
        assert!(p.contains("- comment 10\n"), "newest comment must be kept: {p}");
        assert!(p.contains("- comment 9\n"), "recent comments must be kept: {p}");
    }

    /// Run rows written before the write-side cap may carry unbounded errors —
    /// the Prior runs section quotes them truncated (spec §5.6).
    #[test]
    fn prior_run_error_is_truncated() {
        let runs = vec![run_row(5, "implement", "failed", None, Some("e".repeat(50 * 1024)))];
        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], runs),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("error: "));
        assert!(p.contains("… [truncated, 51200 bytes total]"), "{p}");
        assert!(!p.contains(&"e".repeat(MAX_PRIOR_OUTCOME_BYTES + 1)), "{p}");
    }

    #[test]
    fn truncate_with_marker_respects_char_boundaries() {
        let short = "ok";
        assert_eq!(truncate_with_marker(short, 100), short);
        // 'é' is two bytes — cutting at byte 3 would split it.
        let text = "abécd";
        let t = truncate_with_marker(text, 3);
        assert!(t.starts_with("ab\n… [truncated, 6 bytes total]"), "{t}");
    }

    /// The implement prompt's Attachments section: stable markdown download
    /// links plus the MCP-tool rules (docs/specs/attachments.md §10); omitted
    /// when the task has no attachments.
    #[test]
    fn attachments_section_lists_links_and_tool_rules() {
        let atts = vec![
            AttachmentDto {
                id: 42,
                file_name: "design.png".into(),
                content_type: "image/png".into(),
                size_bytes: 1024,
            },
            AttachmentDto {
                id: 7,
                file_name: "notes.md".into(),
                content_type: "text/markdown".into(),
                size_bytes: 50,
            },
        ];
        let s = attachments_section("http://api:8181/", &atts);
        assert!(s.starts_with("## Attachments\n"));
        assert!(s.contains("- [design.png](http://api:8181/api/v1/attachments/42/download)\n"));
        assert!(s.contains("- [notes.md](http://api:8181/api/v1/attachments/7/download)\n"));
        assert!(s.contains("`read_attachment`"));
        assert!(s.contains("`add_attachment`"));
        assert!(s.contains("cannot delete attachments"));
        assert!(s.contains("`filePath`"), "must instruct agents to prefer filePath: {s}");
        assert!(s.contains("contentBase64"), "must mention contentBase64 fallback: {s}");

        assert!(attachments_section("http://api:8181", &[]).is_empty());
    }

    /// Hybrid resume strategy (spec §5.4): the newest run decides. A bounce
    /// review → implement resumes the succeeded implement session; a re-plan
    /// resumes the succeeded plan session.
    #[test]
    fn resume_session_for_bounce_and_replan() {
        let impl_ok = run_row(3, "implement", "succeeded", Some("sess-impl".into()), None);
        let plan_ok = run_row(2, "plan", "succeeded", Some("sess-plan".into()), None);

        // Bounce review → implement: newest run is the succeeded implement run.
        assert_eq!(
            resume_session_for(RunKind::Implement, &[impl_ok.clone(), plan_ok.clone()]),
            Some("sess-impl".to_string())
        );
        // Re-plan after reject: newest run is the succeeded plan run.
        assert_eq!(
            resume_session_for(RunKind::Plan, &[plan_ok]),
            Some("sess-plan".to_string())
        );
    }

    /// Plan → implement and a plan after a succeeded implement run start
    /// fresh: the newest run's kind doesn't match the run about to start.
    #[test]
    fn resume_session_for_cross_kind_is_fresh() {
        let plan_ok = run_row(2, "plan", "succeeded", Some("sess-plan".into()), None);
        assert_eq!(resume_session_for(RunKind::Implement, &[plan_ok]), None);

        let impl_ok = run_row(3, "implement", "succeeded", Some("sess-impl".into()), None);
        assert_eq!(resume_session_for(RunKind::Plan, &[impl_ok]), None);
    }

    /// A retry after a failure never resumes: the newest run is failed, even
    /// when an older succeeded run of the same kind has a session.
    #[test]
    fn resume_session_for_retry_after_failure_is_fresh() {
        for kind in [RunKind::Plan, RunKind::Implement] {
            let runs = vec![
                run_row(4, kind.as_str(), "failed", None, Some("boom".into())),
                run_row(3, kind.as_str(), "succeeded", Some("sess-old".into()), None),
            ];
            assert_eq!(resume_session_for(kind, &runs), None, "{kind:?}");
        }
    }

    /// A succeeded newest run without a recorded session id has nothing to
    /// resume; an empty history starts fresh.
    #[test]
    fn resume_session_for_missing_session_or_history_is_fresh() {
        let impl_ok = run_row(3, "implement", "succeeded", None, None);
        assert_eq!(resume_session_for(RunKind::Implement, &[impl_ok]), None);
        assert_eq!(resume_session_for(RunKind::Implement, &[]), None);
        assert_eq!(resume_session_for(RunKind::Plan, &[]), None);
    }

    /// The rebase nudge prompt names the base branch, instructs the agent to
    /// rebase onto origin/<base>, run checks, leave the worktree clean, and
    /// explicitly forbids pushing.
    #[test]
    fn rebase_nudge_prompt_contains_required_instructions() {
        let p = rebase_nudge_prompt("main", "agent/task-42-fix");
        assert!(p.contains("agent/task-42-fix"), "{p}");
        assert!(p.contains("origin/main"), "{p}");
        assert!(p.contains("git rebase"), "{p}");
        assert!(p.contains("Do not push"), "{p}");
        assert!(p.contains("just lint"), "{p}");
        assert!(p.contains("just test"), "{p}");
        assert!(p.contains("worktree clean"), "{p}");
    }

    fn action_item(id: i32, description: &str, status: &str) -> crate::client::ActionItem {
        crate::client::ActionItem {
            id,
            description: description.to_string(),
            status: status.to_string(),
            rejection_reason: None,
        }
    }

    fn detail_with_actions(actions: Vec<crate::client::ActionItem>) -> TaskDetail {
        let mut d = detail(vec![], vec![]);
        d.actions = actions;
        d
    }

    /// The actions section renders completed actions with `[x]`, rejected with
    /// `[~]` and a reason, and non-final with `[ ]`.
    #[test]
    fn render_prompt_actions_section_marks_final_states() {
        let actions = vec![
            action_item(1, "done", "completed"),
            action_item(2, "dropped", "rejected"),
            action_item(3, "todo", "inactive"),
        ];
        let p = render_prompt(
            RunKind::Implement,
            &detail_with_actions(actions),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("- [x] #1: done"), "{p}");
        assert!(
            !p.contains("- [~] #2: dropped"),
            "implement should collapse rejected actions: {p}"
        );
        assert!(p.contains("_… 1 rejected action(s) omitted (#2)._"), "{p}");
        assert!(p.contains("- [ ] #3: todo"), "{p}");
    }

    /// Plan prompts keep the full rejected-action reasons so the planner can see
    /// why previous steps were discarded.
    #[test]
    fn render_prompt_plan_shows_rejected_reasons() {
        let mut a = action_item(2, "dropped", "rejected");
        a.rejection_reason = Some("no longer needed".to_string());
        let actions = vec![
            action_item(1, "done", "completed"),
            a,
            action_item(3, "todo", "inactive"),
        ];
        let p = render_prompt(
            RunKind::Plan,
            &detail_with_actions(actions),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("- [x] #1: done"), "{p}");
        assert!(p.contains("- [~] #2: dropped (rejected: no longer needed)"), "{p}");
        assert!(p.contains("- [ ] #3: todo"), "{p}");
    }

    /// The questions section renders every structured question with its status,
    /// options (selected ones marked), and free-text answer; the implement
    /// prompt additionally nudges to delete obsolete questions (spec §5.6).
    #[test]
    fn render_prompt_questions_section_shows_answers() {
        use crate::client::{TaskQuestionDto, TaskQuestionOptionDto};
        let mut d = detail(vec![], vec![]);
        d.questions = Some(vec![
            TaskQuestionDto {
                id: 5,
                body: "Which storage backend?".to_string(),
                multiple: false,
                status: "answered".to_string(),
                answer_text: Some("S3 it is".to_string()),
                options: vec![
                    TaskQuestionOptionDto {
                        body: "local disk".to_string(),
                        selected: false,
                    },
                    TaskQuestionOptionDto {
                        body: "S3".to_string(),
                        selected: true,
                    },
                ],
            },
            TaskQuestionDto {
                id: 6,
                body: "Anything else?".to_string(),
                multiple: true,
                status: "open".to_string(),
                answer_text: None,
                options: vec![],
            },
        ]);
        for kind in [RunKind::Plan, RunKind::Implement] {
            let p = render_prompt(kind, &d, None, 999, "http://api", &[], "");
            assert!(p.contains("## Open questions\n"), "{kind:?}: {p}");
            assert!(
                p.contains("- #5 [answered] Which storage backend? (single-choice)"),
                "{kind:?}: {p}"
            );
            assert!(p.contains("local disk; **S3** (selected)"), "{kind:?}: {p}");
            assert!(p.contains("answer: S3 it is"), "{kind:?}: {p}");
            assert!(p.contains("- #6 [open] Anything else? (multi-choice)"), "{kind:?}: {p}");
        }
        let p = render_prompt(RunKind::Implement, &d, None, 999, "http://api", &[], "");
        assert!(p.contains("`delete_task_question`"), "{p}");
        let p = render_prompt(RunKind::Plan, &d, None, 999, "http://api", &[], "");
        assert!(p.contains("`add_task_question`"), "{p}");
        // No questions → no section at all.
        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(!p.contains("## Open questions"), "{p}");
    }

    /// Plan instructions tell the agent to register steps as actions, reject
    /// outdated ones, and avoid start/complete in plan mode.
    #[test]
    fn plan_instructions_mention_actions() {
        assert!(PLAN_INSTRUCTIONS.contains("add_action"), "{PLAN_INSTRUCTIONS}");
        assert!(PLAN_INSTRUCTIONS.contains("get_task"), "{PLAN_INSTRUCTIONS}");
        assert!(PLAN_INSTRUCTIONS.contains("delete_action"), "{PLAN_INSTRUCTIONS}");
        assert!(PLAN_INSTRUCTIONS.contains("reject_action"), "{PLAN_INSTRUCTIONS}");
        assert!(
            PLAN_INSTRUCTIONS.contains("Do not use `start_action` or `complete_action`"),
            "plan mode must forbid activating/completing actions: {PLAN_INSTRUCTIONS}"
        );
        assert!(
            PLAN_INSTRUCTIONS.contains("unavailable in plan mode"),
            "plan mode must note start/complete are unavailable: {PLAN_INSTRUCTIONS}"
        );
    }

    #[test]
    fn plan_instructions_forbid_start_complete() {
        assert!(
            PLAN_INSTRUCTIONS.contains("Do not use `start_action` or `complete_action`"),
            "{PLAN_INSTRUCTIONS}"
        );
    }

    /// Implement instructions explain the start/complete/reject choreography and
    /// the final-state requirement.
    #[test]
    fn implement_instructions_mention_action_choreography() {
        let p = render_prompt(
            RunKind::Implement,
            &detail(vec![], vec![]),
            None,
            999,
            "http://api",
            &[],
            "",
        );
        assert!(p.contains("start_action"), "{p}");
        assert!(p.contains("complete_action"), "{p}");
        assert!(p.contains("reject_action"), "{p}");
        assert!(p.contains("final state"), "{p}");
        assert!(p.contains("daemon checks"), "{p}");
    }

    /// The action nudge prompt lists every pending action by id and description.
    #[test]
    fn action_nudge_prompt_lists_pending_actions() {
        let a1 = action_item(7, "write migration", "inactive");
        let a2 = action_item(9, "update UI", "active");
        let actions = vec![&a1, &a2];
        let p = action_nudge_prompt(&actions);
        assert!(p.contains("#7: write migration"), "{p}");
        assert!(p.contains("#9: update UI"), "{p}");
        assert!(p.contains("start_action"), "{p}");
        assert!(p.contains("reject_action"), "{p}");
    }

    /// Only completed and rejected actions count as final.
    #[test]
    fn is_action_final_recognizes_terminal_states() {
        assert!(is_action_final("completed"));
        assert!(is_action_final("rejected"));
        assert!(is_action_final("deleted"));
        assert!(!is_action_final("inactive"));
        assert!(!is_action_final("active"));
    }

    fn link(relation: &str, task_id: i32, created_by: Option<i32>) -> LinkDto {
        LinkDto {
            relation: relation.to_string(),
            task_id,
            created_by,
        }
    }

    /// Regression (task #71): from the ticket's perspective, links to its
    /// children carry the `"parent"` relation — the backend resolves link
    /// relations from the viewing task's side. Matching `"subtask"` here never
    /// detects a ticket-creation run and the commit guard fails the run.
    #[test]
    fn child_link_ids_match_parent_relation_from_ticket_perspective() {
        let links = vec![link("parent", 72, Some(7)), link("parent", 73, Some(7))];
        assert_eq!(child_link_ids(7, &links), vec![72, 73]);
    }

    /// A `"subtask"` relation on the ticket's own link list is not a child
    /// (that relation appears only when viewing the child, not the ticket).
    #[test]
    fn child_link_ids_reject_subtask_relation() {
        let links = vec![link("subtask", 72, Some(7))];
        assert!(child_link_ids(7, &links).is_empty());
    }

    /// Children filed by someone else (a human, another agent) don't mark the
    /// run as ticket-creation — only the assignee's own children bypass the
    /// commit guard.
    #[test]
    fn child_link_ids_require_assignee_authorship() {
        let links = vec![link("parent", 72, Some(9)), link("parent", 73, None)];
        assert!(child_link_ids(7, &links).is_empty());
    }

    // ── supervision gate (`supervision_enabled`) ─────────────────────────────

    /// Minimal HTTP/1.1 mock backend (same shape as the claim.rs tests):
    /// serves the queued `(status, body)` responses in request order, one
    /// connection each, and counts accepted requests so a test can assert the
    /// gate stopped the detection early.
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

    fn test_run_context(api_url: &str) -> RunContext {
        // SAFETY: test process env tweak; the env override would otherwise win
        // over the mock URL (spec §5.1).
        unsafe { std::env::remove_var("REMOTER_API_URL") };
        let toml = format!(
            r#"
api_url = "{api_url}"
workspace_root = "/tmp/remoter-agent-run-test"
retry_backoff_secs = 1
cancel_grace_secs = 0

[driver]
kind = "stub"
"#
        );
        let config = Arc::new(Config::from_toml_str(&toml, Some("tok".to_string())).unwrap());
        let log_store = LogStore::new(&config.logs).unwrap();
        RunContext {
            client: RemoterClient::new(api_url, "tok", None),
            config,
            driver: Arc::new(crate::driver::stub::StubDriver::new(0, false)),
            task: TaskSummary {
                id: 42,
                project_id: 1,
                project_name: "proj".into(),
                feature_id: 1,
                feature_description: "feat".into(),
                title: "child ticket".into(),
                description: "child body".into(),
                task_status: "implement".into(),
                task_priority: None,
                actions_total: 0,
                actions_completed: 0,
                time_spent: 0,
                blocked: false,
                agent_review_requested: false,
                goal_id: None,
            },
            project: ProjectRepoConfig {
                project_id: 1,
                repo_url: "https://forge.example/proj.git".into(),
                base_branch: None,
                staging_auto_start: false,
                reference_repos: vec![],
            },
            kind: RunKind::Implement,
            port_block: Some(crate::ports::Ports::default().allocate().unwrap()),
            image_locks: crate::image::ImageLocks::default(),
            cancel: CancellationToken::new(),
            log_store,
        }
    }

    /// A child whose parent has `supervisionEnabled = false` is not
    /// supervised: `supervised_parent` returns `None` right after the parent
    /// detail fetch (before any workspace probing), so the child builds off
    /// master and does its own push/PR like a standalone ticket.
    #[tokio::test]
    async fn supervised_parent_returns_none_when_parent_supervision_disabled() {
        let (url, hits) = spawn_counting_backend(vec![
            (
                200,
                r#"{"id":7,"name":"bot","kind":"agent","workspaces":[{"id":1,"name":"Personal","role":"owner"}]}"#,
            ),
            (
                200,
                r#"{
                "id": 42, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "child ticket",
                "description": "child body", "taskStatus": "implement", "assigneeId": 7,
                "links": [{"relation": "subtask", "taskId": 10, "createdBy": 7}]
            }"#,
            ),
            (
                200,
                r#"{
                "id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "parent ticket",
                "description": "parent body", "taskStatus": "review", "assigneeId": 7,
                "supervisionEnabled": false
            }"#,
            ),
        ])
        .await;
        let rc = test_run_context(&url);

        assert!(supervised_parent(&rc).await.is_none());
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "whoami + child detail + parent detail — the gate trips before any further work"
        );
    }
}
