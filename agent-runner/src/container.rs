//! Per-run OCI containers (docs/specs/remoter-agent-containers.md, PoC-2):
//! one run container per attempt (the project's agent image, see `image.rs`),
//! plus postgres and minio sidecars sharing the run container's network
//! namespace — inside, Postgres is always `127.0.0.1:5432` and MinIO
//! `127.0.0.1:9000`, so the §5.8 port-block contract does not apply.
//!
//! Lifecycle: [`start`] brings everything up (readiness + provisioning) and
//! returns a [`RunContainers`] guard; the guard's teardown (`docker rm -f` +
//! `git worktree unlock`) runs on explicit [`RunContainers::teardown`] or on
//! drop — a cancelled/timed-out run future dropping its guard is what bounds
//! leaked containers to `cancel_grace_secs`. A daemon crash can still leak
//! labeled containers; [`sweep`] reaps them at startup.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ExecutionConfig;
use crate::driver::DriverError;
use crate::workspace::RefMount;

/// Docker label marking every container a run owns (run + sidecars).
pub const RUN_ID_LABEL: &str = "remoter.run_id";
/// Docker label marking the project a container/image belongs to.
pub const PROJECT_ID_LABEL: &str = "remoter.project_id";
/// Docker label recording the ticket that created a container (staging
/// environments, image-init containers) — provenance for operators.
pub const TASK_ID_LABEL: &str = "remoter.task_id";
/// Docker label marking an image-init container (`image.rs`) so the startup
/// [`sweep`] can reap orphans a dropped build future or crashed daemon left
/// behind.
pub const IMAGE_INIT_LABEL: &str = "remoter.image_init";

/// The worktree mount point inside the run container.
pub const WORK_DIR: &str = "/work";

/// How long to wait for the postgres sidecar to accept connections — the
/// same budget `devenv processes wait --timeout 120` gives host mode.
const PG_READY_TIMEOUT: Duration = Duration::from_secs(120);
/// MinIO provisioning retry budget (the server needs a few seconds to come up).
const MINIO_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Databases provisioned on the postgres sidecar — mirrors devenv.nix
/// `initialDatabases` (remoter, remoter-test, remoter_e2e).
const PROVISION_DATABASES: [&str; 3] = ["remoter", "remoter-test", "remoter_e2e"];
/// Bucket provisioned on the minio sidecar — mirrors devenv.nix
/// `services.minio.buckets` (docs/specs/attachments.md).
const PROVISION_BUCKET: &str = "remoter-attachments";

/// The run container's name — the `docker exec` target for agent commands.
pub fn run_container_name(run_id: i32) -> String {
    format!("remoter-run-{run_id}")
}

/// The container-mode env contract (containers spec §3.4): no
/// `REMOTER_AGENT_PORT_BASE` (sidecars share the run container's network
/// namespace, so Postgres is always `127.0.0.1:5432` and MinIO
/// `127.0.0.1:9000`); `REMOTER_CONTAINER=1` tells project tooling (justfile,
/// Playwright config) it runs inside a container.
pub fn container_env(task_id: i32) -> Vec<(String, String)> {
    let db = |name: &str| format!("postgres://postgres:postgres@127.0.0.1:5432/{name}");
    vec![
        ("CI".to_string(), "1".to_string()),
        ("REMOTER_CONTAINER".to_string(), "1".to_string()),
        ("REMOTER_AGENT_TASK_ID".to_string(), task_id.to_string()),
        ("DATABASE_URL".to_string(), db("remoter")),
        ("LOCAL_DATABASE_URL".to_string(), db("remoter")),
        ("TEST_DATABASE_URL".to_string(), db("remoter-test")),
        ("E2E_DATABASE_URL".to_string(), db("remoter_e2e")),
    ]
}

/// kimi auth for run containers (spec §3.1): the project's
/// `.remoter/kimi-config.toml` is rendered into
/// `<agent_home>/.kimi-code/config.toml` (the container's `$HOME`) at every
/// container start. `${KIMI_API_KEY}` in the template is substituted from the
/// daemon's own environment — the secret never touches the image. Overwriting
/// every start keeps key rotation in sync and undoes kimi's own rewrites.
/// No template → no-op (projects without one are unaffected).
pub fn provision_kimi_config(repo: &Path, agent_home: &Path) -> Result<(), String> {
    let key = std::env::var("KIMI_API_KEY").ok().filter(|k| !k.is_empty());
    provision_kimi_config_with(repo, agent_home, key.as_deref())
}

