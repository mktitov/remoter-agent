use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::SharedState;
use super::model::{self, AnalysisOutcome, AnalysisResponse, TriageOutcome};
use super::sandbox::{self, WriteScope};
use super::state::{
    ActiveWrite, AnalysisCircuit, BeginCall, BeginWrite, DoctorState, FailureClass, ProblemSource, QueuedProblem,
    WriteCircuit, WriteEnd,
};
use crate::client::{
    ClaimedDiagnosticAction, DiagnosticAttemptFinish, DiagnosticAttemptProgress, DiagnosticFinding, DoctorStatusUpdate,
    RemoterClient,
};
use crate::config::Config;
use crate::doctor_config::TokenBucket;
use crate::driver::{AgentDriver, DoctorRunOptions, DriverError, RunFailure, RunOutcome, RunSpec};
use crate::logstore::LogStore;
use crate::run::RunLogGuard;
use crate::session_log::SessionLogger;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    config: Arc<Config>,
    client: RemoterClient,
    driver: Arc<dyn AgentDriver>,
    log_store: LogStore,
    state: SharedState,
    active_runs: Arc<Mutex<HashSet<i32>>>,
    wake: mpsc::Sender<()>,
    mut rx: mpsc::Receiver<()>,
    cancel: CancellationToken,
) {
    let path = super::state::state_path(&config.workspace_root);
    let mut worker = Worker {
        config,
        client,
        driver,
        log_store,
        state,
        active_runs,
        path,
        cancel,
    };
    worker.restore_pending().await;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if worker.cancel.is_cancelled() {
            break;
        }
        worker.apply_controls().await;
        worker.flush_report_placeholders().await;
        worker.process_next_problem().await;
        worker.process_next_write().await;
        worker.heartbeat().await;
        tokio::select! {
            _ = tick.tick() => {}
            _ = rx.recv() => {}
            _ = worker.cancel.cancelled() => break,
        }
    }
    worker.heartbeat().await;
    drop(wake);
}

struct Worker {
    config: Arc<Config>,
    client: RemoterClient,
    driver: Arc<dyn AgentDriver>,
    log_store: LogStore,
    state: SharedState,
    active_runs: Arc<Mutex<HashSet<i32>>>,
    path: PathBuf,
    cancel: CancellationToken,
}

#[derive(Debug)]
struct CallFailure {
    message: String,
    class: FailureClass,
    session_id: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

impl Worker {
    fn persist(&self, state: &DoctorState) -> anyhow::Result<()> {
        state.save(&self.path)
    }

    async fn restore_pending(&self) {
        let findings = match self.client.pending_diagnostic_findings().await {
            Ok(findings) => findings,
            Err(error) => {
                tracing::warn!(target: "remoter_agent::doctor", %error, "could not restore pending Doctor findings");
                return;
            }
        };
        let mut state = self.state.lock().expect("Doctor state poisoned");
        for finding in findings {
            let source = match finding.source.as_str() {
                "report" => ProblemSource::Report,
                "acp_log" => ProblemSource::AcpLog,
                _ => ProblemSource::DaemonLog,
            };
            let log_id = state.allocate_log_id();
            let mut problem = QueuedProblem::new(
                source,
                finding.title,
                finding.source_excerpt.unwrap_or_default(),
                finding.source_task_id,
                finding.source_run_id,
                Utc::now(),
                log_id,
            );
            problem.fingerprint = finding.fingerprint;
            problem.occurrences = finding.occurrences.max(1);
            problem.remediation_round = finding.remediation_round;
            problem.evidence = finding.evidence;
            state.enqueue(problem, &self.config.doctor, Utc::now());
        }
        if let Err(error) = self.persist(&state) {
            tracing::warn!(target: "remoter_agent::doctor", %error, "could not persist restored Doctor queue");
        }
    }

    async fn apply_controls(&self) {
        let controls = match self.client.doctor_controls().await {
            Ok(controls) => controls,
            Err(error) => {
                tracing::debug!(target: "remoter_agent::doctor", %error, "could not fetch Doctor controls");
                return;
            }
        };
        let mut state = self.state.lock().expect("Doctor state poisoned");
        state.apply_reset_generations(controls.analysis_reset_generation, controls.write_reset_generation);
        if let Err(error) = self.persist(&state) {
            tracing::warn!(target: "remoter_agent::doctor", %error, "could not persist Doctor reset generations");
        }
    }

