//! Per-run systemd-nspawn machines (`execution.mode = "nspawn"`, NixOS hosts
//! only): one booted machine per attempt from a shared NixOS system closure,
//! with the host `/nix/store` bind-mounted read-only — no image bake. Postgres
//! and minio run as systemd units *inside* the machine, so the container-mode
//! contract applies verbatim: Postgres is `127.0.0.1:5432`, MinIO
//! `127.0.0.1:9000`, and the §5.8 port-block contract does not apply.
//!
//! Privileges: the daemon stays an ordinary user. `systemd-nspawn`,
//! `systemd-run -M`, `machinectl terminate` and the root-owned
//! [`HELPER_BINARY`] script (host veth address + masquerade) are whitelisted
//! in sudoers and always invoked via `sudo -n`.
//!
//! Networking: `--network-veth` gives the machine a private netns (host side
//! `ve-<machine>`, container side `host0`). The daemon manages addresses
//! itself (no host systemd-networkd dependency): a per-run /30 from the
//! [`NetBlocks`] pool, host side configured by the helper's `net-up`, machine
//! side by a generated networkd drop-in bind-mounted over
//! `/etc/systemd/network`. The host is reachable from inside at the block's
//! `.1` — the nspawn analogue of `host.docker.internal`.
//!
//! Machine names are `rr-<run_id>`, NOT `remoter-run-<run_id>`: the host-side
//! veth name is `ve-` + machine name truncated to IFNAMSIZ-1 (15 chars), so a
//! 12+ char prefix would truncate the run id away and make every parallel
//! run's host interface collide (`ve-remoter-run-`). `rr-<id>` keeps the
//! derived `ve-rr-<id>` unique for run ids up to 9 digits.
//!
//! Lifecycle mirrors `container.rs`: [`start`] boots and waits for readiness,
//! returning a [`NspawnMachine`] guard whose teardown (terminate, net-down,
//! `git worktree unlock`) runs on explicit [`NspawnMachine::teardown`] or on
//! drop. A daemon crash can still leak machines; [`sweep`] reaps `rr-*`
//! machines at startup.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::config::ExecutionConfig;
use crate::driver::DriverError;
use crate::workspace::RefMount;

/// The root-owned privileged helper (host veth address + masquerade rule),
/// always invoked as `sudo -n remoter-nspawnctl <check|net-up|net-down>`.
pub const HELPER_BINARY: &str = "remoter-nspawnctl";

/// `systemd-run -M <machine> --pipe` (the per-turn exec path) requires
/// systemd ≥ 256 on the host; startup validation fails closed below that.
pub const MIN_SYSTEMD_VERSION: u32 = 256;

/// Machine name prefix — see module docs for why this is short.
const MACHINE_NAME_PREFIX: &str = "rr-";

/// The worktree mount point inside the machine (same as container mode).
pub const WORK_DIR: &str = crate::container::WORK_DIR;

/// Readiness budgets (same budgets container mode gives its sidecars).
const PG_READY_TIMEOUT: Duration = Duration::from_secs(120);
const MINIO_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// The host veth appears only once nspawn has set up the pair.
const NET_UP_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace for the boot process to exit after `machinectl terminate`.
const TERMINATE_WAIT: Duration = Duration::from_secs(5);

/// `/30` blocks per pool: third octet `4n` for n in 0..BLOCKS.
const NET_POOL_BLOCKS: u32 = 64;
const NET_PREFIX_LEN: u8 = 30;

/// The run's machine name — the `machinectl`/`systemd-run -M` target.
pub fn run_machine_name(run_id: i32) -> String {
    format!("{MACHINE_NAME_PREFIX}{run_id}")
}

/// The host side of the machine's veth pair (`ve-` + machine name; fits
/// IFNAMSIZ because of the short prefix, see module docs).
fn veth_name(run_id: i32) -> String {
    format!("ve-{MACHINE_NAME_PREFIX}{run_id}")
}

