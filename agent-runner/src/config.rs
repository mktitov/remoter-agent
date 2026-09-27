//! Daemon configuration: `remoter-agent.toml` + environment (spec §5.1).
//!
//! The agent token comes from the `REMOTER_AGENT_TOKEN` env var or the
//! config's `token` key (env wins). The tracked example config carries no
//! secrets — a file with `token` must be owner-only (`chmod 600`).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Logging configuration for ACP run logs (spec §7.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LogsConfig {
    /// Directory for `task-<id>/run-<run_id>.jsonl` files.
    /// Default: `<workspace_root>/logs`.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// How many days to keep log files before the retention sweep removes them.
    /// `0` disables sweeping.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            dir: None,
            retention_days: default_retention_days(),
        }
    }
}

fn default_retention_days() -> u64 {
    7
}

/// How run attempts are executed (docs/specs/remoter-agent-containers.md):
/// bare on the host (`devenv shell --`), inside a per-run OCI container, or
/// inside a per-run systemd-nspawn machine (NixOS hosts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Host,
    Container,
    Nspawn,
}

impl ExecutionMode {
    /// The config-file spelling (serde `snake_case`).
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionMode::Host => "host",
            ExecutionMode::Container => "container",
            ExecutionMode::Nspawn => "nspawn",
        }
    }
}

/// Container/nspawn-mode settings (`[execution]`). All of it is irrelevant in
/// host mode — the defaults keep a host-mode config file free of docker and
/// nspawn keys.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecutionConfig {
    /// `host` (default) runs the agent on the daemon host; `container` runs
    /// every attempt inside a per-run docker container with postgres/minio
    /// sidecars; `nspawn` boots a per-run systemd-nspawn machine from a NixOS
    /// closure with the host `/nix/store` bind-mounted read-only. There is no
    /// silent fallback from `container`/`nspawn` to `host`.
    #[serde(default)]
    pub mode: ExecutionMode,
    /// The docker CLI binary (default `docker`).
    #[serde(default = "default_docker_binary")]
    pub docker_binary: String,
    /// Tag prefix for the per-project agent images: `<prefix>:p<id>-<hash>`.
    #[serde(default = "default_image_tag_prefix")]
    pub image_tag_prefix: String,
    /// Pinned sidecar images (PoC: built/pulled by tag, never `latest`).
    #[serde(default = "default_postgres_image")]
    pub postgres_image: String,
    #[serde(default = "default_minio_image")]
    pub minio_image: String,
    #[serde(default = "default_mc_image")]
    pub mc_image: String,
    /// Shared home for agents inside containers (mounted at `/root`): kimi
    /// config/credentials and session state survive runs here.
    /// Default: `<workspace_root>/agent-home`.
    #[serde(default)]
    pub agent_home: Option<PathBuf>,
    /// Shared nix binary cache directory on the daemon host (a `file://`
    /// substituter). When set and the project is nix-based (its
    /// `.remoter/agent.Dockerfile` builds `FROM` a nix image, or the repo has
    /// `devenv.nix`/`devenv.yaml`), image-init containers get it bind-mounted
    /// rw at `/nix-cache` as an extra substituter, export their gcroot
    /// closures into it after the bake, and run containers get it read-only —
    /// repeat bakes and in-container nix commands substitute instead of
    /// rebuilding. Default: unset (disabled).
    #[serde(default)]
    pub nix_binary_cache_dir: Option<PathBuf>,
    /// Nspawn mode only: the NixOS system closure booted per run — either a
    /// flake ref with an attribute (`path:/etc/remoter-agent#agentContainer`,
    /// resolved lazily via `nix build --no-link --print-out-paths` on the
    /// first nspawn run and content-cached in the host store afterwards) or a
    /// plain path to an already-built toplevel. Default assumes operators
    /// deploy a flake checkout at `/etc/remoter-agent` exposing the
    /// `agentContainer` output of this repo's flake.
    #[serde(default = "default_nspawn_closure_ref")]
    pub nspawn_closure_ref: String,
    /// Nspawn mode only: the first two octets of the per-run /30 pool used
    /// for the machine veth pair — run n gets `<base>.<4n>.0/30` (host `.1`,
    /// machine `.2`). Must not collide with any host network.
    #[serde(default = "default_nspawn_net_pool_base")]
    pub nspawn_net_pool_base: String,
    /// Nspawn mode only: the sudo binary for the whitelisted privileged
    /// operations (`systemd-nspawn`, `systemd-run -M`, `machinectl terminate`,
    /// `remoter-nspawnctl`). Default `sudo`.
    #[serde(default = "default_sudo_binary")]
    pub sudo_binary: String,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            mode: ExecutionMode::Host,
            docker_binary: default_docker_binary(),
            image_tag_prefix: default_image_tag_prefix(),
            postgres_image: default_postgres_image(),
            minio_image: default_minio_image(),
            mc_image: default_mc_image(),
            agent_home: None,
            nix_binary_cache_dir: None,
            nspawn_closure_ref: default_nspawn_closure_ref(),
            nspawn_net_pool_base: default_nspawn_net_pool_base(),
            sudo_binary: default_sudo_binary(),
        }
    }
}

impl ExecutionConfig {
    pub fn is_container(&self) -> bool {
        self.mode == ExecutionMode::Container
    }

    pub fn is_nspawn(&self) -> bool {
        self.mode == ExecutionMode::Nspawn
    }

    /// Isolated modes (container, nspawn) share the spec §5.8 exemption:
    /// postgres/minio live inside the per-run runtime, so runs get no
    /// `REMOTER_AGENT_PORT_BASE` and nothing host-side to sweep but the
    /// runtime itself.
    pub fn is_isolated(&self) -> bool {
        self.mode != ExecutionMode::Host
    }

    /// The effective agent home (default `<workspace_root>/agent-home`).
    pub fn agent_home(&self, workspace_root: &Path) -> PathBuf {
        self.agent_home
            .clone()
            .unwrap_or_else(|| workspace_root.join("agent-home"))
    }
}