    async fn flush_report_placeholders(&self) {
        let reports = self
            .state
            .lock()
            .expect("Doctor state poisoned")
            .report_queue
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for problem in reports {
            let body = model::placeholder_body(&problem, None);
            if let Err(error) = self.client.put_diagnostic_finding(&problem.fingerprint, &body).await {
                tracing::warn!(target: "remoter_agent::doctor", fingerprint = %problem.fingerprint, %error, "could not create analyzing Doctor placeholder");
            }
        }
    }

    async fn process_next_problem(&mut self) {
        let now = Utc::now();
        let (mut problem, mut bucket, allowed) = {
            let mut state = self.state.lock().expect("Doctor state poisoned");
            let Some(problem) = state.pop_next() else {
                return;
            };
            let bucket = if problem.source == ProblemSource::Report {
                TokenBucket::Analysis
            } else {
                TokenBucket::Triage
            };
            let allowed = state.begin_call(bucket, &self.config.doctor, now);
            if matches!(allowed, BeginCall::Blocked(_)) {
                if problem.source == ProblemSource::Report {
                    state.report_queue.push_front(problem.clone());
                } else {
                    state.log_queue.push_front(problem.clone());
                }
            }
            if let Err(error) = self.persist(&state) {
                state.last_error = Some(format!("Doctor state persistence failed: {error}"));
                if problem.source == ProblemSource::Report {
                    state.report_queue.push_front(problem.clone());
                } else {
                    state.log_queue.push_front(problem.clone());
                }
                return;
            }
            (problem, bucket, allowed)
        };
        let BeginCall::Allowed { half_open_probe } = allowed else {
            let reason = match allowed {
                BeginCall::Blocked(reason) => reason,
                BeginCall::Allowed { .. } => unreachable!(),
            };
            self.blocked_finding(&reason).await;
            if problem.source == ProblemSource::Report {
                let body = model::placeholder_body(&problem, Some(&reason));
                let _ = self.client.put_diagnostic_finding(&problem.fingerprint, &body).await;
            }
            return;
        };
        if half_open_probe {
            bucket = TokenBucket::Analysis;
        }

        let prompt = if bucket == TokenBucket::Triage {
            model::triage_prompt(&problem)
        } else {
            model::analysis_prompt(&problem, false)
        };
        let result = self
            .invoke_model(
                &problem,
                bucket,
                prompt,
                problem.session_id.clone(),
                true,
                self.read_timeout(),
                problem.log_id,
                None,
            )
            .await;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(failure) => {
                self.model_failure(&problem, bucket, failure).await;
                return;
            }
        };
        problem.session_id = outcome.session_id.clone().or(problem.session_id);
        if bucket == TokenBucket::Triage {
            match model::parse_triage(&outcome.text) {
                Ok(TriageOutcome::Ignore) => {
                    self.model_success();
                    self.complete(problem);
                }
                Ok(TriageOutcome::NeedsDeepAnalysis) => {
                    self.model_success();
                    self.deep_analysis(problem).await;
                }
                Err(error) => {
                    self.protocol_failure(problem, bucket, outcome, error.to_string()).await;
                }
            }
        } else {
            match model::parse_analysis(&outcome.text) {
                Ok(response) => {
                    self.model_success();
                    self.finish_analysis(problem, response).await;
                }
                Err(error) => {
                    self.protocol_failure(problem, bucket, outcome, error.to_string()).await;
                }
            }
        }
    }

