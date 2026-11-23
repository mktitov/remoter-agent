pub mod model;
pub mod sandbox;
pub mod state;
mod worker;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::client::RemoterClient;
use crate::config::Config;
use crate::driver::AgentDriver;
use crate::logstore::LogStore;
use state::{DoctorState, ProblemSource, QueuedProblem};

pub(crate) type SharedState = Arc<Mutex<DoctorState>>;

#[derive(Clone)]
pub struct DoctorHandle {
    enabled: bool,
    state: Option<SharedState>,
    wake: mpsc::Sender<()>,
    cancel: CancellationToken,
    join: Arc<Mutex<Option<JoinHandle<()>>>>,
    active_runs: Arc<Mutex<HashSet<i32>>>,
    queue_config: crate::doctor_config::DoctorConfig,
    state_path: std::path::PathBuf,
}

impl DoctorHandle {
    pub fn disabled() -> Self {
        let (wake, _) = mpsc::channel(1);
        Self {
            enabled: false,
            state: None,
            wake,
            cancel: CancellationToken::new(),
            join: Arc::new(Mutex::new(None)),
            active_runs: Arc::new(Mutex::new(HashSet::new())),
            queue_config: crate::doctor_config::DoctorConfig::default(),
            state_path: std::path::PathBuf::new(),
        }
    }

    pub fn start(
        config: Arc<Config>,
        client: RemoterClient,
        driver: Arc<dyn AgentDriver>,
        log_store: LogStore,
    ) -> anyhow::Result<Self> {
        if !config.doctor.enabled {
            return Ok(Self::disabled());
        }
        let path = state::state_path(&config.workspace_root);
        let state = DoctorState::load(&path, Utc::now())?;
        state.save(&path)?;
        let state = Arc::new(Mutex::new(state));
        let (wake, rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let active_runs = Arc::new(Mutex::new(HashSet::new()));
        let queue_config = config.doctor.clone();
        let join = tokio::spawn(worker::run(
            config,
            client,
            driver,
            log_store,
            state.clone(),
            active_runs.clone(),
            wake.clone(),
            rx,
            cancel.clone(),
        ));
        Ok(Self {
            enabled: true,
            state: Some(state),
            wake,
            cancel,
            join: Arc::new(Mutex::new(Some(join))),
            active_runs,
            queue_config,
            state_path: path,
        })
    }

    pub fn ingest_report(&self, report: &str, task_id: i32, run_id: i32) {
        if !self.enabled {
            return;
        }
        for (title, description) in model::report_problems(report) {
            self.enqueue(ProblemSource::Report, title, description, Some(task_id), Some(run_id));
        }
    }

    pub fn ingest_daemon_log(&self, level: &str, message: &str) {
        if !self.enabled || level != "error" || is_doctor_log(message) || is_expected_retry(message) {
            return;
        }
        self.enqueue(
            ProblemSource::DaemonLog,
            "Daemon ERROR".to_string(),
            message.to_string(),
            extract_id(message, "task_id"),
            extract_id(message, "run_id"),
        );
    }

    pub fn ingest_acp_error(&self, task_id: i32, run_id: i32, message: &str) {
        if !self.enabled || is_expected_retry(message) {
            return;
        }
        self.enqueue(
            ProblemSource::AcpLog,
            "Terminal ACP error".to_string(),
            message.to_string(),
            Some(task_id),
            Some(run_id),
        );
    }

    fn enqueue(
        &self,
        source: ProblemSource,
        title: String,
        excerpt: String,
        task_id: Option<i32>,
        run_id: Option<i32>,
    ) {
        let Some(state) = &self.state else {
            return;
        };
        let mut state = state.lock().expect("Doctor state poisoned");
        let log_id = state.allocate_log_id();
        let problem = QueuedProblem::new(source, title, excerpt, task_id, run_id, Utc::now(), log_id);
        state.enqueue(problem, &self.queue_config, Utc::now());
        if let Err(error) = state.save(&self.state_path) {
            tracing::warn!(target: "remoter_agent::doctor", %error, "could not persist Doctor queue");
        }
        drop(state);
        let _ = self.wake.try_send(());
    }

    pub async fn acquire_run(&self, task_id: i32, cancel: CancellationToken) -> Option<DoctorRunGuard> {
        loop {
            if self.enabled {
                let mut active_runs = self.active_runs.lock().expect("Doctor active runs poisoned");
                let state = self
                    .state
                    .as_ref()
                    .map(|state| state.lock().expect("Doctor state poisoned"));
                let conflicting = state.as_ref().is_some_and(|state| {
                    state
                        .active_write
                        .as_ref()
                        .is_some_and(|write| write.source_task_id == Some(task_id))
                });
                if !conflicting {
                    active_runs.insert(task_id);
                    drop(state);
                    drop(active_runs);
                    return Some(DoctorRunGuard {
                        handle: self.clone(),
                        task_id,
                    });
                }
                drop(state);
                drop(active_runs);
            } else {
                return Some(DoctorRunGuard {
                    handle: self.clone(),
                    task_id,
                });
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                _ = cancel.cancelled() => return None,
            }
        }
    }

    pub async fn shutdown(&self) {
        if !self.enabled {
            return;
        }
        self.cancel.cancel();
        let join = self.join.lock().expect("Doctor join poisoned").take();
        if let Some(join) = join {
            let _ = join.await;
        }
    }
}

fn is_doctor_log(message: &str) -> bool {
    message.starts_with("remoter_agent::doctor")
}

fn is_expected_retry(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("retrying attempt") || lower.contains("expected retry") || lower.contains("will retry")
}

fn extract_id(message: &str, key: &str) -> Option<i32> {
    let start = message.find(key)? + key.len();
    let rest = message[start..].trim_start_matches(['=', ':', ' ']);
    let digits = rest.chars().take_while(char::is_ascii_digit).collect::<String>();
    digits.parse().ok()
}

pub struct DoctorRunGuard {
    handle: DoctorHandle,
    task_id: i32,
}

impl Drop for DoctorRunGuard {
    fn drop(&mut self) {
        if self.handle.enabled {
            self.handle
                .active_runs
                .lock()
                .expect("Doctor active runs poisoned")
                .remove(&self.task_id);
        }
    }
}