fn default_docker_binary() -> String {
    "docker".to_string()
}
fn default_image_tag_prefix() -> String {
    "remoter-agent".to_string()
}
fn default_postgres_image() -> String {
    "postgres:17.6-alpine3.22".to_string()
}
fn default_minio_image() -> String {
    // Newest pinned release tag on Docker Hub at PoC-2 time.
    "minio/minio:RELEASE.2025-09-07T16-13-09Z".to_string()
}
fn default_mc_image() -> String {
    "minio/mc:RELEASE.2025-08-13T08-35-41Z".to_string()
}
fn default_nspawn_closure_ref() -> String {
    "path:/etc/remoter-agent#agentContainer".to_string()
}
fn default_nspawn_net_pool_base() -> String {
    "10.231".to_string()
}
fn default_sudo_binary() -> String {
    "sudo".to_string()
}

const CONFIG_PATH_ENV: &str = "REMOTER_AGENT_CONFIG";
const CONFIG_FILE_NAME: &str = "remoter-agent.toml";
const API_URL_ENV: &str = "REMOTER_API_URL";
const TOKEN_ENV: &str = "REMOTER_AGENT_TOKEN";
const WORKSPACE_ID_ENV: &str = "REMOTER_AGENT_WORKSPACE_ID";

#[derive(Debug, Clone)]
pub struct Config {
    /// Backend base URL, e.g. `http://localhost:8181`.
    pub api_url: String,
    /// The daemon's agent token (`REMOTER_AGENT_TOKEN`).
    pub token: String,
    /// Optional workspace to scope all backend requests to. When set, the
    /// client sends `X-Workspace-Id`. When absent, the daemon relies on the
    /// backend's single-membership fallback (spec §5.1).
    pub workspace_id: Option<i32>,
    pub poll_interval_secs: u64,
    /// WebSocket event stream (spec §4.5): push wake-ups on top of the poll.
    /// `false` = pure polling (fallback for proxies that break upgrades).
    pub events_enabled: bool,
    /// PR review feedback sync cadence (PROD-8, spec §4.4); `0` disables it.
    pub pr_sync_interval_secs: u64,
    /// The daemon's private scratch area on this host — the only place it may
    /// write (clones, worktrees, build state). See spec §5.1.
    pub workspace_root: PathBuf,
    /// Global cap on parallel runs across all projects/tickets.
    pub max_concurrent_runs: usize,
    /// Per-project cap; 0 = unlimited. Set 1 for projects whose devenv services
    /// use hardcoded ports (spec §5.8).
    pub max_concurrent_runs_per_project: usize,
    /// Hard cap per run attempt.
    pub run_timeout_minutes: u64,
    /// Attempts per claim before the ticket escalates out of the claimable
    /// queue (spec §5.7).
    pub max_attempts: u32,
    /// Idle limit for one ACP turn: no session updates, permission/elicitation
    /// callbacks, or agent stderr for this long marks the run stalled — the
    /// driver sends `session/cancel` and the attempt is retried without
    /// waiting for `run_timeout_minutes` (spec §5.7).
    pub stall_idle_secs: u64,
    /// Grace between the stall cancel and the hard kill of a still-wedged
    /// agent process (the ChildGuard's SIGTERM → SIGKILL escalation).
    pub stall_grace_secs: u64,
    /// Base delay between attempts of one claim (doubles per attempt).
    pub retry_backoff_secs: u64,
    /// Grace period after a cancel before a wedged run is hard-aborted
    /// (human cancellation or daemon shutdown, spec §5.7).
    pub cancel_grace_secs: u64,
    pub logs: LogsConfig,
    pub driver: DriverConfig,
    pub execution: ExecutionConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DriverConfig {
    /// `stub` = dry-run driver. `kimi-acp` and `opencode-acp` are real ACP
    /// drivers (spec §5.5).
    pub kind: String,
    /// Stub driver only: artificial run duration, for demos of parallel runs
    /// and cancellation.
    #[serde(default)]
    pub simulate_delay_ms: u64,
    /// Stub driver only: when true, the first implement invocation in an
    /// attempt leaves an untracked file behind; a resumed invocation commits
    /// it, exercising the dirty-worktree nudge loop in tests.
    #[serde(default)]
    pub simulate_uncommitted_changes: bool,
    /// ACP driver agent binary. Defaults to `kimi`, or `opencode` for
    /// `opencode-acp`…
    /// Container mode: reduced to a bare name at startup — the binary is
    /// spawned inside the run container and resolves against the image PATH.
    #[serde(default = "default_agent_program")]
    pub agent_program: String,
    /// …invoked with these args (default `["acp"]`).
    #[serde(default = "default_agent_args")]
    pub agent_args: Vec<String>,
    /// ACP drivers: the remoter MCP server binary injected into every
    /// session (default `remoter-mcp`). A bare name is resolved against the
    /// daemon's own PATH into an absolute path at startup (spec §5.6) — the
    /// MCP client spawns it from inside the project worktree shell, where a
    /// bare name would hit the *project's* PATH instead of the host's.
    /// An explicit path (any value containing `/`) is used as-is.
    #[serde(default = "default_remoter_mcp_command")]
    pub remoter_mcp_command: String,
    /// Per-kind ACP `session/set_config_option` values. `None` leaves the
    /// agent's default in place. Kept in `[driver]` because the values are
    /// agent-CLI-specific (spec §5.1).
    #[serde(default)]
    pub plan_model: Option<String>,
    #[serde(default)]
    pub plan_thinking: Option<String>,
    #[serde(default)]
    pub implement_model: Option<String>,
    #[serde(default)]
    pub implement_thinking: Option<String>,
    /// The supervisor run's model/thinking (#115): the supervise run reviews
    /// child diffs and reports, so it can use a different (usually stronger)
    /// model than the implementing runs.
    #[serde(default)]
    pub supervise_model: Option<String>,
    #[serde(default)]
    pub supervise_thinking: Option<String>,
    /// The standalone review run's model/thinking (#142): a human-requested
    /// agent review of a ticket's own work. Falls back to the supervise keys
    /// when unset — the two runs review code the same way.
    #[serde(default)]
    pub review_model: Option<String>,
    #[serde(default)]
    pub review_thinking: Option<String>,
    /// Kimi only: directory where kimi stores session state
    /// (`~/.kimi-code/sessions` by default). The driver reads per-session
    /// `wire.jsonl` from here when ACP does not report token usage directly.
    #[serde(default)]
    pub sessions_dir: Option<PathBuf>,
}

impl DriverConfig {
    /// ACP config options (`config_id` → `value`) to apply for the given run kind.
    pub fn config_options_for(&self, kind: &str) -> Vec<(String, String)> {
        let mut opts = Vec::new();
        let (model, thinking): (Option<&String>, Option<&String>) = match kind {
            "plan" => (self.plan_model.as_ref(), self.plan_thinking.as_ref()),
            "implement" => (self.implement_model.as_ref(), self.implement_thinking.as_ref()),
            "supervise" => (self.supervise_model.as_ref(), self.supervise_thinking.as_ref()),
            // Review falls back to the supervise keys when its own are unset
            // (#142) — both runs review code, so they share the default.
            "review" => (
                self.review_model.as_ref().or(self.supervise_model.as_ref()),
                self.review_thinking.as_ref().or(self.supervise_thinking.as_ref()),
            ),
            _ => (None, None),
        };
        if let Some(m) = model {
            opts.push(("model".to_string(), m.clone()));
        }
        if let Some(t) = thinking {
            // Kimi calls this setting `thinking`; OpenCode exposes the same
            // per-run control as ACP's `effort` option.
            let option_id = if self.kind == "opencode-acp" {
                "effort"
            } else {
                "thinking"
            };
            opts.push((option_id.to_string(), t.clone()));
        }
        opts
    }
}

/// Raw TOML shape; `Config` adds the env-sourced values on top.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct FileConfig {
    api_url: String,
    /// Optional — `REMOTER_AGENT_TOKEN` wins when both are set (spec §5.1).
    /// Keep the file owner-only (`chmod 600`) when this is present.
    #[serde(default)]
    token: Option<String>,
    /// Optional workspace id. `REMOTER_AGENT_WORKSPACE_ID` wins when both are set.
    #[serde(default)]
    workspace_id: Option<i32>,
    #[serde(default = "default_poll_interval")]
    poll_interval_secs: u64,
    #[serde(default = "default_events_enabled")]
    events_enabled: bool,
    #[serde(default = "default_pr_sync_interval")]
    pr_sync_interval_secs: u64,
    workspace_root: String,
    #[serde(default = "default_max_concurrent_runs")]
    max_concurrent_runs: usize,
    #[serde(default)]
    max_concurrent_runs_per_project: usize,
    #[serde(default = "default_run_timeout")]
    run_timeout_minutes: u64,
    #[serde(default = "default_max_attempts")]
    max_attempts: u32,
    #[serde(default = "default_stall_idle")]
    stall_idle_secs: u64,
    #[serde(default = "default_stall_grace")]
    stall_grace_secs: u64,
    #[serde(default = "default_retry_backoff")]
    retry_backoff_secs: u64,
    #[serde(default = "default_cancel_grace")]
    cancel_grace_secs: u64,
    #[serde(default)]
    logs: LogsConfig,
    driver: DriverConfig,
    #[serde(default)]
    execution: ExecutionConfig,
}