    async fn deep_analysis(&mut self, mut problem: QueuedProblem) {
        let allowed = {
            let mut state = self.state.lock().expect("Doctor state poisoned");
            let allowed = state.begin_call(TokenBucket::Analysis, &self.config.doctor, Utc::now());
            if matches!(allowed, BeginCall::Blocked(_)) {
                state.log_queue.push_front(problem.clone());
            }
            let _ = self.persist(&state);
            allowed
        };
        let BeginCall::Allowed { .. } = allowed else {
            let reason = match allowed {
                BeginCall::Blocked(reason) => reason,
                BeginCall::Allowed { .. } => unreachable!(),
            };
            self.blocked_finding(&reason).await;
            return;
        };
        let result = self
            .invoke_model(
                &problem,
                TokenBucket::Analysis,
                model::analysis_prompt(&problem, false),
                problem.session_id.clone(),
                true,
                self.read_timeout(),
                problem.log_id,
                None,
            )
            .await;
        match result {
            Ok(outcome) => {
                problem.session_id = outcome.session_id.clone().or(problem.session_id);
                match model::parse_analysis(&outcome.text) {
                    Ok(response) => {
                        self.model_success();
                        self.finish_analysis(problem, response).await;
                    }
                    Err(error) => {
                        self.protocol_failure(problem, TokenBucket::Analysis, outcome, error.to_string())
                            .await;
                    }
                }
            }
            Err(failure) => self.model_failure(&problem, TokenBucket::Analysis, failure).await,
        }
    }

    async fn finish_analysis(&mut self, mut problem: QueuedProblem, response: AnalysisResponse) {
        let outcome = match response.outcome {
            AnalysisOutcome::NotImportant if problem.source.is_log() => {
                self.complete(problem);
                return;
            }
            AnalysisOutcome::NotImportant => "notImportant",
            AnalysisOutcome::NeedsAction => "needsAction",
            AnalysisOutcome::Resolved => "resolved",
        };
        let body = model::finding_body(&problem, outcome, Some(&response));
        match self.client.put_diagnostic_finding(&problem.fingerprint, &body).await {
            Ok(finding) => {
                self.comment_on_source(&problem, &finding).await;
                problem.remediation_round = finding.remediation_round;
                self.complete(problem);
            }
            Err(error) => {
                tracing::warn!(target: "remoter_agent::doctor", %error, "could not store Doctor analysis");
                self.requeue(problem);
            }
        }
    }

    async fn comment_on_source(&self, problem: &QueuedProblem, finding: &DiagnosticFinding) {
        let (Some(task_id), Some(finding_id)) = (problem.source_task_id, finding.id) else {
            return;
        };
        let text = format!(
            "Agent Doctor completed analysis: {}. See /diagnostics/{finding_id}.",
            problem.title
        );
        if let Err(error) = self.client.add_comment(task_id, &text).await {
            tracing::warn!(target: "remoter_agent::doctor", task_id, %error, "could not link Doctor finding on source ticket");
        }
    }

    async fn model_failure(&mut self, problem: &QueuedProblem, bucket: TokenBucket, failure: CallFailure) {
        {
            let mut state = self.state.lock().expect("Doctor state poisoned");
            state.analysis_failure(failure.class, &self.config.doctor, Utc::now());
            state.last_error = Some(failure.message.clone());
            if problem.source == ProblemSource::Report {
                state.report_queue.push_front(problem.clone());
            } else {
                state.log_queue.push_front(problem.clone());
            }
            let _ = self.persist(&state);
        }
        let reason = if failure.class == FailureClass::AuthOrQuota {
            "analysis_circuit_open"
        } else {
            "analysis_failure"
        };
        if bucket != TokenBucket::Remediation {
            self.blocked_finding(reason).await;
        }
    }

    async fn protocol_failure(
        &mut self,
        problem: QueuedProblem,
        bucket: TokenBucket,
        outcome: RunOutcome,
        message: String,
    ) {
        self.model_failure(
            &problem,
            bucket,
            CallFailure {
                message,
                class: FailureClass::Infrastructure,
                session_id: outcome.session_id,
                input_tokens: outcome.input_tokens,
                output_tokens: outcome.output_tokens,
            },
        )
        .await;
    }

    fn model_success(&self) {
        let mut state = self.state.lock().expect("Doctor state poisoned");
        state.analysis_success();
        state.last_error = None;
        let _ = self.persist(&state);
    }

    fn complete(&self, problem: QueuedProblem) {
        let mut state = self.state.lock().expect("Doctor state poisoned");
        state.complete_problem(&problem);
        let _ = self.persist(&state);
    }

    fn requeue(&self, problem: QueuedProblem) {
        let mut state = self.state.lock().expect("Doctor state poisoned");
        if problem.source == ProblemSource::Report {
            state.report_queue.push_front(problem);
        } else {
            state.log_queue.push_front(problem);
        }
        let _ = self.persist(&state);
    }

