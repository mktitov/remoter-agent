//! Per-run systemd-nspawn machines (`execution.mode = "nspawn"`, NixOS hosts
//! only). The root-owned `remoter-nspawnctl` helper owns privileged systemd and
//! networking operations, host-wide machine/subnet leases, and orphan sweeping.
//! The daemon retains worktree locking, closure realization, boot logs, and the
//! ACP stdio lifecycle.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::config::ExecutionConfig;
use crate::driver::DriverError;
use crate::workspace::RefMount;

pub const HELPER_BINARY: &str = "remoter-nspawnctl";
pub const MIN_SYSTEMD_VERSION: u32 = 256;
pub const WORK_DIR: &str = crate::container::WORK_DIR;

const MACHINE_READY_TIMEOUT: Duration = Duration::from_secs(60);
const NET_UP_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINATE_WAIT: Duration = Duration::from_secs(5);

pub(crate) fn parse_systemd_version(version_out: &str) -> Option<u32> {
    let line = version_out.lines().next()?;
    let ver = line.strip_prefix("systemd ")?.split_whitespace().next()?;
    let digits: String = ver.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { None } else { digits.parse().ok() }
}

pub fn nspawn_env(task_id: i32) -> Vec<(String, String)> {
    vec![
        ("CI".to_string(), "1".to_string()),
        ("REMOTER_EXECUTION".to_string(), "nspawn".to_string()),
        ("REMOTER_AGENT_TASK_ID".to_string(), task_id.to_string()),
    ]
}

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

/// Persistent identity used by the helper to distinguish this daemon's leases
/// from those of other daemons. The lock prevents two live processes from
/// sharing one identity and sweeping each other's machines.
pub struct NspawnIdentity {
    id: String,
    _lock: std::fs::File,
}

impl NspawnIdentity {
    pub fn id(&self) -> &str {
        &self.id
    }
}

pub fn lock_instance(workspace_root: &Path) -> anyhow::Result<NspawnIdentity> {
    std::fs::create_dir_all(workspace_root)?;
    let lock_path = workspace_root.join("nspawn-instance.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(
            "another remoter-agent daemon holds {} (nspawn mode permits one daemon per workspace)",
            lock_path.display()
        );
    }

    let id_path = workspace_root.join("nspawn-instance-id");
    let id = match std::fs::read_to_string(&id_path) {
        Ok(value) => {
            let value = value.trim().to_string();
            if !valid_instance_id(&value) {
                anyhow::bail!("{} must contain exactly 32 lowercase hex characters", id_path.display());
            }
            value
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = [0u8; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
            let value = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let tmp = workspace_root.join(format!("nspawn-instance-id.tmp-{}", std::process::id()));
            let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
            file.write_all(value.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&tmp, &id_path)?;
            value
        }
        Err(e) => return Err(e.into()),
    };
    Ok(NspawnIdentity { id, _lock: lock })
}

fn valid_instance_id(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[derive(Clone, Debug)]
struct Bins {
    sudo: String,
    nix: String,
    nix_store: String,
}

impl Bins {
    fn from_cfg(cfg: &ExecutionConfig) -> Self {
        Self {
            sudo: cfg.sudo_binary.clone(),
            nix: "nix".to_string(),
            nix_store: "nix-store".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct RunLease {
    machine: String,
    host_ip: String,
}

fn parse_lease(output: &str) -> Result<RunLease, DriverError> {
    let fields: Vec<_> = output.split_whitespace().collect();
    let [machine, _ifname, host_ip, _machine_ip] = fields.as_slice() else {
        return Err(DriverError::Transient(format!(
            "remoter-nspawnctl acquire returned invalid lease {output:?}"
        )));
    };
    Ok(RunLease {
        machine: (*machine).to_string(),
        host_ip: (*host_ip).to_string(),
    })
}

pub struct NspawnSpec<'a> {
    pub cfg: &'a ExecutionConfig,
    pub driver_kind: &'a str,
    pub run_id: i32,
    pub worktree: &'a Path,
    pub repo: &'a Path,
    pub agent_home: &'a Path,
    pub refs: &'a [RefMount],
}

struct WorktreeLockGuard {
    repo: PathBuf,
    worktree: PathBuf,
}

impl WorktreeLockGuard {
    fn new(repo: &Path, worktree: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            worktree: worktree.to_path_buf(),
        }
    }
}

impl Drop for WorktreeLockGuard {
    fn drop(&mut self) {
        if !self.repo.as_os_str().is_empty() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "unlock"])
                .arg(&self.worktree)
                .output();
        }
    }
}