fn default_poll_interval() -> u64 {
    30
}
fn default_events_enabled() -> bool {
    true
}
fn default_pr_sync_interval() -> u64 {
    60
}
fn default_max_concurrent_runs() -> usize {
    4
}
fn default_run_timeout() -> u64 {
    60
}
fn default_max_attempts() -> u32 {
    3
}
fn default_stall_idle() -> u64 {
    300
}
fn default_stall_grace() -> u64 {
    30
}
fn default_retry_backoff() -> u64 {
    5
}
fn default_cancel_grace() -> u64 {
    120
}
fn default_agent_program() -> String {
    "kimi".to_string()
}
fn default_agent_args() -> Vec<String> {
    vec!["acp".to_string()]
}
fn default_remoter_mcp_command() -> String {
    "remoter-mcp".to_string()
}
fn default_sessions_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".kimi-code/sessions")
}

impl Config {
    /// Loads the config file and applies env overrides. Config file
    /// resolution order (spec §5.1): `REMOTER_AGENT_CONFIG` →
    /// `./remoter-agent.toml` → `$XDG_CONFIG_HOME/remoter-agent.toml` →
    /// `~/.config/remoter-agent.toml`. The token comes from
    /// `REMOTER_AGENT_TOKEN` or, failing that, the file's `token` key.
    pub fn load() -> anyhow::Result<Self> {
        let env_token = std::env::var(TOKEN_ENV).ok();
        if let Ok(path) = std::env::var(CONFIG_PATH_ENV) {
            return Self::from_file(&path, env_token);
        }
        let candidates = config_candidates(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        );
        for candidate in &candidates {
            if candidate.is_file() {
                return Self::from_file(&candidate.to_string_lossy(), env_token);
            }
        }
        anyhow::bail!(
            "no config file found — searched {} (or set {CONFIG_PATH_ENV})",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    pub fn from_file(path: &str, env_token: Option<String>) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
        let file: FileConfig = toml::from_str(&text)?;
        let token_from_file = env_token.is_none() && file.token.is_some();
        let cfg = Self::from_file_config(file, env_token)?;
        if token_from_file {
            warn_if_token_file_insecure(path);
        }
        Ok(cfg)
    }

    pub fn from_toml_str(text: &str, env_token: Option<String>) -> anyhow::Result<Self> {
        Self::from_file_config(toml::from_str(text)?, env_token)
    }

    fn from_file_config(file: FileConfig, env_token: Option<String>) -> anyhow::Result<Self> {
        // Env override wins over the file (spec §5.1) — for the token too.
        let api_url = std::env::var(API_URL_ENV).unwrap_or(file.api_url);
        let token = env_token.or(file.token).ok_or_else(|| {
            anyhow::anyhow!(
                "no agent token — set {TOKEN_ENV} or `token` in the config (create one via POST /users/agent)"
            )
        })?;
        let workspace_id = match std::env::var(WORKSPACE_ID_ENV) {
            Ok(s) if s.trim().is_empty() => file.workspace_id,
            Ok(s) => Some(
                s.parse::<i32>()
                    .map_err(|_| anyhow::anyhow!("{WORKSPACE_ID_ENV} must be an integer workspace id, got {s:?}"))?,
            ),
            Err(_) => file.workspace_id,
        };
        let mut driver = file.driver;
        // Keep the existing Kimi defaults while making the more specific
        // OpenCode kind useful without requiring redundant program settings.
        if driver.kind == "opencode-acp"
            && driver.agent_program == default_agent_program()
            && driver.agent_args == default_agent_args()
        {
            driver.agent_program = "opencode".to_string();
        }
        let container_mode = file.execution.mode == ExecutionMode::Container;
        // The stub driver never touches MCP — skip resolution so dry-run
        // setups don't need the binary on the host. Container mode reduces
        // both program values to bare names instead: the agent and
        // `remoter-mcp` are spawned *inside* the run container and resolve
        // against the image's PATH — a host-absolute path (e.g. a NixOS
        // `/run/current-system/sw/bin/kimi`) does not exist there.
        // Nspawn mode keeps host semantics: the machine bind-mounts the host
        // /nix/store read-only, so resolved absolute store paths work inside
        // (programs living outside /nix/store are the operator's problem —
        // the closure documents the requirement).
        if container_mode {
            driver.agent_program = bare_program_name(&driver.agent_program, "agent_program");
            driver.remoter_mcp_command = bare_program_name(&driver.remoter_mcp_command, "remoter_mcp_command");
        } else if driver.kind != "stub" {
            driver.remoter_mcp_command = resolve_mcp_command(&driver.remoter_mcp_command)?;
        }
        let workspace_root = expand_tilde(&file.workspace_root);
        let execution = ExecutionConfig {
            agent_home: file.execution.agent_home.as_deref().map(expand_tilde_path),
            nix_binary_cache_dir: file.execution.nix_binary_cache_dir.as_deref().map(expand_tilde_path),
            ..file.execution
        };
        let agent_home = execution.agent_home(&workspace_root);
        if driver.kind == "kimi-acp" {
            driver.sessions_dir = Some(
                driver
                    .sessions_dir
                    .as_deref()
                    .map(expand_tilde_path)
                    .unwrap_or_else(|| {
                        // Isolated modes mount the agent home at /root inside
                        // the runtime — the wire files land there, not in the
                        // host user's home.
                        if execution.is_isolated() {
                            agent_home.join(".kimi-code/sessions")
                        } else {
                            default_sessions_dir()
                        }
                    }),
            );
        } else {
            driver.sessions_dir = None;
        }
        let logs_dir = file
            .logs
            .dir
            .as_deref()
            .map(expand_tilde_path)
            .unwrap_or_else(|| workspace_root.join("logs"));
        Ok(Self {
            api_url: api_url.trim_end_matches('/').to_string(),
            token,
            workspace_id,
            poll_interval_secs: file.poll_interval_secs,
            events_enabled: file.events_enabled,
            pr_sync_interval_secs: file.pr_sync_interval_secs,
            workspace_root,
            max_concurrent_runs: file.max_concurrent_runs,
            max_concurrent_runs_per_project: file.max_concurrent_runs_per_project,
            run_timeout_minutes: file.run_timeout_minutes,
            max_attempts: file.max_attempts.max(1),
            stall_idle_secs: file.stall_idle_secs.max(1),
            stall_grace_secs: file.stall_grace_secs,
            retry_backoff_secs: file.retry_backoff_secs,
            cancel_grace_secs: file.cancel_grace_secs,
            logs: LogsConfig {
                dir: Some(logs_dir),
                retention_days: file.logs.retention_days,
            },
            driver,
            execution,
        })
    }

    /// Startup validation for the isolated execution modes (spec: `docker
    /// info` must succeed for `container` — a daemon that cannot reach docker
    /// cannot honor `mode = "container"`, and silently degrading to host mode
    /// would strip the requested isolation; nspawn checks its own toolchain
    /// likewise). Cheap and synchronous; called once from `main`.
    pub fn validate_execution(&self) -> anyhow::Result<()> {
        match self.execution.mode {
            ExecutionMode::Host => return Ok(()),
            ExecutionMode::Container => self.validate_container_runtime()?,
            ExecutionMode::Nspawn => self.validate_nspawn_runtime()?,
        }
        self.validate_agent_home()
    }

    fn validate_container_runtime(&self) -> anyhow::Result<()> {
        let docker = &self.execution.docker_binary;
        let out = std::process::Command::new(docker)
            .arg("info")
            .output()
            .map_err(|e| anyhow::anyhow!("execution.mode = \"container\" but `{docker} info` could not run: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "execution.mode = \"container\" but `{docker} info` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Nspawn mode (NixOS hosts only): the nspawn/machinectl/systemd-run
    /// toolchain on PATH, systemd ≥ 256 (`systemd-run -M --pipe` is the exec
    /// path), and the privileged helper reachable via non-interactive sudo.
    fn validate_nspawn_runtime(&self) -> anyhow::Result<()> {
        let paths: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        check_nspawn_binaries(paths.iter().cloned())?;
        // Presence was just checked; the resolved path pins the binary the
        // version check runs (a bare name would re-resolve identically).
        let systemd_run = resolve_in_paths("systemd-run", paths).unwrap_or_else(|| "systemd-run".to_string());
        check_systemd_version(&systemd_run)?;
        check_nspawn_helper(&self.execution.sudo_binary)
    }

    /// Shared isolated-mode tail: the agent home is mounted at `/root` inside
    /// the runtime, so the kimi sessions dir is created up-front and a missing
    /// rendered config warns (the project's `.remoter/kimi-config.toml` is
    /// rendered at runtime start with KIMI_API_KEY from the daemon's
    /// environment).
    fn validate_agent_home(&self) -> anyhow::Result<()> {
        let agent_home = self.execution.agent_home(&self.workspace_root);
        if self.driver.kind == "kimi-acp" {
            std::fs::create_dir_all(agent_home.join(".kimi-code/sessions"))
                .map_err(|e| anyhow::anyhow!("cannot create agent home {}: {e}", agent_home.display()))?;
        }
        if self.driver.kind == "kimi-acp" && !agent_home.join(".kimi-code/config.toml").is_file() {
            tracing::warn!(
                "{} mode: {} does not exist — the agent's auth will fail unless the project's \
                 .remoter/kimi-config.toml is in place (rendered at start with KIMI_API_KEY \
                 from the daemon's environment)",
                self.execution.mode.as_str(),
                agent_home.join(".kimi-code/config.toml").display()
            );
        }
        Ok(())
    }
}

/// Nspawn validation, split for tests: the nspawn toolchain on the daemon's
/// PATH (nspawn mode requires a systemd host — NixOS).
fn check_nspawn_binaries(paths: impl IntoIterator<Item = PathBuf> + Clone) -> anyhow::Result<()> {
    for bin in ["systemd-nspawn", "machinectl", "systemd-run"] {
        if resolve_in_paths(bin, paths.clone()).is_none() {
            anyhow::bail!(
                "execution.mode = \"nspawn\" but `{bin}` is not on the daemon's PATH \
                 (nspawn mode requires a systemd host — NixOS)"
            );
        }
    }
    Ok(())
}

/// Nspawn validation: `systemd-run -M --pipe` (the per-turn exec path) needs
/// systemd ≥ 256.
fn check_systemd_version(systemd_run: &str) -> anyhow::Result<()> {
    let out = std::process::Command::new(systemd_run)
        .arg("--version")
        .output()
        .map_err(|e| anyhow::anyhow!("execution.mode = \"nspawn\" but `systemd-run --version` could not run: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match crate::nspawn::parse_systemd_version(&text) {
        Some(v) if v >= crate::nspawn::MIN_SYSTEMD_VERSION => Ok(()),
        Some(v) => anyhow::bail!(
            "execution.mode = \"nspawn\" requires systemd >= {} (`systemd-run -M --pipe`), found {v}",
            crate::nspawn::MIN_SYSTEMD_VERSION
        ),
        None => anyhow::bail!(
            "execution.mode = \"nspawn\": could not parse `systemd-run --version` output: {}",
            text.lines().next().unwrap_or("").trim()
        ),
    }
}

/// Nspawn validation: the privileged helper must be reachable via
/// non-interactive sudo (the sudoers rule the operator docs ship).
fn check_nspawn_helper(sudo: &str) -> anyhow::Result<()> {
    let helper = crate::nspawn::HELPER_BINARY;
    let out = std::process::Command::new(sudo)
        .args(["-n", helper, "check"])
        .output()
        .map_err(|e| {
            anyhow::anyhow!("execution.mode = \"nspawn\" but `{sudo} -n {helper} check` could not run: {e}")
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "execution.mode = \"nspawn\" but `{sudo} -n {helper} check` failed: {} — \
             install the helper root-owned and whitelist it in sudoers (see README)",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The token is a secret: a config file carrying it must be owner-only.
#[cfg(unix)]
fn warn_if_token_file_insecure(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            path,
            "config file contains the agent token but is readable by group/others — chmod 600 it"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_token_file_insecure(_path: &str) {}

/// Container mode: programs run *inside* the container and must resolve
/// against the image's PATH, so only a bare name works there. A configured
/// absolute path almost always points into the host filesystem (e.g. a NixOS
/// `/run/current-system/sw/bin/kimi`) and would fail with "No such file or
/// directory" in the container — reduce it to its file name and warn.
fn bare_program_name(cmd: &str, key: &str) -> String {
    if !cmd.contains('/') {
        return cmd.to_string();
    }
    let bare = Path::new(cmd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cmd.to_string());
    tracing::warn!(
        "container mode: {key} = {cmd:?} is a host path; using bare name {bare:?} \
         (resolved against the image PATH inside the container)"
    );
    bare
}

/// Resolves a bare MCP server command name against the daemon's own PATH into
/// an absolute path (spec §5.6). Explicit paths pass through unchanged.
fn resolve_mcp_command(cmd: &str) -> anyhow::Result<String> {
    if cmd.contains('/') {
        return Ok(cmd.to_string());
    }
    let paths = std::env::var_os("PATH").unwrap_or_default();
    resolve_in_paths(cmd, std::env::split_paths(&paths)).ok_or_else(|| {
        anyhow::anyhow!(
            "`{cmd}` not found in the daemon's PATH — install the remoter flake packages on the host \
             or set driver.remoter_mcp_command to an absolute path (spec §5.6)"
        )
    })
}

fn resolve_in_paths(cmd: &str, paths: impl IntoIterator<Item = PathBuf>) -> Option<String> {
    paths
        .into_iter()
        .map(|dir| dir.join(cmd))
        .find(|p| is_executable(p))
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Candidate config locations in search order: the working directory first
/// (dev convenience), then the XDG config home (defaults to `~/.config`).
fn config_candidates(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from(format!("./{CONFIG_FILE_NAME}"))];
    let config_home = xdg_config_home.or_else(|| home.map(|h| h.join(".config")));
    if let Some(dir) = config_home {
        candidates.push(dir.join(CONFIG_FILE_NAME));
    }
    candidates
}

/// Expands a leading `~/` against `$HOME` (no extra dependency for one path).
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn expand_tilde_path(path: &std::path::Path) -> PathBuf {
    if let Some(s) = path.to_str()
        && let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = r#"
api_url = "http://localhost:8181"
workspace_root = "~/.local/share/remoter-agent"

[driver]
kind = "stub"
"#;

    #[test]
    fn execution_defaults_to_host_mode() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let cfg = Config::from_toml_str(TOML, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.execution.mode, ExecutionMode::Host);
        assert!(!cfg.execution.is_container());
        assert_eq!(cfg.execution.docker_binary, "docker");
        assert_eq!(
            cfg.execution.agent_home(&cfg.workspace_root),
            cfg.workspace_root.join("agent-home")
        );
    }

    #[test]
    fn container_mode_keeps_mcp_command_bare_and_points_sessions_at_agent_home() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            r#"[driver]
kind = "kimi-acp"

[execution]
mode = "container"
postgres_image = "postgres:17.6-alpine3.22""#,
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert!(cfg.execution.is_container());
        // Not resolved against the host PATH — the image's PATH resolves it.
        assert_eq!(cfg.driver.remoter_mcp_command, "remoter-mcp");
        assert_eq!(
            cfg.driver.sessions_dir.as_deref(),
            Some(
                cfg.execution
                    .agent_home(&cfg.workspace_root)
                    .join(".kimi-code/sessions")
                    .as_path()
            )
        );
        assert_eq!(cfg.execution.postgres_image, "postgres:17.6-alpine3.22");
        // Pinned defaults for the rest.
        assert!(cfg.execution.minio_image.starts_with("minio/minio:RELEASE."));
        assert!(cfg.execution.mc_image.starts_with("minio/mc:RELEASE."));
    }

    #[test]
    fn container_mode_reduces_host_absolute_program_paths_to_bare_names() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            r#"[driver]
kind = "kimi-acp"
agent_program = "/run/current-system/sw/bin/kimi"
remoter_mcp_command = "/nix/store/abc123-remoter-mcp/bin/remoter-mcp"

[execution]
mode = "container""#,
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert!(cfg.execution.is_container());
        // Host-absolute paths do not exist inside the image — the binaries
        // there resolve against the image PATH by bare name.
        assert_eq!(cfg.driver.agent_program, "kimi");
        assert_eq!(cfg.driver.remoter_mcp_command, "remoter-mcp");
    }

    #[test]
    fn unknown_execution_mode_is_a_config_error() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace("[driver]", "[execution]\nmode = \"sidecar\"\n\n[driver]");
        assert!(Config::from_toml_str(&toml, Some("tok".to_string())).is_err());
    }

    #[test]
    fn nspawn_mode_parses_with_defaults() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace("[driver]", "[execution]\nmode = \"nspawn\"\n\n[driver]");
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.execution.mode, ExecutionMode::Nspawn);
        assert!(cfg.execution.is_nspawn());
        assert!(cfg.execution.is_isolated());
        assert!(!cfg.execution.is_container());
        assert_eq!(
            cfg.execution.nspawn_closure_ref,
            "path:/etc/remoter-agent#agentContainer"
        );
        assert_eq!(cfg.execution.nspawn_net_pool_base, "10.231");
        assert_eq!(cfg.execution.sudo_binary, "sudo");
    }

    #[test]
    fn nspawn_mode_parses_custom_values() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]",
            "[execution]\nmode = \"nspawn\"\nnspawn_closure_ref = \"/nix/store/abc-agent-container\"\n\
             nspawn_net_pool_base = \"10.99\"\nsudo_binary = \"/usr/bin/sudo\"\n\n[driver]",
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.execution.nspawn_closure_ref, "/nix/store/abc-agent-container");
        assert_eq!(cfg.execution.nspawn_net_pool_base, "10.99");
        assert_eq!(cfg.execution.sudo_binary, "/usr/bin/sudo");
    }

