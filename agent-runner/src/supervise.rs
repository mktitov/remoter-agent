//! The supervision loop (spec §5.4): a parent ticket in `review` polls its
//! subtask children on the daemon's regular cadence — it kicks off the next
//! `backlog` child into `implement` (one active child at a time, skipping
//! children with unfinished `blocked_by` blockers), triggers a supervise run
//! for the parent when a child lands in `review`, and bounces the parent into
//! the final integration run once every child is completed.
//!
//! The kickoff/status moves go through the backend API under the
//! parent-scoped agent rights (docs/specs/task-links.md): the backend
//! re-checks that the child's parent is assigned to the calling agent on every
//! write.
//!
//! A **supervise run** (#115) executes on the parent: the supervisor agent
//! reads each reviewing child's diff (against the parent's branch) and
//! implementation report, then records a verdict via the `set_task_review`
//! MCP tool. When the run finishes, the daemon acts on the verdicts:
//! `changes_requested` → the review is posted to the child's thread and the
//! child bounces `review → implement`; `approve` → the child's branch is
//! merged into the parent's branch (conflicts go back to the agent on the
//! resumed session) and the child moves `review → completed`.
//!
//! A **circuit breaker** keeps a failed child out of an automatic
//! bounce → fail → escalate loop (spec §5.4/§5.7): a child whose newest run
//! failed and whose thread already records `MAX_FAILURE_CYCLES` escalation
//! notes is neither re-triggered for supervision nor bounced — it stays in
//! `review` for a human, and an `approve` on an empty diff with a failed
//! newest run is refused deterministically.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::{
    client::{CommentDto, ProjectRepoConfig, RemoterClient, TaskDetail, TaskSummary},
    config::Config,
    driver::{AgentDriver, DriverError, RunFailure, RunOutcome, RunSpec},
    logstore::LogStore,
    ports::PortBlock,
    run::{self, RunLogGuard},
    session_log::SessionLogger,
    workspace,
};

/// The run kind a supervision trigger starts for the parent ticket.
pub const SUPERVISE_RUN_KIND: &str = "supervise";

/// Do not re-trigger a supervise run for the same parent within this window
/// (dedup/backoff: repeated polls must not spam runs while a child sits in
/// `review`; a succeeded run row resets the need because the child leaves
/// `review`). This window only bounds the no-verdict / failure re-attempt rate.
const TRIGGER_BACKOFF: Duration = Duration::from_secs(300);

/// How many merge-conflict nudges the daemon issues before asking a human
/// (each nudge runs on the resumed supervise session).
const MAX_MERGE_NUDGES: u32 = 2;

/// Circuit breaker against the bounce → fail → escalate loop (spec §5.4): a
/// child whose implement run failed escalates into `review` (spec §5.7), which
/// is indistinguishable from a finished child to the trigger. While the cause
/// is unhealed (e.g. an infra failure) every bounce just fails again, and each
/// cycle costs a full implement retry budget plus a supervise run. After this
/// many recorded failure cycles the loop stops triggering supervise runs for
/// the child and verdict processing stops bouncing it — it stays in `review`
/// with one note for a human.
const MAX_FAILURE_CYCLES: usize = 2;

/// Markers of the daemon's escalation note (`run::escalate`, spec §5.7) —
/// their conjunction identifies a recorded bounce → fail → escalate cycle on
/// the child's thread.
const ESCALATION_NOTE_MARKERS: [&str; 2] = ["failed after", "needs a human"];

/// Whether the child's newest run row is a failure — the deterministic signal
/// that the child escalated into `review` (run failed) rather than being
/// finished by an agent (run succeeded). `runs` is newest-first.
fn latest_run_failed(child: &TaskDetail) -> bool {
    child
        .runs
        .as_deref()
        .and_then(|runs| runs.first())
        .is_some_and(|r| r.status == "failed")
}

/// How many bounce → fail → escalate cycles the child's thread records (one
/// escalation note per cycle, spec §5.7).
fn failure_cycles(child: &TaskDetail) -> usize {
    child
        .comments
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|c| ESCALATION_NOTE_MARKERS.iter().all(|m| c.body.contains(m)))
        .count()
}

/// The circuit-breaker condition: an escalated child that already burned its
/// automatic-cycle budget — supervise must neither trigger runs for it nor
/// bounce it anymore.
fn circuit_broken(child: &TaskDetail) -> bool {
    latest_run_failed(child) && failure_cycles(child) >= MAX_FAILURE_CYCLES
}

/// Child ticket description quoted in the supervise prompt.
const MAX_CHILD_DESC_BYTES: usize = 4 * 1024;
/// Child implementation report quoted in the supervise prompt.
const MAX_REPORT_BYTES: usize = 16 * 1024;
/// Child diff quoted in the supervise prompt — diffs can be huge, and the
/// agent can always run `git diff` itself when it needs more.
const MAX_DIFF_BYTES: usize = 64 * 1024;

/// Child statuses that count as *active*: one active child blocks the kickoff
/// of any sibling. `review` is deliberately not active — a child awaiting
/// (super-)review does not hold the rollout queue.
const ACTIVE_CHILD_STATUSES: [&str; 5] = ["todo", "plan", "plan_review", "implement", "in_progress"];

/// Ranking for the kickoff queue: higher priority first, then creation order
/// (`id` asc — the repo's own ordering, `task_priority desc, id asc`).
fn priority_rank(priority: Option<&str>) -> u8 {
    match priority.unwrap_or("medium") {
        "critical" => 3,
        "high" => 2,
        "low" => 0,
        _ => 1,
    }
}

/// Pure kickoff selection: `None` when a child is already active or no
/// `backlog` child is kickable. A child assigned to someone else is left
/// alone (a human or another agent owns it); a child in a project the daemon
/// does not manage would strand in `implement`, so it is skipped too.
fn next_kickoff_candidate<'a>(
    children: &'a [TaskDetail],
    agent_id: i32,
    managed_projects: &HashSet<i32>,
) -> Option<&'a TaskDetail> {
    if children
        .iter()
        .any(|c| ACTIVE_CHILD_STATUSES.contains(&c.task_status.as_str()))
    {
        return None;
    }
    children
        .iter()
        .filter(|c| c.task_status == "backlog")
        .filter(|c| !c.blocked)
        .filter(|c| managed_projects.contains(&c.project_id))
        .filter(|c| c.assignee_id.is_none() || c.assignee_id == Some(agent_id))
        .max_by_key(|c| (priority_rank(c.task_priority.as_deref()), std::cmp::Reverse(c.id)))
}

/// A supervise run the supervision loop wants launched for a parent ticket.
pub struct SuperviseTrigger {
    pub parent: TaskSummary,
    /// Children in `review` at trigger time — the supervise run reviews them.
    pub review_children: Vec<i32>,
}

/// Whether a recorded give-up signature still blocks supervise triggers for
/// the child: the signature is `{verdict}:{reason}`, and it is in effect
/// while it matches the child's CURRENT verdict. A new verdict (fresh review
/// outcome) or the child leaving `review` (the loop prunes signatures)
/// re-arms supervision.
fn give_up_blocks(notified: &HashMap<i32, String>, child: &TaskDetail) -> bool {
    notified
        .get(&child.id)
        .is_some_and(|sig| sig.starts_with(&format!("{}:", child.review_verdict.as_deref().unwrap_or("-"))))
}

/// The supervision loop state, plugged into the daemon's poll cycle.
#[derive(Default)]
pub struct Supervisor {
    /// parent_task_id → last supervise-trigger attempt.
    last_trigger: HashMap<i32, Instant>,
    /// parent_task_id → trigger attempts (an in-memory metric for tests and
    /// log correlation).
    trigger_count: HashMap<i32, u64>,
    /// parent_task_id → fingerprint of the children-completed cycle for which
    /// the final integration run was already triggered. The fingerprint is
    /// derived from the children's completion timestamps: when a child leaves
    /// `completed` the fingerprint changes, which re-arms the trigger for the
    /// next cycle (a plain boolean latch would stay armed forever — review
    /// #110). The marker note on the parent's thread (which embeds the same
    /// fingerprint) covers daemon restarts.
    integration_triggered: HashMap<i32, String>,
    /// child_task_id → give-up signature (`{verdict}:{reason}`) for children
    /// the supervise run gave up on with a "left in review for a human" note.
    /// Shared with the run execution: while the signature matches the child's
    /// current verdict, the loop must not re-trigger supervise runs for it —
    /// nothing short of a verdict change or the child leaving `review` would
    /// make another run come to a different conclusion, and re-running would
    /// spam a duplicate note every backoff window.
    human_notified: HumanNotified,
}

/// Shared registry of "a human was already notified about this give-up"
/// signatures (see [`Supervisor::human_notified`]).
pub type HumanNotified = Arc<std::sync::Mutex<HashMap<i32, String>>>;

/// Marker substring of the integration note on the parent's thread — the
/// persistent half of the integration dedup (survives daemon restarts; the
/// in-memory map covers the window between the status move and the note). The
/// note embeds the cycle fingerprint, so notes from earlier completed cycles
/// no longer match once a child leaves `completed`.
const INTEGRATION_MARKER: &str = "final integration run";