fn provision_kimi_config_with(repo: &Path, agent_home: &Path, api_key: Option<&str>) -> Result<(), String> {
    const PLACEHOLDER: &str = "${KIMI_API_KEY}";
    let template = repo.join(".remoter/kimi-config.toml");
    let Ok(text) = std::fs::read_to_string(&template) else {
        return Ok(());
    };
    let rendered = if text.contains(PLACEHOLDER) {
        let key = api_key.ok_or_else(|| {
            format!(
                "{} contains {PLACEHOLDER} but KIMI_API_KEY is not set in the daemon's environment",
                template.display()
            )
        })?;
        text.replace(PLACEHOLDER, key)
    } else {
        text
    };
    let dir = agent_home.join(".kimi-code");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let target = dir.join("config.toml");
    let tmp = dir.join(".config.toml.tmp");
    std::fs::write(&tmp, rendered).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot chmod {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &target).map_err(|e| format!("cannot install {}: {e}", target.display()))?;
    Ok(())
}

/// Everything [`start`] needs to bring up one run's containers.
pub struct ContainerSpec<'a> {
    pub cfg: &'a ExecutionConfig,
    /// Configured ACP driver kind; only Kimi needs config-file provisioning.
    pub driver_kind: &'a str,
    /// The backend `agent_runs` row id — names and labels every container.
    pub run_id: i32,
    pub project_id: i32,
    /// The project agent image tag (from `image::ensure_project_image`).
    pub image: &'a str,
    /// Host directory mounted at `/work` (the ticket worktree, or the central
    /// clone for a supervise run without a parent worktree).
    pub worktree: &'a Path,
    /// The central clone (`p<id>/repo`); its `.git` is mounted at the same
    /// absolute path inside the container so worktree gitdir files resolve.
    pub repo: &'a Path,
    /// Mounted at `/root` (agent config + session state survive runs here).
    pub agent_home: &'a Path,
    /// Env for the run container (`CI`, `REMOTER_CONTAINER=1`, DB URLs, …).
    pub env: &'a [(String, String)],
    /// Reference repos prepared for this run (docs/specs/cross-repo-projects.md)
    /// — each is bind-mounted read-only at `/work/.refs/<mount_name>`.
    pub refs: &'a [RefMount],
}

/// The live containers of one run. Teardown is guaranteed: `teardown()` on
/// the happy path, `Drop` (sync `docker rm -f`) on cancellation/timeout.
pub struct RunContainers {
    docker: String,
    run_name: String,
    sidecars: Vec<String>,
    worktree: PathBuf,
    repo: PathBuf,
}

impl RunContainers {
    /// The run container's name — the `docker exec` target for agent commands.
    pub fn run_container(&self) -> &str {
        &self.run_name
    }

    /// `docker rm -f` every container of the run and unlock the worktree.
    /// Idempotent and best-effort per container — a partial teardown still
    /// attempts the rest.
    pub async fn teardown(mut self) {
        let names = std::mem::take(&mut self.sidecars);
        for name in names {
            docker_fire_and_forget(&self.docker, &["rm", "-f", &name]).await;
        }
        let run = std::mem::take(&mut self.run_name);
        if !run.is_empty() {
            docker_fire_and_forget(&self.docker, &["rm", "-f", &run]).await;
        }
        self.unlock_worktree().await;
    }

    /// Best-effort `git worktree unlock` — the lock is best-effort too.
    async fn unlock_worktree(&self) {
        if self.repo.as_os_str().is_empty() {
            return;
        }
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "unlock"])
            .arg(&self.worktree)
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                tracing::debug!(
                    worktree = %self.worktree.display(),
                    "git worktree unlock: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => tracing::debug!(error = %e, "git worktree unlock failed to spawn"),
        }
    }
}