pub struct NspawnMachine {
    sudo: String,
    instance: String,
    run_id: i32,
    machine: String,
    host_ip: String,
    lease_active: bool,
    worktree: PathBuf,
    repo: PathBuf,
    run_dir: PathBuf,
    child: Option<tokio::process::Child>,
}

impl NspawnMachine {
    pub fn machine_name(&self) -> &str {
        &self.machine
    }

    pub async fn teardown(mut self) {
        self.terminate_lease_async().await;
        if let Some(mut child) = self.child.take() {
            let _ = tokio::time::timeout(TERMINATE_WAIT, child.wait()).await;
            kill_boot_process_group(&child);
            let _ = child.wait().await;
        }
        self.stop_lease_async().await;
        unregister_host_ip(self.run_id);
        self.unlock_worktree().await;
        self.remove_run_dir_if_released();
    }

    async fn terminate_lease_async(&mut self) {
        if !self.lease_active {
            return;
        }
        let args = [
            HELPER_BINARY.to_string(),
            "terminate".to_string(),
            self.instance.clone(),
            self.run_id.to_string(),
        ];
        let _ = helper_output(&self.sudo, &args).await;
    }

    async fn stop_lease_async(&mut self) {
        if !self.lease_active {
            return;
        }
        let args = [
            HELPER_BINARY.to_string(),
            "stop".to_string(),
            self.instance.clone(),
            self.run_id.to_string(),
        ];
        if helper_output(&self.sudo, &args).await.is_ok() {
            self.lease_active = false;
        }
    }

    fn remove_run_dir_if_released(&self) {
        if !self.lease_active {
            let _ = std::fs::remove_dir_all(&self.run_dir);
        }
    }

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
            Ok(o) => tracing::debug!(
                worktree = %self.worktree.display(),
                "git worktree unlock: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => tracing::debug!(error = %e, "git worktree unlock failed to spawn"),
        }
    }
}

impl Drop for NspawnMachine {
    fn drop(&mut self) {
        if self.lease_active {
            lease_command_sync(&self.sudo, "terminate", &self.instance, self.run_id);
        }
        if let Some(child) = self.child.take() {
            kill_boot_process_group(&child);
        }
        if self.lease_active && lease_command_sync(&self.sudo, "stop", &self.instance, self.run_id) {
            self.lease_active = false;
        }
        unregister_host_ip(self.run_id);
        if !self.repo.as_os_str().is_empty() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "unlock"])
                .arg(&self.worktree)
                .output();
        }
        self.remove_run_dir_if_released();
    }
}

fn kill_boot_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

fn lease_command_sync(sudo: &str, command: &str, instance: &str, run_id: i32) -> bool {
    std::process::Command::new(sudo)
        .arg("-n")
        .args([HELPER_BINARY, command, instance, &run_id.to_string()])
        .output()
        .is_ok_and(|out| out.status.success())
}

pub async fn start(spec: NspawnSpec<'_>) -> Result<NspawnMachine, DriverError> {
    let bins = Bins::from_cfg(spec.cfg);
    let lock_guard = WorktreeLockGuard::new(spec.repo, spec.worktree);
    let lock = tokio::process::Command::new("git")
        .arg("-C")
        .arg(spec.repo)
        .args(["worktree", "lock"])
        .arg(spec.worktree)
        .kill_on_drop(true)
        .output()
        .await;
    match lock {
        Ok(o) if o.status.success() => tracing::debug!(worktree = %spec.worktree.display(), "git worktree locked"),
        Ok(o) => tracing::debug!(
            worktree = %spec.worktree.display(),
            "git worktree lock: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::debug!(error = %e, "git worktree lock failed to spawn"),
    }

    let result = async {
        if spec.driver_kind == "kimi-acp" {
            crate::container::provision_kimi_config(spec.repo, spec.agent_home).map_err(DriverError::Permanent)?;
        }
        start_inner(&spec, &bins).await
    }
    .await;
    if result.is_ok() {
        std::mem::forget(lock_guard);
    }
    result
}

struct StartupGuard {
    sudo: String,
    instance: String,
    run_id: i32,
    run_dir: PathBuf,
    lease_active: bool,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.lease_active {
            self.lease_active = !lease_command_sync(&self.sudo, "stop", &self.instance, self.run_id);
        }
        if !self.lease_active {
            let _ = std::fs::remove_dir_all(&self.run_dir);
        }
    }
}

