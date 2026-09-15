//! The daemon core: startup reconciliation and the poll → claim → dispatch loop
//! (spec §5.2/§5.3/§5.7). One `Daemon` serves every agent-managed project; runs
//! are independent tokio tasks bounded by the concurrency caps.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    client::{ProjectRepoConfig, RemoterClient, TaskSummary},
    config::Config,
    driver::AgentDriver,
    logstore::LogStore,
    ports::Ports,
    review::{ReviewContext, ReviewTrigger, Reviewer, execute as review_execute},
    run::{self, RunContext, RunKind},
    supervise::{SuperviseContext, SuperviseTrigger, Supervisor, execute as supervise_execute},
    workspace,
};

/// Startup retry backoff cap (spec §5.7): same cadence as the event stream's
/// reconnect — no reason to retry aggressively at startup either.
const STARTUP_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Shared failure message when a run is hard-aborted after the cancel grace
/// period expires without the task finishing.
const WEDGE_ABORT_MSG: &str = "run wedged after cancel; aborted";

/// A live run: its cancellation token (human-cancellation channel) and join
/// handle.
struct RunningEntry {
    cancel: CancellationToken,
    kind: RunKind,
    project_id: i32,
    join: JoinHandle<()>,
    /// When the cancel token was first fired; used to enforce the grace period
    /// before a hard abort.
    cancelled_at: Option<Instant>,
}

pub struct Daemon {
    config: Arc<Config>,
    client: RemoterClient,
    driver: Arc<dyn AgentDriver>,
    ports: Ports,
    image_locks: crate::image::ImageLocks,
    log_store: LogStore,
    /// task_id → live run. Two runs on the same ticket are impossible by
    /// construction (the claim moves it out of the executable set, spec §5.1).
    running: HashMap<i32, RunningEntry>,
    /// The supervision loop (spec §5.4): `review` parents poll their children —
    /// kick off the next backlog child, trigger supervise runs.
    supervisor: Supervisor,
    /// The review loop (#142): tickets with `agentReviewRequested` get a
    /// standalone review run.
    reviewer: Reviewer,
    /// Long-lived staging environments are independent from per-run containers.
    staging: crate::staging::StagingHandle,
}

impl Daemon {
    pub fn new(config: Arc<Config>, client: RemoterClient, driver: Arc<dyn AgentDriver>, log_store: LogStore) -> Self {
        let image_locks = crate::image::ImageLocks::default();
        Self {
            config: config.clone(),
            client,
            driver,
            ports: Ports::default(),
            image_locks: image_locks.clone(),
            log_store,
            running: HashMap::new(),
            staging: crate::staging::StagingManager::handle(config.clone(), image_locks),
            supervisor: Supervisor::new(),
            reviewer: Reviewer::new(),
        }
    }

