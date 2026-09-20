//! Daemon-side staging environments: validated `.remoter/staging.toml` files,
//! labeled Docker containers, and the data frames used by the stage tunnel.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

use crate::client::ProjectRepoConfig;
use crate::config::Config;
use crate::container::TASK_ID_LABEL;
use crate::image::{self, ImageLocks};
use crate::{container, workspace};

pub const STAGING_LABEL: &str = "remoter.staging";
pub const PROJECT_ID_LABEL: &str = "remoter.project_id";
pub const MAX_STAGING_ENVIRONMENTS: usize = 3;
pub const STAGING_PATH_PREFIX: &str = "/stage/task-";
const STAGING_DATABASE_URL: &str = "postgres://postgres:postgres@127.0.0.1:5432/staging";
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(48 * 60 * 60);
const MIN_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_HEALTH_RETRIES: u32 = 10_000;
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingConfig {
    pub enabled: bool,
    pub start_command: Option<String>,
    pub stop_command: Option<String>,
    pub healthcheck: Option<Healthcheck>,
    pub idle_timeout: Duration,
    pub public: bool,
    pub routes: Vec<StageRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Healthcheck {
    pub command: String,
    pub interval_secs: u64,
    pub retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageRoute {
    pub path: String,
    pub port: u16,
    #[serde(default)]
    pub websocket: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawStagingConfig {
    enabled: Option<bool>,
    start_command: Option<String>,
    stop_command: Option<String>,
    healthcheck: Option<RawHealthcheck>,
    idle_timeout: Option<String>,
    #[serde(default)]
    public: bool,
    #[serde(default)]
    routes: Vec<StageRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawHealthcheck {
    command: Option<String>,
    #[serde(default = "default_health_interval")]
    interval_secs: u64,
    #[serde(default = "default_health_retries")]
    retries: u32,
}

fn default_health_interval() -> u64 {
    5
}
fn default_health_retries() -> u32 {
    60
}

impl StagingConfig {
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawStagingConfig = toml::from_str(text).map_err(|e| format!("invalid staging.toml: {e}"))?;
        let enabled = raw
            .enabled
            .ok_or_else(|| "staging.toml requires `enabled`".to_string())?;
        if !enabled {
            return Ok(Self {
                enabled,
                start_command: None,
                stop_command: None,
                healthcheck: None,
                idle_timeout: parse_idle_timeout(raw.idle_timeout.as_deref())?,
                public: raw.public,
                routes: Vec::new(),
            });
        }
        let start_command = raw
            .start_command
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "enabled staging.toml requires `start_command`".to_string())?;
        if raw.routes.is_empty() {
            return Err("enabled staging.toml requires at least one route".to_string());
        }
        let mut paths = HashSet::new();
        for route in &raw.routes {
            if !route.path.starts_with('/') {
                return Err(format!("route path must start with `/`: {:?}", route.path));
            }
            if route.port == 0 {
                return Err(format!("route {} has an invalid port", route.path));
            }
            if !paths.insert(route.path.clone()) {
                return Err(format!("duplicate staging route path: {}", route.path));
            }
        }
        let healthcheck = raw
            .healthcheck
            .map(|h| {
                let command = h.command.ok_or_else(|| "healthcheck requires `command`".to_string())?;
                if command.trim().is_empty() {
                    return Err("healthcheck command cannot be empty".to_string());
                }
                if h.interval_secs == 0 {
                    return Err("healthcheck interval_secs must be at least 1".to_string());
                }
                if h.retries == 0 || h.retries > MAX_HEALTH_RETRIES {
                    return Err("healthcheck retries must be between 1 and 10000".to_string());
                }
                Ok(Healthcheck {
                    command,
                    interval_secs: h.interval_secs,
                    retries: h.retries,
                })
            })
            .transpose()?;
        Ok(Self {
            enabled,
            start_command: Some(start_command),
            stop_command: raw.stop_command.filter(|s| !s.trim().is_empty()),
            healthcheck,
            idle_timeout: parse_idle_timeout(raw.idle_timeout.as_deref())?,
            public: raw.public,
            routes: raw.routes,
        })
    }

    pub fn from_file(repo: &Path) -> Result<Option<Self>, String> {
        let path = repo.join(".remoter/staging.toml");
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read staging.toml: {e}")),
        }
    }

    pub fn route_for(&self, path: &str) -> Option<&StageRoute> {
        self.routes
            .iter()
            .filter(|r| route_matches(&r.path, path))
            .max_by_key(|r| r.path.len())
    }
}

fn parse_idle_timeout(value: Option<&str>) -> Result<Duration, String> {
    let value = value.unwrap_or("48h");
    let (number, unit) = value.trim().split_at(value.trim().len().saturating_sub(1));
    let amount: u64 = number.parse().map_err(|_| format!("invalid idle_timeout: {value:?}"))?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("invalid idle_timeout: {value:?}; use s, m, h, or d")),
    };
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| format!("idle_timeout is too large: {value:?}"))?;
    let duration = Duration::from_secs(seconds);
    if duration < MIN_IDLE_TIMEOUT {
        return Err("idle_timeout must be at least 5m".to_string());
    }
    Ok(duration)
}