    async fn blocked_finding(&self, reason: &str) {
        let now = Utc::now();
        let should_create = {
            let mut state = self.state.lock().expect("Doctor state poisoned");
            let should_create = state.mark_system_finding(reason, now);
            let _ = self.persist(&state);
            should_create
        };
        if should_create {
            let (fingerprint, body) = model::system_finding(reason, now.date_naive());
            if let Err(error) = self.client.put_diagnostic_finding(&fingerprint, &body).await {
                tracing::warn!(target: "remoter_agent::doctor", %error, "could not create blocked Doctor system finding");
            }
        }
    }

    fn read_timeout(&self) -> Duration {
        Duration::from_secs(self.config.run_timeout_minutes.saturating_mul(60))
    }

    async fn process_next_write(&mut self) {
        let claimed = match self.client.claim_diagnostic_action().await {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::warn!(target: "remoter_agent::doctor", %error, "could not claim Doctor action");
                return;
            }
        };
        let Some(claimed) = claimed else {
            return;
        };
        match claimed.action.kind.as_str() {
            "daemonOperation" => self.remediate(claimed).await,
            "createTask" => self.create_task(claimed).await,
            _ => {
                self.finish_attempt(
                    claimed.attempt.id,
                    DiagnosticAttemptFinish {
                        status: "failed".to_string(),
                        result_error: Some("action is not executable by the daemon".to_string()),
                        ..Default::default()
                    },
                )
                .await;
            }
        }
    }

    fn write_blocked_reason(&self) -> Option<&'static str> {
        if !self.config.doctor.write_enabled {
            return Some("write_disabled");
        }
        let state = self.state.lock().expect("Doctor state poisoned");
        if state.write_circuit == WriteCircuit::Open {
            Some("write_circuit_open")
        } else if state.analysis_circuit != AnalysisCircuit::Closed {
            Some("analysis_circuit_open")
        } else if state.active_write.is_some() {
            Some("conflicting_run")
        } else if state
            .flood_cooldown_until
            .is_some_and(|until| until > Utc::now().timestamp())
        {
            Some("flood_guard")
        } else if state.remediation_tokens_used >= self.config.doctor.budget(TokenBucket::Remediation)
            || state.write_rounds_used >= self.config.doctor.daily_write_round_budget.unwrap_or(0)
        {
            Some("budget_exhausted")
        } else {
            None
        }
    }

    async fn remediate(&mut self, claimed: ClaimedDiagnosticAction) {
        let attempt_id = claimed.attempt.id;
        if let Some(reason) = self.write_blocked_reason() {
            self.finish_attempt(
                attempt_id,
                DiagnosticAttemptFinish {
                    status: "cancelled".to_string(),
                    result_error: Some(reason.to_string()),
                    ..Default::default()
                },
            )
            .await;
            self.blocked_finding(reason).await;
            return;
        }
        let Some(mut problem) = claimed
            .finding
            .as_ref()
            .map(|finding| self.problem_from_finding(finding))
        else {
            self.finish_attempt(
                attempt_id,
                DiagnosticAttemptFinish {
                    status: "failed".to_string(),
                    result_error: Some("claim-next did not include the finding context".to_string()),
                    ..Default::default()
                },
            )
            .await;
            return;
        };
        if claimed.action.round > self.config.doctor.max_remediation_rounds {
            self.finish_attempt(
                attempt_id,
                DiagnosticAttemptFinish {
                    status: "failed".to_string(),
                    result_error: Some("maximum remediation rounds exceeded".to_string()),
                    ..Default::default()
                },
            )
            .await;
            return;
        }
        if problem.source_task_id.is_some_and(|task_id| {
            self.active_runs
                .lock()
                .expect("active runs poisoned")
                .contains(&task_id)
        }) {
            self.finish_attempt(
                attempt_id,
                DiagnosticAttemptFinish {
                    status: "cancelled".to_string(),
                    result_error: Some("conflicting_run".to_string()),
                    ..Default::default()
                },
            )
            .await;
            return;
        }
        let scope = match WriteScope::validate(&self.config, &claimed.action) {
            Ok(scope) => scope,
            Err(error) => {
                self.finish_attempt(
                    attempt_id,
                    DiagnosticAttemptFinish {
                        status: "failed".to_string(),
                        result_error: Some(error.to_string()),
                        ..Default::default()
                    },
                )
                .await;
                return;
            }
        };
        let begin = {
            let mut state = self.state.lock().expect("Doctor state poisoned");
            let begin = state.begin_write(
                ActiveWrite {
                    attempt_id,
                    action_id: claimed.action.id,
                    fingerprint: problem.fingerprint.clone(),
                    started_at: Utc::now(),
                },
                &self.config.doctor,
                Utc::now(),
            );
            if let Err(error) = self.persist(&state) {
                state.active_write = None;
                tracing::warn!(target: "remoter_agent::doctor", %error, "could not persist write launch");
                return;
            }
            begin
        };
        if let BeginWrite::Blocked(reason) = begin {
            self.finish_attempt(
                attempt_id,
                DiagnosticAttemptFinish {
                    status: "cancelled".to_string(),
                    result_error: Some(reason.clone()),
                    ..Default::default()
                },
            )
            .await;
            self.blocked_finding(&reason).await;
            return;
        }
        problem.remediation_round = claimed.action.round;
        let timeout = Duration::from_secs(
            claimed
                .action
                .timeout_secs
                .or_else(|| {
                    claimed
                        .action
                        .payload
                        .get("timeoutSecs")
                        .and_then(|value| value.as_u64())
                })
                .unwrap_or(900)
                .max(1),
        );
        self.client
            .diagnostic_attempt_progress(
                attempt_id,
                &DiagnosticAttemptProgress {
                    session_id: problem.session_id.clone(),
                    transcript: self.transcript(problem.log_id).0,
                    truncated: false,
                    input_tokens: None,
                    output_tokens: None,
                    model: Some(self.config.doctor.analysis_model.clone()),
                    thinking: Some(self.config.doctor.analysis_thinking.clone()),
                },
            )
            .await
            .ok();
        let result = self
            .invoke_model(
                &problem,
                TokenBucket::Remediation,
                model::remediation_prompt(&claimed.action),
                problem.session_id.clone(),
                false,
                timeout,
                problem.log_id,
                Some((attempt_id, Some(&scope))),
            )
            .await;
        match result {
            Ok(outcome) => {
                problem.session_id = outcome.session_id.clone().or(problem.session_id);
                let (transcript, truncated) = self.transcript(problem.log_id);
                let finished = self
                    .client
                    .diagnostic_attempt_finish(
                        attempt_id,
                        &DiagnosticAttemptFinish {
                            status: "succeeded".to_string(),
                            session_id: problem.session_id.clone(),
                            transcript,
                            truncated,
                            agent_summary: Some(outcome.text.clone()),
                            verification_evidence: Some(serde_json::json!({"roundOutput": outcome.text})),
                            input_tokens: outcome.input_tokens,
                            output_tokens: outcome.output_tokens,
                            model: Some(self.config.doctor.analysis_model.clone()),
                            thinking: Some(self.config.doctor.analysis_thinking.clone()),
                            result_error: None,
                        },
                    )
                    .await;
                if let Err(error) = finished {
                    tracing::warn!(target: "remoter_agent::doctor", attempt_id, %error, "Doctor write result is unknown because finish was not acknowledged");
                    self.end_write(WriteEnd::Unknown);
                    self.complete(problem);
                    self.blocked_finding("write_circuit_open").await;
                    return;
                }
                self.end_write(WriteEnd::Succeeded);
                self.complete(problem.clone());
                self.verify_remediation(problem).await;
            }
            Err(failure) => {
                problem.session_id = failure.session_id.clone().or(problem.session_id);
                let unknown = failure.class == FailureClass::Timeout || self.cancel.is_cancelled();
                self.end_write(if unknown { WriteEnd::Unknown } else { WriteEnd::Abnormal });
                let (transcript, truncated) = self.transcript(problem.log_id);
                self.finish_attempt(
                    attempt_id,
                    DiagnosticAttemptFinish {
                        status: "failed".to_string(),
                        session_id: problem.session_id.clone(),
                        transcript,
                        truncated,
                        agent_summary: None,
                        verification_evidence: None,
                        input_tokens: failure.input_tokens,
                        output_tokens: failure.output_tokens,
                        model: Some(self.config.doctor.analysis_model.clone()),
                        thinking: Some(self.config.doctor.analysis_thinking.clone()),
                        result_error: Some(failure.message.clone()),
                    },
                )
                .await;
                self.model_failure(&problem, TokenBucket::Remediation, failure).await;
                self.complete(problem);
                if unknown {
                    self.blocked_finding("write_circuit_open").await;
                }
            }
        }
    }

    async fn verify_remediation(&mut self, problem: QueuedProblem) {
        let allowed = {
            let mut state = self.state.lock().expect("Doctor state poisoned");
            let allowed = state.begin_call(TokenBucket::Analysis, &self.config.doctor, Utc::now());
            let _ = self.persist(&state);
            allowed
        };
        let BeginCall::Allowed { .. } = allowed else {
            self.requeue(problem);
            return;
        };
        let result = self
            .invoke_model(
                &problem,
                TokenBucket::Analysis,
                model::analysis_prompt(&problem, true),
                problem.session_id.clone(),
                true,
                self.read_timeout(),
                problem.log_id,
                None,
            )
            .await;
        match result {
            Ok(outcome) => match model::parse_analysis(&outcome.text) {
                Ok(response) => {
                    self.model_success();
                    self.finish_analysis(problem, response).await;
                }
                Err(error) => {
                    self.protocol_failure(problem, TokenBucket::Analysis, outcome, error.to_string())
                        .await;
                }
            },
            Err(failure) => self.model_failure(&problem, TokenBucket::Analysis, failure).await,
        }
    }

    fn end_write(&self, end: WriteEnd) {
        let mut state = self.state.lock().expect("Doctor state poisoned");
        state.end_write(end);
        let _ = self.persist(&state);
    }

    fn problem_from_finding(&self, finding: &DiagnosticFinding) -> QueuedProblem {
        let mut state = self.state.lock().expect("Doctor state poisoned");
        if let Some(existing) = state.recent.get(&finding.fingerprint) {
            let mut problem = existing.clone();
            problem.occurrences = finding.occurrences.max(problem.occurrences);
            problem.remediation_round = finding.remediation_round;
            return problem;
        }
        let log_id = state.allocate_log_id();
        let source = match finding.source.as_str() {
            "report" => ProblemSource::Report,
            "acp_log" => ProblemSource::AcpLog,
            _ => ProblemSource::DaemonLog,
        };
        let mut problem = QueuedProblem::new(
            source,
            finding.title.clone(),
            finding.source_excerpt.clone().unwrap_or_default(),
            finding.source_task_id,
            finding.source_run_id,
            Utc::now(),
            log_id,
        );
        problem.fingerprint = finding.fingerprint.clone();
        problem.evidence = finding.evidence.clone();
        problem.occurrences = finding.occurrences.max(1);
        problem.remediation_round = finding.remediation_round;
        problem
    }

    async fn create_task(&self, claimed: ClaimedDiagnosticAction) {
        let result = self.client.create_diagnostic_task(&claimed.action).await;
        self.finish_attempt(
            claimed.attempt.id,
            DiagnosticAttemptFinish {
                status: if result.is_ok() { "succeeded" } else { "failed" }.to_string(),
                agent_summary: result.as_ref().ok().map(|id| format!("Created related bug #{id}")),
                result_error: result.err().map(|error| error.to_string()),
                ..Default::default()
            },
        )
        .await;
    }

    async fn finish_attempt(&self, attempt_id: i32, body: DiagnosticAttemptFinish) {
        if let Err(error) = self.client.diagnostic_attempt_finish(attempt_id, &body).await {
            tracing::warn!(target: "remoter_agent::doctor", attempt_id, %error, "could not finish Doctor attempt");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn invoke_model(
        &self,
        _problem: &QueuedProblem,
        bucket: TokenBucket,
        prompt: String,
        resume_session: Option<String>,
        read_only: bool,
        timeout: Duration,
        log_id: i32,
        write: Option<(i32, Option<&WriteScope>)>,
    ) -> Result<RunOutcome, CallFailure> {
        let attempt_id = write.as_ref().map(|(id, _)| *id).unwrap_or(log_id);
        let runtime = sandbox::start(
            &self.config,
            attempt_id,
            read_only,
            write.as_ref().and_then(|(_, scope)| *scope),
        )
        .await
        .map_err(|error| CallFailure {
            message: error.to_string(),
            class: FailureClass::Infrastructure,
            session_id: resume_session.clone(),
            input_tokens: None,
            output_tokens: None,
        })?;
        let (logger, _guard) = match self.log_store.open_run(0, log_id) {
            Ok(logger) => (
                SessionLogger::new(logger),
                Some(RunLogGuard::new(self.log_store.clone(), 0, log_id)),
            ),
            Err(_) => (SessionLogger::noop(), None),
        };
        let (model, thinking) = if bucket == TokenBucket::Triage {
            (
                self.config.doctor.triage_model.clone(),
                self.config.doctor.triage_thinking.clone(),
            )
        } else {
            (
                self.config.doctor.analysis_model.clone(),
                self.config.doctor.analysis_thinking.clone(),
            )
        };
        let spec = RunSpec {
            task_id: 0,
            run_id: log_id,
            cwd: self.config.doctor.source_dir.clone(),
            prompt,
            kind: "doctor",
            resume_session,
            branch: String::new(),
            env: vec![
                ("CI".to_string(), "1".to_string()),
                ("REMOTER_DOCTOR".to_string(), "1".to_string()),
            ],
            exec: runtime.exec.clone(),
            config_options: vec![("model".to_string(), model), ("thinking".to_string(), thinking)],
            logger,
            phase_tx: None,
            doctor: Some(DoctorRunOptions { read_only }),
        };
        let mut run = self.driver.run(spec);
        let mut progress = tokio::time::interval(Duration::from_secs(5));
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let result = tokio::time::timeout(timeout, async {
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    _ = self.cancel.cancelled() => {
                        break Err(RunFailure::new(DriverError::Stalled("Doctor run cancelled".to_string()), None));
                    }
                    _ = progress.tick(), if write.is_some() => {
                        if let Some((attempt_id, _)) = write {
                            let (transcript, truncated) = self.transcript(log_id);
                            let progress_result = self.client.diagnostic_attempt_progress(attempt_id, &DiagnosticAttemptProgress {
                                session_id: None,
                                transcript,
                                truncated,
                                input_tokens: None,
                                output_tokens: None,
                                model: None,
                                thinking: None,
                            }).await;
                            if let Err(error) = progress_result
                                && matches!(error.status, Some(reqwest::StatusCode::CONFLICT | reqwest::StatusCode::GONE))
                            {
                                break Err(RunFailure::new(
                                    DriverError::Stalled(format!("Doctor write lease was lost: {error}")),
                                    None,
                                ));
                            }
                        }
                    }
                }
            }
        })
        .await;
        drop(run);
        runtime.teardown().await;
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                self.charge(bucket, None, None);
                return Err(CallFailure {
                    message: format!("Doctor ACP call timed out after {}s", timeout.as_secs()),
                    class: FailureClass::Timeout,
                    session_id: None,
                    input_tokens: None,
                    output_tokens: None,
                });
            }
        };
        match result {
            Ok(outcome) => {
                self.charge(bucket, outcome.input_tokens, outcome.output_tokens);
                Ok(outcome)
            }
            Err(failure) => {
                self.charge(bucket, failure.input_tokens, failure.output_tokens);
                Err(CallFailure {
                    class: failure_class(&failure.source),
                    message: failure.source.to_string(),
                    session_id: failure.session_id,
                    input_tokens: failure.input_tokens,
                    output_tokens: failure.output_tokens,
                })
            }
        }
    }

    fn charge(&self, bucket: TokenBucket, input: Option<i64>, output: Option<i64>) {
        let mut state = self.state.lock().expect("Doctor state poisoned");
        state.charge(bucket, input, output, self.config.doctor.unknown_usage(bucket));
        if let Err(error) = self.persist(&state) {
            state.last_error = Some(format!("could not persist Doctor token usage: {error}"));
        }
    }

    fn transcript(&self, log_id: i32) -> (Vec<serde_json::Value>, bool) {
        let Ok((entries, total)) = self.log_store.read(0, log_id, None, Some(1000), Some(1000)) else {
            return (Vec::new(), false);
        };
        let truncated = total > entries.len() as i64;
        let entries = entries
            .into_iter()
            .map(|entry| {
                serde_json::to_value(entry)
                    .map(model::sanitize_value)
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect();
        (entries, truncated)
    }

    async fn heartbeat(&self) {
        let now = Utc::now();
        let update = {
            let mut state = self.state.lock().expect("Doctor state poisoned");
            let previous_day = state.utc_day;
            state.rollover(now);
            if state.utc_day != previous_day {
                let _ = self.persist(&state);
            }
            DoctorStatusUpdate {
                day: state.utc_day,
                triage_tokens_used: state.triage_tokens_used,
                triage_daily_token_budget: self.config.doctor.budget(TokenBucket::Triage),
                analysis_tokens_used: state.analysis_tokens_used,
                analysis_daily_token_budget: self.config.doctor.budget(TokenBucket::Analysis),
                remediation_tokens_used: state.remediation_tokens_used,
                remediation_daily_token_budget: self.config.doctor.budget(TokenBucket::Remediation),
                write_rounds_used: state.write_rounds_used,
                daily_write_round_budget: self.config.doctor.daily_write_round_budget.unwrap_or(0),
                pending_report_problems: state.report_queue.len(),
                pending_log_signatures: state.log_queue.len(),
                analysis_circuit: match state.analysis_circuit {
                    AnalysisCircuit::Closed => "closed",
                    AnalysisCircuit::Open => "open",
                    AnalysisCircuit::HalfOpen => "half_open",
                }
                .to_string(),
                write_circuit: match state.write_circuit {
                    WriteCircuit::Closed => "closed",
                    WriteCircuit::Open => "open",
                }
                .to_string(),
                blocked_reason: blocked_reason(&state, &self.config, now),
                last_error: state.last_error.clone(),
                cooldown_until: cooldown(&state, &self.config.doctor),
                analysis_reset_generation: state.applied_analysis_reset_generation,
                write_reset_generation: state.applied_write_reset_generation,
                heartbeat_at: now,
            }
        };
        if let Err(error) = self.client.put_doctor_status(&update).await {
            tracing::debug!(target: "remoter_agent::doctor", %error, "could not publish Doctor heartbeat");
        }
    }
}

fn cooldown(state: &DoctorState, config: &crate::doctor_config::DoctorConfig) -> Option<DateTime<Utc>> {
    let timestamp = match state.analysis_circuit {
        AnalysisCircuit::Open => state
            .analysis_opened_at?
            .saturating_add(i64::try_from(config.analysis_circuit_cooldown_secs).unwrap_or(i64::MAX)),
        _ => state.flood_cooldown_until?,
    };
    DateTime::from_timestamp(timestamp, 0)
}

fn blocked_reason(state: &DoctorState, config: &Config, now: DateTime<Utc>) -> Option<String> {
    if state.write_circuit == WriteCircuit::Open {
        return Some("write_circuit_open".to_string());
    }
    if state.analysis_circuit != AnalysisCircuit::Closed {
        return Some("analysis_circuit_open".to_string());
    }
    if state.flood_cooldown_until.is_some_and(|until| until > now.timestamp()) {
        return Some("flood_guard".to_string());
    }
    for bucket in [TokenBucket::Triage, TokenBucket::Analysis, TokenBucket::Remediation] {
        if state.used(bucket) >= config.doctor.budget(bucket) {
            return Some("budget_exhausted".to_string());
        }
    }
    if state.write_rounds_used >= config.doctor.daily_write_round_budget.unwrap_or(0) {
        return Some("budget_exhausted".to_string());
    }
    state.last_error.as_ref().map(|_| "state_error".to_string())
}

fn failure_class(error: &DriverError) -> FailureClass {
    match error {
        DriverError::Stalled(_) => FailureClass::Timeout,
        DriverError::Permanent(message)
            if {
                let lower = message.to_lowercase();
                lower.contains("quota")
                    || lower.contains("authentication")
                    || lower.contains("invalid api key")
                    || lower.contains("http 401")
            } =>
        {
            FailureClass::AuthOrQuota
        }
        DriverError::Permanent(_) | DriverError::Transient(_) => FailureClass::Infrastructure,
    }
}