    /// Startup token validation (spec §5.1): the token must belong to an agent
    /// principal — a human token is a fatal misconfiguration. Transient
    /// failures (transport errors, 429/5xx — e.g. the backend is down when the
    /// daemon starts) are retried forever with exponential backoff
    /// (`retry_backoff_secs` doubling, capped at 60s): the daemon is run
    /// manually, so a backend restart must not take the agent down with it
    /// (spec §5.7). Non-transient HTTP errors (4xx) stay fatal.
    pub async fn validate_token(&self) -> anyhow::Result<()> {
        let mut backoff = Duration::from_secs(self.config.retry_backoff_secs.max(1));
        loop {
            match self.client.whoami().await {
                Ok(me) => {
                    if me.kind != "agent" {
                        anyhow::bail!(
                            "REMOTER_AGENT_TOKEN belongs to {} user {:?} (id {}) — create an agent token via POST /users/agent",
                            me.kind,
                            me.name,
                            me.id
                        );
                    }
                    let workspace = resolve_workspace(&me, self.config.workspace_id)?;
                    tracing::info!(
                        agent = %me.name,
                        agent_id = me.id,
                        workspace_id = workspace.id,
                        workspace_name = %workspace.name,
                        role = %workspace.role,
                        "token validated"
                    );
                    return Ok(());
                }
                Err(e) if e.is_transient() => {
                    tracing::warn!(error = %e, backoff_secs = backoff.as_secs(), "token validation: transient failure; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(STARTUP_MAX_BACKOFF);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Picks the effective workspace for the daemon.
/// - If `workspace_id` is configured, it must match one of the agent's memberships.
/// - Otherwise, exactly one membership must exist; the backend's single-membership
///   fallback is used for requests without `X-Workspace-Id`.
fn resolve_workspace(
    me: &crate::client::WhoAmI,
    configured: Option<i32>,
) -> anyhow::Result<crate::client::WorkspaceMembership> {
    if let Some(id) = configured {
        let found = me.workspaces.iter().find(|w| w.id == id).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "REMOTER_AGENT_WORKSPACE_ID={id} is not one of this agent's workspaces ({}) — \
                 fix the config or invite the agent to that workspace",
                me.workspaces
                    .iter()
                    .map(|w| w.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        return Ok(found);
    }
    match me.workspaces.len() {
        0 => anyhow::bail!("agent has no workspace memberships — invite it to a workspace first"),
        1 => Ok(me.workspaces[0].clone()),
        _ => anyhow::bail!(
            "agent belongs to multiple workspaces ({}); set REMOTER_AGENT_WORKSPACE_ID or \
             `workspace_id` in the config",
            me.workspaces
                .iter()
                .map(|w| format!("{}={}", w.id, w.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Whether a completed ticket is a supervised child whose commits may not be
/// absorbed into the parent's branch yet (review #106): it has a parent link,
/// the parent has supervision enabled, the parent is assigned to this same
/// agent, and the parent is not
/// `completed`. The child's local branch is then the only copy of its work —
/// housekeeping keeps it on cleanup. Unlike the run-start stacking detection
/// (`run::supervised_parent`) the parent's branch does not need to exist
/// locally: the retention decision is about the child's own branch.
async fn supervised_child_pending_absorption(client: &RemoterClient, t: &TaskSummary) -> bool {
    let (Ok(me), Ok(detail)) = (client.whoami().await, client.task_detail(t.id).await) else {
        return false;
    };
    // From the child's perspective the link to its parent carries the
    // "subtask" relation (the backend resolves relations from the viewing
    // task's side).
    let Some(parent_id) = detail
        .links
        .as_deref()
        .and_then(|links| links.iter().find(|l| l.relation == "subtask").map(|l| l.task_id))
    else {
        return false;
    };
    let Ok(parent) = client.task_detail(parent_id).await else {
        return false;
    };
    parent.supervision_enabled && parent.assignee_id == Some(me.id) && parent.task_status != "completed"
}

impl Daemon {
    /// Startup reconciliation (spec §5.7): at startup no run is legitimately in
    /// flight, so a ticket of this daemon's agent still sitting in a claim
    /// status (`plan`/`in_progress`) is a zombie — from a crash, a killed
    /// process, or a failed status write. Finish any orphan `running` rows as
    /// failed and roll the ticket back so the poll loop re-claims it.
    pub async fn reconcile(&self) {
        // Staging containers are ephemeral across daemon restarts.
        // The manager has no backend state to restore, so remove all leftovers.
        crate::staging::sweep_orphans(&self.config.execution).await;
        // Startup sweep (spec §5.8): stop devenv processes leaked into ticket
        // worktrees by a crash or an older daemon version. Best-effort — a
        // sweep failure must never block reconciliation.
        workspace::down_all_worktrees(&self.config.workspace_root).await;
        // Container mode: reap run/sidecar containers a crashed daemon leaked
        // (labeled remoter.run_id; containers spec §4).
        if self.config.execution.is_container() {
            crate::container::sweep(&self.config.execution).await;
        }
        let mine = match self.client.my_tasks(None).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "reconciliation: could not list my tasks");
                return;
            }
        };
        for task in mine {
            // `review` parents are not claim statuses — but a supervise run
            // on one orphans its `running` row when the daemon crashes. Finish
            // those rows (no rollback; the parent stays in `review`) so the
            // supervision loop's "already running" dedup does not deadlock.
            let claim_rollback = match task.task_status.as_str() {
                "plan" => Some("todo"),
                "in_progress" => Some("implement"),
                _ => None,
            };
            if claim_rollback.is_none() && task.task_status != "review" {
                continue;
            }
            let runs = match self.client.list_runs(task.id).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(task_id = task.id, error = %e, "reconciliation: could not list runs");
                    continue;
                }
            };
            let mut orphans = 0;
            for r in runs.into_iter().filter(|r| r.status == "running") {
                orphans += 1;
                self.client
                    .finish_run(
                        r.id,
                        &crate::client::FinishRunBody::failed("daemon restart".to_string()),
                    )
                    .await
                    .map_err(|e| tracing::warn!(run_id = r.id, error = %e, "reconciliation: finish failed"))
                    .ok();
            }
            let Some(rollback_to) = claim_rollback else {
                continue;
            };
            // Unconditional: a zombie is possible without any orphan row too
            // (crash between claim and start_run, or a success-path status
            // write that failed) — rolling back only when orphans exist would
            // strand those tickets in a claim status forever.
            match self.client.set_task_status(task.id, rollback_to).await {
                Ok(()) => tracing::info!(task_id = task.id, orphans, "reconciled → {rollback_to}"),
                Err(e) if e.is_conflict() || e.is_forbidden() => {
                    tracing::info!(task_id = task.id, "reconciliation skipped: ticket moved by a human")
                }
                Err(e) => tracing::warn!(task_id = task.id, error = %e, "reconciliation: rollback failed"),
            }
        }
    }

    /// One poll cycle: reap finished runs, cancel runs whose ticket a human
    /// touched (and clean up accepted tickets), then claim and dispatch new
    /// work within the concurrency caps, then run the supervision loop over
    /// `review` parents (kickoff queue, supervise-run triggers, the final
    /// integration trigger) and launch the triggered supervise runs, then the
    /// review loop over human-requested agent reviews (#142).
    pub async fn poll_once(&mut self) {
        self.reap_finished();
        self.housekeeping().await;
        self.staging.lock().await.sweep_idle().await;
        self.auto_start_staging().await;
        self.claim_new_work().await;
        let triggers = self.supervisor.poll(&self.client).await;
        for trigger in triggers {
            self.launch_supervise(trigger).await;
        }
        let review_triggers = self.reviewer.poll(&self.client).await;
        for trigger in review_triggers {
            self.launch_review(trigger).await;
        }
    }

    /// The supervision-loop state (test/observability hook).
    pub fn supervisor(&self) -> &Supervisor {
        &self.supervisor
    }

    /// The review-loop state (test/observability hook).
    pub fn reviewer(&self) -> &Reviewer {
        &self.reviewer
    }

    pub fn staging_handle(&self) -> crate::staging::StagingHandle {
        self.staging.clone()
    }

    /// Waits until every live run has finished (used on shutdown and by tests).
    pub async fn wait_idle(&mut self) {
        let handles: Vec<JoinHandle<()>> = self.running.drain().map(|(_, e)| e.join).collect();
        for h in handles {
            let _ = h.await;
        }
    }

    /// Cancels all live runs, waits for them up to `cancel_grace_secs`, then
    /// hard-aborts any stragglers and finishes their orphan run rows.
    pub async fn shutdown(&mut self) {
        let grace = Duration::from_secs(self.config.cancel_grace_secs);
        let deadline = Instant::now() + grace;
        for e in self.running.values() {
            e.cancel.cancel();
        }
        for e in self.running.values_mut() {
            e.cancelled_at.get_or_insert(Instant::now());
        }
        // Wait bounded for live runs to finish.
        while !self.running.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            self.reap_finished();
            if self.running.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Abort any stragglers and finish their orphan rows.
        let remaining: Vec<i32> = self.running.keys().copied().collect();
        for task_id in remaining {
            self.abort_orphan_run(task_id, WEDGE_ABORT_MSG).await;
        }
        self.wait_idle().await;
        self.staging.lock().await.sweep().await;
    }

    fn reap_finished(&mut self) {
        self.running.retain(|task_id, e| {
            if e.join.is_finished() {
                tracing::debug!(task_id, "run reaped");
                false
            } else {
                true
            }
        });
    }

    /// Hard-abort a single live run and finish any of its still-`running` rows
    /// so the next poll can re-claim the ticket.
    async fn abort_orphan_run(&mut self, task_id: i32, reason: &str) {
        let Some(entry) = self.running.remove(&task_id) else {
            return;
        };
        entry.join.abort();
        tracing::error!(task_id, reason, "run wedged after cancel; aborted");
        self.finish_orphan_running_rows(task_id, reason).await;
    }

    /// Finish every run row still marked `running` for this task so a stuck
    /// ticket is not stranded in a claim status forever.
    async fn finish_orphan_running_rows(&self, task_id: i32, reason: &str) {
        match self.client.list_runs(task_id).await {
            Ok(runs) => {
                for r in runs.into_iter().filter(|r| r.status == "running") {
                    let body = crate::client::FinishRunBody::failed(reason.to_string());
                    self.client.finish_run(r.id, &body).await.ok();
                }
            }
            Err(e) => tracing::warn!(task_id, error = %e, "abort backstop: could not list runs"),
        }
    }

    /// Human cancellation (spec §5.7): a ticket that was unassigned or moved out
    /// of `plan`/`in_progress` by hand cancels its live run. Tickets the human
    /// accepted (`completed`) get their worktree cleaned up (spec §5.4).
    async fn housekeeping(&mut self) {
        let mine = match self.client.my_tasks(None).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "housekeeping: could not list my tasks");
                return;
            }
        };

        if !self.running.is_empty() {
            let statuses: HashMap<i32, String> = mine.iter().map(|t| (t.id, t.task_status.clone())).collect();
            for (task_id, entry) in self.running.iter_mut() {
                // Human interference = the ticket left the run's active state
                // BACKWARDS (or was unassigned — it then vanishes from my
                // list). A forward hop (plan→plan_review, in_progress→review)
                // is either a human UI move or the daemon's own authoritative
                // status write at run end — never cancel for that; the run
                // finishes and its status write simply becomes a no-op conflict.
                let expected = match entry.kind {
                    RunKind::Plan => "plan",
                    RunKind::Implement => "in_progress",
                    // A supervise run lives on the parent, which stays in
                    // `review` for the whole run; a review run likewise lives
                    // on the ticket itself in `review` (#142) — a human moving
                    // it out cancels the run.
                    RunKind::Supervise | RunKind::Review => "review",
                };
                let forward = match entry.kind {
                    RunKind::Plan => "plan_review",
                    RunKind::Implement => "review",
                    RunKind::Supervise | RunKind::Review => "review",
                };
                let alive = statuses.get(task_id).is_some_and(|s| s == expected || s == forward);
                if !alive {
                    if entry.cancelled_at.is_none() {
                        tracing::info!(task_id, "cancelling run");
                        entry.cancelled_at = Some(Instant::now());
                    }
                    entry.cancel.cancel();
                }
            }
        }

        // After cancellation requests, hard-abort any run that has exceeded the
        // grace period without finishing.
        let grace = Duration::from_secs(self.config.cancel_grace_secs);
        let mut to_abort: Vec<i32> = Vec::new();
        for (&task_id, entry) in self.running.iter() {
            if let Some(at) = entry.cancelled_at
                && at.elapsed() >= grace
                && !entry.join.is_finished()
            {
                to_abort.push(task_id);
            }
        }
        for task_id in to_abort {
            self.abort_orphan_run(task_id, WEDGE_ABORT_MSG).await;
        }

        // Staging stays warm across review → implement, but any other status
        // means the review chain ended or the ticket was reset.
        for t in &mine {
            if !matches!(t.task_status.as_str(), "review" | "implement" | "in_progress")
                && self.staging.lock().await.stop(t.id).await
            {
                let _ = self.client.add_comment(t.id, "Staging environment stopped.").await;
            }
        }

        // Accepted tickets: remove the worktree + branch. The exists-checks
        // keep this a cheap no-op for tickets already fully cleaned; the
        // branch check matters because a retained supervised-child branch
        // (see below) outlives its worktree and must still be cleaned once
        // the parent completes.
        for t in mine.iter().filter(|t| t.task_status == "completed") {
            // The branch name comes from the run row (recorded at finish
            // time); recomputing it from the title would break if the
            // ticket was renamed mid-flight. Fall back to the recompute for
            // runs old enough to lack a recorded branch.
            let branch = match self.client.list_runs(t.id).await {
                Ok(runs) => runs
                    .iter()
                    .find_map(|r| r.branch.clone())
                    .unwrap_or_else(|| workspace::branch_name(t.id, &t.title)),
                Err(e) => {
                    tracing::warn!(task_id = t.id, error = %e, "could not list runs; guessing branch name");
                    workspace::branch_name(t.id, &t.title)
                }
            };
            let repo = workspace::repo_dir(&self.config.workspace_root, t.project_id);
            let wt = workspace::worktree_dir(&self.config.workspace_root, t.project_id, t.id);
            if !wt.exists() && !workspace::local_branch_exists(&repo, &branch).await {
                continue;
            }
            tracing::info!(task_id = t.id, "ticket completed; cleaning up worktree");
            // A supervised child accepted before its commits were absorbed
            // into the parent's branch: the local branch is the only copy
            // of the work, so it survives until the parent completes
            // (review #106). The worktree is still removed.
            let keep_branch = supervised_child_pending_absorption(&self.client, t).await;
            if let Err(e) =
                workspace::cleanup(&self.config.workspace_root, t.project_id, t.id, &branch, keep_branch).await
            {
                tracing::warn!(task_id = t.id, error = %e, "worktree cleanup failed");
            }
        }
    }

    async fn auto_start_staging(&mut self) {
        let tasks = match self.client.my_tasks(Some("review")).await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::debug!(error = %e, "staging auto-start: could not list review tasks");
                return;
            }
        };
        let projects: HashMap<i32, ProjectRepoConfig> = match self.client.projects().await {
            Ok(ps) => ps
                .into_iter()
                .filter_map(ProjectRepoConfig::from_dto)
                .map(|p| (p.project_id, p))
                .collect(),
            Err(e) => {
                tracing::debug!(error = %e, "staging auto-start: could not list projects");
                return;
            }
        };
        for task in tasks {
            let Some(project) = projects.get(&task.project_id) else {
                continue;
            };
            if project.staging_auto_start && self.staging.lock().await.get(task.id).is_none() {
                match self.staging.lock().await.start(task.id, project).await {
                    Ok(()) => {
                        let _ = self.client.add_comment(task.id, "Staging environment started.").await;
                    }
                    Err(e) => tracing::warn!(task_id = task.id, error = %e, "staging auto-start failed"),
                }
            }
        }
    }

    async fn claim_new_work(&mut self) {
        let mut candidates = match self.client.my_tasks(Some("todo")).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "poll: could not list todo tasks");
                return;
            }
        };
        match self.client.my_tasks(Some("implement")).await {
            Ok(mut t) => candidates.append(&mut t),
            Err(e) => tracing::warn!(error = %e, "poll: could not list implement tasks"),
        }
        candidates.retain(|t| !t.blocked);
        if candidates.is_empty() {
            return;
        }