/// Startup-sweep predicate: our machines are `rr-<digits>` exactly.
fn is_run_machine(name: &str) -> bool {
    name.strip_prefix(MACHINE_NAME_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// `systemd 257 (257.9)` → 257. The first line of `systemd-run --version`.
pub(crate) fn parse_systemd_version(version_out: &str) -> Option<u32> {
    let line = version_out.lines().next()?;
    let ver = line.strip_prefix("systemd ")?.split_whitespace().next()?;
    let digits: String = ver.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { None } else { digits.parse().ok() }
}

/// The nspawn-mode env contract: like `container::container_env` (loopback DB
/// URLs, no `REMOTER_AGENT_PORT_BASE`) but `REMOTER_EXECUTION=nspawn` instead
/// of `REMOTER_CONTAINER=1` — the machine is not an OCI container.
pub fn nspawn_env(task_id: i32) -> Vec<(String, String)> {
    let db = |name: &str| format!("postgres://postgres:postgres@127.0.0.1:5432/{name}");
    vec![
        ("CI".to_string(), "1".to_string()),
        ("REMOTER_EXECUTION".to_string(), "nspawn".to_string()),
        ("REMOTER_AGENT_TASK_ID".to_string(), task_id.to_string()),
        ("DATABASE_URL".to_string(), db("remoter")),
        ("LOCAL_DATABASE_URL".to_string(), db("remoter")),
        ("TEST_DATABASE_URL".to_string(), db("remoter-test")),
        ("E2E_DATABASE_URL".to_string(), db("remoter_e2e")),
    ]
}

/// The API URL as reachable from inside a run machine: loopback hosts go to
/// the run block's host-side veth address (`<base>.<4n>.1`). Non-loopback
/// URLs pass through unchanged. Mirrors `container::container_api_url`.
pub fn nspawn_api_url(api_url: &str, host_ip: &str) -> String {
    for loopback in ["localhost", "127.0.0.1", "[::1]", "0.0.0.0"] {
        for scheme in ["http://", "https://", "ws://", "wss://"] {
            let prefix = format!("{scheme}{loopback}");
            if let Some(rest) = api_url.strip_prefix(&prefix)
                && (rest.is_empty() || rest.starts_with(':') || rest.starts_with('/'))
            {
                return format!("{scheme}{host_ip}{rest}");
            }
        }
    }
    api_url.to_string()
}

/// The API URL for one run's machine, resolving the host IP from the
/// run-id → net-block registry that [`start`] populates. A missing entry can
/// only mean the machine was never started (or is already torn down) — the
/// URL then stays unrewritten rather than pointing at a guessed block.
pub fn nspawn_api_url_for(api_url: &str, run_id: i32) -> String {
    match host_ip_for(run_id) {
        Some(ip) => nspawn_api_url(api_url, &ip),
        None => {
            tracing::warn!(
                run_id,
                "nspawn: no net block registered for the run; API URL left unrewritten"
            );
            api_url.to_string()
        }
    }
}

/// run id → host-side veth IP of the live machines, so `exec_env` can rewrite
/// the API URL for a run it did not itself start (same derivation pattern as
/// `run_container_name`).
fn host_ips() -> &'static Mutex<HashMap<i32, String>> {
    static IPS: OnceLock<Mutex<HashMap<i32, String>>> = OnceLock::new();
    IPS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_host_ip(run_id: i32, ip: String) {
    host_ips().lock().unwrap().insert(run_id, ip);
}

fn unregister_host_ip(run_id: i32) {
    host_ips().lock().unwrap().remove(&run_id);
}

pub fn host_ip_for(run_id: i32) -> Option<String> {
    host_ips().lock().unwrap().get(&run_id).cloned()
}

// ── per-run /30 net blocks (mirror of ports.rs) ─────────────────────────────

/// An allocated /30: host `<base>.<4n>.1`, machine `<base>.<4n>.2`. Release
/// happens automatically on drop (run end).
#[derive(Debug)]
pub struct NetBlock {
    index: u32,
    base: [u8; 2],
    in_use: Arc<Mutex<HashSet<u32>>>,
}

impl NetBlock {
    /// The host's address on the run's veth pair — what the machine reaches
    /// the daemon's API through, and what the helper configures on `ve-…`.
    pub fn host_ip(&self) -> String {
        format!("{}.{}.{}.1", self.base[0], self.base[1], self.index * 4)
    }

    /// The machine's own address (static, via the generated drop-in).
    pub fn machine_ip(&self) -> String {
        format!("{}.{}.{}.2", self.base[0], self.base[1], self.index * 4)
    }
}

impl Drop for NetBlock {
    fn drop(&mut self) {
        self.in_use.lock().unwrap().remove(&self.index);
    }
}

/// The daemon-wide pool of /30 blocks, one per concurrent nspawn run —
/// `ports.rs` for veth subnets instead of TCP ports.
#[derive(Clone)]
pub struct NetBlocks {
    base: [u8; 2],
    in_use: Arc<Mutex<HashSet<u32>>>,
}

impl NetBlocks {
    /// `pool_base` is the first two octets, e.g. `10.231`.
    pub fn new(pool_base: &str) -> Result<Self, String> {
        let mut octets = pool_base.split('.');
        let (Some(a), Some(b), None) = (octets.next(), octets.next(), octets.next()) else {
            return Err(format!(
                "execution.nspawn_net_pool_base {pool_base:?} must be two octets like \"10.231\""
            ));
        };
        let parse = |s: &str| s.parse::<u8>().ok();
        let (Some(a), Some(b)) = (parse(a), parse(b)) else {
            return Err(format!(
                "execution.nspawn_net_pool_base {pool_base:?} must be two octets like \"10.231\""
            ));
        };
        Ok(Self {
            base: [a, b],
            in_use: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// The smallest free block index, or `None` when the pool is exhausted
    /// (64 concurrent nspawn runs — exhaustion means a leak).
    pub fn allocate(&self) -> Option<NetBlock> {
        let mut in_use = self.in_use.lock().unwrap();
        (0..NET_POOL_BLOCKS).find(|i| in_use.insert(*i)).map(|index| NetBlock {
            index,
            base: self.base,
            in_use: self.in_use.clone(),
        })
    }

    #[cfg(test)]
    fn allocated_count(&self) -> usize {
        self.in_use.lock().unwrap().len()
    }
}

/// The daemon-process-global pool, keyed off the configured pool base on
/// first use (the config is fixed for the process lifetime).
fn global_net_blocks(cfg: &ExecutionConfig) -> Result<&'static NetBlocks, DriverError> {
    static POOL: OnceLock<NetBlocks> = OnceLock::new();
    if let Some(pool) = POOL.get() {
        return Ok(pool);
    }
    let pool = NetBlocks::new(&cfg.nspawn_net_pool_base).map_err(DriverError::Permanent)?;
    Ok(POOL.get_or_init(|| pool))
}

// ── closure resolution ──────────────────────────────────────────────────────

/// Resolves `execution.nspawn_closure_ref` to the NixOS toplevel directory
/// the machine boots from. A plain path (optionally `path:`-prefixed, no
/// `#attr`) must already exist; a flake ref with an attribute is built lazily
/// with `nix build --no-link --print-out-paths` — content-addressed, so a
/// no-op once the host store has the closure. Serialized process-wide: two
/// parallel first runs must not race the same `nix build`.
pub async fn resolve_closure(cfg: &ExecutionConfig) -> Result<PathBuf, DriverError> {
    let r = cfg.nspawn_closure_ref.trim();
    if r.is_empty() {
        return Err(DriverError::Permanent(
            "execution.nspawn_closure_ref is empty — point it at a flake ref (path:/etc/remoter-agent#agentContainer) \
             or a pre-built NixOS toplevel"
                .to_string(),
        ));
    }
    let Some((_, attr)) = r.split_once('#') else {
        let p = PathBuf::from(r.strip_prefix("path:").unwrap_or(r));
        if p.is_dir() {
            return Ok(p);
        }
        return Err(DriverError::Permanent(format!(
            "execution.nspawn_closure_ref {r:?} is neither an existing directory nor a flake ref with an attribute"
        )));
    };
    if attr.is_empty() {
        return Err(DriverError::Permanent(format!(
            "execution.nspawn_closure_ref {r:?} has an empty attribute (expected e.g. path:/etc/remoter-agent#agentContainer)"
        )));
    }
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    let out = tokio::process::Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "build",
            r,
            "--no-link",
            "--print-out-paths",
        ])
        .output()
        .await
        .map_err(|e| DriverError::Transient(format!("nix build {r}: {e}")))?;
    if !out.status.success() {
        return Err(DriverError::Transient(format!(
            "nix build {r} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Err(DriverError::Transient(format!("nix build {r} printed no store path")));
    }
    Ok(PathBuf::from(path))
}

// ── machine lifecycle ───────────────────────────────────────────────────────

/// The binaries nspawn operations go through. All privileged calls are
/// `<sudo> -n <binary> …`; `machinectl list` runs unprivileged. Fields are
/// injectable for tests (stub scripts on a temp PATH).
#[derive(Clone)]
struct Bins {
    sudo: String,
    nspawn: String,
    systemd_run: String,
    machinectl: String,
}

impl Bins {
    fn from_cfg(cfg: &ExecutionConfig) -> Self {
        Self {
            sudo: cfg.sudo_binary.clone(),
            nspawn: "systemd-nspawn".to_string(),
            systemd_run: "systemd-run".to_string(),
            machinectl: "machinectl".to_string(),
        }
    }
}

/// Everything [`start`] needs to boot one run's machine.
pub struct NspawnSpec<'a> {
    pub cfg: &'a ExecutionConfig,
    /// Configured ACP driver kind; only Kimi needs config-file provisioning.
    pub driver_kind: &'a str,
    /// The backend `agent_runs` row id — names the machine and its veth.
    pub run_id: i32,
    /// The NixOS toplevel the machine boots (from [`resolve_closure`]).
    pub closure: &'a Path,
    /// Host directory mounted at `/work` (the ticket worktree, or the central
    /// clone for a supervise run without a parent worktree).
    pub worktree: &'a Path,
    /// The central clone (`p<id>/repo`); its `.git` is mounted at the same
    /// absolute path inside the machine so worktree gitdir files resolve.
    pub repo: &'a Path,
    /// Mounted at `/root` (agent config + session state survive runs here).
    pub agent_home: &'a Path,
    /// Reference repos prepared for this run (docs/specs/cross-repo-projects.md)
    /// — each is bind-mounted read-only at `/work/.refs/<mount_name>`.
    pub refs: &'a [RefMount],
}

/// The live machine of one run. Teardown is guaranteed: `teardown()` on the
/// happy path, `Drop` (sync terminate + net-down) on cancellation/timeout.
pub struct NspawnMachine {
    sudo: String,
    machine: String,
    ifname: String,
    host_ip: String,
    run_id: i32,
    /// Held for its Drop (pool release); the addresses live in the fields.
    #[allow(dead_code)]
    block: Option<NetBlock>,
    worktree: PathBuf,
    repo: PathBuf,
    /// Per-run temp dir: the networkd drop-in bind-mounted into the machine
    /// plus the boot log. Must outlive the machine (the bind mount resolves
    /// against it); removed on teardown.
    run_dir: PathBuf,
    /// The `sudo systemd-nspawn` supervisor process. Kept so a dead boot is
    /// detected during readiness waits and the child is reaped on teardown.
    child: Option<tokio::process::Child>,
}

impl NspawnMachine {
    /// The machine name — the `systemd-run -M` target for agent commands.
    pub fn machine_name(&self) -> &str {
        &self.machine
    }

    /// Terminate the machine, remove the host veth config, release the net
    /// block, and unlock the worktree. Idempotent and best-effort per step —
    /// a partial teardown still attempts the rest.
    pub async fn teardown(mut self) {
        let machine = std::mem::take(&mut self.machine);
        if !machine.is_empty() {
            sudo_fire_and_forget(&self.sudo, &["machinectl", "terminate", &machine]).await;
            if let Some(mut child) = self.child.take() {
                // The supervisor exits once the machine is down; bound the
                // wait so a wedged boot cannot hang run teardown.
                let _ = tokio::time::timeout(TERMINATE_WAIT, child.wait()).await;
                let _ = child.start_kill();
            }
            unregister_host_ip(self.run_id);
        }
        let ifname = std::mem::take(&mut self.ifname);
        if !ifname.is_empty() {
            sudo_fire_and_forget(&self.sudo, &[HELPER_BINARY, "net-down", &ifname, &self.host_ip]).await;
        }
        self.unlock_worktree().await;
        let _ = std::fs::remove_dir_all(&self.run_dir);
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

impl Drop for NspawnMachine {
    /// Cancellation path: the run future is dropped (timeout, human cancel,
    /// daemon shutdown within `cancel_grace_secs`). Sync terminate + net-down,
    /// mirroring `RunContainers::drop`.
    fn drop(&mut self) {
        if self.machine.is_empty() && self.ifname.is_empty() {
            return;
        }
        if !self.machine.is_empty() {
            let _ = std::process::Command::new(&self.sudo)
                .args(["-n", "machinectl", "terminate", &self.machine])
                .output();
            unregister_host_ip(self.run_id);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
        if !self.ifname.is_empty() {
            let _ = std::process::Command::new(&self.sudo)
                .args(["-n", HELPER_BINARY, "net-down", &self.ifname, &self.host_ip])
                .output();
        }
        if !self.repo.as_os_str().is_empty() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "unlock"])
                .arg(&self.worktree)
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }
}

/// Boots the run's machine, brings up its network, and waits for postgres and
/// minio readiness. Any failure tears down whatever came up and returns a
/// transient error (machined hiccups, a stale closure are all retryable, spec
/// §5.7); a broken config (bad closure ref, missing kimi key) is permanent.
pub async fn start(spec: NspawnSpec<'_>) -> Result<NspawnMachine, DriverError> {
    let bins = Bins::from_cfg(spec.cfg);
    // Lock the worktree against host-side cleanup (`git worktree remove`)
    // while a machine has it mounted. Best-effort: locking the main checkout
    // (supervise run in the central clone) fails harmlessly.
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
        crate::container::provision_kimi_config(spec.repo, spec.agent_home).map_err(DriverError::Permanent)?;
    }

    let result = start_inner(&spec, &bins).await;
    if result.is_err() {
        // Roll back whatever came up; the guard was never handed out.
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

async fn start_inner(spec: &NspawnSpec<'_>, bins: &Bins) -> Result<NspawnMachine, DriverError> {
    let block = global_net_blocks(spec.cfg)?.allocate().ok_or_else(|| {
        DriverError::Transient(format!("no free nspawn net block ({NET_POOL_BLOCKS} concurrent runs)"))
    })?;
    let machine = run_machine_name(spec.run_id);
    let ifname = veth_name(spec.run_id);
    let host_ip = block.host_ip();

    let run_dir = std::env::temp_dir().join(format!("remoter-nspawn-{}-{}", std::process::id(), spec.run_id));
    let _ = std::fs::remove_dir_all(&run_dir);
    let net_dir = write_network_dropin(&run_dir, &block)?;

    let git_dir = spec.repo.join(".git");
    let mut args = vec![
        bins.nspawn.clone(),
        "--boot".to_string(),
        format!("--machine={machine}"),
        format!("--directory={}", spec.closure.display()),
        "--register=yes".to_string(),
        "--network-veth".to_string(),
        // The host store *is* the package cache — read-only, shared live.
        "--bind-ro=/nix/store".to_string(),
        format!("--bind={}:{}", spec.worktree.display(), WORK_DIR),
        // Same absolute host path inside the machine: the worktree's `.git`
        // file points at `<repo>/.git/worktrees/<wt>`, and that path must
        // resolve identically inside.
        format!("--bind={}:{}", git_dir.display(), git_dir.display()),
        format!("--bind={}:/root", spec.agent_home.display()),
        // Per-run networkd config (static host0 address) — mounted over the
        // closure's /etc/systemd/network before boot.
        format!("--bind={}:/etc/systemd/network", net_dir.display()),
    ];
    // Reference repos (#169): read-only bind mounts at /work/.refs/<mount>,
    // with the ref clone's `.git` at its absolute host path (like the main
    // repo's above) — same contract as container.rs.
    for r in spec.refs {
        args.push(format!(
            "--bind-ro={}:{WORK_DIR}/.refs/{}",
            r.worktree.display(),
            r.mount_name
        ));
        let ref_git = r.repo.join(".git");
        args.push(format!("--bind-ro={}:{}", ref_git.display(), ref_git.display()));
    }

    let boot_log_path = run_dir.join("boot.log");
    let boot_log = std::fs::File::create(&boot_log_path)
        .map_err(|e| DriverError::Transient(format!("nspawn boot log {}: {e}", boot_log_path.display())))?;
    let child = tokio::process::Command::new(&bins.sudo)
        .arg("-n")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(boot_log))
        .spawn()
        .map_err(|e| DriverError::Transient(format!("sudo -n {} …: {e}", bins.nspawn)))?;

    let mut guard = NspawnMachine {
        sudo: bins.sudo.clone(),
        machine,
        ifname,
        host_ip,
        run_id: spec.run_id,
        block: Some(block),
        worktree: spec.worktree.to_path_buf(),
        repo: spec.repo.to_path_buf(),
        run_dir,
        child: Some(child),
    };

    let ready = async {
        net_up(bins, &mut guard, &boot_log_path).await?;
        wait_postgres(bins, &mut guard, &boot_log_path).await?;
        wait_minio(bins, &mut guard, &boot_log_path).await
    }
    .await;
    if let Err(e) = ready {
        guard.teardown().await;
        return Err(e);
    }
    register_host_ip(spec.run_id, guard.host_ip.clone());
    Ok(guard)
}

/// `remoter-nspawnctl net-up <ifname> <host_ip> <prefix>` — idempotent (the
/// helper tolerates an existing address), retried until nspawn has created
/// the host side of the veth pair.
async fn net_up(bins: &Bins, guard: &mut NspawnMachine, boot_log: &Path) -> Result<(), DriverError> {
    let deadline = std::time::Instant::now() + NET_UP_TIMEOUT;
    let args = [
        HELPER_BINARY.to_string(),
        "net-up".to_string(),
        guard.ifname.clone(),
        guard.host_ip.clone(),
        NET_PREFIX_LEN.to_string(),
    ];
    loop {
        if let Some(dead) = boot_died(guard, boot_log) {
            return Err(DriverError::Transient(format!(
                "nspawn machine died during network setup: {dead}"
            )));
        }
        match sudo(bins, &args).await {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(e.context("nspawn net-up"));
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Poll `pg_isready` inside the machine until postgres answers on TCP or the
/// 120s budget runs out — the same probe container mode uses against its
/// sidecar (here postgres is a systemd unit of the machine closure).
async fn wait_postgres(bins: &Bins, guard: &mut NspawnMachine, boot_log: &Path) -> Result<(), DriverError> {
    let deadline = std::time::Instant::now() + PG_READY_TIMEOUT;
    let args = exec_args(
        bins,
        &guard.machine,
        &["pg_isready", "-h", "127.0.0.1", "-p", "5432", "-U", "postgres"],
    );
    loop {
        if let Some(dead) = boot_died(guard, boot_log) {
            return Err(DriverError::Transient(format!(
                "nspawn machine died before postgres was ready: {dead}"
            )));
        }
        match sudo(bins, &args).await {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(DriverError::Transient(format!(
                    "postgres in machine {} not ready within {}s ({e})",
                    guard.machine,
                    PG_READY_TIMEOUT.as_secs()
                )));
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// MinIO is a systemd unit of the machine closure (its bucket-creation
/// oneshot is ordered after it by the closure module), so readiness = the
/// unit being active. `systemctl` is the one binary a booted systemd machine
/// guarantees.
async fn wait_minio(bins: &Bins, guard: &mut NspawnMachine, boot_log: &Path) -> Result<(), DriverError> {
    let deadline = std::time::Instant::now() + MINIO_READY_TIMEOUT;
    let args = exec_args(bins, &guard.machine, &["systemctl", "is-active", "--quiet", "minio"]);
    loop {
        if let Some(dead) = boot_died(guard, boot_log) {
            return Err(DriverError::Transient(format!(
                "nspawn machine died before minio was ready: {dead}"
            )));
        }
        match sudo(bins, &args).await {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(DriverError::Transient(format!(
                    "minio in machine {} not ready within {}s ({e})",
                    guard.machine,
                    MINIO_READY_TIMEOUT.as_secs()
                )));
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// `systemd-run -M <machine> --pipe --quiet --wait --collect -- <cmd…>` — the
/// same exec contract `workspace::wrap_command` uses per ACP turn.
fn exec_args(bins: &Bins, machine: &str, cmd: &[&str]) -> Vec<String> {
    let mut args = vec![
        bins.systemd_run.clone(),
        "-M".to_string(),
        machine.to_string(),
        "--pipe".to_string(),
        "--quiet".to_string(),
        "--wait".to_string(),
        "--collect".to_string(),
        "--".to_string(),
    ];
    args.extend(cmd.iter().map(|s| s.to_string()));
    args
}

/// A boot process that exited before readiness means the machine will never
/// come up — fail fast with the boot log tail instead of riding the timeout.
fn boot_died(guard: &mut NspawnMachine, boot_log: &Path) -> Option<String> {
    let child = guard.child.as_mut()?;
    let status = match child.try_wait() {
        Ok(Some(status)) => status,
        Ok(None) => return None,
        Err(e) => return Some(format!("boot process wait failed: {e}")),
    };
    let tail = std::fs::read_to_string(boot_log)
        .map(|t| {
            t.chars()
                .skip(t.chars().count().saturating_sub(400))
                .collect::<String>()
        })
        .unwrap_or_default();
    Some(format!("boot process exited with {status}: {}", tail.trim()))
}

/// The generated per-run networkd config: static host0 address from the
/// run's /30, gateway (and DNS, via the helper's masquerade) at the host.
/// Returns the directory to bind over `/etc/systemd/network`.
fn write_network_dropin(run_dir: &Path, block: &NetBlock) -> Result<PathBuf, DriverError> {
    let net_dir = run_dir.join("network");
    std::fs::create_dir_all(&net_dir)
        .map_err(|e| DriverError::Transient(format!("cannot create {}: {e}", net_dir.display())))?;
    let conf = format!(
        "[Match]\nName=host0\n\n[Network]\nAddress={}/{NET_PREFIX_LEN}\nGateway={}\nDNS=1.1.1.1\n",
        block.machine_ip(),
        block.host_ip()
    );
    std::fs::write(net_dir.join("10-host0.network"), conf)
        .map_err(|e| DriverError::Transient(format!("cannot write network drop-in in {}: {e}", net_dir.display())))?;
    Ok(net_dir)
}

/// `<sudo> -n <args>` to success; stderr becomes a transient driver error.
async fn sudo(bins: &Bins, args: &[String]) -> Result<String, DriverError> {
    let out = tokio::process::Command::new(&bins.sudo)
        .arg("-n")
        .args(args)
        .output()
        .await
        .map_err(|e| DriverError::Transient(format!("sudo -n {}: {e}", args.join(" "))))?;
    if !out.status.success() {
        return Err(DriverError::Transient(format!(
            "sudo -n {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Teardown-style sudo call: logged at debug, never returned.
async fn sudo_fire_and_forget(sudo_bin: &str, args: &[&str]) {
    match tokio::process::Command::new(sudo_bin)
        .arg("-n")
        .args(args)
        .output()
        .await
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => tracing::debug!(
            "sudo -n {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::debug!(error = %e, "sudo -n {} failed to spawn", args.join(" ")),
    }
}

/// Startup sweep (spec §5.7): `machinectl terminate` every `rr-*` machine —
/// leftovers of a crashed daemon. `machinectl list` works unprivileged; the
/// terminate goes through sudo. Best-effort, never fails the caller.
pub async fn sweep(cfg: &ExecutionConfig) {
    sweep_with_bins(&Bins::from_cfg(cfg)).await;
}

async fn sweep_with_bins(bins: &Bins) {
    let out = tokio::process::Command::new(&bins.machinectl)
        .args(["list", "--output=json"])
        .output()
        .await;
    let stdout = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            tracing::warn!(
                "nspawn sweep: machinectl list failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "nspawn sweep: machinectl list failed to spawn");
            return;
        }
    };
    let machines: Vec<serde_json::Value> = match serde_json::from_str(&stdout) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "nspawn sweep: cannot parse machinectl list JSON");
            return;
        }
    };
    for machine in machines {
        let Some(name) = machine.get("machine").and_then(|m| m.as_str()) else {
            continue;
        };
        if !is_run_machine(name) {
            continue;
        }
        tracing::info!(machine = %name, "startup sweep: terminating orphaned nspawn machine");
        sudo_fire_and_forget(&bins.sudo, &["machinectl", "terminate", name]).await;
    }
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
    fn machine_and_veth_names_fit_ifnamsiz() {
        assert_eq!(run_machine_name(42), "rr-42");
        assert_eq!(veth_name(42), "ve-rr-42");
        // IFNAMSIZ-1 = 15: even a 9-digit run id fits with room to spare.
        assert!(veth_name(999_999_999).len() <= 15);
        assert!(is_run_machine("rr-42"));
        assert!(!is_run_machine("rr-"));
        assert!(!is_run_machine("rr-web"));
        assert!(!is_run_machine("remoter-run-42"));
        assert!(!is_run_machine("debian"));
    }

    #[test]
    fn parse_systemd_version_reads_the_first_line() {
        assert_eq!(parse_systemd_version("systemd 257 (257.9)\n+PAM +AUDIT"), Some(257));
        assert_eq!(parse_systemd_version("systemd 256\n"), Some(256));
        assert_eq!(parse_systemd_version("garbage"), None);
        assert_eq!(parse_systemd_version(""), None);
        assert_eq!(parse_systemd_version("systemd (257)"), None);
    }

    #[test]
    fn nspawn_env_matches_the_container_contract_with_nspawn_marker() {
        let env = nspawn_env(7);
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("CI"), Some("1"));
        assert_eq!(get("REMOTER_EXECUTION"), Some("nspawn"));
        assert_eq!(get("REMOTER_AGENT_TASK_ID"), Some("7"));
        let db = |name: &str| Some(format!("postgres://postgres:postgres@127.0.0.1:5432/{name}"));
        assert_eq!(get("DATABASE_URL"), db("remoter").as_deref());
        assert_eq!(get("LOCAL_DATABASE_URL"), db("remoter").as_deref());
        assert_eq!(get("TEST_DATABASE_URL"), db("remoter-test").as_deref());
        assert_eq!(get("E2E_DATABASE_URL"), db("remoter_e2e").as_deref());
        // Services are systemd units inside the machine, so the §5.8
        // port-block contract does not apply.
        assert!(env.iter().all(|(k, _)| k != "REMOTER_AGENT_PORT_BASE"));
        assert!(env.iter().all(|(k, _)| k != "REMOTER_CONTAINER"));
    }

    #[test]
    fn nspawn_api_url_rewrites_loopback_only() {
        assert_eq!(
            nspawn_api_url("http://localhost:8181", "10.231.8.1"),
            "http://10.231.8.1:8181"
        );
        assert_eq!(
            nspawn_api_url("http://127.0.0.1:8181", "10.231.8.1"),
            "http://10.231.8.1:8181"
        );
        assert_eq!(
            nspawn_api_url("ws://localhost:8181/api/v1/agent/events", "10.231.8.1"),
            "ws://10.231.8.1:8181/api/v1/agent/events"
        );
        assert_eq!(
            nspawn_api_url("http://[::1]:8181", "10.231.8.1"),
            "http://10.231.8.1:8181"
        );
        assert_eq!(
            nspawn_api_url("https://remoter.example.com", "10.231.8.1"),
            "https://remoter.example.com"
        );
        assert_eq!(
            nspawn_api_url("http://localhost.evil.com", "10.231.8.1"),
            "http://localhost.evil.com"
        );
        assert_eq!(
            nspawn_api_url("https://example.com/localhost", "10.231.8.1"),
            "https://example.com/localhost"
        );
    }

    #[test]
    fn net_blocks_allocate_distinct_and_release_on_drop() {
        let pool = NetBlocks::new("10.231").unwrap();
        let a = pool.allocate().unwrap();
        let b = pool.allocate().unwrap();
        assert_eq!(a.host_ip(), "10.231.0.1");
        assert_eq!(a.machine_ip(), "10.231.0.2");
        assert_eq!(b.host_ip(), "10.231.4.1");
        assert_eq!(pool.allocated_count(), 2);
        drop(a);
        assert_eq!(pool.allocated_count(), 1);
        // The released block is handed out again.
        let c = pool.allocate().unwrap();
        assert_eq!(c.host_ip(), "10.231.0.1");
    }

    #[test]
    fn net_blocks_exhaust_and_validate_base() {
        let pool = NetBlocks::new("10.231").unwrap();
        let blocks: Vec<_> = (0..NET_POOL_BLOCKS).map(|_| pool.allocate().unwrap()).collect();
        assert_eq!(blocks.len() as u32, NET_POOL_BLOCKS);
        assert!(pool.allocate().is_none());
        // Highest block stays inside the /16: 4*(64-1)+3 = 255.
        assert_eq!(blocks[63].machine_ip(), "10.231.252.2");

        assert!(NetBlocks::new("10.231.1").is_err());
        assert!(NetBlocks::new("10").is_err());
        assert!(NetBlocks::new("10.x").is_err());
        assert!(NetBlocks::new("10.300").is_err());
    }

    #[test]
    fn network_dropin_carries_the_block_addresses() {
        let pool = NetBlocks::new("10.231").unwrap();
        let block = pool.allocate().unwrap();
        let dir = std::env::temp_dir().join(format!("remoter-nspawn-dropin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let net_dir = write_network_dropin(&dir, &block).unwrap();
        let conf = std::fs::read_to_string(net_dir.join("10-host0.network")).unwrap();
        assert!(conf.contains("Name=host0"), "{conf}");
        assert!(conf.contains("Address=10.231.0.2/30"), "{conf}");
        assert!(conf.contains("Gateway=10.231.0.1"), "{conf}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stub `sudo`: records every argv line; the boot command (`-n
    /// systemd-nspawn …`) is replaced by a short-lived sleep so the guard's
    /// child handling has a live process; everything else exits 0 (readiness
    /// probes succeed immediately, terminate/net-down are recorded).
    fn stub_sudo(dir: &Path) -> PathBuf {
        let stub = dir.join("sudo-stub.sh");
        crate::testutil::write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = \"-n\" ] && [ \"$2\" = \"systemd-nspawn\" ]; then exec sleep 2; fi\nexit 0\n",
                dir.join("sudo.log").display()
            ),
        );
        stub
    }

    fn test_spec<'a>(cfg: &'a ExecutionConfig, run_id: i32, paths: &'a TestPaths) -> NspawnSpec<'a> {
        NspawnSpec {
            cfg,
            driver_kind: "stub",
            run_id,
            closure: &paths.closure,
            worktree: &paths.worktree,
            repo: &paths.repo,
            agent_home: &paths.agent_home,
            refs: &[],
        }
    }

    struct TestPaths {
        root: PathBuf,
        closure: PathBuf,
        worktree: PathBuf,
        repo: PathBuf,
        agent_home: PathBuf,
    }

    impl TestPaths {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("remoter-nspawn-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let paths = Self {
                closure: root.join("closure"),
                worktree: root.join("wt"),
                repo: root.join("repo"),
                agent_home: root.join("agent-home"),
                root,
            };
            for d in [&paths.closure, &paths.worktree, &paths.repo, &paths.agent_home] {
                std::fs::create_dir_all(d).unwrap();
            }
            paths
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    fn bins_for(sudo_stub: &Path) -> Bins {
        Bins {
            sudo: sudo_stub.to_string_lossy().into_owned(),
            nspawn: "systemd-nspawn".to_string(),
            systemd_run: "systemd-run".to_string(),
            machinectl: "machinectl".to_string(),
        }
    }

    #[tokio::test]
    async fn start_boots_with_the_mount_contract_and_teardown_cleans_up() {
        let paths = TestPaths::new("start");
        let sudo_stub = stub_sudo(&paths.root);
        let cfg = ExecutionConfig::default();
        let bins = bins_for(&sudo_stub);

        let guard = start_inner(&test_spec(&cfg, 77, &paths), &bins).await.unwrap();
        assert_eq!(guard.machine_name(), "rr-77");
        // exec_env's API-URL rewrite resolves through the registry.
        assert_eq!(
            nspawn_api_url_for("http://localhost:8181", 77),
            format!("http://{}:8181", guard.host_ip)
        );

        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        // Boot argv: machine name, closure rootfs, private veth, and the
        // mount contract (host store ro, worktree at /work, repo .git at its
        // host path, agent home at /root, networkd drop-in).
        let boot = format!(
            "-n systemd-nspawn --boot --machine=rr-77 --directory={}",
            paths.closure.display()
        );
        assert!(log.contains(&boot), "{log}");
        assert!(
            log.contains("--register=yes --network-veth --bind-ro=/nix/store"),
            "{log}"
        );
        assert!(
            log.contains(&format!("--bind={}:{}", paths.worktree.display(), WORK_DIR)),
            "{log}"
        );
        let git_dir = paths.repo.join(".git");
        assert!(
            log.contains(&format!("--bind={}:{}", git_dir.display(), git_dir.display())),
            "{log}"
        );
        assert!(
            log.contains(&format!("--bind={}:/root", paths.agent_home.display())),
            "{log}"
        );
        assert!(
            log.contains(&format!(
                "--bind={}/network:/etc/systemd/network",
                guard.run_dir.display()
            )),
            "{log}"
        );
        // Host veth config via the pinned helper interface.
        assert!(
            log.contains(&format!("-n remoter-nspawnctl net-up ve-rr-77 {} 30", guard.host_ip)),
            "{log}"
        );
        // Readiness probes run inside the machine via systemd-run.
        assert!(
            log.contains(
                "-n systemd-run -M rr-77 --pipe --quiet --wait --collect -- pg_isready -h 127.0.0.1 -p 5432 -U postgres"
            ),
            "{log}"
        );
        assert!(
            log.contains(
                "-n systemd-run -M rr-77 --pipe --quiet --wait --collect -- systemctl is-active --quiet minio"
            ),
            "{log}"
        );
        // The networkd drop-in is live while the machine runs.
        let conf = std::fs::read_to_string(guard.run_dir.join("network/10-host0.network")).unwrap();
        assert!(conf.contains(&format!("Gateway={}", guard.host_ip)), "{conf}");

        guard.teardown().await;
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains("-n machinectl terminate rr-77"), "{log}");
        assert!(log.contains("-n remoter-nspawnctl net-down ve-rr-77"), "{log}");
        assert_eq!(host_ip_for(77), None, "teardown unregisters the host IP");
    }

    #[tokio::test]
    async fn drop_without_teardown_still_terminates_and_nets_down() {
        let paths = TestPaths::new("drop");
        let sudo_stub = stub_sudo(&paths.root);
        let cfg = ExecutionConfig::default();
        let bins = bins_for(&sudo_stub);

        let guard = start_inner(&test_spec(&cfg, 78, &paths), &bins).await.unwrap();
        let run_dir = guard.run_dir.clone();
        drop(guard);
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains("-n machinectl terminate rr-78"), "{log}");
        assert!(log.contains("-n remoter-nspawnctl net-down ve-rr-78"), "{log}");
        assert_eq!(host_ip_for(78), None);
        assert!(!run_dir.exists(), "drop removes the per-run temp dir");
    }

    #[tokio::test]
    async fn failed_readiness_rolls_back_the_boot() {
        let paths = TestPaths::new("rollback");
        // sudo stub: everything fails → net-up never succeeds; the boot
        // process (sleep) outlives the probe loop only if we bail on time.
        // Make the boot die instantly instead: readiness waits fail fast on
        // the dead child, and the rollback path runs.
        let stub = paths.root.join("sudo-stub.sh");
        crate::testutil::write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = \"-n\" ] && [ \"$2\" = \"systemd-nspawn\" ]; then exit 1; fi\nexit 0\n",
                paths.root.join("sudo.log").display()
            ),
        );
        let cfg = ExecutionConfig::default();
        let bins = bins_for(&stub);

        let err = start_inner(&test_spec(&cfg, 79, &paths), &bins).await.err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("died"), "{msg}");
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains("-n machinectl terminate rr-79"), "{log}");
        assert!(log.contains("-n remoter-nspawnctl net-down ve-rr-79"), "{log}");
    }

    #[tokio::test]
    async fn sweep_terminates_only_remoter_run_machines() {
        let dir = std::env::temp_dir().join(format!("remoter-nspawn-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sudo_stub = stub_sudo(&dir);
        let machinectl = dir.join("machinectl-stub.sh");
        crate::testutil::write_executable_script(
            &machinectl,
            "#!/bin/sh\nprintf '%s' '[{\"machine\":\"rr-9\",\"class\":\"container\"},{\"machine\":\"debian\",\"class\":\"container\"},{\"machine\":\"rr-web\",\"class\":\"container\"},{\"machine\":\"rr-\",\"class\":\"container\"}]'\n",
        );
        let bins = Bins {
            machinectl: machinectl.to_string_lossy().into_owned(),
            ..bins_for(&sudo_stub)
        };
        sweep_with_bins(&bins).await;
        let log = std::fs::read_to_string(dir.join("sudo.log")).unwrap();
        assert!(log.contains("-n machinectl terminate rr-9"), "{log}");
        assert!(!log.contains("debian"), "{log}");
        assert!(!log.contains("rr-web"), "{log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn sweep_tolerates_a_failing_machinectl() {
        let dir = std::env::temp_dir().join(format!("remoter-nspawn-sweep-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sudo_stub = stub_sudo(&dir);
        let machinectl = dir.join("machinectl-stub.sh");
        crate::testutil::write_executable_script(&machinectl, "#!/bin/sh\nexit 1\n");
        let bins = Bins {
            machinectl: machinectl.to_string_lossy().into_owned(),
            ..bins_for(&sudo_stub)
        };
        sweep_with_bins(&bins).await; // must not panic
        assert!(!dir.join("sudo.log").exists(), "no terminate without a listing");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn resolve_closure_accepts_an_existing_directory() {
        let dir = std::env::temp_dir().join(format!("remoter-nspawn-closure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = ExecutionConfig {
            nspawn_closure_ref: dir.to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert_eq!(resolve_closure(&cfg).await.unwrap(), dir);
        let missing = ExecutionConfig {
            nspawn_closure_ref: dir.join("nope").to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert!(resolve_closure(&missing).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exec_args_match_the_wrap_command_contract() {
        let bins = Bins {
            sudo: "sudo".to_string(),
            nspawn: "systemd-nspawn".to_string(),
            systemd_run: "systemd-run".to_string(),
            machinectl: "machinectl".to_string(),
        };
        assert_eq!(
            exec_args(&bins, "rr-5", &["pg_isready"]),
            [
                "systemd-run",
                "-M",
                "rr-5",
                "--pipe",
                "--quiet",
                "--wait",
                "--collect",
                "--",
                "pg_isready"
            ]
        );
    }
}