    /// Nspawn keeps host-style absolute program paths (the host /nix/store is
    /// bind-mounted into the machine) — unlike container mode, which reduces
    /// them to bare names.
    #[test]
    fn nspawn_mode_keeps_absolute_program_paths() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            r#"[driver]
kind = "kimi-acp"
agent_program = "/nix/store/abc123-kimi/bin/kimi"
remoter_mcp_command = "/nix/store/abc123-remoter-mcp/bin/remoter-mcp"

[execution]
mode = "nspawn""#,
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert!(cfg.execution.is_nspawn());
        assert_eq!(cfg.driver.agent_program, "/nix/store/abc123-kimi/bin/kimi");
        assert_eq!(
            cfg.driver.remoter_mcp_command,
            "/nix/store/abc123-remoter-mcp/bin/remoter-mcp"
        );
        // Sessions land in the shared agent home (mounted at /root inside the
        // machine), like container mode.
        assert_eq!(
            cfg.driver.sessions_dir.as_deref(),
            Some(
                cfg.execution
                    .agent_home(&cfg.workspace_root)
                    .join(".kimi-code/sessions")
                    .as_path()
            )
        );
    }

    /// Nspawn validation, piecewise (no PATH mutation — parallel test threads
    /// share the process environment): toolchain presence against a stub dir,
    /// the systemd ≥ 256 version gate against a stub `systemd-run`, and the
    /// helper check against a stub `sudo`.
    #[test]
    fn nspawn_validation_checks_toolchain_version_and_helper() {
        let dir = std::env::temp_dir().join(format!("remoter-nspawn-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for bin in ["systemd-nspawn", "machinectl", "systemd-run", "sudo"] {
            crate::testutil::write_executable_script(&dir.join(bin), "#!/bin/sh\nexit 0\n");
        }

        // Toolchain present in the stub dir, missing from an empty one.
        check_nspawn_binaries(vec![dir.clone()]).unwrap();
        let err = check_nspawn_binaries(vec![dir.join("empty")]).unwrap_err().to_string();
        assert!(err.contains("systemd-nspawn"), "{err}");

        // systemd 257 passes the version gate; 254 fails closed.
        crate::testutil::write_executable_script(
            &dir.join("systemd-run"),
            "#!/bin/sh\necho 'systemd 257 (257.9)'\nexit 0\n",
        );
        check_systemd_version(dir.join("systemd-run").to_str().unwrap()).unwrap();
        crate::testutil::write_executable_script(
            &dir.join("systemd-run"),
            "#!/bin/sh\necho 'systemd 254 (254.22)'\nexit 0\n",
        );
        let err = check_systemd_version(dir.join("systemd-run").to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("systemd >= 256"), "{err}");

        // `sudo -n remoter-nspawnctl check` succeeding/failing gates startup.
        check_nspawn_helper(dir.join("sudo").to_str().unwrap()).unwrap();
        crate::testutil::write_executable_script(
            &dir.join("sudo"),
            "#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n",
        );
        let err = check_nspawn_helper(dir.join("sudo").to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("remoter-nspawnctl check"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nix_binary_cache_dir_is_optional_and_tilde_expanded() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let cfg = Config::from_toml_str(TOML, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.execution.nix_binary_cache_dir, None);

        let toml = TOML.replace(
            "[driver]",
            "[execution]\nnix_binary_cache_dir = \"/var/cache/remoter-nix\"\n\n[driver]",
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(
            cfg.execution.nix_binary_cache_dir.as_deref(),
            Some(std::path::Path::new("/var/cache/remoter-nix"))
        );

        let toml = TOML.replace(
            "[driver]",
            "[execution]\nnix_binary_cache_dir = \"~/nix-cache\"\n\n[driver]",
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                cfg.execution.nix_binary_cache_dir.as_deref(),
                Some(std::path::Path::new(&home).join("nix-cache").as_path())
            );
        }
    }

    #[test]
    fn execution_section_parses_custom_values() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]",
            r#"[execution]
mode = "container"
docker_binary = "/usr/local/bin/docker"
image_tag_prefix = "acme/agent"
postgres_image = "postgres:16-alpine"
minio_image = "minio/minio:RELEASE.2025-01-01T00-00-00Z"
mc_image = "minio/mc:RELEASE.2025-01-01T00-00-00Z"
agent_home = "/var/lib/remoter-agent-home"

[driver]"#,
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert!(cfg.execution.is_container());
        assert_eq!(cfg.execution.docker_binary, "/usr/local/bin/docker");
        assert_eq!(cfg.execution.image_tag_prefix, "acme/agent");
        assert_eq!(cfg.execution.postgres_image, "postgres:16-alpine");
        assert_eq!(cfg.execution.minio_image, "minio/minio:RELEASE.2025-01-01T00-00-00Z");
        assert_eq!(cfg.execution.mc_image, "minio/mc:RELEASE.2025-01-01T00-00-00Z");
        assert_eq!(
            cfg.execution.agent_home(&cfg.workspace_root),
            std::path::PathBuf::from("/var/lib/remoter-agent-home")
        );
    }

    #[test]
    fn parses_minimal_config_with_defaults() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let cfg = Config::from_toml_str(TOML, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.api_url, "http://localhost:8181");
        assert_eq!(cfg.token, "tok");
        assert_eq!(cfg.poll_interval_secs, 30);
        assert!(cfg.events_enabled);
        assert_eq!(cfg.pr_sync_interval_secs, 60);
        assert_eq!(cfg.max_concurrent_runs, 4);
        assert_eq!(cfg.max_concurrent_runs_per_project, 0);
        assert_eq!(cfg.run_timeout_minutes, 60);
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.cancel_grace_secs, 120);
        assert_eq!(cfg.stall_idle_secs, 300);
        assert_eq!(cfg.stall_grace_secs, 30);
        assert!(cfg.workspace_root.ends_with(".local/share/remoter-agent"));
        assert!(!cfg.workspace_root.starts_with("~"));
        assert_eq!(cfg.workspace_id, None);
    }

    #[test]
    fn driver_config_options_parsed_and_mapped_by_kind() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            r#"[driver]