        // Project repo config is backend-owned (spec §4.4): fetched and cached
        // for this poll cycle, so a UI edit takes effect without a restart.
        let projects: HashMap<i32, ProjectRepoConfig> = match self.client.projects().await {
            Ok(ps) => ps
                .into_iter()
                .filter_map(ProjectRepoConfig::from_dto)
                .map(|p| (p.project_id, p))
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "poll: could not list projects");
                return;
            }
        };

        for task in candidates {
            if self.running.contains_key(&task.id) {
                continue;
            }
            // Executability (spec §5.1): the project must be agent-managed
            // (non-NULL `repo_url`). Anything else is skipped silently.
            let Some(project) = projects.get(&task.project_id).cloned() else {
                continue;
            };
            if !self.has_capacity(task.project_id) {
                tracing::debug!(task_id = task.id, "at concurrency cap; leaving for next poll");
                continue;
            }

            // Allocate the run's port block *before* claiming: if the range is
            // exhausted the ticket stays claimable for a later poll (spec §5.8).
            // Container mode needs no ports — sidecars replace the contract.
            let port_block = if self.config.execution.is_container() {
                None
            } else {
                let Some(block) = self.ports.allocate() else {
                    tracing::warn!(task_id = task.id, "no free port block; leaving for next poll");
                    continue;
                };
                Some(block)
            };

            let (kind, claim_status) = match task.task_status.as_str() {
                "todo" => (RunKind::Plan, "plan"),
                _ => (RunKind::Implement, "in_progress"),
            };

            // The claim is the atomic status write-back (spec §5.3).
            match self.client.set_task_status(task.id, claim_status).await {
                Ok(()) => tracing::info!(task_id = task.id, "claimed → {claim_status}"),
                Err(e) if e.is_conflict() => {
                    tracing::debug!(task_id = task.id, "claim lost (409); skipping");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(task_id = task.id, error = %e, "claim failed; skipping");
                    continue;
                }
            }

            let cancel = CancellationToken::new();
            let rc = RunContext {
                client: self.client.clone(),
                config: self.config.clone(),
                driver: self.driver.clone(),
                task,
                project,
                kind,
                port_block,
                image_locks: self.image_locks.clone(),
                cancel: cancel.clone(),
                log_store: self.log_store.clone(),
            };
            let task_id = rc.task.id;
            let project_id = rc.project.project_id;
            let join = tokio::spawn(run::execute(rc));
            self.running.insert(
                task_id,
                RunningEntry {
                    cancel,
                    kind,
                    project_id,
                    join,
                    cancelled_at: None,
                },
            );
        }
    }

    /// Launches a supervise run for a parent ticket: capacity + port block
    /// like a claim run, tracked in the running registry under the parent's
    /// id so a human moving the parent out of `review` cancels it.
    async fn launch_supervise(&mut self, trigger: SuperviseTrigger) {
        let parent_id = trigger.parent.id;
        let project_id = trigger.parent.project_id;
        if self.running.contains_key(&parent_id) {
            tracing::debug!(task_id = parent_id, "supervise run already tracked; skipping");
            return;
        }
        if !self.has_capacity(project_id) {
            tracing::debug!(
                task_id = parent_id,
                "at concurrency cap; supervise trigger deferred to next poll"
            );
            return;
        }
        let project: Option<ProjectRepoConfig> = match self.client.projects().await {
            Ok(ps) => ps
                .into_iter()
                .filter_map(ProjectRepoConfig::from_dto)
                .find(|p| p.project_id == project_id),
            Err(e) => {
                tracing::warn!(task_id = parent_id, error = %e, "supervise: could not list projects; trigger deferred");
                return;
            }
        };
        let port_block = if self.config.execution.is_container() {
            None
        } else {
            let Some(block) = self.ports.allocate() else {
                tracing::warn!(task_id = parent_id, "no free port block; supervise trigger deferred");
                return;
            };
            Some(block)
        };
        let cancel = CancellationToken::new();
        let rc = SuperviseContext {
            client: self.client.clone(),
            config: self.config.clone(),
            driver: self.driver.clone(),
            parent: trigger.parent,
            review_children: trigger.review_children,
            project,
            port_block,
            image_locks: self.image_locks.clone(),
            cancel: cancel.clone(),
            log_store: self.log_store.clone(),
            human_notified: self.supervisor.give_up_registry(),
        };
        let join = tokio::spawn(supervise_execute(rc));
        // The backoff stamp is applied only now that the run actually
        // launched — a deferred trigger (capacity, ports, project fetch)
        // must not eat the 300s window (review #110).
        self.supervisor.mark_triggered(parent_id);
        tracing::info!(task_id = parent_id, "supervise run launched");
        self.running.insert(
            parent_id,
            RunningEntry {
                cancel,
                kind: RunKind::Supervise,
                project_id,
                join,
                cancelled_at: None,
            },
        );
    }

    /// Launches a review run for a ticket whose human requested an agent
    /// review (#142): capacity + port block like a supervise launch, tracked
    /// in the running registry under the ticket's id so a human moving the
    /// ticket out of `review` cancels it.
    async fn launch_review(&mut self, trigger: ReviewTrigger) {
        let task_id = trigger.task.id;
        let project_id = trigger.task.project_id;
        if self.running.contains_key(&task_id) {
            tracing::debug!(task_id, "review run already tracked; skipping");
            return;
        }
        if !self.has_capacity(project_id) {
            tracing::debug!(task_id, "at concurrency cap; review trigger deferred to next poll");
            return;
        }
        let project: Option<ProjectRepoConfig> = match self.client.projects().await {
            Ok(ps) => ps
                .into_iter()
                .filter_map(ProjectRepoConfig::from_dto)
                .find(|p| p.project_id == project_id),
            Err(e) => {
                tracing::warn!(task_id, error = %e, "review: could not list projects; trigger deferred");
                return;
            }
        };
        let port_block = if self.config.execution.is_container() {
            None
        } else {
            let Some(block) = self.ports.allocate() else {
                tracing::warn!(task_id, "no free port block; review trigger deferred");
                return;
            };
            Some(block)
        };
        let cancel = CancellationToken::new();
        let rc = ReviewContext {
            client: self.client.clone(),
            config: self.config.clone(),
            driver: self.driver.clone(),
            task: trigger.task,
            project,
            port_block,
            image_locks: self.image_locks.clone(),
            cancel: cancel.clone(),
            log_store: self.log_store.clone(),
        };
        let join = tokio::spawn(review_execute(rc));
        // The backoff stamp is applied only now that the run actually launched
        // — a deferred trigger (capacity, ports, project fetch) must not eat
        // the window (same rule as the supervise trigger, review #110).
        self.reviewer.mark_triggered(task_id);
        tracing::info!(task_id, "review run launched");
        self.running.insert(
            task_id,
            RunningEntry {
                cancel,
                kind: RunKind::Review,
                project_id,
                join,
                cancelled_at: None,
            },
        );
    }

    /// Global and per-project concurrency caps (spec §5.1).
    fn has_capacity(&self, project_id: i32) -> bool {
        let active = self.running.values().filter(|e| !e.join.is_finished()).count();
        if active >= self.config.max_concurrent_runs {
            return false;
        }
        let per_project = self.config.max_concurrent_runs_per_project;
        if per_project > 0 {
            let active_in_project = self
                .running
                .values()
                .filter(|e| !e.join.is_finished() && e.project_id == project_id)
                .count();
            if active_in_project >= per_project {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::stub::StubDriver;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal HTTP/1.1 mock backend: serves the queued `(status, body)`
    /// responses in request order, one connection each (`connection: close`
    /// forces reqwest to reconnect, so response N maps to request N).
    async fn spawn_mock_backend(responses: Vec<(u16, &'static str)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
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
        format!("http://{addr}")
    }

    fn test_daemon(api_url: &str) -> Daemon {
        // SAFETY: test process env tweak; the env override would otherwise win
        // over the mock URL (spec §5.1).
        unsafe { std::env::remove_var("REMOTER_API_URL") };
        let toml = format!(
            r#"
api_url = "{api_url}"
workspace_root = "/tmp/remoter-agent-claim-test"
retry_backoff_secs = 1
cancel_grace_secs = 0

[driver]
kind = "stub"
"#
        );
        let config = Arc::new(Config::from_toml_str(&toml, Some("tok".to_string())).unwrap());
        let log_store = LogStore::new(&config.logs).unwrap();
        Daemon::new(
            config,
            RemoterClient::new(api_url, "tok", None),
            Arc::new(StubDriver::new(0, false)),
            log_store,
        )
    }

    /// Spec §5.7: a backend that is down at daemon startup (transport errors,
    /// 5xx) is retried with backoff until the token validates.
    #[tokio::test]
    async fn validate_token_retries_transient_errors_then_succeeds() {
        let url = spawn_mock_backend(vec![
            (503, ""),
            (503, ""),
            (200, r#"{"id":2,"name":"kimi-agent","kind":"agent","workspaces":[{"id":1,"name":"Personal","role":"owner"}]}"#),
        ])
        .await;
        let daemon = test_daemon(&url);
        tokio::time::timeout(Duration::from_secs(30), daemon.validate_token())
            .await
            .expect("transient retries must eventually validate")
            .unwrap();
    }

    /// Non-transient HTTP errors stay fatal: a single 401 on the wire, and the
    /// call must fail fast (a retry would hang waiting for a second response
    /// that never comes, tripping the 10s timeout).
    #[tokio::test]
    async fn validate_token_4xx_is_fatal_without_retry() {
        let url = spawn_mock_backend(vec![(401, "")]).await;
        let daemon = test_daemon(&url);
        let err = tokio::time::timeout(Duration::from_secs(10), daemon.validate_token())
            .await
            .expect("4xx must not be retried")
            .unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    #[tokio::test]
    async fn validate_token_rejects_a_human_token() {
        let url = spawn_mock_backend(vec![(
            200,
            r#"{"id":1,"name":"tim","kind":"human","workspaces":[{"id":1,"name":"Personal","role":"owner"}]}"#,
        )])
        .await;
        let daemon = test_daemon(&url);
        let err = daemon.validate_token().await.unwrap_err();
        assert!(err.to_string().contains("belongs to human user"), "{err}");
    }

    fn running_run(_task_id: i32, cancelled_at: Option<Instant>) -> RunningEntry {
        RunningEntry {
            cancel: CancellationToken::new(),
            kind: RunKind::Implement,
            project_id: 1,
            join: tokio::spawn(std::future::pending()),
            cancelled_at,
        }
    }

    /// The abort backstop removes a run that stayed alive past the cancel grace
    /// period and finishes its still-`running` rows so the ticket can be re-claimed.
    #[tokio::test]
    async fn test_housekeeping_aborts_wedged_run_after_grace() {
        let url = spawn_mock_backend(vec![
            (200, r#"[]"#),
            (
                200,
                r#"[{"id":42,"task_id":7,"agent_user_id":2,"kind":"implement","status":"running","session_id":null,"branch":null,"plan":null,"summary":null,"error":null,"pr_url":null,"pr_error":null,"input_tokens":null,"output_tokens":null,"attempt":1}]"#,
            ),
            (
                200,
                r#"{"id":42,"task_id":7,"agent_user_id":2,"kind":"implement","status":"failed","session_id":null,"branch":null,"plan":null,"summary":null,"error":null,"pr_url":null,"pr_error":null,"input_tokens":null,"output_tokens":null,"attempt":1}"#,
            ),
        ])
        .await;
        let mut daemon = test_daemon(&url);
        daemon
            .running
            .insert(7, running_run(7, Some(Instant::now() - Duration::from_secs(10))));

        daemon.housekeeping().await;

        assert!(!daemon.running.contains_key(&7), "wedged run must be removed");
    }

    /// Shutdown cancels live runs, waits for the grace period, then aborts stragglers
    /// and finishes their orphan rows.
    #[tokio::test]
    async fn test_shutdown_aborts_wedged_run() {
        let url = spawn_mock_backend(vec![
            (
                200,
                r#"[{"id":42,"task_id":7,"agent_user_id":2,"kind":"implement","status":"running","session_id":null,"branch":null,"plan":null,"summary":null,"error":null,"pr_url":null,"pr_error":null,"input_tokens":null,"output_tokens":null,"attempt":1}]"#,
            ),
            (
                200,
                r#"{"id":42,"task_id":7,"agent_user_id":2,"kind":"implement","status":"failed","session_id":null,"branch":null,"plan":null,"summary":null,"error":null,"pr_url":null,"pr_error":null,"input_tokens":null,"output_tokens":null,"attempt":1}"#,
            ),
        ])
        .await;
        let mut daemon = test_daemon(&url);
        daemon.running.insert(7, running_run(7, None));

        daemon.shutdown().await;

        assert!(
            !daemon.running.contains_key(&7),
            "wedged run must be removed by shutdown"
        );
    }

    #[test]
    fn resolve_workspace_uses_configured_id_when_valid() {
        let me = whoami(vec![
            membership(1, "Personal", "owner"),
            membership(2, "Team", "member"),
        ]);
        let w = resolve_workspace(&me, Some(2)).unwrap();
        assert_eq!(w.id, 2);
        assert_eq!(w.name, "Team");
        assert_eq!(w.role, "member");
    }

    #[test]
    fn resolve_workspace_rejects_unknown_configured_id() {
        let me = whoami(vec![membership(1, "Personal", "owner")]);
        let err = resolve_workspace(&me, Some(7)).unwrap_err();
        assert!(err.to_string().contains("REMOTER_AGENT_WORKSPACE_ID=7"), "{err}");
    }

    #[test]
    fn resolve_workspace_falls_back_to_single_membership() {
        let me = whoami(vec![membership(1, "Personal", "owner")]);
        let w = resolve_workspace(&me, None).unwrap();
        assert_eq!(w.id, 1);
    }

    #[test]
    fn resolve_workspace_errors_when_multiple_memberships_without_config() {
        let me = whoami(vec![
            membership(1, "Personal", "owner"),
            membership(2, "Team", "member"),
        ]);
        let err = resolve_workspace(&me, None).unwrap_err();
        assert!(err.to_string().contains("multiple workspaces"), "{err}");
    }

    fn whoami(workspaces: Vec<crate::client::WorkspaceMembership>) -> crate::client::WhoAmI {
        crate::client::WhoAmI {
            id: 1,
            name: "test".into(),
            kind: "agent".into(),
            workspaces,
        }
    }

    fn membership(id: i32, name: &str, role: &str) -> crate::client::WorkspaceMembership {
        crate::client::WorkspaceMembership {
            id,
            name: name.into(),
            role: role.into(),
        }
    }
}
