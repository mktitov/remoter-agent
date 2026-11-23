use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::doctor_config::{DoctorConfig, TokenBucket};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemSource {
    Report,
    DaemonLog,
    AcpLog,
    System,
}

impl ProblemSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::DaemonLog => "daemon_log",
            Self::AcpLog => "acp_log",
            Self::System => "system",
        }
    }

    pub fn is_log(self) -> bool {
        matches!(self, Self::DaemonLog | Self::AcpLog)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedProblem {
    pub fingerprint: String,
    pub source: ProblemSource,
    pub title: String,
    pub excerpt: String,
    pub evidence: serde_json::Value,
    pub source_task_id: Option<i32>,
    pub source_run_id: Option<i32>,
    pub occurrences: u64,
    pub last_seen_at: DateTime<Utc>,
    pub session_id: Option<String>,
    pub log_id: i32,
    pub remediation_round: u32,
    #[serde(default)]
    pub pending_verification: bool,
    #[serde(default)]
    pub placeholder_created: bool,
    #[serde(default)]
    pub next_attempt_at: i64,
}

impl QueuedProblem {
    pub fn new(
        source: ProblemSource,
        title: String,
        excerpt: String,
        source_task_id: Option<i32>,
        source_run_id: Option<i32>,
        now: DateTime<Utc>,
        log_id: i32,
    ) -> Self {
        let normalized = normalize(&format!("{} {title} {excerpt}", source.as_str()));
        let fingerprint = fingerprint(source, &normalized);
        Self {
            fingerprint,
            source,
            title,
            excerpt,
            evidence: serde_json::json!({ "samples": [] }),
            source_task_id,
            source_run_id,
            occurrences: 1,
            last_seen_at: now,
            session_id: None,
            log_id,
            remediation_round: 0,
            pending_verification: false,
            placeholder_created: false,
            next_attempt_at: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisCircuit {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteCircuit {
    Closed,
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Infrastructure,
    ProcessLost,
    AuthOrQuota,
    Timeout,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteEnd {
    Succeeded,
    Abnormal,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveWrite {
    pub attempt_id: i32,
    pub action_id: i32,
    pub fingerprint: String,
    #[serde(default)]
    pub source_task_id: Option<i32>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveCall {
    pub problem: QueuedProblem,
    pub bucket: TokenBucket,
    pub estimate: u64,
    pub half_open_probe: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorState {
    pub schema_version: u32,
    pub utc_day: NaiveDate,
    pub triage_tokens_used: u64,
    pub analysis_tokens_used: u64,
    pub remediation_tokens_used: u64,
    pub write_rounds_used: u64,
    pub report_queue: VecDeque<QueuedProblem>,
    pub log_queue: VecDeque<QueuedProblem>,
    pub recent: HashMap<String, QueuedProblem>,
    pub analysis_circuit: AnalysisCircuit,
    pub analysis_failures: VecDeque<i64>,
    pub analysis_opened_at: Option<i64>,
    pub analysis_half_open_probe: bool,
    pub analysis_human_reset_required: bool,
    pub write_circuit: WriteCircuit,
    pub consecutive_abnormal_writes: u32,
    pub active_write: Option<ActiveWrite>,
    #[serde(default)]
    pub active_call: Option<ActiveCall>,
    pub applied_analysis_reset_generation: u64,
    pub applied_write_reset_generation: u64,
    pub flood_events: VecDeque<(i64, String)>,
    pub flood_cooldown_until: Option<i64>,
    pub daily_system_findings: BTreeMap<String, NaiveDate>,
    pub next_log_id: i32,
    pub last_error: Option<String>,
}

impl Default for DoctorState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            utc_day: Utc::now().date_naive(),
            triage_tokens_used: 0,
            analysis_tokens_used: 0,
            remediation_tokens_used: 0,
            write_rounds_used: 0,
            report_queue: VecDeque::new(),
            log_queue: VecDeque::new(),
            recent: HashMap::new(),
            analysis_circuit: AnalysisCircuit::Closed,
            analysis_failures: VecDeque::new(),
            analysis_opened_at: None,
            analysis_half_open_probe: false,
            analysis_human_reset_required: false,
            write_circuit: WriteCircuit::Closed,
            consecutive_abnormal_writes: 0,
            active_write: None,
            active_call: None,
            applied_analysis_reset_generation: 0,
            applied_write_reset_generation: 0,
            flood_events: VecDeque::new(),
            flood_cooldown_until: None,
            daily_system_findings: BTreeMap::new(),
            next_log_id: -1,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginCall {
    Allowed { half_open_probe: bool },
    Blocked(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginWrite {
    Allowed,
    Blocked(String),
}

impl DoctorState {
    pub fn load(path: &Path, now: DateTime<Utc>) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self {
                utc_day: now.date_naive(),
                ..Self::default()
            });
        }
        let bytes = fs::read(path)?;
        let mut state: Self = serde_json::from_slice(&bytes)?;
        if state.schema_version != 1 {
            anyhow::bail!("unsupported Doctor state schema {}", state.schema_version);
        }
        if state.active_write.take().is_some() {
            state.write_circuit = WriteCircuit::Open;
            state.last_error = Some("daemon restarted during a write round; result is unknown".to_string());
        }
        if let Some(active) = state.active_call.take() {
            state.charge(active.bucket, None, None, active.estimate);
            if active.half_open_probe {
                state.analysis_circuit = AnalysisCircuit::Open;
                state.analysis_opened_at = Some(now.timestamp());
                state.analysis_half_open_probe = false;
            }
            state.requeue(active.problem);
        }
        state.rollover(now);
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self)?;
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn rollover(&mut self, now: DateTime<Utc>) {
        let day = now.date_naive();
        if self.utc_day == day {
            return;
        }
        self.utc_day = day;
        self.triage_tokens_used = 0;
        self.analysis_tokens_used = 0;
        self.remediation_tokens_used = 0;
        self.write_rounds_used = 0;
    }

    pub fn used(&self, bucket: TokenBucket) -> u64 {
        match bucket {
            TokenBucket::Triage => self.triage_tokens_used,
            TokenBucket::Analysis => self.analysis_tokens_used,
            TokenBucket::Remediation => self.remediation_tokens_used,
        }
    }

    fn set_used(&mut self, bucket: TokenBucket, value: u64) {
        match bucket {
            TokenBucket::Triage => self.triage_tokens_used = value,
            TokenBucket::Analysis => self.analysis_tokens_used = value,
            TokenBucket::Remediation => self.remediation_tokens_used = value,
        }
    }

    pub fn charge(&mut self, bucket: TokenBucket, input: Option<i64>, output: Option<i64>, estimate: u64) -> u64 {
        let tokens = match (input, output) {
            (Some(input), Some(output)) => input.max(0) as u64 + output.max(0) as u64,
            _ => estimate,
        };
        self.set_used(bucket, self.used(bucket).saturating_add(tokens));
        tokens
    }

    pub fn flood_blocked(&mut self, now: DateTime<Utc>) -> bool {
        if self.flood_cooldown_until.is_some_and(|until| now.timestamp() < until) {
            return true;
        }
        self.flood_cooldown_until = None;
        false
    }

    pub fn begin_call(&mut self, bucket: TokenBucket, config: &DoctorConfig, now: DateTime<Utc>) -> BeginCall {
        self.rollover(now);
        if self.flood_blocked(now) {
            return BeginCall::Blocked("flood_guard".to_string());
        }
        let budget_bucket = if self.analysis_circuit != AnalysisCircuit::Closed && bucket == TokenBucket::Triage {
            TokenBucket::Analysis
        } else {
            bucket
        };
        if self.used(budget_bucket) >= config.budget(budget_bucket) {
            return BeginCall::Blocked("budget_exhausted".to_string());
        }
        match self.analysis_circuit {
            AnalysisCircuit::Open => {
                if self.analysis_human_reset_required {
                    return BeginCall::Blocked("analysis_circuit_open".to_string());
                }
                let cooldown =
                    self.analysis_opened_at.unwrap_or(now.timestamp()) + config.analysis_circuit_cooldown_secs as i64;
                if now.timestamp() < cooldown {
                    return BeginCall::Blocked("analysis_circuit_open".to_string());
                }
                self.analysis_circuit = AnalysisCircuit::HalfOpen;
                self.analysis_half_open_probe = false;
            }
            AnalysisCircuit::HalfOpen | AnalysisCircuit::Closed => {}
        }
        if self.analysis_circuit == AnalysisCircuit::HalfOpen {
            if self.analysis_half_open_probe {
                return BeginCall::Blocked("analysis_circuit_open".to_string());
            }
            self.analysis_half_open_probe = true;
            return BeginCall::Allowed { half_open_probe: true };
        }
        BeginCall::Allowed { half_open_probe: false }
    }

    pub fn analysis_success(&mut self) {
        if self.analysis_circuit == AnalysisCircuit::HalfOpen {
            self.close_analysis();
        }
        self.analysis_half_open_probe = false;
    }

    pub fn analysis_failure(&mut self, class: FailureClass, config: &DoctorConfig, now: DateTime<Utc>) {
        if class == FailureClass::Command {
            return;
        }
        let now = now.timestamp();
        if class == FailureClass::AuthOrQuota {
            self.analysis_circuit = AnalysisCircuit::Open;
            self.analysis_opened_at = Some(now);
            self.analysis_half_open_probe = false;
            self.analysis_human_reset_required = true;
            return;
        }
        if self.analysis_circuit == AnalysisCircuit::HalfOpen {
            self.analysis_circuit = AnalysisCircuit::Open;
            self.analysis_opened_at = Some(now);
            self.analysis_half_open_probe = false;
            return;
        }
        let cutoff = now - config.analysis_failure_window_secs as i64;
        self.analysis_failures.push_back(now);
        while self
            .analysis_failures
            .front()
            .is_some_and(|timestamp| *timestamp < cutoff)
        {
            self.analysis_failures.pop_front();
        }
        if self.analysis_failures.len() >= config.analysis_failure_threshold.max(1) as usize {
            self.analysis_circuit = AnalysisCircuit::Open;
            self.analysis_opened_at = Some(now);
            self.analysis_half_open_probe = false;
        }
    }

    fn close_analysis(&mut self) {
        self.analysis_circuit = AnalysisCircuit::Closed;
        self.analysis_failures.clear();
        self.analysis_opened_at = None;
        self.analysis_half_open_probe = false;
        self.analysis_human_reset_required = false;
    }

    pub fn begin_write(&mut self, active: ActiveWrite, config: &DoctorConfig, now: DateTime<Utc>) -> BeginWrite {
        self.rollover(now);
        if self.write_circuit == WriteCircuit::Open {
            return BeginWrite::Blocked("write_circuit_open".to_string());
        }
        if self.analysis_circuit != AnalysisCircuit::Closed || self.flood_blocked(now) {
            return BeginWrite::Blocked("analysis_circuit_open".to_string());
        }
        if self.active_write.is_some() {
            return BeginWrite::Blocked("conflicting_run".to_string());
        }
        if self.remediation_tokens_used >= config.budget(TokenBucket::Remediation)
            || self.write_rounds_used >= config.daily_write_round_budget.unwrap_or(0)
        {
            return BeginWrite::Blocked("budget_exhausted".to_string());
        }
        self.write_rounds_used = self.write_rounds_used.saturating_add(1);
        self.active_write = Some(active);
        BeginWrite::Allowed
    }

    pub fn end_write(&mut self, end: WriteEnd) {
        self.active_write = None;
        match end {
            WriteEnd::Succeeded => self.consecutive_abnormal_writes = 0,
            WriteEnd::Abnormal => {
                self.consecutive_abnormal_writes = self.consecutive_abnormal_writes.saturating_add(1);
                if self.consecutive_abnormal_writes >= 2 {
                    self.write_circuit = WriteCircuit::Open;
                }
            }
            WriteEnd::Unknown => self.write_circuit = WriteCircuit::Open,
        }
    }

    pub fn apply_reset_generations(&mut self, analysis: u64, write: u64) {
        if analysis > self.applied_analysis_reset_generation {
            self.applied_analysis_reset_generation = analysis;
            self.close_analysis();
        }
        if write > self.applied_write_reset_generation {
            self.applied_write_reset_generation = write;
            self.write_circuit = WriteCircuit::Closed;
            self.consecutive_abnormal_writes = 0;
            self.active_write = None;
            self.last_error = None;
        }
    }

    pub fn arm_call(&mut self, problem: &mut QueuedProblem, bucket: TokenBucket, estimate: u64) {
        self.merge_active(problem);
        let half_open_probe = self.analysis_half_open_probe && bucket == TokenBucket::Analysis;
        self.active_call = Some(ActiveCall {
            problem: problem.clone(),
            bucket,
            estimate,
            half_open_probe,
        });
    }

    fn merge_active(&self, problem: &mut QueuedProblem) {
        let Some(active) = self
            .active_call
            .as_ref()
            .filter(|active| active.problem.fingerprint == problem.fingerprint)
        else {
            return;
        };
        problem.occurrences = problem.occurrences.max(active.problem.occurrences);
        problem.last_seen_at = problem.last_seen_at.max(active.problem.last_seen_at);
        problem.excerpt = active.problem.excerpt.clone();
        problem.evidence = active.problem.evidence.clone();
        problem.pending_verification |= active.problem.pending_verification;
        problem.placeholder_created |= active.problem.placeholder_created;
        if problem.session_id.is_none() {
            problem.session_id = active.problem.session_id.clone();
        }
    }

    pub fn requeue(&mut self, mut problem: QueuedProblem) {
        self.merge_active(&mut problem);
        self.active_call = None;
        if problem.source == ProblemSource::Report {
            self.report_queue.push_front(problem);
        } else {
            self.log_queue.push_front(problem);
        }
    }

    pub fn enqueue(&mut self, mut problem: QueuedProblem, config: &DoctorConfig, now: DateTime<Utc>) {
        let now_ts = now.timestamp();
        self.flood_events.push_back((now_ts, problem.fingerprint.clone()));
        let cutoff = now_ts - config.flood_window_secs as i64;
        while self
            .flood_events
            .front()
            .is_some_and(|(timestamp, _)| *timestamp < cutoff)
        {
            self.flood_events.pop_front();
        }
        let unique = self
            .flood_events
            .iter()
            .map(|(_, fingerprint)| fingerprint)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        if self.flood_events.len() > config.flood_event_threshold as usize || unique > config.max_pending_log_signatures
        {
            self.flood_cooldown_until = Some(now_ts + config.flood_cooldown_secs as i64);
        }
        let history_limit = (config.flood_event_threshold as usize)
            .max(config.max_pending_log_signatures)
            .saturating_add(1);
        while self.flood_events.len() > history_limit {
            self.flood_events.pop_front();
        }
        if let Some(active) = self
            .active_call
            .as_mut()
            .filter(|active| active.problem.fingerprint == problem.fingerprint)
        {
            active.problem.occurrences = active.problem.occurrences.saturating_add(1);
            active.problem.last_seen_at = now;
            active.problem.excerpt = problem.excerpt;
            push_sample(&mut active.problem.evidence, &active.problem.excerpt);
            return;
        }

        let queue = if problem.source == ProblemSource::Report {
            &mut self.report_queue
        } else {
            &mut self.log_queue
        };
        if let Some(existing) = queue
            .iter_mut()
            .find(|queued| queued.fingerprint == problem.fingerprint)
        {
            existing.occurrences = existing.occurrences.saturating_add(1);
            existing.last_seen_at = now;
            existing.excerpt = problem.excerpt;
            push_sample(&mut existing.evidence, &existing.excerpt);
            return;
        }
        if let Some(recent) = self.recent.get(&problem.fingerprint) {
            problem.occurrences = recent.occurrences.saturating_add(1);
            problem.session_id = recent.session_id.clone();
            problem.log_id = recent.log_id;
            problem.remediation_round = recent.remediation_round;
            problem.pending_verification = recent.pending_verification;
            problem.placeholder_created = recent.placeholder_created;
            if problem.source.is_log() {
                problem.next_attempt_at = now_ts + config.flood_window_secs as i64;
            }
        }
        push_sample(&mut problem.evidence, &problem.excerpt);
        let limit = if problem.source == ProblemSource::Report {
            config.max_pending_report_problems
        } else {
            config.max_pending_log_signatures
        };
        if queue.len() < limit {
            queue.push_back(problem);
        }
    }

    pub fn pop_next(&mut self) -> Option<QueuedProblem> {
        let now = Utc::now().timestamp();
        if let Some(index) = self
            .report_queue
            .iter()
            .position(|problem| problem.next_attempt_at <= now)
        {
            return self.report_queue.remove(index);
        }
        let index = self
            .log_queue
            .iter()
            .position(|problem| problem.next_attempt_at <= now)?;
        self.log_queue.remove(index)
    }

    pub fn complete_problem(&mut self, problem: &QueuedProblem) {
        let mut problem = problem.clone();
        self.merge_active(&mut problem);
        self.active_call = None;
        self.recent.insert(problem.fingerprint.clone(), problem);
        if self.recent.len() > 1024 {
            let oldest = self
                .recent
                .iter()
                .min_by_key(|(_, problem)| problem.last_seen_at)
                .map(|(fingerprint, _)| fingerprint.clone());
            if let Some(oldest) = oldest {
                self.recent.remove(&oldest);
            }
        }
    }

    pub fn allocate_log_id(&mut self) -> i32 {
        let id = self.next_log_id;
        self.next_log_id = self.next_log_id.saturating_sub(1).min(-1);
        id
    }

    pub fn mark_system_finding(&mut self, reason: &str, now: DateTime<Utc>) -> bool {
        if self
            .daily_system_findings
            .get(reason)
            .is_some_and(|day| *day == now.date_naive())
        {
            return false;
        }
        self.daily_system_findings.insert(reason.to_string(), now.date_naive());
        true
    }
}

fn push_sample(evidence: &mut serde_json::Value, sample: &str) {
    let samples = evidence
        .as_object_mut()
        .expect("queued evidence is an object")
        .entry("samples")
        .or_insert_with(|| serde_json::json!([]));
    let Some(samples) = samples.as_array_mut() else {
        return;
    };
    if samples.len() < 5 && !samples.iter().any(|value| value.as_str() == Some(sample)) {
        samples.push(serde_json::Value::String(sample.chars().take(4096).collect()));
    }
}

pub fn fingerprint(source: ProblemSource, normalized: &str) -> String {
    let digest = Sha256::digest(format!("{}:{normalized}", source.as_str()).as_bytes());
    format!("{:x}", digest)
}

pub fn normalize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_number = false;
    let mut previous_space = true;
    for character in input.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_digit() {
            if !in_number {
                output.push_str("<num>");
            }
            in_number = true;
            previous_space = false;
            continue;
        }
        in_number = false;
        if character.is_whitespace() {
            if !previous_space {
                output.push(' ');
            }
            previous_space = true;
        } else if character.is_alphanumeric() || "<num>".contains(character) {
            output.push(character);
            previous_space = false;
        } else {
            output.push(' ');
            previous_space = true;
        }
    }
    output.trim().to_string()
}

pub fn state_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join("doctor-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn config() -> DoctorConfig {
        DoctorConfig {
            enabled: true,
            triage_daily_token_budget: Some(100),
            analysis_daily_token_budget: Some(100),
            remediation_daily_token_budget: Some(100),
            daily_write_round_budget: Some(2),
            max_pending_report_problems: 2,
            max_pending_log_signatures: 2,
            analysis_failure_threshold: 3,
            analysis_failure_window_secs: 600,
            analysis_circuit_cooldown_secs: 60,
            flood_event_threshold: 200,
            ..DoctorConfig::default()
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 23, 12, 0, 0).unwrap()
    }

    fn active() -> ActiveWrite {
        ActiveWrite {
            attempt_id: 1,
            action_id: 2,
            fingerprint: "finding".to_string(),
            source_task_id: None,
            started_at: now(),
        }
    }

    #[test]
    fn exact_budget_boundaries_and_overshoot_are_enforced_per_bucket() {
        let config = config();
        for bucket in [TokenBucket::Triage, TokenBucket::Analysis, TokenBucket::Remediation] {
            let mut state = DoctorState::default();
            state.set_used(bucket, 99);
            assert!(matches!(
                state.begin_call(bucket, &config, now()),
                BeginCall::Allowed { .. }
            ));
            assert_eq!(state.charge(bucket, Some(20), Some(30), 999), 50);
            assert_eq!(state.used(bucket), 149);
            assert_eq!(
                state.begin_call(bucket, &config, now()),
                BeginCall::Blocked("budget_exhausted".to_string())
            );
        }
    }

    #[test]
    fn unknown_usage_charges_conservative_estimate() {
        let mut state = DoctorState::default();
        assert_eq!(state.charge(TokenBucket::Analysis, None, Some(4), 100), 100);
        assert_eq!(state.analysis_tokens_used, 100);
        assert_eq!(
            state.begin_call(TokenBucket::Analysis, &config(), now()),
            BeginCall::Blocked("budget_exhausted".to_string())
        );
    }

    #[test]
    fn utc_rollover_resets_only_daily_counters() {
        let mut state = DoctorState {
            triage_tokens_used: 100,
            analysis_tokens_used: 100,
            remediation_tokens_used: 100,
            write_rounds_used: 2,
            analysis_circuit: AnalysisCircuit::Open,
            write_circuit: WriteCircuit::Open,
            ..DoctorState::default()
        };
        state.recent.insert(
            "finding".to_string(),
            QueuedProblem::new(
                ProblemSource::Report,
                "title".to_string(),
                "excerpt".to_string(),
                Some(1),
                Some(2),
                now(),
                -1,
            ),
        );
        state.rollover(now() + chrono::Duration::days(1));
        assert_eq!(state.triage_tokens_used, 0);
        assert_eq!(state.analysis_tokens_used, 0);
        assert_eq!(state.remediation_tokens_used, 0);
        assert_eq!(state.write_rounds_used, 0);
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Open);
        assert_eq!(state.write_circuit, WriteCircuit::Open);
        assert_eq!(state.recent.len(), 1);
    }

    #[test]
    fn atomic_restart_preserves_counters_and_queues() {
        let dir = std::env::temp_dir().join(format!("remoter-doctor-state-{}", std::process::id()));
        let path = dir.join("doctor-state.json");
        let mut state = DoctorState {
            analysis_tokens_used: 77,
            ..DoctorState::default()
        };
        state.enqueue(
            QueuedProblem::new(
                ProblemSource::Report,
                "title".to_string(),
                "excerpt".to_string(),
                Some(1),
                Some(2),
                now(),
                -1,
            ),
            &config(),
            now(),
        );
        state.save(&path).unwrap();
        let restored = DoctorState::load(&path, now()).unwrap();
        assert_eq!(restored.analysis_tokens_used, 77);
        assert_eq!(restored.report_queue.len(), 1);
        assert!(!dir.join("doctor-state.json.tmp").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn restart_restores_active_analysis_and_charges_unknown_usage() {
        let dir = std::env::temp_dir().join(format!("remoter-doctor-active-call-{}", std::process::id()));
        let path = dir.join("doctor-state.json");
        let mut state = DoctorState::default();
        let mut problem = QueuedProblem::new(
            ProblemSource::DaemonLog,
            "title".to_string(),
            "excerpt".to_string(),
            Some(1),
            Some(2),
            now(),
            -1,
        );
        problem.pending_verification = true;
        state.arm_call(&mut problem, TokenBucket::Analysis, 90);
        state.enqueue(problem, &config(), now());
        state.save(&path).unwrap();
        let restored = DoctorState::load(&path, now()).unwrap();
        assert_eq!(restored.analysis_tokens_used, 90);
        assert!(restored.active_call.is_none());
        assert_eq!(restored.log_queue.len(), 1);
        assert_eq!(restored.log_queue[0].occurrences, 2);
        assert!(restored.log_queue[0].pending_verification);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn analysis_threshold_window_and_single_half_open_probe() {
        let config = config();
        let mut state = DoctorState::default();
        state.analysis_failure(FailureClass::Infrastructure, &config, now());
        state.analysis_failure(FailureClass::Infrastructure, &config, now());
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Closed);
        state.analysis_failure(FailureClass::Infrastructure, &config, now());
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Open);
        assert_eq!(
            state.begin_call(TokenBucket::Analysis, &config, now()),
            BeginCall::Blocked("analysis_circuit_open".to_string())
        );
        let probe_time = now() + chrono::Duration::seconds(61);
        assert_eq!(
            state.begin_call(TokenBucket::Analysis, &config, probe_time),
            BeginCall::Allowed { half_open_probe: true }
        );
        assert_eq!(
            state.begin_call(TokenBucket::Analysis, &config, probe_time),
            BeginCall::Blocked("analysis_circuit_open".to_string())
        );
        state.analysis_failure(FailureClass::Infrastructure, &config, probe_time);
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Open);
        let second_probe = probe_time + chrono::Duration::seconds(61);
        assert!(matches!(
            state.begin_call(TokenBucket::Analysis, &config, second_probe),
            BeginCall::Allowed { half_open_probe: true }
        ));
        state.analysis_success();
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Closed);
    }

    #[test]
    fn log_backlog_can_supply_the_half_open_analysis_probe() {
        let mut config = config();
        config.analysis_failure_threshold = 1;
        config.triage_daily_token_budget = Some(0);
        let mut state = DoctorState::default();
        state.analysis_failure(FailureClass::Infrastructure, &config, now());
        let probe_time = now() + chrono::Duration::seconds(61);
        assert_eq!(
            state.begin_call(TokenBucket::Triage, &config, probe_time),
            BeginCall::Allowed { half_open_probe: true }
        );
        assert_eq!(
            state.begin_call(TokenBucket::Analysis, &config, probe_time),
            BeginCall::Blocked("analysis_circuit_open".to_string())
        );
        state.analysis_success();
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Closed);
        assert_eq!(
            state.begin_call(TokenBucket::Triage, &config, probe_time),
            BeginCall::Blocked("budget_exhausted".to_string())
        );
    }

    #[test]
    fn failures_outside_window_do_not_open_and_command_errors_do_not_count() {
        let config = config();
        let mut state = DoctorState::default();
        state.analysis_failure(FailureClass::Infrastructure, &config, now());
        state.analysis_failure(FailureClass::Command, &config, now());
        state.analysis_failure(
            FailureClass::Infrastructure,
            &config,
            now() + chrono::Duration::seconds(601),
        );
        state.analysis_failure(
            FailureClass::Infrastructure,
            &config,
            now() + chrono::Duration::seconds(601),
        );
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Closed);
    }

    #[test]
    fn auth_opens_immediately_until_human_generation() {
        let config = config();
        let mut state = DoctorState::default();
        state.analysis_failure(FailureClass::AuthOrQuota, &config, now());
        assert!(state.analysis_human_reset_required);
        assert_eq!(
            state.begin_call(TokenBucket::Analysis, &config, now() + chrono::Duration::hours(2)),
            BeginCall::Blocked("analysis_circuit_open".to_string())
        );
        state.apply_reset_generations(1, 0);
        assert_eq!(state.analysis_circuit, AnalysisCircuit::Closed);
        assert_eq!(state.applied_analysis_reset_generation, 1);
    }

    #[test]
    fn write_unknown_survives_restart_and_requires_human_reset() {
        let dir = std::env::temp_dir().join(format!("remoter-doctor-write-state-{}", std::process::id()));
        let path = dir.join("doctor-state.json");
        let mut state = DoctorState::default();
        assert_eq!(state.begin_write(active(), &config(), now()), BeginWrite::Allowed);
        assert_eq!(state.write_rounds_used, 1);
        state.save(&path).unwrap();
        let mut restored = DoctorState::load(&path, now()).unwrap();
        assert_eq!(restored.write_circuit, WriteCircuit::Open);
        assert!(restored.active_write.is_none());
        assert_eq!(
            restored.begin_write(active(), &config(), now()),
            BeginWrite::Blocked("write_circuit_open".to_string())
        );
        restored.apply_reset_generations(0, 4);
        assert_eq!(restored.write_circuit, WriteCircuit::Closed);
        assert_eq!(restored.applied_write_reset_generation, 4);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn two_abnormal_writes_open_write_circuit() {
        let mut state = DoctorState::default();
        assert_eq!(state.begin_write(active(), &config(), now()), BeginWrite::Allowed);
        state.end_write(WriteEnd::Abnormal);
        assert_eq!(state.write_circuit, WriteCircuit::Closed);
        assert_eq!(state.begin_write(active(), &config(), now()), BeginWrite::Allowed);
        state.end_write(WriteEnd::Abnormal);
        assert_eq!(state.write_circuit, WriteCircuit::Open);
    }

    #[test]
    fn report_and_log_queues_are_independently_bounded() {
        let config = config();
        let mut state = DoctorState::default();
        for index in 0..4 {
            for source in [ProblemSource::Report, ProblemSource::DaemonLog] {
                state.enqueue(
                    QueuedProblem::new(
                        source,
                        format!("title {}", ['a', 'b', 'c', 'd'][index as usize]),
                        "same excerpt".to_string(),
                        None,
                        None,
                        now(),
                        -(index + 1),
                    ),
                    &config,
                    now(),
                );
            }
        }
        assert_eq!(state.report_queue.len(), 2);
        assert_eq!(state.log_queue.len(), 2);
        assert_eq!(state.pop_next().unwrap().source, ProblemSource::Report);
    }

    #[test]
    fn flood_history_stays_bounded_while_duplicates_aggregate() {
        let mut config = config();
        config.flood_event_threshold = 3;
        let mut state = DoctorState::default();
        for _ in 0..20 {
            state.enqueue(
                QueuedProblem::new(
                    ProblemSource::DaemonLog,
                    "same".to_string(),
                    "failure".to_string(),
                    None,
                    None,
                    now(),
                    -1,
                ),
                &config,
                now(),
            );
        }
        assert_eq!(state.flood_events.len(), 4);
        assert_eq!(state.log_queue.len(), 1);
        assert_eq!(state.log_queue[0].occurrences, 20);
        assert!(state.flood_cooldown_until.is_some());
        assert_eq!(
            state.begin_call(TokenBucket::Triage, &config, now()),
            BeginCall::Blocked("flood_guard".to_string())
        );
    }

    #[test]
    fn duplicate_events_aggregate_occurrences_and_samples() {
        let config = config();
        let mut state = DoctorState::default();
        for text in ["failure 123", "failure 456"] {
            state.enqueue(
                QueuedProblem::new(
                    ProblemSource::DaemonLog,
                    "same".to_string(),
                    text.to_string(),
                    None,
                    None,
                    now(),
                    -1,
                ),
                &config,
                now(),
            );
        }
        assert_eq!(state.log_queue.len(), 1);
        state.enqueue(
            QueuedProblem::new(
                ProblemSource::DaemonLog,
                "same".to_string(),
                "failure 123".to_string(),
                None,
                None,
                now(),
                -1,
            ),
            &config,
            now(),
        );
        assert_eq!(state.log_queue[0].occurrences, 3);
        assert!(state.log_queue[0].evidence["samples"].as_array().unwrap().len() <= 5);
    }

    #[test]
    fn repeated_completed_signature_waits_for_cooldown_and_keeps_occurrences() {
        let current = Utc::now();
        let config = config();
        let mut state = DoctorState::default();
        let problem = QueuedProblem::new(
            ProblemSource::DaemonLog,
            "same".to_string(),
            "failure".to_string(),
            None,
            None,
            current,
            -1,
        );
        state.complete_problem(&problem);
        state.enqueue(problem, &config, current);
        assert_eq!(state.log_queue.len(), 1);
        assert_eq!(state.log_queue[0].occurrences, 2);
        assert!(state.log_queue[0].next_attempt_at > current.timestamp());
        assert!(state.pop_next().is_none());
    }

    #[test]
    fn zero_budget_disables_stage_and_write_round_is_charged_at_launch() {
        let mut config = config();
        config.triage_daily_token_budget = Some(0);
        config.daily_write_round_budget = Some(0);
        let mut state = DoctorState::default();
        assert_eq!(
            state.begin_call(TokenBucket::Triage, &config, now()),
            BeginCall::Blocked("budget_exhausted".to_string())
        );
        assert_eq!(state.write_rounds_used, 0);
        assert_eq!(
            state.begin_write(active(), &config, now()),
            BeginWrite::Blocked("budget_exhausted".to_string())
        );
        assert_eq!(state.write_rounds_used, 0);
    }
}