kind = "kimi-acp"
remoter_mcp_command = "/opt/remoter/remoter-mcp"
plan_model = "kimi-code/k3"
plan_thinking = "max"
implement_model = "kimi-code/kimi-for-coding"
implement_thinking = "high"
supervise_model = "kimi-code/k3"
supervise_thinking = "max"
review_model = "kimi-code/k3-reasoning"
review_thinking = "max""#,
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.driver.plan_model.as_deref(), Some("kimi-code/k3"));
        assert_eq!(cfg.driver.plan_thinking.as_deref(), Some("max"));
        assert_eq!(cfg.driver.implement_model.as_deref(), Some("kimi-code/kimi-for-coding"));
        assert_eq!(cfg.driver.implement_thinking.as_deref(), Some("high"));
        assert_eq!(cfg.driver.supervise_model.as_deref(), Some("kimi-code/k3"));
        assert_eq!(cfg.driver.supervise_thinking.as_deref(), Some("max"));
        assert_eq!(cfg.driver.review_model.as_deref(), Some("kimi-code/k3-reasoning"));
        assert_eq!(cfg.driver.review_thinking.as_deref(), Some("max"));

        assert_eq!(
            cfg.driver.config_options_for("plan"),
            vec![
                ("model".to_string(), "kimi-code/k3".to_string()),
                ("thinking".to_string(), "max".to_string()),
            ]
        );
        assert_eq!(
            cfg.driver.config_options_for("implement"),
            vec![
                ("model".to_string(), "kimi-code/kimi-for-coding".to_string()),
                ("thinking".to_string(), "high".to_string()),
            ]
        );
        assert_eq!(
            cfg.driver.config_options_for("supervise"),
            vec![
                ("model".to_string(), "kimi-code/k3".to_string()),
                ("thinking".to_string(), "max".to_string()),
            ]
        );
        assert_eq!(
            cfg.driver.config_options_for("review"),
            vec![
                ("model".to_string(), "kimi-code/k3-reasoning".to_string()),
                ("thinking".to_string(), "max".to_string()),
            ]
        );
        assert!(cfg.driver.config_options_for("unknown").is_empty());
    }

    /// The review kind falls back to the supervise keys when its own
    /// `review_model`/`review_thinking` are unset (#142); with neither set it
    /// yields no options, like the other kinds.
    #[test]
    fn review_config_options_fall_back_to_supervise() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            r#"[driver]