fn route_matches(prefix: &str, path: &str) -> bool {
    prefix == "/" || path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StageState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}

#[derive(Debug, Clone)]
pub struct StagingEnvironment {
    pub task_id: i32,
    pub project_id: i32,
    pub state: StageState,
    pub routes: Vec<PublishedRoute>,
    pub public: bool,
    pub last_error: Option<String>,
    pub last_activity: Instant,
    pub idle_timeout: Duration,
    container: String,
    sidecars: Vec<String>,
    docker: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedRoute {
    pub path: String,
    pub port: u16,
    #[serde(skip_serializing, skip_deserializing)]
    pub host_port: u16,
    pub websocket: bool,
}

impl StagingEnvironment {
    pub fn url_base(&self) -> String {
        format!("/stage/task-{}/", self.task_id)
    }
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }
}

pub type StagingHandle = Arc<Mutex<StagingManager>>;

#[derive(Clone)]
pub struct StagingManager {
    config: Arc<Config>,
    image_locks: ImageLocks,
    environments: HashMap<i32, StagingEnvironment>,
    updates: broadcast::Sender<StageTunnelFrame>,
}

impl StagingManager {
    pub fn handle(config: Arc<Config>, image_locks: ImageLocks) -> StagingHandle {
        Arc::new(Mutex::new(Self::new(config, image_locks)))
    }