async fn start_inner(spec: &NspawnSpec<'_>, bins: &Bins) -> Result<NspawnMachine, DriverError> {
    let instance = instance_id(spec.cfg)?.to_string();
    let run_dir = std::env::temp_dir().join(format!("remoter-nspawn-{}-{}", std::process::id(), spec.run_id));
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir)
        .map_err(|e| DriverError::Transient(format!("cannot create {}: {e}", run_dir.display())))?;
    let mut startup = StartupGuard {
        sudo: bins.sudo.clone(),
        instance: instance.clone(),
        run_id: spec.run_id,
        run_dir: run_dir.clone(),
        lease_active: false,
    };

    let closure = resolve_closure_with_bins(spec.cfg, &run_dir.join("closure"), bins).await?;
    startup.lease_active = true;
    let lease = acquire_lease(bins, &instance, spec.run_id, &spec.cfg.nspawn_net_pool_base).await?;
    let mut guard = NspawnMachine {
        sudo: bins.sudo.clone(),
        instance: instance.clone(),
        run_id: spec.run_id,
        machine: lease.machine,
        host_ip: lease.host_ip,
        lease_active: true,
        worktree: spec.worktree.to_path_buf(),
        repo: spec.repo.to_path_buf(),
        run_dir: run_dir.clone(),
        child: None,
    };
    std::mem::forget(startup);

    let boot_log_path = run_dir.join("boot.log");
    let boot_log = std::fs::File::create(&boot_log_path)
        .map_err(|e| DriverError::Transient(format!("nspawn boot log {}: {e}", boot_log_path.display())))?;
    let args = boot_args(&instance, spec.run_id, &closure, spec);
    let child = tokio::process::Command::new(&bins.sudo)
        .arg("-n")
        .args(&args)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(boot_log))
        .spawn()
        .map_err(|e| DriverError::Transient(format!("sudo -n {HELPER_BINARY} boot: {e}")))?;
    guard.child = Some(child);

    let ready = async {
        net_up(bins, &mut guard, &boot_log_path).await?;
        wait_machine_ready(bins, &mut guard, &boot_log_path).await
    }
    .await;
    if let Err(e) = ready {
        guard.teardown().await;
        return Err(e);
    }
    register_host_ip(spec.run_id, guard.host_ip.clone());
    Ok(guard)
}

fn instance_id(cfg: &ExecutionConfig) -> Result<&str, DriverError> {
    cfg.nspawn_instance_id
        .as_deref()
        .filter(|id| valid_instance_id(id))
        .ok_or_else(|| DriverError::Permanent("nspawn daemon instance identity is not initialized".to_string()))
}

async fn acquire_lease(bins: &Bins, instance: &str, run_id: i32, pool_base: &str) -> Result<RunLease, DriverError> {
    let args = [
        HELPER_BINARY.to_string(),
        "acquire".to_string(),
        instance.to_string(),
        run_id.to_string(),
        pool_base.to_string(),
    ];
    parse_lease(&helper(&bins.sudo, &args).await?)
}

fn boot_args(instance: &str, run_id: i32, closure: &Path, spec: &NspawnSpec<'_>) -> Vec<String> {
    let mut args = vec![
        HELPER_BINARY.to_string(),
        "boot".to_string(),
        instance.to_string(),
        run_id.to_string(),
        closure.to_string_lossy().into_owned(),
        spec.worktree.to_string_lossy().into_owned(),
        spec.repo.to_string_lossy().into_owned(),
        spec.agent_home.to_string_lossy().into_owned(),
    ];
    for r in spec.refs {
        args.push("--ref".to_string());
        args.push(r.mount_name.clone());
        args.push(r.worktree.to_string_lossy().into_owned());
        args.push(r.repo.to_string_lossy().into_owned());
    }
    args
}