kind = "kimi-acp"
remoter_mcp_command = "/opt/remoter/remoter-mcp"
supervise_model = "kimi-code/k3"
supervise_thinking = "max""#,
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(
            cfg.driver.config_options_for("review"),
            vec![
                ("model".to_string(), "kimi-code/k3".to_string()),
                ("thinking".to_string(), "max".to_string()),
            ]
        );
    }

    /// An explicit review key wins over the supervise fallback per-field:
    /// `review_model` set, `review_thinking` unset → model from review,
    /// thinking from supervise.
    #[test]
    fn review_config_options_mix_explicit_and_fallback() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            r#"[driver]
kind = "kimi-acp"
remoter_mcp_command = "/opt/remoter/remoter-mcp"
supervise_model = "kimi-code/k3"
supervise_thinking = "max"
review_model = "kimi-code/k3-reasoning""#,
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(
            cfg.driver.config_options_for("review"),
            vec![
                ("model".to_string(), "kimi-code/k3-reasoning".to_string()),
                ("thinking".to_string(), "max".to_string()),
            ]
        );
    }

    #[test]
    fn empty_model_config_yields_no_config_options() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let cfg = Config::from_toml_str(TOML, Some("tok".to_string())).unwrap();
        assert!(cfg.driver.config_options_for("plan").is_empty());
        assert!(cfg.driver.config_options_for("implement").is_empty());
        assert!(cfg.driver.config_options_for("supervise").is_empty());
        assert!(cfg.driver.config_options_for("review").is_empty());
    }

    #[test]
    fn stall_knobs_parse_explicit_values_and_clamp_zero_idle() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "workspace_root",
            "stall_idle_secs = 120\nstall_grace_secs = 5\nworkspace_root",
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.stall_idle_secs, 120);
        assert_eq!(cfg.stall_grace_secs, 5);
        // A zero idle limit would stall every run instantly — clamped to 1s.
        let zero = Config::from_toml_str(
            &TOML.replace("workspace_root", "stall_idle_secs = 0\nworkspace_root"),
            Some("tok".to_string()),
        )
        .unwrap();
        assert_eq!(zero.stall_idle_secs, 1);
    }

    #[test]
    fn pr_sync_interval_explicit_and_disabled() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let cfg = Config::from_toml_str(
            &TOML.replace("workspace_root", "pr_sync_interval_secs = 120\nworkspace_root"),
            Some("tok".to_string()),
        )
        .unwrap();
        assert_eq!(cfg.pr_sync_interval_secs, 120);
        let disabled = Config::from_toml_str(
            &TOML.replace("workspace_root", "pr_sync_interval_secs = 0\nworkspace_root"),
            Some("tok".to_string()),
        )
        .unwrap();
        assert_eq!(disabled.pr_sync_interval_secs, 0);
    }

    #[test]
    fn tilde_expansion() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = home {
            assert_eq!(expand_tilde("~/x/y"), home.join("x/y"));
        }
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("rel/path"), PathBuf::from("rel/path"));
    }

    #[test]
    fn mcp_command_resolves_against_daemon_path() {
        let dir = std::env::temp_dir().join(format!("remoter-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("remoter-mcp");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            resolve_in_paths("remoter-mcp", vec![dir.clone()]).as_deref(),
            Some(bin.to_string_lossy().as_ref())
        );
        // Non-executable and missing files are not picked up.
        assert!(resolve_in_paths("remoter-mcp-missing", vec![dir.clone()]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_command_explicit_path_passes_through() {
        assert_eq!(
            resolve_mcp_command("/opt/remoter/remoter-mcp").unwrap(),
            "/opt/remoter/remoter-mcp"
        );
    }

    #[test]
    fn stub_driver_skips_mcp_resolution() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let cfg = Config::from_toml_str(TOML, Some("tok".to_string())).unwrap();
        // Bare name stays unresolved — the stub driver never spawns MCP.
        assert_eq!(cfg.driver.remoter_mcp_command, "remoter-mcp");
    }

    #[test]
    fn opencode_driver_uses_opencode_defaults_without_kimi_sessions() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            "[driver]\nkind = \"opencode-acp\"\nremoter_mcp_command = \"/opt/remoter-mcp\"",
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(cfg.driver.agent_program, "opencode");
        assert_eq!(cfg.driver.agent_args, vec!["acp"]);
        assert_eq!(cfg.driver.sessions_dir, None);
    }

    #[test]
    fn opencode_driver_maps_thinking_settings_to_effort() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace(
            "[driver]\nkind = \"stub\"",
            "[driver]\nkind = \"opencode-acp\"\nremoter_mcp_command = \"/opt/remoter-mcp\"\nplan_thinking = \"high\"",
        );
        let cfg = Config::from_toml_str(&toml, Some("tok".to_string())).unwrap();
        assert_eq!(
            cfg.driver.config_options_for("plan"),
            vec![("effort".to_string(), "high".to_string())]
        );
    }

    #[test]
    fn token_from_file_when_env_absent() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace("api_url", "token = \"file-tok\"\napi_url");
        let cfg = Config::from_toml_str(&toml, None).unwrap();
        assert_eq!(cfg.token, "file-tok");
    }

    #[test]
    fn env_token_wins_over_file() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let toml = TOML.replace("api_url", "token = \"file-tok\"\napi_url");
        let cfg = Config::from_toml_str(&toml, Some("env-tok".to_string())).unwrap();
        assert_eq!(cfg.token, "env-tok");
    }

    #[test]
    fn missing_token_everywhere_is_an_error() {
        // SAFETY: single-threaded test process env tweak; removed immediately.
        unsafe { std::env::remove_var(API_URL_ENV) };
        let err = Config::from_toml_str(TOML, None).unwrap_err().to_string();
        assert!(err.contains("REMOTER_AGENT_TOKEN"), "{err}");
    }

    #[test]
    fn config_candidates_search_order() {
        let c = config_candidates(Some(PathBuf::from("/xdg")), Some(PathBuf::from("/home")));
        assert_eq!(
            c,
            vec![
                PathBuf::from("./remoter-agent.toml"),
                PathBuf::from("/xdg/remoter-agent.toml")
            ]
        );
        // No XDG_CONFIG_HOME → falls back to ~/.config; no HOME at all → cwd only.
        let c = config_candidates(None, Some(PathBuf::from("/home")));
        assert_eq!(c[1], PathBuf::from("/home/.config/remoter-agent.toml"));
        assert_eq!(config_candidates(None, None).len(), 1);
    }
}