    pub fn new(config: Arc<Config>, image_locks: ImageLocks) -> Self {
        let (updates, _) = broadcast::channel(64);
        Self {
            config,
            image_locks,
            environments: HashMap::new(),
            updates,
        }
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<StageTunnelFrame> {
        self.updates.subscribe()
    }

    fn publish(&self, environment: &StagingEnvironment) {
        let _ = self.updates.send(env_changed_frame(environment));
    }

    pub fn environments(&self) -> impl Iterator<Item = &StagingEnvironment> {
        self.environments.values()
    }

    pub fn snapshot(&self) -> Vec<StagingEnvironment> {
        self.environments.values().cloned().collect()
    }

    pub fn touch(&mut self, task_id: i32) -> bool {
        let Some(environment) = self.environments.get_mut(&task_id) else {
            return false;
        };
        environment.touch();
        true
    }
    pub fn get(&self, task_id: i32) -> Option<&StagingEnvironment> {
        self.environments.get(&task_id)
    }
    pub fn get_mut(&mut self, task_id: i32) -> Option<&mut StagingEnvironment> {
        self.environments.get_mut(&task_id)
    }

    pub async fn start(&mut self, task_id: i32, project: &ProjectRepoConfig) -> Result<(), String> {
        if let Some(env) = self.environments.get(&task_id)
            && matches!(env.state, StageState::Starting | StageState::Running)
        {
            return Ok(());
        }
        if self
            .environments
            .values()
            .filter(|e| matches!(e.state, StageState::Starting | StageState::Running))
            .count()
            >= MAX_STAGING_ENVIRONMENTS
        {
            return Err(format!(
                "staging limit reached (maximum {MAX_STAGING_ENVIRONMENTS} environments)"
            ));
        }
        let repo = workspace::repo_dir(&self.config.workspace_root, project.project_id);
        let stage = StagingConfig::from_file(&repo)?.ok_or_else(|| "staging is not configured".to_string())?;
        if !stage.enabled {
            return Err("staging is disabled".to_string());
        }
        let image = image::ensure_project_image(&self.config.execution, &self.image_locks, &repo, project, task_id)
            .await
            .map_err(|e| format!("ensure staging image: {e}"))?;
        let name = format!("remoter-stage-task-{task_id}");
        let postgres_name = format!("{name}-pg");
        let minio_name = format!("{name}-minio");
        let docker = self.config.execution.docker_binary.clone();
        for stale in [&postgres_name, &minio_name, &name] {
            let _ = container::docker(&docker, &["rm".into(), "-f".into(), (*stale).clone()]).await;
        }
        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("{STAGING_LABEL}=1"),
            "--label".into(),
            format!("{TASK_ID_LABEL}={task_id}"),
            "--label".into(),
            format!("{PROJECT_ID_LABEL}={}", project.project_id),
            "-v".into(),
            format!("{}:/work", repo.display()),
            "-w".into(),
            "/work".into(),
            "-e".into(),
            format!("REMOTER_STAGE_PATH_BASE={STAGING_PATH_PREFIX}{task_id}"),
            "-e".into(),
            "REMOTER_CONTAINER=1".into(),
            "-e".into(),
            format!("REMOTER_STAGE_DATABASE_URL={STAGING_DATABASE_URL}"),
            "-e".into(),
            format!("DATABASE_URL={STAGING_DATABASE_URL}"),
            "-e".into(),
            format!("LOCAL_DATABASE_URL={STAGING_DATABASE_URL}"),
            "-e".into(),
            "MINIO_ENDPOINT=http://127.0.0.1:9000".into(),
        ];
        let ports: HashSet<u16> = stage.routes.iter().map(|r| r.port).collect();
        for port in ports {
            args.extend(["-p".into(), format!("127.0.0.1:0:{port}")]);
        }
        args.extend([image, "sleep".into(), "infinity".into()]);
        container::docker(&docker, &args)
            .await
            .map_err(|e| format!("start staging container: {e}"))?;
        let label_stage = format!("{STAGING_LABEL}=1");
        let label_project = format!("{PROJECT_ID_LABEL}={}", project.project_id);
        let sidecar_result = async {
            container::docker(
                &docker,
                &[
                    "run".into(),
                    "-d".into(),
                    "--name".into(),
                    postgres_name.clone(),
                    "--label".into(),
                    label_stage.clone(),
                    "--label".into(),
                    label_project.clone(),
                    "--network".into(),
                    format!("container:{name}"),
                    "-e".into(),
                    "POSTGRES_USER=postgres".into(),
                    "-e".into(),
                    "POSTGRES_PASSWORD=postgres".into(),
                    self.config.execution.postgres_image.clone(),
                ],
            )
            .await
            .map_err(|e| format!("start staging postgres: {e}"))?;
            container::docker(
                &docker,
                &[
                    "run".into(),
                    "-d".into(),
                    "--name".into(),
                    minio_name.clone(),
                    "--label".into(),
                    label_stage,
                    "--label".into(),
                    label_project,
                    "--network".into(),
                    format!("container:{name}"),
                    "-e".into(),
                    "MINIO_ROOT_USER=minioadmin".into(),
                    "-e".into(),
                    "MINIO_ROOT_PASSWORD=minioadmin".into(),
                    self.config.execution.minio_image.clone(),
                    "server".into(),
                    "/data".into(),
                ],
            )
            .await
            .map_err(|e| format!("start staging minio: {e}"))?;
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = sidecar_result {
            for stale in [&postgres_name, &minio_name, &name] {
                let _ = container::docker(&docker, &["rm".into(), "-f".into(), (*stale).clone()]).await;
            }
            return Err(error);
        }
        let mut env = StagingEnvironment {
            task_id,
            project_id: project.project_id,
            state: StageState::Starting,
            routes: Vec::new(),
            public: stage.public,
            last_error: None,
            last_activity: Instant::now(),
            idle_timeout: stage.idle_timeout,
            container: name.clone(),
            sidecars: vec![postgres_name.clone(), minio_name.clone()],
            docker: docker.clone(),
        };
        let result = async {
            let routes = published_routes(&docker, &name, &stage.routes).await?;
            async_start(&docker, &name, &stage, task_id).await?;
            Ok::<_, String>(routes)
        }
        .await;
        match result {
            Ok(routes) => {
                env.routes = routes;
                env.state = StageState::Running;
                self.environments.insert(task_id, env);
                if let Some(environment) = self.environments.get(&task_id) {
                    self.publish(environment);
                }
                Ok(())
            }
            Err(error) => {
                for stale in [&postgres_name, &minio_name, &name] {
                    let _ = container::docker(&docker, &["rm".into(), "-f".into(), (*stale).clone()]).await;
                }
                env.state = StageState::Error;
                env.last_error = Some(error.clone());
                self.environments.insert(task_id, env);
                if let Some(environment) = self.environments.get(&task_id) {
                    self.publish(environment);
                }
                Err(error)
            }
        }
    }

    pub async fn stop(&mut self, task_id: i32) -> bool {
        let Some(mut env) = self.environments.remove(&task_id) else {
            return false;
        };
        env.state = StageState::Stopping;
        self.publish(&env);
        if let Some(stage) = self.stage_config(task_id, env.project_id)
            && let Some(command) = stage.stop_command
        {
            let _ = exec(&env.docker, &env.container, &command, task_id).await;
        }
        for sidecar in &env.sidecars {
            let _ = container::docker(&env.docker, &["rm".into(), "-f".into(), sidecar.clone()]).await;
        }
        let _ = container::docker(&env.docker, &["rm".into(), "-f".into(), env.container.clone()]).await;
        env.state = StageState::Stopped;
        env.routes.clear();
        self.publish(&env);
        true
    }

    fn stage_config(&self, _task_id: i32, project_id: i32) -> Option<StagingConfig> {
        StagingConfig::from_file(&workspace::repo_dir(&self.config.workspace_root, project_id))
            .ok()
            .flatten()
    }

    pub async fn sweep(&mut self) {
        sweep_orphans(&self.config.execution).await;
        self.environments.clear();
    }

    pub async fn sweep_idle(&mut self) {
        let expired: Vec<i32> = self
            .environments
            .values()
            .filter(|e| e.last_activity.elapsed() >= e.idle_timeout)
            .map(|e| e.task_id)
            .collect();
        for task_id in expired {
            self.stop(task_id).await;
        }
    }
}

pub async fn sweep_orphans(cfg: &crate::config::ExecutionConfig) {
    let docker = &cfg.docker_binary;
    if let Ok(ids) = container::docker(
        docker,
        &[
            "ps".into(),
            "-aq".into(),
            "--filter".into(),
            format!("label={STAGING_LABEL}"),
        ],
    )
    .await
    {
        for id in ids.lines().filter(|id| !id.trim().is_empty()) {
            let _ = container::docker(docker, &["rm".into(), "-f".into(), id.to_string()]).await;
        }
    }
}

async fn published_routes(
    docker: &str,
    container_name: &str,
    routes: &[StageRoute],
) -> Result<Vec<PublishedRoute>, String> {
    let mut out = Vec::new();
    let mut host_ports = HashSet::new();
    for route in routes {
        let text = container::docker(
            docker,
            &["port".into(), container_name.to_string(), format!("{}/tcp", route.port)],
        )
        .await
        .map_err(|e| format!("discover staging port {}: {e}", route.port))?;
        let host_port = text
            .rsplit(':')
            .next()
            .and_then(|p| p.trim().parse().ok())
            .ok_or_else(|| format!("docker port returned invalid port: {text}"))?;
        if !host_ports.insert(host_port) {
            return Err(format!("docker published duplicate host port: {host_port}"));
        }
        out.push(PublishedRoute {
            path: route.path.clone(),
            port: route.port,
            host_port,
            websocket: route.websocket,
        });
    }
    Ok(out)
}

async fn async_start(docker: &str, name: &str, stage: &StagingConfig, task_id: i32) -> Result<(), String> {
    exec(docker, name, stage.start_command.as_deref().unwrap_or("true"), task_id).await?;
    if let Some(check) = &stage.healthcheck {
        for attempt in 0..check.retries {
            if exec(docker, name, &check.command, task_id).await.is_ok() {
                return Ok(());
            }
            if attempt + 1 < check.retries {
                tokio::time::sleep(Duration::from_secs(check.interval_secs)).await;
            }
        }
        return Err("staging healthcheck exhausted retries".to_string());
    }
    Ok(())
}

async fn exec(docker: &str, name: &str, command: &str, task_id: i32) -> Result<(), String> {
    let base = format!("{STAGING_PATH_PREFIX}{task_id}");
    let args = vec![
        "exec".into(),
        "-w".into(),
        "/work".into(),
        "-e".into(),
        format!("REMOTER_STAGE_PATH_BASE={base}"),
        "-e".into(),
        "REMOTER_CONTAINER=1".into(),
        "-e".into(),
        format!("REMOTER_STAGE_DATABASE_URL={STAGING_DATABASE_URL}"),
        name.to_string(),
        "devenv".into(),
        "shell".into(),
        "--no-tui".into(),
        "--no-eval-cache".into(),
        "--".into(),
        "sh".into(),
        "-lc".into(),
        command.to_string(),
    ];
    tokio::time::timeout(EXEC_TIMEOUT, container::docker(docker, &args))
        .await
        .map_err(|_| format!("staging command timed out after {}s", EXEC_TIMEOUT.as_secs()))?
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ── stage tunnel frames ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StageCommand {
    StageStart { task_id: i32 },
    StageStop { task_id: i32 },
    StageRestart { task_id: i32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StageTunnelFrame {
    Hello {
        envs: Vec<StageHelloEnvironment>,
    },
    EnvChanged {
        task_id: i32,
        state: StageState,
        #[serde(skip_serializing_if = "Option::is_none")]
        routes: Option<Vec<PublishedRoute>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_error: Option<String>,
    },
    HttpRequest {
        stream_id: String,
        task_id: i32,
        route_path: String,
        method: String,
        path: String,
        query: String,
        headers: HashMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        body_base64: Option<String>,
    },
    WsOpen {
        stream_id: String,
        task_id: i32,
        route_path: String,
        path: String,
        headers: HashMap<String, String>,
    },
    WsMessage {
        stream_id: String,
        payload_base64: String,
    },
    WsClose {
        stream_id: String,
        code: u16,
        reason: String,
    },
    HttpResponse {
        stream_id: String,
        status: u16,
        headers: HashMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        body_base64: Option<String>,
        last_chunk: bool,
    },
    WsAccepted {
        stream_id: String,
        status: u16,
        headers: HashMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StageHelloEnvironment {
    pub task_id: i32,
    pub project_id: i32,
    pub routes: Vec<PublishedRoute>,
}

pub fn hello_frame(environments: impl Iterator<Item = StagingEnvironment>) -> StageTunnelFrame {
    StageTunnelFrame::Hello {
        envs: environments
            .map(|e| StageHelloEnvironment {
                task_id: e.task_id,
                project_id: e.project_id,
                routes: e.routes,
            })
            .collect(),
    }
}

pub fn env_changed_frame(env: &StagingEnvironment) -> StageTunnelFrame {
    StageTunnelFrame::EnvChanged {
        task_id: env.task_id,
        state: env.state,
        routes: (!env.routes.is_empty()).then(|| env.routes.clone()),
        last_error: env.last_error.clone(),
    }
}

pub fn tunnel_url(api_url: &str) -> String {
    format!(
        "{}/api/v1/agent/stage-tunnel",
        api_url.trim_end_matches('/').replacen("http", "ws", 1)
    )
}

pub fn map_route<'a>(routes: &'a [PublishedRoute], path: &str) -> Option<&'a PublishedRoute> {
    routes
        .iter()
        .filter(|r| route_matches(&r.path, path))
        .max_by_key(|r| r.path.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates_staging_config() {
        let cfg = StagingConfig::parse(
            r#"enabled = true
start_command = "./start"
healthcheck = { command = "curl health" }
[[routes]]
path = "/"
port = 5173
[[routes]]
path = "/api"
port = 8181
websocket = true
"#,
        )
        .unwrap();
        assert_eq!(cfg.idle_timeout, DEFAULT_IDLE_TIMEOUT);
        assert_eq!(cfg.healthcheck.as_ref().unwrap().interval_secs, 5);
        assert_eq!(cfg.route_for("/api/v1").unwrap().port, 8181);
        assert_eq!(cfg.route_for("/anything").unwrap().port, 5173);
    }

    #[test]
    fn rejects_bad_config_and_accepts_disabled_config() {
        assert!(StagingConfig::parse("enabled = true\nstart_command = \"x\"\n").is_err());
        assert!(
            StagingConfig::parse(
                "enabled = true\nstart_command = \"x\"\nidle_timeout = \"1m\"\n[[routes]]\npath=\"/\"\nport=1"
            )
            .is_err()
        );
        assert!(!StagingConfig::parse("enabled = false").unwrap().enabled);
    }

    #[test]
    fn duplicate_paths_and_prefix_boundaries_are_checked() {
        let duplicate =
            "enabled=true\nstart_command=\"x\"\n[[routes]]\npath=\"/api\"\nport=1\n[[routes]]\npath=\"/api\"\nport=2";
        assert!(StagingConfig::parse(duplicate).is_err());
        assert!(route_matches("/api", "/api/v1"));
        assert!(!route_matches("/api", "/apix"));
    }

    #[test]
    fn tunnel_frames_and_longest_route_mapping_are_stable() {
        let routes = vec![
            PublishedRoute {
                path: "/".into(),
                port: 1,
                host_port: 2,
                websocket: false,
            },
            PublishedRoute {
                path: "/api".into(),
                port: 3,
                host_port: 4,
                websocket: true,
            },
        ];
        assert_eq!(map_route(&routes, "/api/v1").unwrap().port, 3);
        let frame = StageTunnelFrame::EnvChanged {
            task_id: 7,
            state: StageState::Running,
            routes: Some(routes),
            last_error: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"envChanged\""));
        let decoded = serde_json::from_str::<StageTunnelFrame>(&json).unwrap();
        assert!(matches!(
            decoded,
            StageTunnelFrame::EnvChanged {
                task_id: 7,
                state: StageState::Running,
                ..
            }
        ));
    }

    #[test]
    fn hello_and_url_follow_protocol() {
        let env = StagingEnvironment {
            task_id: 7,
            project_id: 1,
            state: StageState::Running,
            routes: vec![],
            public: false,
            last_error: None,
            last_activity: Instant::now(),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            container: "x".into(),
            sidecars: vec![],
            docker: "docker".into(),
        };
        assert!(
            serde_json::to_string(&hello_frame(vec![env].into_iter()))
                .unwrap()
                .contains("hello")
        );
        assert_eq!(
            tunnel_url("https://example.test"),
            "wss://example.test/api/v1/agent/stage-tunnel"
        );
    }
}