async fn net_up(bins: &Bins, guard: &mut NspawnMachine, boot_log: &Path) -> Result<(), DriverError> {
    let deadline = std::time::Instant::now() + NET_UP_TIMEOUT;
    let args = [
        HELPER_BINARY.to_string(),
        "net-up".to_string(),
        guard.instance.clone(),
        guard.run_id.to_string(),
    ];
    loop {
        if let Some(dead) = boot_died(guard, boot_log) {
            return Err(DriverError::Transient(format!(
                "nspawn machine died during network setup: {dead}"
            )));
        }
        match helper(&bins.sudo, &args).await {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() >= deadline => return Err(e.context("nspawn net-up")),
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

async fn wait_machine_ready(bins: &Bins, guard: &mut NspawnMachine, boot_log: &Path) -> Result<(), DriverError> {
    let deadline = std::time::Instant::now() + MACHINE_READY_TIMEOUT;
    let args = exec_args(&guard.instance, guard.run_id, &["true"]);
    loop {
        if let Some(dead) = boot_died(guard, boot_log) {
            return Err(DriverError::Transient(format!(
                "nspawn machine died before it accepted exec: {dead}"
            )));
        }
        match helper(&bins.sudo, &args).await {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(DriverError::Transient(format!(
                    "machine {} did not accept exec within {}s ({e})",
                    guard.machine,
                    MACHINE_READY_TIMEOUT.as_secs()
                )));
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

fn exec_args(instance: &str, run_id: i32, cmd: &[&str]) -> Vec<String> {
    let mut args = vec![
        HELPER_BINARY.to_string(),
        "exec".to_string(),
        instance.to_string(),
        run_id.to_string(),
        "--".to_string(),
    ];
    args.extend(cmd.iter().map(|s| s.to_string()));
    args
}

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

async fn helper(sudo: &str, args: &[String]) -> Result<String, DriverError> {
    let out = helper_output(sudo, args).await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn helper_output(sudo: &str, args: &[String]) -> Result<std::process::Output, DriverError> {
    let out = tokio::process::Command::new(sudo)
        .arg("-n")
        .args(args)
        .kill_on_drop(true)
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
    Ok(out)
}

pub async fn resolve_closure(cfg: &ExecutionConfig, root_link: &Path) -> Result<PathBuf, DriverError> {
    resolve_closure_with_bins(cfg, root_link, &Bins::from_cfg(cfg)).await
}

async fn resolve_closure_with_bins(
    cfg: &ExecutionConfig,
    root_link: &Path,
    bins: &Bins,
) -> Result<PathBuf, DriverError> {
    let r = cfg.nspawn_closure_ref.trim();
    if r.is_empty() {
        return Err(DriverError::Permanent(
            "execution.nspawn_closure_ref is empty".to_string(),
        ));
    }
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    let out = if let Some((_, attr)) = r.split_once('#') {
        if attr.is_empty() {
            return Err(DriverError::Permanent(format!(
                "execution.nspawn_closure_ref {r:?} has an empty attribute"
            )));
        }
        tokio::process::Command::new(&bins.nix)
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "build",
                r,
                "--out-link",
            ])
            .arg(root_link)
            .arg("--print-out-paths")
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| DriverError::Transient(format!("nix build {r}: {e}")))?
    } else {
        let requested = PathBuf::from(r.strip_prefix("path:").unwrap_or(r));
        let path = std::fs::canonicalize(&requested).map_err(|e| {
            DriverError::Permanent(format!("cannot resolve nspawn closure {}: {e}", requested.display()))
        })?;
        if !path.starts_with("/nix/store") || !path.is_dir() {
            return Err(DriverError::Permanent(format!(
                "nspawn closure {} must be a directory below /nix/store",
                path.display()
            )));
        }
        tokio::process::Command::new(&bins.nix_store)
            .arg("--realise")
            .arg(&path)
            .arg("--add-root")
            .arg(root_link)
            .arg("--indirect")
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| DriverError::Transient(format!("nix-store --realise {}: {e}", path.display())))?
    };
    if !out.status.success() {
        return Err(DriverError::Transient(format!(
            "nspawn closure realization failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let path = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| PathBuf::from(line.trim()))
        .ok_or_else(|| DriverError::Transient("nspawn closure realization printed no store path".to_string()))?;
    if !path.join("init").is_file() {
        return Err(DriverError::Transient(format!(
            "nspawn closure {} has no init",
            path.display()
        )));
    }
    Ok(path)
}

pub async fn sweep(cfg: &ExecutionConfig) {
    let bins = Bins::from_cfg(cfg);
    if let Err(e) = sweep_with_bins(cfg, &bins).await {
        tracing::warn!("nspawn sweep: {e}");
    }
}

async fn sweep_with_bins(cfg: &ExecutionConfig, bins: &Bins) -> Result<(), DriverError> {
    let args = [
        HELPER_BINARY.to_string(),
        "sweep".to_string(),
        instance_id(cfg)?.to_string(),
    ];
    helper(&bins.sudo, &args).await.map(|_| ())
}

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

    const INSTANCE: &str = "0123456789abcdef0123456789abcdef";

    fn test_cfg() -> ExecutionConfig {
        ExecutionConfig {
            mode: crate::config::ExecutionMode::Nspawn,
            nspawn_closure_ref: "test#agentContainer".to_string(),
            nspawn_instance_id: Some(INSTANCE.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn nspawn_env_carries_no_daemon_provided_services() {
        let env = nspawn_env(7);
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("CI"), Some("1"));
        assert_eq!(get("REMOTER_EXECUTION"), Some("nspawn"));
        assert_eq!(get("REMOTER_AGENT_TASK_ID"), Some("7"));
        assert_eq!(env.len(), 3);
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
    }

    #[test]
    fn instance_identity_is_persistent_and_exclusively_locked() {
        let dir = std::env::temp_dir().join(format!("remoter-nspawn-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let identity = lock_instance(&dir).unwrap();
        assert!(valid_instance_id(identity.id()));
        assert!(lock_instance(&dir).is_err());
        let id = identity.id().to_string();
        drop(identity);
        assert_eq!(
            std::fs::read_to_string(dir.join("nspawn-instance-id")).unwrap().trim(),
            id
        );
        std::fs::remove_dir_all(&dir).ok();
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
            std::fs::write(paths.closure.join("init"), "#!/bin/sh\n").unwrap();
            paths
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    fn test_spec<'a>(cfg: &'a ExecutionConfig, run_id: i32, paths: &'a TestPaths) -> NspawnSpec<'a> {
        NspawnSpec {
            cfg,
            driver_kind: "stub",
            run_id,
            worktree: &paths.worktree,
            repo: &paths.repo,
            agent_home: &paths.agent_home,
            refs: &[],
        }
    }

    fn stub_sudo(dir: &Path, boot_succeeds: bool) -> PathBuf {
        let stub = dir.join("sudo-stub.sh");
        let net_up = if boot_succeeds {
            ""
        } else {
            "if [ \"$3\" = net-up ]; then exit 1; fi\n"
        };
        let boot = if boot_succeeds { "exec sleep 2" } else { "exit 1" };
        crate::testutil::write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$3\" = acquire ]; then echo 'rr-0 ve-rr-0 10.231.0.1 10.231.0.2'; exit 0; fi\nif [ \"$3\" = boot ]; then {boot}; fi\n{net_up}exit 0\n",
                dir.join("sudo.log").display()
            ),
        );
        stub
    }

    fn bins_for(paths: &TestPaths, sudo: &Path) -> Bins {
        let nix = paths.root.join("nix-stub.sh");
        crate::testutil::write_executable_script(
            &nix,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\necho '{}'\n",
                paths.root.join("nix.log").display(),
                paths.closure.display()
            ),
        );
        Bins {
            sudo: sudo.to_string_lossy().into_owned(),
            nix: nix.to_string_lossy().into_owned(),
            nix_store: "nix-store".to_string(),
        }
    }

    #[tokio::test]
    async fn start_boots_through_the_helper_and_teardown_cleans_up() {
        let paths = TestPaths::new("start");
        let sudo = stub_sudo(&paths.root, true);
        let cfg = test_cfg();
        let bins = bins_for(&paths, &sudo);
        let guard = start_inner(&test_spec(&cfg, 77, &paths), &bins).await.unwrap();
        assert_eq!(guard.machine_name(), "rr-0");
        assert_eq!(
            nspawn_api_url_for("http://localhost:8181", 77),
            "http://10.231.0.1:8181"
        );
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains(&format!("remoter-nspawnctl acquire {INSTANCE} 77 10.231")));
        assert!(log.contains(&format!(
            "remoter-nspawnctl boot {INSTANCE} 77 {} {} {} {}",
            paths.closure.display(),
            paths.worktree.display(),
            paths.repo.display(),
            paths.agent_home.display()
        )));
        assert!(log.contains(&format!("remoter-nspawnctl net-up {INSTANCE} 77")));
        assert!(log.contains(&format!("remoter-nspawnctl exec {INSTANCE} 77 -- true")));
        assert!(!log.contains("systemd-nspawn"));
        assert!(!log.contains("systemd-run"));
        assert!(!log.contains("machinectl"));
        let nix_log = std::fs::read_to_string(paths.root.join("nix.log")).unwrap();
        assert!(nix_log.contains("--out-link"), "{nix_log}");
        assert!(!nix_log.contains("--no-link"), "{nix_log}");
        let run_dir = guard.run_dir.clone();
        guard.teardown().await;
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains(&format!("remoter-nspawnctl stop {INSTANCE} 77")));
        assert_eq!(host_ip_for(77), None);
        assert!(!run_dir.exists());
    }

    #[tokio::test]
    async fn drop_without_teardown_still_stops_the_lease() {
        let paths = TestPaths::new("drop");
        let sudo = stub_sudo(&paths.root, true);
        let cfg = test_cfg();
        let bins = bins_for(&paths, &sudo);
        let guard = start_inner(&test_spec(&cfg, 78, &paths), &bins).await.unwrap();
        let run_dir = guard.run_dir.clone();
        drop(guard);
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains(&format!("remoter-nspawnctl stop {INSTANCE} 78")));
        assert_eq!(host_ip_for(78), None);
        assert!(!run_dir.exists());
    }

    #[tokio::test]
    async fn drop_kills_an_unregistered_boot_process_group() {
        let paths = TestPaths::new("drop-unregistered");
        let sudo = paths.root.join("sudo-boot-group.sh");
        crate::testutil::write_executable_script(
            &sudo,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$3\" = acquire ]; then echo 'rr-0 ve-rr-0 10.231.0.1 10.231.0.2'; exit 0; fi\nif [ \"$3\" = boot ]; then sleep 30 & echo $! > '{}'; wait; fi\nexit 0\n",
                paths.root.join("sudo.log").display(),
                paths.root.join("boot.pid").display()
            ),
        );
        let bins = bins_for(&paths, &sudo);
        let cfg = test_cfg();
        let guard = start_inner(&test_spec(&cfg, 83, &paths), &bins).await.unwrap();
        let pid = std::fs::read_to_string(paths.root.join("boot.pid")).unwrap();
        let pid = pid.trim();
        drop(guard);

        let mut reaped = false;
        for _ in 0..100 {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", pid])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&out.stdout);
            if state.trim().is_empty() || state.starts_with('Z') {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(reaped, "unregistered boot process {pid} is still running");
    }

    #[tokio::test]
    async fn failed_readiness_rolls_back_the_boot() {
        let paths = TestPaths::new("rollback");
        let sudo = stub_sudo(&paths.root, false);
        let cfg = test_cfg();
        let bins = bins_for(&paths, &sudo);
        let err = start_inner(&test_spec(&cfg, 79, &paths), &bins).await.err().unwrap();
        assert!(err.to_string().contains("died"), "{err}");
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains(&format!("remoter-nspawnctl stop {INSTANCE} 79")));
    }

    #[tokio::test]
    async fn cancelling_closure_realization_removes_the_run_root() {
        let paths = TestPaths::new("cancel-closure");
        let nix = paths.root.join("nix-hang.sh");
        crate::testutil::write_executable_script(&nix, "#!/bin/sh\nsleep 30\n");
        let bins = Bins {
            sudo: "sudo".to_string(),
            nix: nix.to_string_lossy().into_owned(),
            nix_store: "nix-store".to_string(),
        };
        let cfg = test_cfg();
        let run_dir = std::env::temp_dir().join(format!("remoter-nspawn-{}-81", std::process::id()));
        let spec = test_spec(&cfg, 81, &paths);
        let mut future = Box::pin(start_inner(&spec, &bins));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            _ = &mut future => panic!("closure realization unexpectedly completed"),
        }
        drop(future);
        assert!(!run_dir.exists());
    }

    #[tokio::test]
    async fn cancelling_lease_acquisition_stops_and_removes_startup_state() {
        let paths = TestPaths::new("cancel-acquire");
        let sudo = paths.root.join("sudo-hang.sh");
        crate::testutil::write_executable_script(
            &sudo,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$3\" = acquire ]; then sleep 30; fi\nexit 0\n",
                paths.root.join("sudo.log").display()
            ),
        );
        let bins = bins_for(&paths, &sudo);
        let cfg = test_cfg();
        let run_dir = std::env::temp_dir().join(format!("remoter-nspawn-{}-82", std::process::id()));
        let spec = test_spec(&cfg, 82, &paths);
        let mut future = Box::pin(start_inner(&spec, &bins));
        for _ in 0..100 {
            let started = std::fs::read_to_string(paths.root.join("sudo.log"))
                .is_ok_and(|log| log.contains(&format!("remoter-nspawnctl acquire {INSTANCE} 82")));
            if started {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                _ = &mut future => panic!("lease acquisition unexpectedly completed"),
            }
        }
        drop(future);
        assert!(!run_dir.exists());
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains(&format!("remoter-nspawnctl stop {INSTANCE} 82")), "{log}");
    }

    #[tokio::test]
    async fn provisioning_failure_unlocks_the_worktree() {
        let paths = TestPaths::new("provision-lock");
        std::fs::remove_dir(&paths.worktree).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&paths.repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(paths.repo.join("tracked"), "content\n").unwrap();
        git(&["add", "tracked"]);
        git(&["commit", "-m", "initial"]);
        git(&["worktree", "add", "--detach", paths.worktree.to_str().unwrap()]);
        std::fs::create_dir_all(paths.repo.join(".remoter")).unwrap();
        std::fs::write(paths.repo.join(".remoter/kimi-config.toml"), "model = 'test'\n").unwrap();
        std::fs::remove_dir_all(&paths.agent_home).unwrap();
        std::fs::write(&paths.agent_home, "not a directory").unwrap();
        let cfg = test_cfg();
        let mut spec = test_spec(&cfg, 80, &paths);
        spec.driver_kind = "kimi-acp";

        assert!(start(spec).await.is_err());

        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&paths.repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(!listing.contains("locked"), "{listing}");
    }

    #[tokio::test]
    async fn sweep_is_scoped_to_the_daemon_instance() {
        let paths = TestPaths::new("sweep");
        let sudo = stub_sudo(&paths.root, true);
        let cfg = test_cfg();
        let bins = bins_for(&paths, &sudo);
        sweep_with_bins(&cfg, &bins).await.unwrap();
        let log = std::fs::read_to_string(paths.root.join("sudo.log")).unwrap();
        assert!(log.contains(&format!("remoter-nspawnctl sweep {INSTANCE}")));
        assert!(!log.contains("machinectl"));
    }

    #[test]
    fn exec_args_use_the_helper_contract() {
        assert_eq!(
            exec_args(INSTANCE, 5, &["true"]),
            [HELPER_BINARY, "exec", INSTANCE, "5", "--", "true"]
        );
    }

    #[test]
    fn lease_parser_requires_all_addresses() {
        let lease = parse_lease("rr-2 ve-rr-2 10.231.8.1 10.231.8.2").unwrap();
        assert_eq!(lease.machine, "rr-2");
        assert_eq!(lease.host_ip, "10.231.8.1");
        assert!(parse_lease("rr-2").is_err());
    }

    fn helper_command(
        root: &Path,
        state: &Path,
        policy: &Path,
        path: &std::ffi::OsStr,
        args: &[&str],
    ) -> std::process::Output {
        std::process::Command::new(root.join("scripts/remoter-nspawnctl"))
            .args(args)
            .env("REMOTER_NSPAWNCTL_TEST", "1")
            .env("REMOTER_NSPAWNCTL_STATE_DIR", state)
            .env("REMOTER_NSPAWNCTL_CONFIG", policy)
            .env("PATH", path)
            .output()
            .unwrap()
    }

    #[test]
    fn helper_leases_are_host_wide_and_sweep_is_instance_scoped() {
        if unsafe { libc::getuid() } == 0 {
            return;
        }
        let dir = std::env::temp_dir().join(format!("remoter-nspawn-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let workspace = dir.join("workspace");
        let agent_home = dir.join("agent-home");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&agent_home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let state = dir.join("state");
        let policy = dir.join("nspawnctl.conf");
        std::fs::write(
            &policy,
            format!(
                "workspace_root={}\nagent_home={}\npool_base=10.231\n",
                workspace.display(),
                agent_home.display()
            ),
        )
        .unwrap();
        crate::testutil::write_executable_script(
            &bin.join("machinectl"),
            "#!/bin/sh\necho \"Machine ${2:-unknown} does not exist.\" >&2\nexit 1\n",
        );
        crate::testutil::write_executable_script(&bin.join("ip"), "#!/bin/sh\nexit 0\n");
        crate::testutil::write_executable_script(&bin.join("sysctl"), "#!/bin/sh\nexit 0\n");
        let iptables_log = dir.join("iptables.log");
        crate::testutil::write_executable_script(
            &bin.join("iptables"),
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \" $* \" in *' -C '*) exit 1;; esac\nexit 0\n",
                iptables_log.display()
            ),
        );
        let path = std::env::join_paths(
            std::iter::once(bin.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let instance_b = "fedcba9876543210fedcba9876543210";
        let a = helper_command(&root, &state, &policy, &path, &["acquire", INSTANCE, "7", "10.231"]);
        assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
        assert_eq!(
            String::from_utf8_lossy(&a.stdout).trim(),
            "rr-0 ve-rr-0 10.231.0.1 10.231.0.2"
        );
        let b = helper_command(&root, &state, &policy, &path, &["acquire", instance_b, "7", "10.231"]);
        assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
        assert_eq!(
            String::from_utf8_lossy(&b.stdout).trim(),
            "rr-1 ve-rr-1 10.231.4.1 10.231.4.2"
        );
        let again = helper_command(&root, &state, &policy, &path, &["acquire", INSTANCE, "7", "10.231"]);
        assert_eq!(again.stdout, a.stdout);

        let net = helper_command(&root, &state, &policy, &path, &["net-up", instance_b, "7"]);
        assert!(net.status.success(), "{}", String::from_utf8_lossy(&net.stderr));
        let rules = std::fs::read_to_string(&iptables_log).unwrap();
        assert!(
            rules.contains("-I FORWARD 1 -i ve-rr-+ -o ve-rr-+ -j REJECT"),
            "{rules}"
        );
        let rejected = helper_command(&root, &state, &policy, &path, &["exec", instance_b, "7", "--help"]);
        assert!(!rejected.status.success());

        let swept = helper_command(&root, &state, &policy, &path, &["sweep", INSTANCE]);
        assert!(swept.status.success(), "{}", String::from_utf8_lossy(&swept.stderr));
        let b_still_live = helper_command(&root, &state, &policy, &path, &["acquire", instance_b, "7", "10.231"]);
        assert_eq!(b_still_live.stdout, b.stdout);
        let a_reallocated = helper_command(&root, &state, &policy, &path, &["acquire", INSTANCE, "7", "10.231"]);
        assert_eq!(a_reallocated.stdout, a.stdout);

        crate::testutil::write_executable_script(
            &bin.join("machinectl"),
            "#!/bin/sh\necho 'DBus connection temporarily unavailable' >&2\nexit 1\n",
        );
        let unknown = helper_command(&root, &state, &policy, &path, &["stop", instance_b, "7"]);
        assert!(!unknown.status.success());
        let b_retained = helper_command(&root, &state, &policy, &path, &["acquire", instance_b, "7", "10.231"]);
        assert_eq!(b_retained.stdout, b.stdout);
        std::fs::remove_dir_all(&dir).ok();
    }
}