/// Fingerprint of the children-completed cycle: the completed children's ids
/// and completion timestamps. Any change (a child bounces out of `completed`
/// and back, a new completion) yields a different fingerprint.
fn completed_cycle_signature(children: &[TaskDetail]) -> String {
    let mut parts: Vec<String> = children
        .iter()
        .filter(|c| c.task_status == "completed")
        .map(|c| {
            format!(
                "{}@{}",
                c.id,
                c.completed_at.map(|t| t.to_rfc3339()).unwrap_or_default()
            )
        })
        .collect();
    parts.sort();
    parts.join(",")
}

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The give-up registry, shared into supervise run contexts so a run can
    /// record "a human was already notified" signatures (claim wiring).
    pub(crate) fn give_up_registry(&self) -> HumanNotified {
        self.human_notified.clone()
    }

    /// One supervision cycle over every `review` ticket assigned to this
    /// daemon's agent. Individual failures are logged and skipped — a
    /// backend hiccup must not disturb the claim loop. Returns the supervise
    /// runs to launch; the daemon (which owns the driver, ports, and the
    /// running registry) performs the launch itself.
    pub async fn poll(&mut self, client: &RemoterClient) -> Vec<SuperviseTrigger> {
        let mut triggers = Vec::new();
        let parents = match client.my_tasks(Some("review")).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "supervision: could not list review tasks");
                return triggers;
            }
        };
        if parents.is_empty() {
            return triggers;
        }
        // Kickoff gate: the child's project must be agent-managed (non-NULL
        // `repo_url`), otherwise the daemon could never claim and run it.
        let managed_projects: HashSet<i32> = match client.projects().await {
            Ok(ps) => ps
                .into_iter()
                .filter_map(crate::client::ProjectRepoConfig::from_dto)
                .map(|p| p.project_id)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "supervision: could not list projects; kickoff skipped this cycle");
                HashSet::new()
            }
        };
        // Resolved once per poll — the kickoff decision needs it, and one
        // whoami per cycle suffices for every parent.
        let agent_id = match client.whoami().await {
            Ok(me) => Some(me.id),
            Err(e) => {
                tracing::warn!(error = %e, "supervision: could not resolve agent id; kickoff skipped this cycle");
                None
            }
        };
        for parent in parents {
            self.supervise_parent(client, &parent, &managed_projects, agent_id, &mut triggers)
                .await;
        }
        triggers
    }

    async fn supervise_parent(
        &mut self,
        client: &RemoterClient,
        parent: &TaskSummary,
        managed_projects: &HashSet<i32>,
        agent_id: Option<i32>,
        triggers: &mut Vec<SuperviseTrigger>,
    ) {
        let detail = match client.task_detail(parent.id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(task_id = parent.id, error = %e, "supervision: could not fetch parent detail");
                return;
            }
        };
        // Supervision disabled on the parent: the whole subtree is left alone —
        // no child kickoff, no supervise-run trigger, no integration bounce.
        if !detail.supervision_enabled {
            tracing::debug!(
                task_id = parent.id,
                "supervision: disabled for this parent; subtree skipped"
            );
            return;
        }
        // From the parent's perspective, links to its children carry the
        // `"parent"` relation (the backend resolves relations from the
        // viewing task's side — same rule as `run::child_link_ids`).
        let child_ids: Vec<i32> = detail
            .links
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|l| l.relation == "parent")
            .map(|l| l.task_id)
            .collect();
        if child_ids.is_empty() {
            return;
        }
        // A failed fetch must not feed a partial child list downstream: the
        // integration check (`all(completed)`) would be vacuously true on an
        // empty vec, and the kickoff gate would see a wrong "active child"
        // picture. Bail the whole parent until the next poll instead (review
        // #110).
        let mut children = Vec::with_capacity(child_ids.len());
        for id in child_ids {
            match client.task_detail(id).await {
                Ok(c) => children.push(c),
                Err(e) => {
                    tracing::warn!(task_id = parent.id, child_id = id, error = %e, "supervision: could not fetch child; parent skipped this cycle");
                    return;
                }
            }
        }
        // Prune give-up signatures of children that left `review` — a later
        // review stay is a fresh situation that must be supervisable again.
        {
            let mut notified = self.human_notified.lock().expect("human_notified poisoned");
            for c in &children {
                if c.task_status != "review" {
                    notified.remove(&c.id);
                }
            }
        }

        // A child in review is ready for (super-)review: trigger the parent's
        // supervise run (deduped — see `trigger_warranted`). Children whose
        // give-up signature still matches their current verdict are awaiting
        // a human, not another agent run — skip them. Children whose circuit
        // breaker tripped (repeated bounce → fail → escalate cycles) are
        // skipped too: the cause is unhealed, so another supervise run would
        // only restart the loop (spec §5.4).
        let review_children: Vec<i32> = children
            .iter()
            .filter(|c| c.task_status == "review")
            .filter(|c| !circuit_broken(c))
            .filter(|c| {
                let notified = self.human_notified.lock().expect("human_notified poisoned");
                !give_up_blocks(&notified, c)
            })
            .map(|c| c.id)
            .collect();
        if !review_children.is_empty() && self.trigger_warranted(client, parent.id).await {
            triggers.push(SuperviseTrigger {
                parent: parent.clone(),
                review_children,
            });
        }

        // Final integration run: every child is completed, none awaits review
        // or rollout — bounce the parent `review → implement`; the normal
        // claim loop picks it up and the implement run (resuming the parent's
        // last session) merges the base branch, runs the checks, and pushes
        // the PR. Exactly once per children-completed cycle: the fingerprinted
        // in-memory latch plus the marker note on the parent's thread dedup
        // re-polls (the integration run itself lands the parent back in
        // `review`; the fingerprint changes when a child leaves `completed`,
        // re-arming the trigger for the next cycle).
        if children.iter().all(|c| c.task_status == "completed") {
            let signature = completed_cycle_signature(&children);
            let already_triggered = self.integration_triggered.get(&parent.id) == Some(&signature)
                || detail
                    .comments
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .any(|c| c.body.contains(INTEGRATION_MARKER) && c.body.contains(&signature));
            // Never bounce the parent while a supervise run row is still open:
            // the bounce would make the housekeeping cancel the still
            // finishing run and record a misleading "cancelled" failure.
            let supervise_running = match client.list_runs(parent.id).await {
                Ok(runs) => runs
                    .iter()
                    .any(|r| r.kind == SUPERVISE_RUN_KIND && r.status == "running"),
                Err(e) => {
                    tracing::warn!(task_id = parent.id, error = %e, "supervision: could not list runs; integration deferred this cycle");
                    true
                }
            };
            if !already_triggered && !supervise_running {
                match client.set_task_status(parent.id, "implement").await {
                    Ok(()) => {
                        self.integration_triggered.insert(parent.id, signature.clone());
                        note(
                            client,
                            parent.id,
                            format!(
                                "all child tickets completed — starting the final integration run: bring the \
                                 branch up to date with the project base, resolve conflicts, run the project \
                                 checks; the daemon pushes the branch and the PR targets the base branch \
                                 (cycle: {signature})"
                            ),
                        )
                        .await;
                        tracing::info!(
                            task_id = parent.id,
                            "supervision: all children completed → integration run"
                        );
                    }
                    Err(e) if e.is_conflict() || e.is_forbidden() => {
                        tracing::debug!(task_id = parent.id, error = %e, "supervision: parent moved under us; integration deferred");
                    }
                    Err(e) => {
                        tracing::warn!(task_id = parent.id, error = %e, "supervision: integration trigger failed; deferred to next poll")
                    }
                }
            }
            return;
        }
        // A child left `completed` (new child cycle, bounce, …) — re-arm the
        // integration trigger for when this cycle completes.
        self.integration_triggered.remove(&parent.id);

        // Kick off the next backlog child, if none is active.
        let Some(agent_id) = agent_id else {
            return;
        };
        let Some(candidate) = next_kickoff_candidate(&children, agent_id, managed_projects) else {
            return;
        };
        let child_id = candidate.id;
        if candidate.assignee_id.is_none()
            && let Err(e) = client.assign_task(child_id, agent_id).await
        {
            tracing::warn!(task_id = parent.id, child_id, error = %e, "supervision: could not assign child; kickoff deferred");
            return;
        }
        match client.set_task_status(child_id, "implement").await {
            Ok(()) => tracing::info!(
                task_id = parent.id,
                child_id,
                "supervision: kicked off child → implement"
            ),
            Err(e) if e.is_conflict() || e.is_forbidden() => {
                tracing::debug!(task_id = parent.id, child_id, error = %e, "supervision: child moved under us; skipping");
            }
            Err(e) => {
                tracing::warn!(task_id = parent.id, child_id, error = %e, "supervision: kickoff failed; deferred to next poll")
            }
        }
    }

    /// Whether a supervise run should be triggered for the parent: skip when
    /// one was attempted within the backoff window or a supervise run row is
    /// still `running` (the backend check survives daemon restarts). The
    /// backoff stamp is applied by [`Supervisor::mark_triggered`] only after
    /// the daemon actually launched the run — a capacity/port deferral must
    /// not eat the backoff window (review #110).
    async fn trigger_warranted(&mut self, client: &RemoterClient, parent_id: i32) -> bool {
        if self
            .last_trigger
            .get(&parent_id)
            .is_some_and(|t| t.elapsed() < TRIGGER_BACKOFF)
        {
            tracing::debug!(
                task_id = parent_id,
                "supervision: supervise-run trigger within backoff; skipping"
            );
            return false;
        }
        let runs = match client.list_runs(parent_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id = parent_id, error = %e, "supervision: could not list runs; trigger deferred");
                return false;
            }
        };
        if runs
            .iter()
            .any(|r| r.kind == SUPERVISE_RUN_KIND && r.status == "running")
        {
            tracing::debug!(
                task_id = parent_id,
                "supervision: supervise-run already active; skipping"
            );
            return false;
        }
        true
    }

    /// Records a successfully launched supervise trigger: stamps the backoff
    /// window and the attempt counter. Called by the daemon (which owns the
    /// capacity/port checks) only after the run was actually spawned.
    pub fn mark_triggered(&mut self, parent_id: i32) {
        self.last_trigger.insert(parent_id, Instant::now());
        *self.trigger_count.entry(parent_id).or_default() += 1;
    }

    /// Give-up signatures of children awaiting a human (test/observability
    /// hook). Keyed by child task id; the value is `{verdict}:{reason}`.
    pub fn human_notified_signatures(&self) -> Vec<(i32, String)> {
        self.human_notified
            .lock()
            .expect("human_notified poisoned")
            .iter()
            .map(|(id, sig)| (*id, sig.clone()))
            .collect()
    }

    /// Trigger attempts per parent ticket (test/observability hook).
    pub fn trigger_count(&self, parent_id: i32) -> u64 {
        self.trigger_count.get(&parent_id).copied().unwrap_or(0)
    }

    /// Whether a trigger attempt was recorded within the backoff window.
    pub fn triggered(&self, parent_id: i32) -> bool {
        self.last_trigger.contains_key(&parent_id)
    }
}

// ── supervise run execution ───────────────────────────────────────────────────

/// Everything the supervise run needs. Deliberately mirrors `run::RunContext`
/// (same lifecycle, different post-run actions).
pub struct SuperviseContext {
    pub client: RemoterClient,
    pub config: Arc<Config>,
    pub driver: Arc<dyn AgentDriver>,
    pub parent: TaskSummary,
    /// Children that were in `review` when the run was triggered.
    pub review_children: Vec<i32>,
    /// The parent's project repo config; `None` when the project is not
    /// agent-managed (the run then reviews reports without git context).
    pub project: Option<ProjectRepoConfig>,
    /// The run's port block (`REMOTER_AGENT_PORT_BASE`, spec §5.8); released on
    /// drop. `None` in container mode — sidecars make the port contract
    /// obsolete (containers spec §3.4).
    pub port_block: Option<PortBlock>,
    /// Serializes per-project agent-image builds (container mode).
    pub image_locks: crate::image::ImageLocks,
    /// Human-cancellation signal (the parent left `review`, spec §5.7).
    pub cancel: CancellationToken,
    pub log_store: LogStore,
    /// Shared with the supervision loop: records "a human was already notified"
    /// give-up signatures so the loop stops re-triggering supervise runs for
    /// a child whose give-up state has not changed (review #110).
    pub human_notified: HumanNotified,
}