impl Drop for RunContainers {
    /// Cancellation path: the run future is dropped (timeout, human cancel,
    /// daemon shutdown within `cancel_grace_secs`). `docker rm -f` is fast —
    /// a synchronous call here is what keeps `docker ps -a` clean without an
    /// async runtime to drive teardown.
    fn drop(&mut self) {
        if self.run_name.is_empty() && self.sidecars.is_empty() {
            return;
        }
        let mut names = std::mem::take(&mut self.sidecars);
        if !self.run_name.is_empty() {
            names.push(std::mem::take(&mut self.run_name));
        }
        for name in names {
            let out = std::process::Command::new(&self.docker)
                .args(["rm", "-f", &name])
                .output();
            if let Err(e) = out {
                tracing::warn!(container = %name, error = %e, "drop teardown: docker rm failed to spawn");
            }
        }
        // Sync unlock — mirrors the async one; failures are harmless (a stale
        // lock only blocks `git worktree remove --force`? no — it blocks plain
        // `remove`; cleanup uses --force, and the sweep/unlock retries).
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "unlock"])
            .arg(&self.worktree)
            .output();
    }
}

/// Brings up the run container and its sidecars, waits for readiness, and
/// provisions databases/bucket. Any failure tears down whatever was started
/// and returns a transient error (docker daemon hiccups, image pulls, sidecar
/// crashes are all retryable, spec §5.7).
pub async fn start(spec: ContainerSpec<'_>) -> Result<RunContainers, DriverError> {
    let cfg = spec.cfg;
    let docker = &cfg.docker_binary;
    let run_name = run_container_name(spec.run_id);
    let pg_name = format!("{run_name}-pg");
    let minio_name = format!("{run_name}-minio");

    // Lock the worktree against host-side cleanup (`git worktree remove`)
    // while a container has it mounted. Best-effort: locking the main
    // checkout (supervise run in the central clone) fails harmlessly.
    let lock = tokio::process::Command::new("git")
        .arg("-C")
        .arg(spec.repo)
        .args(["worktree", "lock"])
        .arg(spec.worktree)
        .output()
        .await;
    match lock {
        Ok(o) if o.status.success() => {
            tracing::debug!(worktree = %spec.worktree.display(), "git worktree locked")
        }
        Ok(o) => tracing::debug!(
            worktree = %spec.worktree.display(),
            "git worktree lock: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::debug!(error = %e, "git worktree lock failed to spawn"),
    }

    if spec.driver_kind == "kimi-acp" {
        provision_kimi_config(spec.repo, spec.agent_home).map_err(DriverError::Permanent)?;
    }

    let result = start_inner(&spec, &run_name, &pg_name, &minio_name).await;
    if result.is_err() {
        // Roll back whatever came up; the guard was never handed out.
        for name in [&pg_name, &minio_name, &run_name] {
            docker_fire_and_forget(docker, &["rm", "-f", name]).await;
        }
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(spec.repo)
            .args(["worktree", "unlock"])
            .arg(spec.worktree)
            .output()
            .await;
    }
    result
}

async fn start_inner(
    spec: &ContainerSpec<'_>,
    run_name: &str,
    pg_name: &str,
    minio_name: &str,
) -> Result<RunContainers, DriverError> {
    let cfg = spec.cfg;
    let docker_bin = &cfg.docker_binary;
    let label_run = format!("{RUN_ID_LABEL}={}", spec.run_id);
    let label_project = format!("{PROJECT_ID_LABEL}={}", spec.project_id);
    let git_dir = spec.repo.join(".git");

    // ── run container ────────────────────────────────────────────────────
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        run_name.to_string(),
        "--label".to_string(),
        label_run.clone(),
        "--label".to_string(),
        label_project.clone(),
        "-v".to_string(),
        format!("{}:{WORK_DIR}", spec.worktree.display()),
        "-w".to_string(),
        WORK_DIR.to_string(),
        // Same absolute host path inside the container: the worktree's `.git`
        // file points at `<repo>/.git/worktrees/<wt>`, and that path must
        // resolve identically inside.
        "-v".to_string(),
        format!("{}:{}", git_dir.display(), git_dir.display()),
        "-v".to_string(),
        format!("{}:/root", spec.agent_home.display()),
    ];
    // Reference repos (#169): read-only bind mounts at /work/.refs/<mount> —
    // the agent must never write to a reference repo (its checkout is
    // detached and never pushed). The ref clone's `.git` rides at the same
    // absolute host path (like the main repo's above) so git commands work
    // inside the mount; read-only as well.
    for r in spec.refs {
        args.extend([
            "-v".to_string(),
            format!("{}:{WORK_DIR}/.refs/{}:ro", r.worktree.display(), r.mount_name),
        ]);
        let ref_git = r.repo.join(".git");
        args.extend([
            "-v".to_string(),
            format!("{}:{}:ro", ref_git.display(), ref_git.display()),
        ]);
    }
    #[cfg(target_os = "linux")]
    args.extend([
        "--add-host".to_string(),
        "host.docker.internal:host-gateway".to_string(),
    ]);
    // Shared nix binary cache (image.rs module docs): read-only here — the
    // agent's nix/devenv commands substitute from what the bakes exported.
    args.extend(crate::image::nix_cache_run_args(cfg, spec.repo));
    for (k, v) in spec.env {
        args.extend(["-e".to_string(), format!("{k}={v}")]);
    }
    args.push(spec.image.to_string());
    args.extend(["sleep".to_string(), "infinity".to_string()]);
    docker(docker_bin, &args)
        .await
        .map_err(|e| e.context("run container start"))?;

    // ── postgres sidecar (shared network namespace) ──────────────────────
    docker(
        docker_bin,
        &[
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            pg_name.to_string(),
            "--label".to_string(),
            label_run.clone(),
            "--label".to_string(),
            label_project.clone(),
            "--network".to_string(),
            format!("container:{run_name}"),
            "-e".to_string(),
            "POSTGRES_USER=postgres".to_string(),
            "-e".to_string(),
            "POSTGRES_PASSWORD=postgres".to_string(),
            cfg.postgres_image.clone(),
        ],
    )
    .await
    .map_err(|e| e.context("postgres sidecar start"))?;

    // ── minio sidecar (shared network namespace) ─────────────────────────
    docker(
        docker_bin,
        &[
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            minio_name.to_string(),
            "--label".to_string(),
            label_run,
            "--label".to_string(),
            label_project,
            "--network".to_string(),
            format!("container:{run_name}"),
            "-e".to_string(),
            "MINIO_ROOT_USER=minioadmin".to_string(),
            "-e".to_string(),
            "MINIO_ROOT_PASSWORD=minioadmin".to_string(),
            cfg.minio_image.clone(),
            "server".to_string(),
            "/data".to_string(),
        ],
    )
    .await
    .map_err(|e| e.context("minio sidecar start"))?;

    wait_postgres(docker_bin, pg_name).await?;
    provision_postgres(docker_bin, pg_name).await?;
    provision_minio(cfg, run_name).await?;

    Ok(RunContainers {
        docker: docker_bin.clone(),
        run_name: run_name.to_string(),
        sidecars: vec![pg_name.to_string(), minio_name.to_string()],
        worktree: spec.worktree.to_path_buf(),
        repo: spec.repo.to_path_buf(),
    })
}

