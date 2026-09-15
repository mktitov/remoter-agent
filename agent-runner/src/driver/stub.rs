//! The dry-run driver: no LLM, no subprocess. Simulates a successful run so the
//! daemon's orchestration (poll → claim → run rows → status transitions) can be
//! exercised end-to-end — in the PoC integration tests and in local demos —
//! before the real ACP driver exists (PoC-5). Implement runs leave a real
//! commit: the daemon's commit guard (spec §5.4 step 5) rejects a zero-commit
//! implement run, so the dry run must honor the same contract as a real agent.

use async_trait::async_trait;

use super::{AgentDriver, DriverError, RunFailure, RunOutcome, RunSpec};

pub struct StubDriver {
    /// Artificial run duration (0 = immediate), for demos of parallelism and
    /// cancellation.
    delay_ms: u64,
    /// When true, the first implement invocation in an attempt leaves an
    /// untracked file behind; a resumed (nudge) invocation commits it so the
    /// dirty-worktree guard can be exercised end-to-end.
    simulate_uncommitted_changes: bool,
}

impl StubDriver {
    pub fn new(delay_ms: u64, simulate_uncommitted_changes: bool) -> Self {
        Self {
            delay_ms,
            simulate_uncommitted_changes,
        }
    }
}

#[async_trait]
impl AgentDriver for StubDriver {
    async fn run(&self, spec: RunSpec) -> Result<RunOutcome, RunFailure> {
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        let text = match spec.kind {
            "plan" => format!(
                "# Plan (dry-run)\n\nSimulated plan for worktree `{}`.\n\n## Plan\n\nNo-op — dry-run driver.",
                spec.cwd.display()
            ),
            // Supervise and review runs are read-only: the agent reviews diffs
            // and writes verdicts, it must not touch the worktree.
            "supervise" => {
                "Dry-run supervise: reviewed the child diff and report (no verdict written — dry-run driver)."
                    .to_string()
            }
            "review" => {
                "Dry-run review: reviewed the ticket diff and report (no verdict written — dry-run driver).".to_string()
            }
            _ => {
                if self.simulate_uncommitted_changes && spec.resume_session.is_none() {
                    leave_stub_untracked(&spec).await?;
                    format!("Dry-run implement: left an uncommitted change in `{}`.", spec.branch)
                } else {
                    commit_stub_change(&spec).await?;
                    format!("Dry-run implement: committed changes to `{}`.", spec.branch)
                }
            }
        };
        let (model, thinking) = outcome_model_thinking(&spec.config_options);
        Ok(RunOutcome {
            session_id: Some(format!("stub-session-{}", std::process::id())),
            text,
            input_tokens: Some(0),
            output_tokens: Some(0),
            model,
            thinking,
        })
    }
}

/// Writes an untracked marker file without committing it, simulating an
/// implement run that forgot to clean up.
async fn leave_stub_untracked(spec: &RunSpec) -> Result<(), RunFailure> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let file = format!("dry-run-{}-{nanos}.txt", std::process::id());
    std::fs::write(spec.cwd.join(&file), "dry-run uncommitted change\n")
        .map_err(|e| RunFailure::new(DriverError::Permanent(format!("stub uncommitted: write: {e}")), None))?;
    Ok(())
}

/// A uniquely-named marker file, committed with an explicit identity so no git
/// config is needed. Uniqueness keeps re-runs (review → implement bounces)
/// committable — an unchanged file would leave `git commit` with nothing to do.
async fn commit_stub_change(spec: &RunSpec) -> Result<(), RunFailure> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let file = format!("dry-run-{}-{nanos}.txt", std::process::id());
    std::fs::write(spec.cwd.join(&file), "dry-run change\n")
        .map_err(|e| RunFailure::new(DriverError::Permanent(format!("stub commit: write: {e}")), None))?;
    let add = ["add", "."];
    let commit = [
        "-c",
        "user.email=stub@remoter",
        "-c",
        "user.name=remoter-stub",
        "commit",
        "-m",
        "dry-run implement",
    ];
    for args in [add.as_slice(), commit.as_slice()] {
        let out = tokio::process::Command::new("git")
            .current_dir(&spec.cwd)
            .args(args)
            .output()
            .await
            .map_err(|e| RunFailure::new(DriverError::Permanent(format!("stub commit: git: {e}")), None))?;
        if !out.status.success() {
            return Err(RunFailure::new(
                DriverError::Permanent(format!(
                    "stub commit: git {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr)
                )),
                None,
            ));
        }
    }
    Ok(())
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