/// Where a supervise attempt runs: the agent's working directory, whether
/// devenv services wrap the turn (spec §5.8), and the branch recorded on the
/// run row. Identical for every attempt of a run; merge-conflict nudges reuse
/// the parent's worktree as the cwd.
struct AttemptEnv<'a> {
    cwd: &'a std::path::Path,
    devenv: bool,
    branch: &'a str,
}

/// Run-row state threaded through the verdict actions: the run id, the
/// accumulating outcome (conflict nudges extend the session and sum tokens),
/// the per-attempt logger, and the attempt environment.
struct VerdictContext<'a> {
    run_id: i32,
    outcome: &'a mut RunOutcome,
    logger: &'a SessionLogger,
    env: AttemptEnv<'a>,
}

/// One child's diff against the parent's branch, or why it is unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ChildDiff {
    /// `git diff <base>...<child>`, capped to the prompt budget.
    Present(String),
    /// The child's branch has no local ref.
    ChildBranchMissing,
    /// The parent's branch exists neither locally nor on `origin` — there is
    /// no diff base at all.
    ParentBranchMissing,
    /// Both refs exist but `git diff` failed.
    DiffFailed,
}

/// One child's git facts for the supervise prompt.
#[derive(Clone)]
struct ChildGit {
    branch: String,
    /// Commits the child branch adds on top of the diff base.
    commits_ahead: Option<u32>,
    diff: ChildDiff,
}

/// Git facts for the whole supervise run; `None` when the project has no
/// local clone (the prompt degrades to a report-only review).
struct GitContext {
    parent_branch: String,
    /// The ref the diffs and commit counts were computed against: the
    /// parent's local branch, or `origin/<parent>` when only a remote copy
    /// survived; `None` when the parent's branch is missing everywhere.
    diff_base: Option<String>,
    children: HashMap<i32, ChildGit>,
}

/// Executes the supervise run to completion (success or recorded failure) and
/// then acts on the verdicts. Like `run::execute`, this only logs — a failed
/// supervise run must not disturb the poll loop, and it never moves the
/// parent (the supervision loop re-triggers on its own cadence).
pub async fn execute(rc: SuperviseContext) {
    let parent_id = rc.parent.id;
    // The agent works in the parent's worktree when one exists, else in the
    // project clone (reviewing diffs needs no checkout); with neither, the
    // workspace root keeps the session spawnable and the prompt degrades.
    let cwd = supervise_cwd(&rc);
    let devenv = workspace::uses_devenv(&cwd);
    let branch = parent_branch(&rc).await.unwrap_or_default();

    // Each attempt opens its own run row (mirroring `run::execute`): a
    // transient failure closes its row as failed, and a retry continues in a
    // fresh row — otherwise the retry's session id, tokens, and summary would
    // be written onto the dead row of attempt 1 and the success would look
    // like a failure (review #110).
    for attempt in 1..=rc.config.max_attempts {
        if rc.cancel.is_cancelled() {
            tracing::info!(task_id = parent_id, "supervise run cancelled before attempt {attempt}");
            return;
        }

        // A rejection here means the parent moved under us (human
        // intervention) — the human owns the status now, so just stop.
        let run = match rc.client.start_run(parent_id, SUPERVISE_RUN_KIND).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id = parent_id, error = %e, "supervise: could not start run row; skipping");
                return;
            }
        };
        tracing::info!(task_id = parent_id, run_id = run.id, attempt, "supervise run started");

        let (logger, _log_guard) = match rc.log_store.open_run(parent_id, run.id) {
            Ok(run_logger) => {
                let guard = RunLogGuard::new(rc.log_store.clone(), parent_id, run.id);
                (SessionLogger::new(run_logger), Some(guard))
            }
            Err(e) => {
                tracing::warn!(task_id = parent_id, run_id = run.id, error = %e, "supervise: could not open ACP log file; logging disabled");
                (SessionLogger::noop(), None)
            }
        };

        let prompt = match build_prompt(&rc).await {
            Ok(p) => p,
            Err(e) => {
                run::finish_failed(
                    &rc.client,
                    parent_id,
                    run.id,
                    format!("attempt {attempt}: prompt context fetch failed: {e}"),
                    None,
                    None,
                )
                .await;
                if e.is_transient() && attempt < rc.config.max_attempts {
                    tracing::warn!(task_id = parent_id, run_id = run.id, attempt, error = %e, "supervise: transient context-fetch failure; retrying");
                    backoff(&rc, attempt).await;
                    continue;
                }
                return;
            }
        };

        // Posted after build_prompt so the daemon's own note doesn't land in
        // this run's Conversation section (mirroring `run::execute`).
        note(
            &rc.client,
            parent_id,
            format!("supervise run #{} started (attempt {attempt})", run.id),
        )
        .await;

        let env = AttemptEnv {
            cwd: &cwd,
            devenv,
            branch: &branch,
        };
        let mut outcome = match run_attempt(&rc, run.id, &logger, &prompt, None, &env).await {
            Some(result) => match result {
                Ok(outcome) => outcome,
                Err(failure) => {
                    let retrying = matches!(failure.source, DriverError::Transient(_) | DriverError::Stalled(_))
                        && attempt < rc.config.max_attempts;
                    run::finish_failed(
                        &rc.client,
                        parent_id,
                        run.id,
                        format!("attempt {attempt}: {failure}"),
                        failure.input_tokens,
                        failure.output_tokens,
                    )
                    .await;
                    if retrying {
                        tracing::warn!(task_id = parent_id, run_id = run.id, attempt, error = %failure, "supervise: transient failure; retrying");
                        backoff(&rc, attempt).await;
                        continue;
                    }
                    tracing::warn!(task_id = parent_id, run_id = run.id, attempt, error = %failure, "supervise: run failed");
                    return;
                }
            },
            None => {
                // Cancelled mid-attempt (the parent left `review`).
                run::finish_failed(
                    &rc.client,
                    parent_id,
                    run.id,
                    "cancelled (human intervention or daemon shutdown)".to_string(),
                    None,
                    None,
                )
                .await;
                tracing::info!(task_id = parent_id, run_id = run.id, "supervise run cancelled");
                return;
            }
        };

        if rc.cancel.is_cancelled() {
            run::finish_failed(
                &rc.client,
                parent_id,
                run.id,
                "cancelled (human intervention or daemon shutdown)".to_string(),
                None,
                None,
            )
            .await;
            return;
        }

        // Act on the verdicts — merge-conflict nudges extend the session and
        // accumulate tokens, so this happens before the run row is finished.
        process_verdicts(&rc, run.id, run.created_at, &mut outcome, &logger, devenv, &branch).await;
        if rc.cancel.is_cancelled() {
            run::finish_failed(
                &rc.client,
                parent_id,
                run.id,
                "cancelled (human intervention or daemon shutdown)".to_string(),
                outcome.input_tokens,
                outcome.output_tokens,
            )
            .await;
            return;
        }

        let mut body = crate::client::FinishRunBody::succeeded();
        body.session_id = outcome.session_id.clone();
        body.summary = Some(outcome.text.clone());
        body.input_tokens = outcome.input_tokens;
        body.output_tokens = outcome.output_tokens;
        body.model = outcome.model.clone();
        body.thinking = outcome.thinking.clone();
        run::finish_reporting(&rc.client, run.id, &body).await;
        note(&rc.client, parent_id, format!("supervise run #{} finished", run.id)).await;
        tracing::info!(task_id = parent_id, run_id = run.id, "supervise run succeeded");
        return;
    }
}