/// Poll `pg_isready` inside the sidecar until it answers **on TCP** or the
/// 120s budget runs out (replaces `devenv processes wait --timeout 120`).
///
/// The probe must be TCP (`-h 127.0.0.1`), not the default unix socket: the
/// official postgres image first boots a temporary server for its init
/// scripts that listens on the socket only (`listen_addresses=''`), then
/// shuts it down before starting the real one. A socket probe reports ready
/// during that window and provisioning hits "the database system is
/// shutting down" (run #950); TCP answers only once the final server is up —
/// which is also the interface the run container connects through.
async fn wait_postgres(docker: &str, pg_name: &str) -> Result<(), DriverError> {
    let deadline = std::time::Instant::now() + PG_READY_TIMEOUT;
    loop {
        let out = tokio::process::Command::new(docker)
            .args([
                "exec",
                pg_name,
                "pg_isready",
                "-h",
                "127.0.0.1",
                "-p",
                "5432",
                "-U",
                "postgres",
            ])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => return Ok(()),
            _ if std::time::Instant::now() >= deadline => {
                return Err(DriverError::Transient(format!(
                    "postgres sidecar {pg_name} not ready within {}s",
                    PG_READY_TIMEOUT.as_secs()
                )));
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Creates the run's databases and grants CREATEDB (backend tests create
/// per-test databases via sqlx::test). `CREATE DATABASE` has no IF NOT
/// EXISTS, so a "already exists" error is success — fresh sidecars make this
/// a first-boot path anyway.
async fn provision_postgres(docker_bin: &str, pg_name: &str) -> Result<(), DriverError> {
    docker(
        docker_bin,
        &[
            "exec".to_string(),
            pg_name.to_string(),
            "psql".to_string(),
            "-U".to_string(),
            "postgres".to_string(),
            "-c".to_string(),
            "ALTER ROLE postgres CREATEDB".to_string(),
        ],
    )
    .await
    .map_err(|e| e.context("postgres provisioning (ALTER ROLE)"))?;
    for db in PROVISION_DATABASES {
        let out = tokio::process::Command::new(docker_bin)
            .args(["exec", pg_name, "psql", "-U", "postgres", "-c"])
            .arg(format!("CREATE DATABASE \"{db}\""))
            .output()
            .await
            .map_err(|e| DriverError::Transient(format!("postgres provisioning ({db}): {e}")))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() && !stderr.contains("already exists") {
            return Err(DriverError::Transient(format!(
                "postgres provisioning ({db}): {}",
                stderr.trim()
            )));
        }
    }
    Ok(())
}

/// One-shot `mc` container on the run's network namespace: alias + bucket.
/// Retried until MinIO answers (its readiness endpoint has no CLI client in
/// the sidecar image, so a failed alias/mb *is* the not-ready signal).
async fn provision_minio(cfg: &ExecutionConfig, run_name: &str) -> Result<(), DriverError> {
    let script = format!(
        "mc alias set local http://127.0.0.1:9000 minioadmin minioadmin && mc mb --ignore-existing local/{PROVISION_BUCKET}"
    );
    let deadline = std::time::Instant::now() + MINIO_READY_TIMEOUT;
    loop {
        let out = tokio::process::Command::new(&cfg.docker_binary)
            .args([
                "run",
                "--rm",
                "--network",
                &format!("container:{run_name}"),
                // The mc image's entrypoint IS `mc` — override it to run a
                // shell (alias + mb are two commands).
                "--entrypoint",
                "sh",
                &cfg.mc_image,
                "-c",
                &script,
            ])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) if std::time::Instant::now() >= deadline => {
                return Err(DriverError::Transient(format!(
                    "minio provisioning failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                )));
            }
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(DriverError::Transient(format!("minio provisioning: {e}")));
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

/// Startup sweep (spec §5.7): `docker rm -f` every container labeled
/// `remoter.run_id` — leftovers of a crashed daemon — and every image-init
/// container (`remoter.image_init`, `image.rs`) orphaned by a dropped build
/// future or daemon restart (such an orphan can still be *running*; its work
/// is useless — nobody will commit its result). Best-effort, never fails the
/// caller.
pub async fn sweep(cfg: &ExecutionConfig) {
    let docker = &cfg.docker_binary;
    sweep_labeled(docker, RUN_ID_LABEL, "run").await;
    sweep_labeled(docker, IMAGE_INIT_LABEL, "image-init").await;
}

async fn sweep_labeled(docker: &str, label: &str, kind: &str) {
    let out = tokio::process::Command::new(docker)
        .args(["ps", "-aq", "--filter", &format!("label={label}")])
        .output()
        .await;
    let ids = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(o) => {
            tracing::warn!(
                "container sweep: docker ps failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "container sweep: docker ps failed to spawn");
            return;
        }
    };
    for id in ids.lines().filter(|l| !l.trim().is_empty()) {
        tracing::info!(container = %id, "startup sweep: removing orphaned {kind} container");
        docker_fire_and_forget(docker, &["rm", "-f", id]).await;
    }
}

/// `<docker> <args>` to success; stderr becomes a transient driver error.
pub(crate) async fn docker(docker: &str, args: &[String]) -> Result<String, DriverError> {
    let out = tokio::process::Command::new(docker)
        .args(args)
        .output()
        .await
        .map_err(|e| DriverError::Transient(format!("docker {}: {e}", args.join(" "))))?;
    if !out.status.success() {
        return Err(DriverError::Transient(format!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Teardown-style docker call: logged at debug, never returned.
async fn docker_fire_and_forget(docker: &str, args: &[&str]) {
    match tokio::process::Command::new(docker).args(args).output().await {
        Ok(o) if o.status.success() => {}
        Ok(o) => tracing::debug!(
            "docker {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::debug!(error = %e, "docker {} failed to spawn", args.join(" ")),
    }
}

/// The API URL as reachable from inside a run container: loopback hosts go
/// through `host.docker.internal` (which `start` maps to the host gateway on
/// Linux; Docker Desktop provides it natively). Non-loopback URLs pass
/// through unchanged.
pub fn container_api_url(api_url: &str) -> String {
    const HOST: &str = "host.docker.internal";
    for loopback in ["localhost", "127.0.0.1", "[::1]", "0.0.0.0"] {
        for scheme in ["http://", "https://", "ws://", "wss://"] {
            let prefix = format!("{scheme}{loopback}");
            if let Some(rest) = api_url.strip_prefix(&prefix)
                && (rest.is_empty() || rest.starts_with(':') || rest.starts_with('/'))
            {
                return format!("{scheme}{HOST}{rest}");
            }
        }
    }
    api_url.to_string()
}

/// Extra context for error messages.
trait Context {
    fn context(self, what: &str) -> DriverError;
}

impl Context for DriverError {
    fn context(self, what: &str) -> DriverError {
        match self {
            DriverError::Transient(e) => DriverError::Transient(format!("{what}: {e}")),
            DriverError::Permanent(e) => DriverError::Permanent(format!("{what}: {e}")),
            DriverError::Stalled(e) => DriverError::Stalled(format!("{what}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_api_url_rewrites_loopback_only() {
        assert_eq!(
            container_api_url("http://localhost:8181"),
            "http://host.docker.internal:8181"
        );
        assert_eq!(
            container_api_url("http://127.0.0.1:8181"),
            "http://host.docker.internal:8181"
        );
        assert_eq!(container_api_url("http://localhost"), "http://host.docker.internal");
        assert_eq!(
            container_api_url("https://remoter.example.com"),
            "https://remoter.example.com"
        );
        // "localhost" inside a path is not a host.
        assert_eq!(
            container_api_url("https://example.com/localhost"),
            "https://example.com/localhost"
        );
    }

    #[test]
    fn container_api_url_handles_ipv6_wildcard_and_ws_schemes() {
        assert_eq!(
            container_api_url("http://[::1]:8181"),
            "http://host.docker.internal:8181"
        );
        assert_eq!(
            container_api_url("http://0.0.0.0:8181"),
            "http://host.docker.internal:8181"
        );
        assert_eq!(
            container_api_url("ws://localhost:8181/api/v1/agent/events"),
            "ws://host.docker.internal:8181/api/v1/agent/events"
        );
        assert_eq!(container_api_url("wss://127.0.0.1"), "wss://host.docker.internal");
    }

    #[test]
    fn container_api_url_leaves_remote_and_lookalike_hosts_alone() {
        assert_eq!(
            container_api_url("https://staging.internal:9443/api"),
            "https://staging.internal:9443/api"
        );
        // A host merely starting with a loopback name is not loopback.
        assert_eq!(
            container_api_url("http://localhost.evil.com"),
            "http://localhost.evil.com"
        );
        assert_eq!(
            container_api_url("http://127.0.0.1.evil.com"),
            "http://127.0.0.1.evil.com"
        );
        assert_eq!(container_api_url("http://0.0.0.0.evil.com"), "http://0.0.0.0.evil.com");
    }

    #[test]
    fn run_container_name_is_scoped_to_the_run() {
        assert_eq!(run_container_name(42), "remoter-run-42");
    }

    #[tokio::test]
    async fn sweep_removes_run_and_image_init_leftovers() {
        let dir = std::env::temp_dir().join(format!("remoter-sweep-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Stub docker: logs every argv line; `ps` answers with two ids.
        let stub = dir.join("docker-stub.sh");
        crate::testutil::write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = ps ]; then printf 'aaa\\nbbb\\n'; fi\nexit 0\n",
                dir.join("docker.log").display()
            ),
        );
        let cfg = ExecutionConfig {
            docker_binary: stub.to_string_lossy().into_owned(),
            ..Default::default()
        };
        sweep(&cfg).await;
        let log = std::fs::read_to_string(dir.join("docker.log")).unwrap();
        assert!(log.contains(&format!("ps -aq --filter label={RUN_ID_LABEL}")), "{log}");
        assert!(
            log.contains(&format!("ps -aq --filter label={IMAGE_INIT_LABEL}")),
            "{log}"
        );
        // Both ids from both passes are force-removed.
        assert_eq!(log.matches("rm -f aaa").count(), 2, "{log}");
        assert_eq!(log.matches("rm -f bbb").count(), 2, "{log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn container_env_matches_the_section_3_4_contract() {
        let env = container_env(7);
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("CI"), Some("1"));
        assert_eq!(get("REMOTER_CONTAINER"), Some("1"));
        assert_eq!(get("REMOTER_AGENT_TASK_ID"), Some("7"));
        let db = |name: &str| Some(format!("postgres://postgres:postgres@127.0.0.1:5432/{name}"));
        assert_eq!(get("DATABASE_URL"), db("remoter").as_deref());
        assert_eq!(get("LOCAL_DATABASE_URL"), db("remoter").as_deref());
        assert_eq!(get("TEST_DATABASE_URL"), db("remoter-test").as_deref());
        assert_eq!(get("E2E_DATABASE_URL"), db("remoter_e2e").as_deref());
        // Sidecars share the run container's network namespace, so the §5.8
        // port-block contract does not apply inside a container.
        assert!(env.iter().all(|(k, _)| k != "REMOTER_AGENT_PORT_BASE"));
    }

    #[test]
    fn provision_kimi_config_substitutes_the_api_key() {
        let dir = std::env::temp_dir().join(format!("remoter-kimi-prov-{}", std::process::id()));
        let repo = dir.join("repo");
        let home = dir.join("agent-home");
        std::fs::create_dir_all(repo.join(".remoter")).unwrap();
        std::fs::write(
            repo.join(".remoter/kimi-config.toml"),
            "api_key = \"${KIMI_API_KEY}\"\n",
        )
        .unwrap();
        provision_kimi_config_with(&repo, &home, Some("sk-test")).unwrap();
        let rendered = std::fs::read_to_string(home.join(".kimi-code/config.toml")).unwrap();
        assert_eq!(rendered, "api_key = \"sk-test\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join(".kimi-code/config.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provision_kimi_config_missing_key_is_an_error() {
        let dir = std::env::temp_dir().join(format!("remoter-kimi-prov-err-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".remoter")).unwrap();
        std::fs::write(
            repo.join(".remoter/kimi-config.toml"),
            "api_key = \"${KIMI_API_KEY}\"\n",
        )
        .unwrap();
        let err = provision_kimi_config_with(&repo, &dir.join("agent-home"), None).unwrap_err();
        assert!(err.contains("KIMI_API_KEY"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provision_kimi_config_without_template_is_a_noop() {
        let dir = std::env::temp_dir().join(format!("remoter-kimi-prov-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        provision_kimi_config_with(&dir.join("repo"), &dir.join("agent-home"), None).unwrap();
        assert!(!dir.join("agent-home/.kimi-code/config.toml").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provisioned_databases_and_bucket_mirror_devenv_nix() {
        // devenv.nix `initialDatabases` — backend tests create per-test
        // databases on the same sidecar, hence the CREATEDB grant + test DBs.
        assert_eq!(PROVISION_DATABASES, ["remoter", "remoter-test", "remoter_e2e"]);
        // devenv.nix `services.minio.buckets` (docs/specs/attachments.md).
        assert_eq!(PROVISION_BUCKET, "remoter-attachments");
    }
}