/// Working directory for the supervise session (see `execute`).
fn supervise_cwd(rc: &SuperviseContext) -> PathBuf {
    let root = &rc.config.workspace_root;
    if let Some(project) = &rc.project {
        let wt = workspace::worktree_dir(root, project.project_id, rc.parent.id);
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

/// The parent's branch: the worktree's branch when one exists, else the name
/// recomputed from the title (same rule as `run::execute`).
async fn parent_branch(rc: &SuperviseContext) -> Option<String> {
    let project = rc.project.as_ref()?;
    let root = &rc.config.workspace_root;
    Some(
        workspace::existing_branch(root, project.project_id, rc.parent.id)
            .await
            .unwrap_or_else(|| workspace::branch_name(rc.parent.id, &rc.parent.title)),
    )
}

/// The env every supervise turn (and its services/sidecars) gets — the same
/// contract as claim runs (spec §5.8 + containers spec §3.4): host mode
/// exports the port block, container mode exports `REMOTER_CONTAINER=1` and
/// loopback DB URLs.
fn supervise_env(rc: &SuperviseContext) -> Vec<(String, String)> {
    if rc.config.execution.is_container() {
        return crate::container::container_env(rc.parent.id);
    }
    let mut env = vec![
        ("CI".to_string(), "1".to_string()),
        ("REMOTER_AGENT_TASK_ID".to_string(), rc.parent.id.to_string()),
    ];
    if let Some(block) = &rc.port_block {
        env.push(("REMOTER_AGENT_PORT_BASE".to_string(), block.base().to_string()));
    }
    env
}

/// One driver turn with the cancellation/timeout envelope: devenv services up
/// around the turn, always down after. `None` = cancelled mid-attempt.
async fn run_attempt(
    rc: &SuperviseContext,
    run_id: i32,
    logger: &SessionLogger,
    prompt: &str,
    resume_session: Option<String>,
    env: &AttemptEnv<'_>,
) -> Option<Result<RunOutcome, RunFailure>> {
    let env_vars = supervise_env(rc);
    // Container mode: the supervise turn runs in a container like any other
    // run — no silent fallback to host (a container-start failure is a normal
    // transient error with retry). The guard's drop is the teardown on every
    // exit path, including cancellation mid-turn. A parent whose project is
    // not agent-managed has no repo to build an image from — it keeps the
    // host path (its review is report-only anyway).
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
                &[],
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
        task_id: rc.parent.id,
        run_id,
        cwd: env.cwd.to_path_buf(),
        prompt: prompt.to_string(),
        kind: SUPERVISE_RUN_KIND,
        resume_session,
        branch: env.branch.to_string(),
        env: env_vars,
        exec,
        config_options: rc.config.driver.config_options_for(SUPERVISE_RUN_KIND),
        logger: logger.clone(),
        phase_tx: None,
    };
    let timeout = Duration::from_secs(rc.config.run_timeout_minutes * 60);
    let result = tokio::select! {
        _ = rc.cancel.cancelled() => None,
        r = tokio::time::timeout(timeout, rc.driver.run(spec)) => Some(r),
    };
    if containers.is_none() && env.devenv {
        // Best-effort, must not mask the turn's real outcome (spec §5.8).
        workspace::services_down(env.cwd, &supervise_env(rc)).await;
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

/// Exponential backoff between attempts (spec §5.7), interruptible by
/// cancellation.
async fn backoff(rc: &SuperviseContext, attempt: u32) {
    let shift = (attempt - 1).min(10);
    let delay = rc.config.retry_backoff_secs.saturating_mul(1u64 << shift);
    tokio::select! {
        _ = rc.cancel.cancelled() => {}
        _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
    }
}

/// Best-effort daemon note (spec §5.6); failures never affect the run.
async fn note(client: &RemoterClient, task_id: i32, body: String) {
    run::note(client, task_id, body).await
}

// ── prompt ────────────────────────────────────────────────────────────────────

/// Fetches the parent detail and the reviewing children's details, computes
/// the git context, and renders the prompt.
async fn build_prompt(rc: &SuperviseContext) -> Result<String, crate::client::ClientError> {
    let parent_detail = rc.client.task_detail(rc.parent.id).await?;
    let mut children = Vec::with_capacity(rc.review_children.len());
    for id in &rc.review_children {
        match rc.client.task_detail(*id).await {
            Ok(d) => children.push(d),
            Err(e) => tracing::warn!(child_id = id, error = %e, "supervise: could not fetch child detail"),
        }
    }
    let git = git_context(rc, &children).await;
    Ok(render_prompt(
        &rc.parent,
        &parent_detail,
        &children,
        git.as_ref(),
        rc.config.execution.is_container(),
    ))
}

/// Git facts for the prompt: the parent's branch plus, per child, the branch,
/// commits-ahead count, and the three-dot diff against the parent. `None`
/// when the project is not agent-managed or has no local clone yet.
async fn git_context(rc: &SuperviseContext, children: &[TaskDetail]) -> Option<GitContext> {
    let project = rc.project.as_ref()?;
    let root = &rc.config.workspace_root;
    let repo = workspace::repo_dir(root, project.project_id);
    if !repo.exists() {
        return None;
    }
    let parent_branch = parent_branch(rc).await?;
    let mut child_branches = Vec::with_capacity(children.len());
    for child in children {
        let branch = workspace::existing_branch(root, project.project_id, child.id)
            .await
            .unwrap_or_else(|| workspace::branch_name(child.id, &child.title));
        child_branches.push((child.id, branch));
    }
    Some(collect_git_context(&repo, &parent_branch, &child_branches).await)
}

/// Pure git-ref facts for the prompt, split out from `git_context` (which
/// resolves branch names from the workspace) so tests can run it against
/// scratch repositories.
///
/// The diff base is the parent's local branch; when that ref is gone (a
/// wiped/recreated workspace, a renamed ticket) the parent's remote-tracking
/// ref `origin/<parent>` is fetched best-effort and used instead. When the
/// parent's branch exists nowhere, one precise WARN names the missing ref and
/// every child degrades to `ParentBranchMissing` — instead of a cryptic git
/// "ambiguous argument" error per child.
async fn collect_git_context(repo: &Path, parent_branch: &str, children: &[(i32, String)]) -> GitContext {
    let diff_base = if workspace::local_branch_exists(repo, parent_branch).await {
        Some(parent_branch.to_string())
    } else {
        // Best-effort: the branch may still exist on origin (a wiped workspace
        // re-cloned from the remote keeps the remote-tracking ref).
        let _ = workspace::fetch_branch(repo, parent_branch).await;
        if workspace::remote_tracking_branch_exists(repo, parent_branch).await {
            Some(format!("origin/{parent_branch}"))
        } else {
            tracing::warn!(
                parent_branch,
                "supervise: parent branch not found locally or on origin — child diffs unavailable"
            );
            None
        }
    };
    let mut map = HashMap::new();
    for (child_id, branch) in children {
        let Some(base) = &diff_base else {
            map.insert(
                *child_id,
                ChildGit {
                    branch: branch.clone(),
                    commits_ahead: None,
                    diff: ChildDiff::ParentBranchMissing,
                },
            );
            continue;
        };
        if !workspace::local_branch_exists(repo, branch).await {
            map.insert(
                *child_id,
                ChildGit {
                    branch: branch.clone(),
                    commits_ahead: None,
                    diff: ChildDiff::ChildBranchMissing,
                },
            );
            continue;
        }
        let ahead = workspace::commit_count(repo, &format!("{base}..{branch}")).await.ok();
        let diff = match workspace::diff(repo, &format!("{base}...{branch}")).await {
            Ok(d) => ChildDiff::Present(run::truncate_with_marker(&d, MAX_DIFF_BYTES)),
            Err(e) => {
                tracing::warn!(child_id, error = %e, "supervise: could not compute child diff");
                ChildDiff::DiffFailed
            }
        };
        map.insert(
            *child_id,
            ChildGit {
                branch: branch.clone(),
                commits_ahead: ahead,
                diff,
            },
        );
    }
    GitContext {
        parent_branch: parent_branch.to_string(),
        diff_base,
        children: map,
    }
}

/// Supervise-run instructions (spec §5.6): the supervisor reviews each child's
/// diff + report and records a verdict per child. Module-scope so tests can
/// pin the contract.
const SUPERVISE_INSTRUCTIONS: &str = "## Instructions\nYou are the supervisor for the parent ticket above: its child tickets are \
     implemented by agent runs and await your review before their work is merged into the \
     parent's branch. For EACH child section below, study the diff against the parent's branch \
     and the implementation report, then record your verdict with the `set_task_review` MCP \
     tool — exactly one call per child: `set_task_review(taskId, markdown, verdict)`.\n\n\
     Verdicts:\n\
     - `approve` — the diff fully implements the child ticket, the report explains what was \
     done and how to test/deploy it, and the work is complete. The daemon merges the child's \
     branch into the parent's branch and completes the ticket.\n\
     - `changes_requested` — anything is missing, wrong, untested, or the report is absent. \
     The markdown MUST list concrete numbered findings the implementer can act on: the daemon \
     posts it to the child's thread and returns the ticket to `implement`.\n\n\
     Rules:\n\
     - READ-ONLY: do not create, edit, or delete files and do not commit anything — you review, \
     the daemon merges. Never end your turn with background tasks still running — your turn's \
     end is final: the daemon shuts the agent down and pending tasks are killed, their \
     completion never arrives.\n\
     - Approve only what you would merge into the parent branch yourself.\n\
     - When a child produced no code changes (a ticket-creation run), review its outcome and \
     either approve or list what is missing.\n\n";

/// Extra rule appended to the instructions in container mode: child worktrees
/// are not mounted into the supervise run's container, so the supervisor must
/// read child work through git objects of the shared clone, never the FS.
const CONTAINER_INSTRUCTIONS: &str = "- CONTAINER MODE: child worktrees (`wt-<child>`) are NOT available on the \
     filesystem here — read the child's work via git objects of the shared clone: \
     `git show <child-branch>:<path>` for file contents, `git diff <base>...<child-branch>` \
     for changes.\n\n";

/// Pure prompt assembly, split out for tests. `git` is `None` when no local
/// clone exists — the prompt then degrades to a report-only review.
fn render_prompt(
    parent: &TaskSummary,
    parent_detail: &TaskDetail,
    children: &[TaskDetail],
    git: Option<&GitContext>,
    container_mode: bool,
) -> String {
    let mut p = String::new();
    p.push_str("## Ticket\n");
    p.push_str(&format!("Ticket #{}\n\n", parent.id));
    p.push_str(&format!("{}\n\n", parent.title));
    p.push_str(&format!("{}\n\n", parent.description));
    p.push_str(&format!(
        "Project: {} — Feature: {}\n\n",
        parent.project_name, parent.feature_description
    ));
    conversation_section(&mut p, parent_detail.comments.as_deref().unwrap_or(&[]));

    p.push_str("## Children awaiting review\n\n");
    if children.is_empty() {
        p.push_str("_None — review the child's thread on the parent for context._\n\n");
    }
    for child in children {
        p.push_str(&format!("### Child #{} — {}\n\n", child.id, child.title));
        p.push_str(&format!(
            "{}\n\n",
            run::truncate_with_marker(&child.description, MAX_CHILD_DESC_BYTES)
        ));
        if latest_run_failed(child) {
            p.push_str(
                "_The child's newest run FAILED and the daemon escalated this ticket to review — \
                 the work was NOT finished by the agent. Judge the diff on its own merits: approve \
                 only complete, verifiable work; an empty diff with a failed run is never \
                 approvable._\n\n",
            );
        }
        match git {
            None => p.push_str(
                "_No local clone of the project repository — review the child's implementation \
                 report and thread instead of a diff._\n\n",
            ),
            Some(g) => match g.children.get(&child.id) {
                None => p.push_str("_Git context unavailable for this child._\n\n"),
                Some(cg) => {
                    let ahead = cg
                        .commits_ahead
                        .map(|n| format!("{n} commit(s)"))
                        .unwrap_or_else(|| "unknown".to_string());
                    match (&cg.diff, &g.diff_base) {
                        (ChildDiff::Present(diff), Some(base)) => {
                            p.push_str(&format!("Branch: `{}` ({} ahead of `{}`)\n\n", cg.branch, ahead, base));
                            p.push_str(&format!("#### Diff (`{base}`...`{}`)\n\n", cg.branch));
                            p.push_str(&format!("```diff\n{diff}\n```\n\n"));
                        }
                        (ChildDiff::ChildBranchMissing, _) => p.push_str(&format!(
                            "_Child branch `{}` not found locally — the diff is unavailable; \
                             review the report and thread._\n\n",
                            cg.branch
                        )),
                        (ChildDiff::ParentBranchMissing, _) => p.push_str(&format!(
                            "_Parent branch `{}` not found locally or on origin — there is no \
                             diff base; review the report and thread. On `approve` the daemon \
                             recreates the parent's branch from the base branch before \
                             merging._\n\n",
                            g.parent_branch
                        )),
                        (ChildDiff::DiffFailed, base) => p.push_str(&format!(
                            "_The diff against `{}` could not be computed — review the report \
                             and thread._\n\n",
                            base.as_deref().unwrap_or(&g.parent_branch)
                        )),
                        (ChildDiff::Present(_), None) => {
                            unreachable!("a diff cannot exist without a diff base")
                        }
                    }
                }
            },
        }
        p.push_str(&format!("#### Implementation report (#{})\n\n", child.id));
        match child.report.as_deref().filter(|r| !r.trim().is_empty()) {
            Some(report) => p.push_str(&format!("{}\n\n", run::truncate_with_marker(report, MAX_REPORT_BYTES))),
            None => p.push_str(
                "_The child wrote no implementation report — that alone is \
                                worth a `changes_requested`._\n\n",
            ),
        }
    }

    // Language policy (same as the other run kinds): reasoning in English is
    // cheaper and the strongest for the model, but everything the human reads
    // mirrors the ticket's language.
    p.push_str(
        "## Language\nThink and reason in English — it is more token-efficient. Write everything the \
         human reads in the ticket's language: the reviews and comments. Code, commit messages, and \
         tool calls stay in English.\n\n",
    );
    p.push_str(SUPERVISE_INSTRUCTIONS);
    if container_mode {
        p.push_str(CONTAINER_INSTRUCTIONS);
    }
    p
}

/// The parent's newest comments, bounded like the other prompts (spec §5.6) —
/// human steering ("don't merge yet", "watch out for X") belongs in context.
/// Shared with the review run's prompt (#142).
pub(crate) fn conversation_section(p: &mut String, comments: &[CommentDto]) {
    if comments.is_empty() {
        return;
    }
    p.push_str("## Conversation\n");
    let skipped_by_count = comments.len().saturating_sub(run::MAX_CONVERSATION_COMMENTS);
    let candidates = &comments[skipped_by_count..];
    let mut remaining = run::MAX_CONVERSATION_BYTES;
    let mut selected_start = candidates.len();
    for (i, c) in candidates.iter().enumerate().rev() {
        let cost = c.body.len().min(run::MAX_COMMENT_BYTES);
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
        p.push_str(&format!(
            "- {}\n",
            run::truncate_with_marker(&c.body, run::MAX_COMMENT_BYTES)
        ));
    }
    p.push('\n');
}

// ── verdicts ──────────────────────────────────────────────────────────────────

/// Acts on the review verdicts the supervisor wrote during the run: bounces
/// `changes_requested` children to `implement`, merges `approve` children into
/// the parent's branch and completes them. Merge-conflict resolution goes back
/// to the agent on the resumed session (up to `MAX_MERGE_NUDGES` turns).
///
/// A verdict counts only when it was written at or after this run's start:
/// the outcome row survives status changes, so without the freshness check a
/// crashed run would re-apply the verdict of a previous review cycle — e.g.
/// bounce the child with a stale `changes_requested` nobody looked at (review
/// #110).
async fn process_verdicts(
    rc: &SuperviseContext,
    run_id: i32,
    run_started_at: Option<chrono::DateTime<chrono::Utc>>,
    outcome: &mut RunOutcome,
    logger: &SessionLogger,
    devenv: bool,
    branch: &str,
) {
    for child_id in &rc.review_children {
        if rc.cancel.is_cancelled() {
            return;
        }
        let detail = match rc.client.task_detail(*child_id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(child_id, error = %e, "supervise: could not fetch child for verdict processing");
                continue;
            }
        };
        // The child moved under us (human, or a previous verdict pass).
        if detail.task_status != "review" {
            continue;
        }
        let verdict_fresh = match (&detail.review_outcome_updated_at, run_started_at) {
            (Some(written_at), Some(started_at)) => written_at >= &started_at,
            // Unknown timestamps (an older backend that does not serialize
            // them): keep the legacy behavior and act on the verdict.
            _ => true,
        };
        match detail.review_verdict.as_deref() {
            Some(verdict) if !verdict_fresh => {
                note(
                    &rc.client,
                    rc.parent.id,
                    format!(
                        "supervise run: child #{child_id} still carries the `{verdict}` verdict from a \
                         previous review cycle — not re-applied; re-reviewing after the backoff"
                    ),
                )
                .await;
            }
            Some("approve") => accept_child(rc, run_id, outcome, logger, devenv, branch, &detail).await,
            Some("changes_requested") => bounce_child(rc, &detail).await,
            _ => {
                note(
                    &rc.client,
                    rc.parent.id,
                    format!("supervise run: no review verdict written for child #{child_id} — left in review; re-reviewing after the backoff"),
                )
                .await;
            }
        }
    }
}

/// Records a give-up signature so the supervision loop stops re-triggering
/// supervise runs for this child while its verdict (and thus the give-up
/// state) is unchanged.
fn record_give_up(rc: &SuperviseContext, child_id: i32, verdict: Option<&str>, reason: &str) {
    rc.human_notified
        .lock()
        .expect("human_notified poisoned")
        .insert(child_id, format!("{}:{reason}", verdict.unwrap_or("-")));
}

/// `changes_requested`: return the ticket to `implement` (a regular agent
/// transition — the daemon is the assignee), then post the review to the
/// child's thread (the bounced agent reads its prompt from the thread). The
/// note reflects the actual outcome: it is only posted once the status move
/// landed, so a transient failure cannot leave a "bounced" note on a child
/// that is still in `review` (review #110).
async fn bounce_child(rc: &SuperviseContext, child: &TaskDetail) {
    // Circuit-breaker backstop: a supervise run already in flight when the
    // breaker tripped (or a trigger from before the last failure was
    // recorded) must not bounce the child into yet another failing run —
    // leave it in `review` with one human note and latch the give-up so the
    // loop stops re-triggering while the verdict is unchanged (spec §5.4).
    if circuit_broken(child) {
        record_give_up(rc, child.id, child.review_verdict.as_deref(), "repeated-failure");
        note(
            &rc.client,
            child.id,
            format!(
                "supervisor requested changes, but this ticket already failed and escalated \
                 {MAX_FAILURE_CYCLES} times in a row (the newest run failed again after the last \
                 bounce) — the automatic bounce loop stops here; left in review for a human"
            ),
        )
        .await;
        tracing::warn!(
            child_id = child.id,
            "supervise: circuit breaker tripped — no automatic bounce"
        );
        return;
    }
    let body = child
        .review
        .as_deref()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or("(supervisor requested changes but wrote no review body)");
    match rc.client.set_task_status(child.id, "implement").await {
        Ok(()) => {
            note(
                &rc.client,
                child.id,
                format!(
                    "## Supervisor review — changes requested\n\n{body}\n\n— bounced to implement by the supervisor."
                ),
            )
            .await;
            tracing::info!(child_id = child.id, "supervise: changes requested → implement");
        }
        Err(e) if e.is_conflict() || e.is_forbidden() => {
            tracing::debug!(child_id = child.id, error = %e, "supervise: child moved under us; bounce skipped");
        }
        Err(e) => {
            note(
                &rc.client,
                child.id,
                format!("## Supervisor review — changes requested\n\n{body}\n\n— the supervisor tried to return the ticket to `implement`, but the status write failed ({e}); it will retry."),
            )
            .await;
            tracing::warn!(child_id = child.id, error = %e, "supervise: bounce failed; the supervision loop retries");
        }
    }
}

/// `approve`: absorb the child's branch into the parent's branch, then
/// complete the child (`review → completed` under the parent-scoped agent
/// rights). Conflicts are delegated to the agent on the resumed session.
async fn accept_child(
    rc: &SuperviseContext,
    run_id: i32,
    outcome: &mut RunOutcome,
    logger: &SessionLogger,
    devenv: bool,
    branch: &str,
    child: &TaskDetail,
) {
    let Some(project) = &rc.project else {
        record_give_up(rc, child.id, child.review_verdict.as_deref(), "no-repo");
        note(
            &rc.client,
            child.id,
            "supervisor approved, but the project has no repository configured — the daemon \
             cannot merge the branch; left in review for a human"
                .to_string(),
        )
        .await;
        return;
    };
    let root = &rc.config.workspace_root;
    let repo = workspace::repo_dir(root, project.project_id);
    let Some(parent_branch) = parent_branch(rc).await else {
        return;
    };
    let child_branch = workspace::existing_branch(root, project.project_id, child.id)
        .await
        .unwrap_or_else(|| workspace::branch_name(child.id, &child.title));
    // The child may have run standalone or on another host: its branch can
    // exist on origin only. Fetch and reattach it before giving up —
    // `child-branch-missing` is for a ref confirmed absent everywhere
    // (#138, symmetric to the parent's self-heal from #137).
    match workspace::ensure_local_branch(&repo, &child_branch).await {
        Ok(workspace::LocalBranchState::Present) => {}
        Ok(workspace::LocalBranchState::Reattached) => {
            note(
                &rc.client,
                child.id,
                format!(
                    "supervisor approved — the child's branch `{child_branch}` was missing \
                     locally and has been restored from `origin/{child_branch}`; the merge \
                     continues"
                ),
            )
            .await;
            tracing::info!(
                child_id = child.id,
                child_branch,
                "supervise: child branch reattached from origin"
            );
        }
        Ok(workspace::LocalBranchState::Missing) => {
            record_give_up(rc, child.id, child.review_verdict.as_deref(), "child-branch-missing");
            note(
                &rc.client,
                child.id,
                format!(
                    "supervisor approved, but the child's branch `{child_branch}` does not \
                     exist locally or on origin — cannot merge; left in review for a human"
                ),
            )
            .await;
            return;
        }
        Err(e) => {
            record_give_up(
                rc,
                child.id,
                child.review_verdict.as_deref(),
                "child-branch-reattach-failed",
            );
            note(
                &rc.client,
                child.id,
                format!(
                    "supervisor approved and the child's branch `{child_branch}` exists on \
                     origin, but recreating it locally failed ({e}) — cannot merge; left in \
                     review for a human"
                ),
            )
            .await;
            return;
        }
    }

    // Approve guard (spec §5.4): an escalated child — newest run failed — whose
    // branch adds no commits over the parent's branch has no finished work to
    // merge; approving it would move an unfinished ticket to `completed` and
    // bury the failure. Refuse deterministically and leave it for a human.
    if latest_run_failed(child)
        && matches!(
            workspace::commit_count(&repo, &format!("{parent_branch}..{child_branch}")).await,
            Ok(0)
        )
    {
        record_give_up(rc, child.id, child.review_verdict.as_deref(), "empty-diff-failed-run");
        note(
            &rc.client,
            child.id,
            format!(
                "supervisor approved, but the child's newest run failed and the branch \
                 `{child_branch}` adds no commits over `{parent_branch}` — there is no finished \
                 work to merge; left in review for a human"
            ),
        )
        .await;
        return;
    }

    // The merge needs the parent's branch checked out: reuse the parent's
    // worktree, re-creating it when a cleanup removed it earlier. `prepare`
    // also recreates the parent's branch itself when its local ref is gone
    // (wiped workspace) — from `origin/<branch>`, else fresh off the base
    // branch — so an `approve` self-heals instead of stranding the child.
    let wt = workspace::worktree_dir(root, project.project_id, rc.parent.id);
    if !wt.exists()
        && let Err(e) = workspace::prepare(
            root,
            project.project_id,
            rc.parent.id,
            &rc.parent.title,
            &project.repo_url,
            project.base_branch.as_deref(),
        )
        .await
    {
        record_give_up(rc, child.id, child.review_verdict.as_deref(), "worktree-prepare-failed");
        note(&rc.client, child.id, format!("supervisor approved, but the parent's worktree could not be prepared ({e}) — left in review for a human"))
            .await;
        return;
    }
    if !workspace::local_branch_exists(&repo, &parent_branch).await {
        record_give_up(rc, child.id, child.review_verdict.as_deref(), "parent-branch-missing");
        note(
            &rc.client,
            child.id,
            format!(
                "supervisor approved, but the parent's branch `{parent_branch}` does not exist \
                 locally and could not be recreated from origin or the base branch — cannot \
                 merge; left in review for a human"
            ),
        )
        .await;
        return;
    }

    if let Ok(status) = workspace::dirty_status(&wt).await
        && !status.is_empty()
    {
        record_give_up(rc, child.id, child.review_verdict.as_deref(), "worktree-dirty");
        note(
            &rc.client,
            child.id,
            format!(
                "supervisor approved, but the parent's worktree is dirty — cannot merge safely; \
                 left in review for a human:\n```\n{}\n```",
                run::truncate_with_marker(&status, 500)
            ),
        )
        .await;
        return;
    }

    let message = format!("Merge child #{} ({child_branch}) into {parent_branch}", child.id);
    match workspace::merge(&wt, &child_branch, &message).await {
        Ok(workspace::MergeOutcome::Clean) => {
            complete_child(rc, child.id, &child_branch, &parent_branch).await;
        }
        Ok(workspace::MergeOutcome::Conflicted) => {
            let vx = VerdictContext {
                run_id,
                outcome,
                logger,
                env: AttemptEnv {
                    cwd: &wt,
                    devenv,
                    branch,
                },
            };
            resolve_conflicts_then_complete(rc, vx, child, &wt, &child_branch, &parent_branch).await;
        }
        Err(e) => {
            record_give_up(rc, child.id, child.review_verdict.as_deref(), "merge-failed");
            note(
                &rc.client,
                child.id,
                format!("supervisor approved, but the merge failed: {e} — left in review for a human"),
            )
            .await;
        }
    }
}

/// Complete a successfully merged child (`review → completed`) and note the
/// absorption on its thread.
async fn complete_child(rc: &SuperviseContext, child_id: i32, child_branch: &str, parent_branch: &str) {
    match rc.client.set_task_status(child_id, "completed").await {
        Ok(()) => {
            note(
                &rc.client,
                child_id,
                format!("supervisor approved — `{child_branch}` merged into `{parent_branch}`; ticket completed"),
            )
            .await;
            tracing::info!(child_id, "supervise: approved → merged → completed");
        }
        Err(e) if e.is_conflict() || e.is_forbidden() => {
            tracing::debug!(child_id, error = %e, "supervise: child moved under us; completion skipped");
        }
        Err(e) => tracing::warn!(child_id, error = %e, "supervise: completion failed; the supervision loop retries"),
    }
}

/// The merge stopped on conflicts: ask the agent (resumed session, working in
/// the parent's worktree) to resolve them, run the project checks, and
/// complete the merge commit. Completes the child once the child's tip is
/// absorbed and the worktree is clean; otherwise leaves it in review with a
/// human-help note.
async fn resolve_conflicts_then_complete(
    rc: &SuperviseContext,
    vx: VerdictContext<'_>,
    child: &TaskDetail,
    wt: &std::path::Path,
    child_branch: &str,
    parent_branch: &str,
) {
    let child_tip = match workspace::git_rev_parse(wt, child_branch).await {
        Ok(tip) => tip,
        Err(e) => {
            record_give_up(rc, child.id, child.review_verdict.as_deref(), "child-tip-unresolvable");
            note(
                &rc.client,
                child.id,
                format!("supervisor approved, but the merge cannot proceed: {e}"),
            )
            .await;
            return;
        }
    };
    for nudge in 1..=MAX_MERGE_NUDGES {
        // Resolved already (also before the first nudge — a merge cannot be
        // double-checked too often): complete and stop.
        match conflict_resolved(wt, &child_tip).await {
            Ok(true) => {
                complete_child(rc, child.id, child_branch, parent_branch).await;
                return;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(child_id = child.id, nudge, error = %e, "supervise: pre-nudge verification failed");
            }
        }
        let unmerged = workspace::unmerged_paths(wt).await.unwrap_or_default();
        let prompt = merge_conflict_nudge_prompt(parent_branch, child_branch, &unmerged);
        let nudge_env = AttemptEnv {
            cwd: wt,
            devenv: vx.env.devenv,
            branch: vx.env.branch,
        };
        let Some(result) = run_attempt(
            rc,
            vx.run_id,
            vx.logger,
            &prompt,
            vx.outcome.session_id.clone(),
            &nudge_env,
        )
        .await
        else {
            // Cancelled mid-nudge: roll the half-open merge back so the
            // parent's worktree stays usable.
            let _ = workspace::merge_abort(wt).await;
            return;
        };
        match result {
            Ok(nudge_outcome) => {
                vx.outcome.session_id = nudge_outcome.session_id.clone().or(vx.outcome.session_id.clone());
                if !nudge_outcome.text.trim().is_empty() {
                    vx.outcome.text = nudge_outcome.text.clone();
                }
                vx.outcome.input_tokens = sum_tokens(vx.outcome.input_tokens, nudge_outcome.input_tokens);
                vx.outcome.output_tokens = sum_tokens(vx.outcome.output_tokens, nudge_outcome.output_tokens);
            }
            Err(e) => {
                tracing::warn!(child_id = child.id, nudge, error = %e, "supervise: conflict-resolution nudge failed");
            }
        }
        match conflict_resolved(wt, &child_tip).await {
            Ok(true) => {
                complete_child(rc, child.id, child_branch, parent_branch).await;
                return;
            }
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!(child_id = child.id, nudge, error = %e, "supervise: post-nudge verification failed");
                continue;
            }
        }
    }
    record_give_up(rc, child.id, child.review_verdict.as_deref(), "conflict-unresolved");
    note(
        &rc.client,
        child.id,
        format!(
            "supervisor approved, but the merge conflicts were not resolved after {MAX_MERGE_NUDGES} \
             attempts — left in review for a human (the parent's worktree holds the half-resolved \
             merge of `{child_branch}` into `{parent_branch}`)"
        ),
    )
    .await;
}

/// The merge counts as resolved once the child's tip is an ancestor of HEAD,
/// no unmerged entries remain, and the worktree is clean.
async fn conflict_resolved(wt: &std::path::Path, child_tip: &str) -> Result<bool, workspace::WorkspaceError> {
    if !workspace::contains_commit(wt, child_tip).await? {
        return Ok(false);
    }
    if !workspace::unmerged_paths(wt).await?.is_empty() {
        return Ok(false);
    }
    Ok(workspace::dirty_status(wt).await?.is_empty())
}

/// Prompt sent back to the agent when the child's merge into the parent's
/// branch stops on conflicts. The agent must resolve, check, and complete the
/// merge — the daemon verifies and completes the child ticket. An empty
/// `unmerged` means the agent resolved the files but did not complete the
/// merge commit — the nudge then only asks to finish it (the loop no longer
/// skips a nudge on an empty list, review #110).
fn merge_conflict_nudge_prompt(parent_branch: &str, child_branch: &str, unmerged: &[String]) -> String {
    if unmerged.is_empty() {
        return format!(
            "The merge of `{child_branch}` into `{parent_branch}` is still open — no conflicted \
             files remain, but the merge commit is not done. Run the project's checks (e.g. `just \
             lint test` or the repo's equivalent), complete the merge with `git commit`, and leave \
             the worktree clean (`git status --porcelain` empty). Do NOT push and do NOT abort the \
             merge — the daemon verifies the merge and finishes the child ticket."
        );
    }
    let files = unmerged.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n");
    format!(
        "Merging `{child_branch}` into `{parent_branch}` stopped on conflicts. Resolve every \
         conflicted file:\n{files}\n\nThen run the project's checks (e.g. `just lint test` or the \
         repo's equivalent), complete the merge with `git commit`, and leave the worktree clean \
         (`git status --porcelain` empty). Do NOT push and do NOT abort the merge — the daemon \
         verifies the merge and finishes the child ticket."
    )
}

fn sum_tokens(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (Some(x), Some(y)) => Some(x + y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(id: i32, status: &str, blocked: bool, priority: Option<&str>, assignee_id: Option<i32>) -> TaskDetail {
        TaskDetail {
            id,
            project_id: 1,
            project_name: "p".into(),
            feature_id: 1,
            feature_description: "f".into(),
            title: format!("child {id}"),
            description: String::new(),
            task_status: status.into(),
            task_priority: priority.map(str::to_string),
            blocked,
            assignee_id,
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

    fn managed() -> HashSet<i32> {
        HashSet::from([1])
    }

    #[test]
    fn picks_single_backlog_child() {
        let children = vec![
            child(1, "backlog", false, None, Some(7)),
            child(2, "completed", false, None, Some(7)),
        ];
        assert_eq!(next_kickoff_candidate(&children, 7, &managed()).map(|c| c.id), Some(1));
    }

    #[test]
    fn active_child_blocks_kickoff() {
        for active in ACTIVE_CHILD_STATUSES {
            let children = vec![
                child(1, active, false, None, Some(7)),
                child(2, "backlog", false, None, Some(7)),
            ];
            assert!(
                next_kickoff_candidate(&children, 7, &managed()).is_none(),
                "active child in {active} must block kickoff"
            );
        }
    }

    #[test]
    fn blocked_child_is_skipped_and_child_in_review_is_not_active() {
        // Child 1 sits in review (awaiting supervision), child 2 is blocked,
        // child 3 is kickable — the rollout queue continues.
        let children = vec![
            child(1, "review", false, None, Some(7)),
            child(2, "backlog", true, None, Some(7)),
            child(3, "backlog", false, None, Some(7)),
        ];
        assert_eq!(next_kickoff_candidate(&children, 7, &managed()).map(|c| c.id), Some(3));
    }

    #[test]
    fn priority_order_then_creation_order() {
        let children = vec![
            child(1, "backlog", false, Some("low"), Some(7)),
            child(2, "backlog", false, Some("critical"), Some(7)),
            child(3, "backlog", false, Some("high"), Some(7)),
            child(4, "backlog", false, None, Some(7)), // default medium
        ];
        assert_eq!(next_kickoff_candidate(&children, 7, &managed()).map(|c| c.id), Some(2));
        let without_2: Vec<TaskDetail> = children.into_iter().filter(|c| c.id != 2).collect();
        assert_eq!(next_kickoff_candidate(&without_2, 7, &managed()).map(|c| c.id), Some(3));
    }

    #[test]
    fn same_priority_falls_back_to_lowest_id() {
        let children = vec![
            child(9, "backlog", false, Some("high"), Some(7)),
            child(5, "backlog", false, Some("high"), Some(7)),
        ];
        assert_eq!(next_kickoff_candidate(&children, 7, &managed()).map(|c| c.id), Some(5));
    }

    #[test]
    fn child_assigned_to_someone_else_is_left_alone() {
        let children = vec![
            child(1, "backlog", false, None, Some(42)),
            child(2, "backlog", false, None, None),
        ];
        assert_eq!(next_kickoff_candidate(&children, 7, &managed()).map(|c| c.id), Some(2));
    }

    #[test]
    fn child_in_unmanaged_project_is_skipped() {
        let mut c = child(1, "backlog", false, None, Some(7));
        c.project_id = 99;
        let children = vec![c, child(2, "backlog", false, None, Some(7))];
        assert_eq!(next_kickoff_candidate(&children, 7, &managed()).map(|c| c.id), Some(2));
    }

    // ── prompt ────────────────────────────────────────────────────────────

    fn summary() -> TaskSummary {
        TaskSummary {
            id: 10,
            project_id: 1,
            project_name: "proj".into(),
            feature_id: 1,
            feature_description: "feat".into(),
            title: "parent ticket".into(),
            description: "parent body".into(),
            task_status: "review".into(),
            task_priority: None,
            actions_total: 0,
            actions_completed: 0,
            time_spent: 0,
            blocked: false,
            agent_review_requested: false,
        }
    }

    fn git_context() -> GitContext {
        let mut children = HashMap::new();
        children.insert(
            42,
            ChildGit {
                branch: "agent/task-42-child".into(),
                commits_ahead: Some(2),
                diff: ChildDiff::Present("diff --git a/x b/x".into()),
            },
        );
        GitContext {
            parent_branch: "agent/task-10-parent".into(),
            diff_base: Some("agent/task-10-parent".into()),
            children,
        }
    }

    /// The supervise prompt carries the parent ticket, the child's branch,
    /// diff, and report, and instructs the agent to write a verdict per child
    /// via set_task_review.
    #[test]
    fn render_prompt_carries_child_diff_report_and_verdict_instructions() {
        let mut c = child(42, "review", false, None, Some(7));
        c.title = "child ticket".into();
        c.description = "child body".into();
        c.report = Some("did the thing".into());

        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[c],
            Some(&git_context()),
            false,
        );

        assert!(
            p.starts_with("## Ticket\nTicket #10\n\nparent ticket\n\nparent body\n\n"),
            "{p}"
        );
        assert!(p.contains("### Child #42 — child ticket"), "{p}");
        assert!(p.contains("child body"), "{p}");
        assert!(
            p.contains("Branch: `agent/task-42-child` (2 commit(s) ahead of `agent/task-10-parent`)"),
            "{p}"
        );
        assert!(
            p.contains("#### Diff (`agent/task-10-parent`...`agent/task-42-child`)"),
            "{p}"
        );
        assert!(p.contains("diff --git a/x b/x"), "{p}");
        assert!(p.contains("#### Implementation report (#42)"), "{p}");
        assert!(p.contains("did the thing"), "{p}");
        assert!(p.contains("`set_task_review(taskId, markdown, verdict)`"), "{p}");
        assert!(p.contains("changes_requested"), "{p}");
        assert!(p.contains("READ-ONLY"), "{p}");
        assert!(p.contains("## Language\n"), "{p}");
    }

    /// A child that wrote no implementation report is called out in the
    /// prompt — a missing report alone justifies a bounce.
    #[test]
    fn render_prompt_flags_missing_child_report() {
        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[child(42, "review", false, None, Some(7))],
            None,
            false,
        );
        assert!(p.contains("no implementation report"), "{p}");
        assert!(p.contains("changes_requested"), "{p}");
    }

    /// Without a local clone the prompt degrades to a report-only review
    /// instead of a diff.
    #[test]
    fn render_prompt_without_git_context() {
        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[child(42, "review", false, None, Some(7))],
            None,
            false,
        );
        assert!(p.contains("No local clone of the project repository"), "{p}");
        assert!(!p.contains("#### Diff"), "{p}");
    }

    /// A child whose branch is missing locally gets an explicit note, not a
    /// silent omission.
    #[test]
    fn render_prompt_missing_child_branch() {
        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[child(43, "review", false, None, Some(7))],
            Some(&git_context_with(
                43,
                ChildDiff::ChildBranchMissing,
                Some("agent/task-10-parent"),
            )),
            false,
        );
        assert!(p.contains("Child branch `agent/task-43-gone` not found locally"), "{p}");
    }

    fn git_context_with(child_id: i32, diff: ChildDiff, diff_base: Option<&str>) -> GitContext {
        let mut children = HashMap::new();
        children.insert(
            child_id,
            ChildGit {
                branch: "agent/task-43-gone".into(),
                commits_ahead: None,
                diff,
            },
        );
        GitContext {
            parent_branch: "agent/task-10-parent".into(),
            diff_base: diff_base.map(str::to_string),
            children,
        }
    }

    /// A missing PARENT branch must be reported as such — the old code blamed
    /// the child's branch, sending the supervisor after a phantom problem
    /// (ticket #137).
    #[test]
    fn render_prompt_missing_parent_branch_names_the_parent() {
        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[child(43, "review", false, None, Some(7))],
            Some(&git_context_with(43, ChildDiff::ParentBranchMissing, None)),
            false,
        );
        assert!(
            p.contains("Parent branch `agent/task-10-parent` not found locally or on origin"),
            "{p}"
        );
        assert!(p.contains("recreates the parent's branch"), "{p}");
        assert!(!p.contains("Child branch"), "{p}");
    }

    /// A plain diff failure (both refs exist, git errored) names the base it
    /// failed against.
    #[test]
    fn render_prompt_diff_failed_names_the_base() {
        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[child(43, "review", false, None, Some(7))],
            Some(&git_context_with(
                43,
                ChildDiff::DiffFailed,
                Some("origin/agent/task-10-parent"),
            )),
            false,
        );
        assert!(
            p.contains("The diff against `origin/agent/task-10-parent` could not be computed"),
            "{p}"
        );
    }

    // ── collect_git_context (scratch git repos) ────────────────────────────

    /// A scratch git repo with one commit on `master`, unique per test name +
    /// process (same pattern as the workspace.rs tests).
    fn scratch_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("remoter-supervise-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_in(&dir, &["init", "-b", "master"]);
        git_in(&dir, &["commit", "--allow-empty", "-m", "init"]);
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) {
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

    /// A repo with one child branch ahead of `agent/task-10-parent` by a
    /// single commit adding `child.txt`.
    fn scratch_repo_with_child(tag: &str) -> PathBuf {
        let repo = scratch_repo(tag);
        git_in(&repo, &["checkout", "-b", "agent/task-10-parent"]);
        git_in(&repo, &["checkout", "-b", "agent/task-42-child"]);
        std::fs::write(repo.join("child.txt"), "work").unwrap();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "child work"]);
        repo
    }

    #[tokio::test]
    async fn collect_git_context_diffs_against_the_local_parent() {
        let repo = scratch_repo_with_child("local-parent");
        let ctx = collect_git_context(&repo, "agent/task-10-parent", &[(42, "agent/task-42-child".into())]).await;
        assert_eq!(ctx.diff_base.as_deref(), Some("agent/task-10-parent"));
        let cg = &ctx.children[&42];
        assert_eq!(cg.commits_ahead, Some(1));
        match &cg.diff {
            ChildDiff::Present(d) => assert!(d.contains("child.txt"), "{d}"),
            other => panic!("expected a present diff, got {other:?}"),
        }
    }

    /// Ticket #137: the parent's branch is gone (wiped workspace) — every
    /// child degrades to `ParentBranchMissing` with one precise WARN instead
    /// of a cryptic git "ambiguous argument" error per child.
    #[tokio::test]
    async fn collect_git_context_missing_parent_branch() {
        let repo = scratch_repo_with_child("missing-parent");
        git_in(&repo, &["branch", "-D", "agent/task-10-parent"]);
        let ctx = collect_git_context(&repo, "agent/task-10-parent", &[(42, "agent/task-42-child".into())]).await;
        assert_eq!(ctx.diff_base, None);
        let cg = &ctx.children[&42];
        assert_eq!(cg.diff, ChildDiff::ParentBranchMissing);
        assert_eq!(cg.commits_ahead, None);
    }

    /// The parent's branch survives only as a remote-tracking ref — the diff
    /// base falls back to `origin/<parent>` and the child diff is computed.
    #[tokio::test]
    async fn collect_git_context_falls_back_to_origin_parent() {
        let repo = scratch_repo_with_child("origin-parent");
        git_in(
            &repo,
            &[
                "update-ref",
                "refs/remotes/origin/agent/task-10-parent",
                "agent/task-10-parent",
            ],
        );
        git_in(&repo, &["branch", "-D", "agent/task-10-parent"]);
        let ctx = collect_git_context(&repo, "agent/task-10-parent", &[(42, "agent/task-42-child".into())]).await;
        assert_eq!(ctx.diff_base.as_deref(), Some("origin/agent/task-10-parent"));
        let cg = &ctx.children[&42];
        assert_eq!(cg.commits_ahead, Some(1));
        match &cg.diff {
            ChildDiff::Present(d) => assert!(d.contains("child.txt"), "{d}"),
            other => panic!("expected a present diff, got {other:?}"),
        }
    }

    /// The parent's conversation is quoted (bounded) — human steering belongs
    /// in the supervisor's context.
    #[test]
    fn render_prompt_quotes_parent_conversation() {
        let mut parent_detail = child(10, "review", false, None, Some(7));
        parent_detail.comments = Some(vec![CommentDto {
            id: 1,
            task_id: 10,
            author_id: 1,
            body: "don't merge until I check".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }]);
        let p = render_prompt(&summary(), &parent_detail, &[], None, false);
        assert!(p.contains("## Conversation\n"), "{p}");
        assert!(p.contains("- don't merge until I check"), "{p}");
    }

    /// The conflict nudge names both branches and the conflicted files, and
    /// forbids pushing and aborting.
    #[test]
    fn merge_conflict_nudge_prompt_contents() {
        let p = merge_conflict_nudge_prompt("agent/task-10-parent", "agent/task-42-child", &["src/x.rs".into()]);
        assert!(p.contains("agent/task-10-parent"), "{p}");
        assert!(p.contains("agent/task-42-child"), "{p}");
        assert!(p.contains("- src/x.rs"), "{p}");
        assert!(p.contains("just lint test"), "{p}");
        assert!(p.contains("git commit"), "{p}");
        assert!(p.contains("Do NOT push"), "{p}");
        assert!(p.contains("do NOT abort the merge"), "{p}");
    }

    /// A resolved-but-uncommitted merge gets a "finish the merge commit" nudge
    /// instead of an empty conflicted-files list (review #110).
    #[test]
    fn merge_conflict_nudge_prompt_without_unmerged_files() {
        let p = merge_conflict_nudge_prompt("agent/task-10-parent", "agent/task-42-child", &[]);
        assert!(p.contains("still open"), "{p}");
        assert!(p.contains("no conflicted files remain"), "{p}");
        assert!(p.contains("git commit"), "{p}");
        assert!(!p.contains("- \n"), "{p}");
    }

    /// A recorded give-up blocks re-triggering only while the child's current
    /// verdict is unchanged (review #110).
    #[test]
    fn give_up_blocks_only_while_the_verdict_is_unchanged() {
        let mut c = child(42, "review", false, None, Some(7));
        c.review_verdict = Some("approve".into());
        let mut notified = HashMap::new();
        // No signature → not blocked.
        assert!(!give_up_blocks(&notified, &c));
        // Matching verdict → blocked.
        notified.insert(42, "approve:conflict-unresolved".to_string());
        assert!(give_up_blocks(&notified, &c));
        // A fresh verdict (new review cycle) re-arms supervision.
        c.review_verdict = Some("changes_requested".into());
        assert!(!give_up_blocks(&notified, &c));
        // A missing verdict does not accidentally match.
        c.review_verdict = None;
        notified.insert(42, "-:no-repo".to_string());
        assert!(give_up_blocks(&notified, &c));
    }

    // ── circuit breaker ─────────────────────────────────────────────────

    fn run_row(status: &str) -> crate::client::AgentRunDto {
        crate::client::AgentRunDto {
            id: 1,
            task_id: 42,
            agent_user_id: 7,
            kind: "implement".into(),
            status: status.into(),
            session_id: None,
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

    fn escalation_note(id: i32) -> CommentDto {
        CommentDto {
            id,
            task_id: 42,
            author_id: 7,
            body: format!("implement run #{id} failed after 3 attempt(s): boom — moving to review; needs a human"),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    /// The breaker needs BOTH signals: a failed newest run (the escalation
    /// state) and the recorded cycle count at the budget.
    #[test]
    fn circuit_breaker_requires_failed_run_and_cycle_budget() {
        let mut c = child(42, "review", false, None, Some(7));
        assert!(!circuit_broken(&c), "no runs, no notes — not broken");

        c.runs = Some(vec![run_row("failed")]);
        assert!(latest_run_failed(&c));
        assert!(!circuit_broken(&c), "first failure — the automatic budget is not spent");

        c.comments = Some(vec![escalation_note(1)]);
        assert!(!circuit_broken(&c), "one recorded cycle — still within the budget");

        c.comments = Some(vec![escalation_note(1), escalation_note(2)]);
        assert_eq!(failure_cycles(&c), 2);
        assert!(circuit_broken(&c), "budget spent with a failed newest run — broken");

        // A succeeded newest run means the child finished — never broken.
        c.runs = Some(vec![run_row("succeeded"), run_row("failed")]);
        assert!(
            !circuit_broken(&c),
            "a successful finish is supervisable however many cycles came before"
        );
    }

    /// Unrelated thread chatter must not count as failure cycles.
    #[test]
    fn failure_cycles_count_only_escalation_notes() {
        let mut c = child(42, "review", false, None, Some(7));
        c.comments = Some(vec![
            CommentDto {
                id: 1,
                task_id: 42,
                author_id: 1,
                body: "the deploy broke on the migration; needs a human eye".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
            },
            CommentDto {
                id: 2,
                task_id: 42,
                author_id: 1,
                body: "run failed after 2 attempt(s): boom".into(),
                created_at: "2026-01-01T00:00:01Z".into(),
            },
            escalation_note(3),
        ]);
        assert_eq!(failure_cycles(&c), 1, "only the full escalation-note shape counts");
    }

    /// The supervise prompt flags an escalated child so the supervisor does
    /// not mistake a failed run for finished work.
    #[test]
    fn render_prompt_flags_failed_child() {
        let mut c = child(42, "review", false, None, Some(7));
        c.runs = Some(vec![run_row("failed")]);
        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[c],
            None,
            false,
        );
        assert!(p.contains("newest run FAILED"), "{p}");
        assert!(p.contains("empty diff with a failed run is never"), "{p}");

        let ok = child(43, "review", false, None, Some(7));
        let p = render_prompt(
            &summary(),
            &child(10, "review", false, None, Some(7)),
            &[ok],
            None,
            false,
        );
        assert!(!p.contains("newest run FAILED"), "{p}");
    }

    /// The completed-cycle fingerprint changes when a child leaves
    /// `completed`, which is what re-arms the final integration trigger.
    #[test]
    fn completed_cycle_signature_tracks_completions() {
        let mut a = child(1, "completed", false, None, Some(7));
        let mut b = child(2, "completed", false, None, Some(7));
        a.completed_at = Some("2026-09-01T10:00:00Z".parse().unwrap());
        b.completed_at = Some("2026-09-02T10:00:00Z".parse().unwrap());
        let sig = completed_cycle_signature(&[a.clone(), b.clone()]);
        assert!(sig.contains("1@"), "{sig}");
        assert!(sig.contains("2@"), "{sig}");

        // Same completions → same fingerprint (dedup holds).
        assert_eq!(sig, completed_cycle_signature(&[a.clone(), b.clone()]));

        // Child 1 bounces out of completed → different fingerprint.
        a.task_status = "implement".into();
        a.completed_at = None;
        assert_ne!(sig, completed_cycle_signature(&[a, b]));
    }

    // ── supervision gate (`supervision_enabled`) ─────────────────────────────

    /// Minimal HTTP/1.1 mock backend (same shape as the claim.rs tests):
    /// serves the queued `(status, body)` responses in request order, one
    /// connection each, and counts accepted requests so a test can assert the
    /// gate stopped the parent early.
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

    /// A review child of the parent from `summary()`.
    const CHILD_REVIEW_JSON: &str = r#"{
        "id": 42, "projectId": 1, "projectName": "proj", "featureId": 1,
        "featureDescription": "feat", "title": "child ticket",
        "description": "child body", "taskStatus": "review", "assigneeId": 7
    }"#;

    /// A review parent with `supervisionEnabled = false` is left entirely
    /// alone: the parent detail fetch is the only request — no child fetches,
    /// no supervise trigger, no kickoff, no integration bounce.
    #[tokio::test]
    async fn disabled_parent_is_skipped_entirely() {
        let (url, hits) = spawn_counting_backend(vec![
            (
                200,
                r#"{
                "id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "parent ticket",
                "description": "parent body", "taskStatus": "review", "assigneeId": 7,
                "supervisionEnabled": false,
                "links": [{"relation": "parent", "taskId": 42, "createdBy": null}]
            }"#,
            ),
            // Regression fodder only: a broken gate would fetch the child and
            // list the parent's runs, then push a trigger.
            (200, CHILD_REVIEW_JSON),
            (200, "[]"),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let mut supervisor = Supervisor::new();
        let mut triggers = Vec::new();

        supervisor
            .supervise_parent(&client, &summary(), &managed(), Some(7), &mut triggers)
            .await;

        assert!(triggers.is_empty(), "disabled parent must not trigger supervise runs");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the parent detail fetch must be the only request"
        );
    }

    /// A parent JSON without `supervisionEnabled` (older backend) keeps
    /// supervision on: the review child is fetched and the supervise run
    /// triggers.
    #[tokio::test]
    async fn parent_without_flag_still_supervises() {
        let (url, hits) = spawn_counting_backend(vec![
            (
                200,
                r#"{
                "id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "parent ticket",
                "description": "parent body", "taskStatus": "review", "assigneeId": 7,
                "links": [{"relation": "parent", "taskId": 42, "createdBy": null}]
            }"#,
            ),
            (200, CHILD_REVIEW_JSON),
            (200, "[]"),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let mut supervisor = Supervisor::new();
        let mut triggers = Vec::new();

        supervisor
            .supervise_parent(&client, &summary(), &managed(), Some(7), &mut triggers)
            .await;

        assert_eq!(triggers.len(), 1, "review child must trigger the supervise run");
        assert_eq!(triggers[0].review_children, vec![42]);
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "parent detail + child detail + run list"
        );
    }

    /// Circuit breaker (ticket #130): a review child whose newest run failed
    /// and whose thread already records the escalation-note budget is NOT
    /// triggered for supervision — it stays in `review` for a human.
    #[tokio::test]
    async fn circuit_broken_child_is_not_triggered() {
        let (url, hits) = spawn_counting_backend(vec![
            (
                200,
                r#"{
                "id": 10, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "parent ticket",
                "description": "parent body", "taskStatus": "review", "assigneeId": 7,
                "links": [{"relation": "parent", "taskId": 42, "createdBy": null}]
            }"#,
            ),
            (
                200,
                r#"{
                "id": 42, "projectId": 1, "projectName": "proj", "featureId": 1,
                "featureDescription": "feat", "title": "child ticket",
                "description": "child body", "taskStatus": "review", "assigneeId": 7,
                "runs": [{
                    "id": 9, "task_id": 42, "agent_user_id": 7, "kind": "implement",
                    "status": "failed", "session_id": null, "branch": null, "plan": null,
                    "summary": null, "error": "boom", "pr_url": null, "pr_error": null,
                    "input_tokens": null, "output_tokens": null, "attempt": 3
                }],
                "comments": [
                    {"id": 1, "task_id": 42, "author_id": 7,
                     "body": "implement run #7 failed after 3 attempt(s): boom — moving to review; needs a human",
                     "created_at": "2026-01-01T00:00:00Z"},
                    {"id": 2, "task_id": 42, "author_id": 7,
                     "body": "implement run #9 failed after 3 attempt(s): boom — moving to review; needs a human",
                     "created_at": "2026-01-01T01:00:00Z"}
                ]
            }"#,
            ),
        ])
        .await;
        let client = RemoterClient::new(&url, "tok", None);
        let mut supervisor = Supervisor::new();
        let mut triggers = Vec::new();

        supervisor
            .supervise_parent(&client, &summary(), &managed(), Some(7), &mut triggers)
            .await;

        assert!(
            triggers.is_empty(),
            "a circuit-broken child must not trigger supervise runs"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "no run-list fetch — the breaker fires before the trigger check"
        );
    }
}
