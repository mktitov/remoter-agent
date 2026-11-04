use anyhow::{Context, bail};
use rusqlite::{Connection, OpenFlags, backup::Backup};
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const INTERNAL_LAUNCH_ARG: &str = "__bwrap-launch";

const OWNER_FILE: &str = "owner";
const RUN_PREFIX: &str = "bwrap-run-";
const NIX_DB: &str = "/nix/var/nix/db/db.sqlite";
const NIX_SCHEMA: &str = "/nix/var/nix/db/schema";
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-bundle.crt";
const NETWORK_TIMEOUT: Duration = Duration::from_secs(20);
const NIX_CONFIG: &str =
    "experimental-features = nix-command flakes\nextra-experimental-features = read-only-local-store";
const GUEST_SCRIPT: &str = r#"
devenv_enabled=$1
project_dir=$2
shift 2
if [ "$devenv_enabled" = "1" ]; then
    cd "$project_dir" || exit
    processes=$(devenv --nix-option extra-experimental-features read-only-local-store --no-eval-cache eval processes) || exit
    processes=$(printf '%s' "$processes" | tr -d '[:space:]')
    if [ "$processes" != '{"processes":{}}' ]; then
        trap 'devenv --nix-option extra-experimental-features read-only-local-store --no-eval-cache processes down >&2 2>/dev/null || :' EXIT
        devenv --nix-option extra-experimental-features read-only-local-store --no-eval-cache processes down >&2 2>/dev/null || :
        devenv --nix-option extra-experimental-features read-only-local-store --no-eval-cache up -d >&2 || exit
        devenv --nix-option extra-experimental-features read-only-local-store --no-eval-cache processes wait --timeout 120 >&2 || exit
    fi
    devenv --nix-option extra-experimental-features read-only-local-store --no-eval-cache shell --no-tui -- "$@"
    status=$?
    exit "$status"
fi
exec "$@"
"#;

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct LaunchOptions {
    bwrap: String,
    slirp4netns: String,
    agent_home: PathBuf,
    state_root: PathBuf,
    worktree: PathBuf,
    devenv: bool,
    retain: Vec<String>,
    env: Vec<(String, String)>,
    command: Vec<String>,
}

pub fn api_url(input: &str) -> String {
    let Some((scheme, _)) = input.split_once("://") else {
        return input.to_string();
    };
    if !["http", "https", "ws", "wss"]
        .into_iter()
        .any(|candidate| scheme.eq_ignore_ascii_case(candidate))
    {
        return input.to_string();
    }
    let authority_start = scheme.len() + 3;
    let authority_end = input[authority_start..]
        .find(['/', '?', '#'])
        .map_or(input.len(), |offset| authority_start + offset);
    let authority = &input[authority_start..authority_end];
    let host_start = authority
        .rfind('@')
        .map_or(authority_start, |at| authority_start + at + 1);
    let host_port = &input[host_start..authority_end];
    let host_end = if host_port.starts_with('[') {
        host_port.find(']').map(|end| host_start + end + 1)
    } else {
        host_port
            .find(':')
            .map_or(Some(authority_end), |end| Some(host_start + end))
    };
    let Some(host_end) = host_end else {
        return input.to_string();
    };
    let host = &input[host_start..host_end];
    if !(host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]" || host == "0.0.0.0") {
        return input.to_string();
    }
    let mut rewritten = String::with_capacity(input.len() + 3);
    rewritten.push_str(&input[..host_start]);
    rewritten.push_str("10.0.2.2");
    rewritten.push_str(&input[host_end..]);
    rewritten
}

pub fn run_env(task_id: i32) -> Vec<(String, String)> {
    vec![
        ("CI".to_string(), "1".to_string()),
        ("REMOTER_EXECUTION".to_string(), "bwrap".to_string()),
        ("REMOTER_AGENT_TASK_ID".to_string(), task_id.to_string()),
    ]
}

pub async fn prewarm(
    worktree: &Path,
    agent_home: &Path,
    env: &[(String, String)],
) -> Result<(), crate::driver::DriverError> {
    prewarm_inner(worktree, agent_home, env)
        .await
        .map_err(|error| crate::driver::DriverError::Transient(format!("devenv pre-warm: {error:#}")))
}

async fn prewarm_inner(worktree: &Path, agent_home: &Path, env: &[(String, String)]) -> anyhow::Result<()> {
    require_absolute(worktree, "worktree")?;
    require_absolute(agent_home, "agent home")?;
    if !worktree.is_dir() {
        bail!("worktree {} is not a directory", worktree.display());
    }
    fs::create_dir_all(agent_home.join(".cache"))
        .with_context(|| format!("create Nix cache under {}", agent_home.display()))?;
    let environment = merge_supplied_env(host_environment(agent_home), env);
    let mut command = tokio::process::Command::new("devenv");
    command
        .args(["--no-eval-cache", "shell", "--no-tui", "--", "true"])
        .current_dir(worktree)
        .env_clear()
        .envs(environment)
        .kill_on_drop(true);
    let output = command.output().await.context("start host devenv shell")?;
    if !output.status.success() {
        bail!(
            "host shell exited with {}: {}",
            display_status(output.status),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn sweep(state_root: &Path) {
    let Ok(entries) = fs::read_dir(state_root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(pid) = recognized_run_pid(&entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let Ok(owner) = read_owner(&path) else {
            continue;
        };
        if owner.pid != pid {
            continue;
        }
        match process_starttime(owner.pid) {
            Ok(starttime) if starttime == owner.starttime => {}
            Ok(_) => remove_stale_dir(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => remove_stale_dir(&path),
            Err(_) => {}
        }
    }
}

pub fn validate_prerequisites(bwrap: &str, slirp4netns: &str) -> anyhow::Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("bubblewrap execution is supported only on Linux");
    }
    check_version(bwrap, "bubblewrap")?;
    check_version(slirp4netns, "slirp4netns")?;
    for path in [Path::new("/nix/store"), Path::new(NIX_DB), Path::new(NIX_SCHEMA)] {
        if !path.exists() {
            bail!("required Nix path {} is missing", path.display());
        }
    }
    let db = open_source_db(Path::new(NIX_DB))?;
    db.query_row("SELECT 1", [], |_| Ok(()))
        .context("read the host Nix database")?;
    drop(db);
    ca_bundle()?;
    let shell = resolve_store_shell()?;
    resolve_store_tool("env")?;
    find_host_tool_on_path("nix-store").context("nix-store must be available on the host PATH")?;
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let output = Command::new(bwrap)
        .args([
            OsString::from("--unshare-user"),
            OsString::from("--unshare-pid"),
            OsString::from("--unshare-ipc"),
            OsString::from("--unshare-uts"),
            OsString::from("--unshare-net"),
            OsString::from("--uid"),
            OsString::from(uid.to_string()),
            OsString::from("--gid"),
            OsString::from(gid.to_string()),
            OsString::from("--die-with-parent"),
            OsString::from("--new-session"),
            OsString::from("--proc"),
            OsString::from("/proc"),
            OsString::from("--dev"),
            OsString::from("/dev"),
            OsString::from("--tmpfs"),
            OsString::from("/tmp"),
            OsString::from("--ro-bind"),
            OsString::from("/nix/store"),
            OsString::from("/nix/store"),
            OsString::from("--dir"),
            OsString::from("/bin"),
            OsString::from("--symlink"),
            shell.into_os_string(),
            OsString::from("/bin/sh"),
            OsString::from("--"),
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from("true"),
        ])
        .output()
        .context("run the bubblewrap namespace smoke test")?;
    if !output.status.success() {
        bail!(
            "bubblewrap namespace smoke test failed with {}: {}",
            display_status(output.status),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn terminate_with_daemon() -> anyhow::Result<()> {
    // SAFETY: getppid and prctl do not access Rust memory; SIGKILL cannot be handled or ignored.
    let parent = unsafe { libc::getppid() };
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        return Err(std::io::Error::last_os_error()).context("set launcher parent-death signal");
    }
    // SAFETY: getppid is always valid in the current process.
    if unsafe { libc::getppid() } != parent {
        bail!("daemon parent exited during launcher startup");
    }
    Ok(())
}

pub fn launch_from_args(args: &[String]) -> anyhow::Result<i32> {
    terminate_with_daemon()?;
    let mut options = parse_args(args)?;
    prepare_launch_paths(&mut options)?;
    sweep(&options.state_root);
    let run_dir = RunDir::create(&options.state_root)?;
    let shell = resolve_store_shell()?;
    let env_tool = resolve_store_tool("env")?;
    let cert = ca_bundle()?;
    retain_store_paths(&options, run_dir.path())?;
    snapshot_nix_db(Path::new(NIX_DB), Path::new(NIX_SCHEMA), &run_dir.path().join("nix-db"))?;
    write_run_files(&options, run_dir.path())?;

    let status = cloexec_pipe().context("create bubblewrap status pipe")?;
    let gate = cloexec_pipe().context("create bubblewrap gate pipe")?;
    let arguments = bwrap_arguments(
        &options,
        run_dir.path(),
        &cert,
        &shell,
        &env_tool,
        status.1.as_raw_fd(),
        gate.0.as_raw_fd(),
    )?;
    let mut command = Command::new(&options.bwrap);
    command.args(arguments);
    pass_fds(&mut command, [status.1.as_raw_fd(), gate.0.as_raw_fd()]);
    let child = command.spawn().context("start bubblewrap")?;
    drop(status.1);
    drop(gate.0);
    let mut processes = ProcessGuard {
        bwrap: child,
        slirp: None,
        slirp_exit: None,
    };

    let child_pid = read_child_pid(&status.0)?;
    let exit = cloexec_pipe().context("create slirp4netns exit pipe")?;
    let ready = cloexec_pipe().context("create slirp4netns readiness pipe")?;
    processes.slirp_exit = Some(exit.1);
    let mut network = Command::new(&options.slirp4netns);
    network
        .args(["--configure", "--mtu=65520"])
        .arg(format!("--exit-fd={}", exit.0.as_raw_fd()))
        .arg(format!("--ready-fd={}", ready.1.as_raw_fd()))
        .arg(child_pid.to_string())
        .arg("tap0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    pass_fds(&mut network, [exit.0.as_raw_fd(), ready.1.as_raw_fd()]);
    let network = network.spawn().context("start slirp4netns")?;
    processes.slirp = Some(network);
    drop(exit.0);
    drop(ready.1);
    wait_network_ready(ready.0)?;

    let mut gate = File::from(gate.1);
    gate.write_all(b"1").context("release the bubblewrap gate")?;
    drop(gate);
    let status = processes.bwrap.wait().context("wait for bubblewrap")?;
    Ok(exit_code(status))
}

#[derive(Debug)]
struct Owner {
    pid: u32,
    starttime: u64,
}

struct RunDir {
    path: PathBuf,
}

impl RunDir {
    fn create(state_root: &Path) -> anyhow::Result<Self> {
        let pid = std::process::id();
        let starttime = process_starttime(pid).context("read this launcher's /proc start time")?;
        for _ in 0..128 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let sequence = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
            let path = state_root.join(format!("{RUN_PREFIX}{pid}-{nonce}{sequence}"));
            match fs::create_dir(&path) {
                Ok(()) => {
                    if let Err(error) = write_owner(&path, Owner { pid, starttime }) {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
            }
        }
        bail!(
            "could not allocate a unique launch directory under {}",
            state_root.display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn recognized_run_pid(name: &OsStr) -> Option<u32> {
    let name = name.to_str()?;
    let suffix = name.strip_prefix(RUN_PREFIX)?;
    let (pid, nonce) = suffix.split_once('-')?;
    if pid.is_empty()
        || nonce.is_empty()
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || !nonce.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    pid.parse().ok()
}

fn write_owner(run_dir: &Path, owner: Owner) -> anyhow::Result<()> {
    fs::write(
        run_dir.join(OWNER_FILE),
        format!("{}\n{}\n", owner.pid, owner.starttime),
    )
    .with_context(|| format!("write launcher owner metadata in {}", run_dir.display()))
}

fn read_owner(run_dir: &Path) -> anyhow::Result<Owner> {
    let content = fs::read_to_string(run_dir.join(OWNER_FILE))?;
    let mut lines = content.lines();
    let pid = lines.next().context("missing launcher PID")?.parse()?;
    let starttime = lines.next().context("missing launcher start time")?.parse()?;
    if lines.next().is_some() {
        bail!("unexpected launcher owner metadata");
    }
    Ok(Owner { pid, starttime })
}

fn process_starttime(pid: u32) -> std::io::Result<u64> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after_comm = content
        .rfind(") ")
        .map(|end| &content[end + 2..])
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed /proc stat record"))?;
    after_comm
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing /proc start time"))?
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid /proc start time"))
}

fn remove_stale_dir(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn parse_args(args: &[String]) -> anyhow::Result<LaunchOptions> {
    let args = if args.first().is_some_and(|arg| arg == INTERNAL_LAUNCH_ARG) {
        &args[1..]
    } else {
        args
    };
    let mut bwrap = None;
    let mut slirp4netns = None;
    let mut agent_home = None;
    let mut state_root = None;
    let mut worktree = None;
    let mut devenv = None;
    let mut retain = Vec::new();
    let mut environment = Vec::new();
    let mut index = 0;
    let command = loop {
        let Some(argument) = args.get(index) else {
            bail!("missing -- before the agent program");
        };
        if argument == "--" {
            index += 1;
            break &args[index..];
        }
        let option = argument.as_str();
        if option == "--retain" {
            retain.push(option_value(args, index, option)?.to_string());
            index += 2;
            continue;
        }
        if option == "--env" {
            let value = option_value(args, index, option)?;
            let (key, value) = value.split_once('=').context("--env requires K=V")?;
            validate_env_entry(key, value)?;
            environment.push((key.to_string(), value.to_string()));
            index += 2;
            continue;
        }
        let value = option_value(args, index, option)?;
        let slot = match option {
            "--bwrap" => &mut bwrap,
            "--slirp4netns" => &mut slirp4netns,
            "--agent-home" => &mut agent_home,
            "--state-root" => &mut state_root,
            "--worktree" => &mut worktree,
            "--devenv" => &mut devenv,
            _ => bail!("unknown launcher option {option}"),
        };
        if slot.replace(value.to_string()).is_some() {
            bail!("duplicate launcher option {option}");
        }
        index += 2;
    };
    if command.first().is_none_or(|program| program.is_empty()) {
        bail!("an agent program is required after --");
    }
    let bwrap = required_option(bwrap, "--bwrap")?;
    let slirp4netns = required_option(slirp4netns, "--slirp4netns")?;
    if bwrap.is_empty() || slirp4netns.is_empty() {
        bail!("--bwrap and --slirp4netns must not be empty");
    }
    let agent_home = PathBuf::from(required_option(agent_home, "--agent-home")?);
    let state_root = PathBuf::from(required_option(state_root, "--state-root")?);
    let worktree = PathBuf::from(required_option(worktree, "--worktree")?);
    require_absolute(&agent_home, "agent home")?;
    require_absolute(&state_root, "state root")?;
    require_absolute(&worktree, "worktree")?;
    if agent_home == Path::new("/") || worktree == Path::new("/") {
        bail!("agent home and worktree must not be the filesystem root");
    }
    let devenv = match required_option(devenv, "--devenv")?.as_str() {
        "0" => false,
        "1" => true,
        _ => bail!("--devenv must be 0 or 1"),
    };
    Ok(LaunchOptions {
        bwrap,
        slirp4netns,
        agent_home,
        state_root,
        worktree,
        devenv,
        retain,
        env: environment,
        command: command.to_vec(),
    })
}

fn option_value<'a>(args: &'a [String], index: usize, option: &str) -> anyhow::Result<&'a str> {
    if !matches!(
        option,
        "--bwrap"
            | "--slirp4netns"
            | "--agent-home"
            | "--state-root"
            | "--worktree"
            | "--devenv"
            | "--retain"
            | "--env"
    ) {
        bail!("unknown launcher option {option}");
    }
    args.get(index + 1)
        .map(String::as_str)
        .filter(|value| !value.starts_with("--"))
        .with_context(|| format!("missing value for {option}"))
}

fn required_option(value: Option<String>, name: &str) -> anyhow::Result<String> {
    value.with_context(|| format!("missing required option {name}"))
}

fn validate_env_entry(key: &str, _value: &str) -> anyhow::Result<()> {
    if key.is_empty() || key.contains(['=', '\0']) {
        bail!("invalid environment variable name");
    }
    Ok(())
}

fn require_absolute(path: &Path, description: &str) -> anyhow::Result<()> {
    if !path.is_absolute() {
        bail!("{description} must be an absolute path: {}", path.display());
    }
    Ok(())
}

fn prepare_launch_paths(options: &mut LaunchOptions) -> anyhow::Result<()> {
    if !options.worktree.is_dir() {
        bail!("worktree {} is not a directory", options.worktree.display());
    }
    fs::create_dir_all(&options.agent_home)
        .with_context(|| format!("create agent home {}", options.agent_home.display()))?;
    fs::create_dir_all(&options.state_root)
        .with_context(|| format!("create state root {}", options.state_root.display()))?;
    Ok(())
}

fn host_path() -> String {
    env::var("PATH").unwrap_or_else(|_| "/run/current-system/sw/bin:/usr/bin:/bin".to_string())
}

fn username() -> String {
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .ok()
        .filter(|name| !name.is_empty() && !name.contains([':', '\n', '\r']))
        .unwrap_or_else(|| "agent".to_string())
}

fn host_environment(agent_home: &Path) -> Vec<(String, String)> {
    let user = username();
    vec![
        ("PATH".to_string(), host_path()),
        ("HOME".to_string(), agent_home.to_string_lossy().into_owned()),
        ("USER".to_string(), user.clone()),
        ("LOGNAME".to_string(), user),
        (
            "XDG_CACHE_HOME".to_string(),
            agent_home.join(".cache").to_string_lossy().into_owned(),
        ),
        ("XDG_RUNTIME_DIR".to_string(), "/tmp".to_string()),
        ("CI".to_string(), "1".to_string()),
    ]
}

fn sandbox_environment(options: &LaunchOptions, cert: &Path) -> Vec<(String, String)> {
    let mut base = host_environment(&options.agent_home);
    base.extend([
        ("REMOTER_EXECUTION".to_string(), "bwrap".to_string()),
        ("NIX_REMOTE".to_string(), "local?read-only=true".to_string()),
        ("NIX_CONFIG".to_string(), NIX_CONFIG.to_string()),
        ("SSL_CERT_FILE".to_string(), cert.to_string_lossy().into_owned()),
        ("NIX_SSL_CERT_FILE".to_string(), cert.to_string_lossy().into_owned()),
    ]);
    merge_supplied_env(base, &options.env)
}

fn merge_supplied_env(base: Vec<(String, String)>, supplied: &[(String, String)]) -> Vec<(String, String)> {
    let mut merged = base;
    for (key, value) in supplied {
        if forbidden_env(key) || merged.iter().any(|(existing, _)| existing == key) {
            continue;
        }
        merged.push((key.clone(), value.clone()));
    }
    merged
}

fn forbidden_env(key: &str) -> bool {
    matches!(
        key,
        "REMOTER_TOKEN" | "REMOTER_CONTAINER" | "REMOTER_AGENT_PORT_BASE" | "DATABASE_URL"
    ) || key.ends_with("_DATABASE_URL")
}

fn write_run_files(options: &LaunchOptions, run_dir: &Path) -> anyhow::Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let user = username();
    let files = [
        ("resolv.conf", "nameserver 10.0.2.3\n".to_string()),
        (
            "passwd",
            format!("{user}:x:{uid}:{gid}:Agent:{}:/bin/sh\n", options.agent_home.display()),
        ),
        ("group", format!("agent:x:{gid}:\n")),
        (
            "gitconfig",
            "[safe]\n\tdirectory = *\n[user]\n\tname = remoter-agent\n\temail = remoter-agent@localhost\n".to_string(),
        ),
    ];
    for (name, content) in files {
        fs::write(run_dir.join(name), content).with_context(|| format!("write sandbox {name}"))?;
    }
    Ok(())
}

fn open_source_db(path: &Path) -> anyhow::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("open host Nix database {} read-only", path.display()))
}

fn snapshot_nix_db(source: &Path, schema: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::create_dir(destination).with_context(|| format!("create {}", destination.display()))?;
    let source = open_source_db(source)?;
    let mut target = Connection::open(destination.join("db.sqlite")).context("create per-run Nix database")?;
    {
        let backup = Backup::new(&source, &mut target).context("start Nix database online backup")?;
        backup
            .run_to_completion(256, Duration::from_millis(10), None)
            .context("copy Nix database online backup")?;
    }
    target
        .close()
        .map_err(|(_, error)| error)
        .context("close per-run Nix database")?;
    drop(source);
    fs::copy(schema, destination.join("schema")).context("copy Nix database schema")?;
    File::create(destination.join("reserved")).context("create Nix database reservation file")?;
    File::create(destination.join("big-lock")).context("create Nix database lock file")?;
    Ok(())
}

fn ca_bundle() -> anyhow::Result<PathBuf> {
    let cert = Path::new(CA_BUNDLE)
        .canonicalize()
        .context("resolve the system CA bundle")?;
    if !cert.is_file() || !cert.starts_with("/nix/store") {
        bail!("a CA bundle in /nix/store is required");
    }
    Ok(cert)
}

fn resolve_store_shell() -> anyhow::Result<PathBuf> {
    resolve_store_tool("sh").or_else(|_| resolve_store_tool("bash"))
}

fn resolve_store_tool(name: &str) -> anyhow::Result<PathBuf> {
    let tool = find_host_tool(name).with_context(|| format!("{name} must be available on the host PATH"))?;
    if !tool.starts_with("/nix/store") {
        bail!("{name} must resolve into /nix/store, got {}", tool.display());
    }
    Ok(tool)
}

fn find_host_tool(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return fs::canonicalize(name).ok().filter(|path| is_executable(path));
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|entry| entry.join(name))
        .find_map(|candidate| fs::canonicalize(candidate).ok().filter(|path| is_executable(path)))
}

fn find_host_tool_on_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return is_executable(Path::new(name)).then(|| PathBuf::from(name));
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|entry| entry.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    path.is_file() && fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

fn retain_store_paths(options: &LaunchOptions, run_dir: &Path) -> anyhow::Result<()> {
    let current = env::current_exe()
        .and_then(|path| path.canonicalize())
        .context("resolve the current executable for Nix GC retention")?;
    let mut store_paths = BTreeSet::new();
    if current.starts_with("/nix/store") {
        store_paths.insert(current);
    }
    for program in std::iter::once(&options.command[0]).chain(&options.retain) {
        let path = resolve_program(program, &options.worktree)?;
        if path.starts_with("/nix/store") {
            store_paths.insert(path);
        }
    }
    if store_paths.is_empty() {
        return Ok(());
    }
    let nix_store = find_host_tool_on_path("nix-store")
        .context("nix-store is required to retain sandbox executables in /nix/store")?;
    for (index, path) in store_paths.into_iter().enumerate() {
        let root = run_dir.join(format!("gc-root-{index}"));
        let output = Command::new(&nix_store)
            .arg("--realise")
            .arg(&path)
            .arg("--add-root")
            .arg(&root)
            .output()
            .with_context(|| format!("retain {} with a Nix GC root", path.display()))?;
        if !output.status.success() {
            bail!(
                "could not retain {} with a Nix GC root: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(())
}

fn resolve_program(program: &str, worktree: &Path) -> anyhow::Result<PathBuf> {
    if program.contains('/') {
        let path = Path::new(program);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            worktree.join(path)
        };
        return path
            .canonicalize()
            .with_context(|| format!("resolve program {program} for Nix GC retention"));
    }
    find_host_tool(program).with_context(|| format!("program {program} is not on the host PATH"))
}

struct Mounts {
    destinations: BTreeSet<PathBuf>,
    rw_roots: Vec<PathBuf>,
}

impl Mounts {
    fn new() -> Self {
        Self {
            destinations: BTreeSet::new(),
            rw_roots: Vec::new(),
        }
    }

    fn bind(&mut self, args: &mut Vec<String>, source: &Path, destination: &Path, writable: bool) {
        if !self.destinations.insert(destination.to_path_buf()) {
            return;
        }
        args.push(if writable { "--bind" } else { "--ro-bind" }.to_string());
        args.push(source.to_string_lossy().into_owned());
        args.push(destination.to_string_lossy().into_owned());
        if writable {
            self.rw_roots.push(destination.to_path_buf());
        }
    }
}

fn bwrap_arguments(
    options: &LaunchOptions,
    run_dir: &Path,
    cert: &Path,
    shell: &Path,
    env_tool: &Path,
    status_fd: RawFd,
    gate_fd: RawFd,
) -> anyhow::Result<Vec<String>> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mut args = vec![
        "--unshare-user".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
        "--unshare-net".to_string(),
        "--uid".to_string(),
        uid.to_string(),
        "--gid".to_string(),
        gid.to_string(),
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--tmpfs".to_string(),
        "/run".to_string(),
        "--dir".to_string(),
        "/etc".to_string(),
    ];
    let mut mounts = Mounts::new();
    mounts.bind(&mut args, Path::new("/nix/store"), Path::new("/nix/store"), false);
    args.extend(["--dir".to_string(), "/nix/var/nix".to_string()]);
    mounts.bind(&mut args, &run_dir.join("nix-db"), Path::new("/nix/var/nix/db"), true);
    mounts.bind(&mut args, &options.worktree, &options.worktree, true);
    args.extend([
        "--symlink".to_string(),
        options.worktree.to_string_lossy().into_owned(),
        "/work".to_string(),
    ]);
    mounts.bind(&mut args, &options.agent_home, &options.agent_home, true);
    if Path::new("/etc/ssl").exists() {
        args.extend([
            "--ro-bind-try".to_string(),
            "/etc/ssl".to_string(),
            "/etc/ssl".to_string(),
        ]);
    }
    for (name, destination) in [
        ("resolv.conf", "/etc/resolv.conf"),
        ("passwd", "/etc/passwd"),
        ("group", "/etc/group"),
        ("gitconfig", "/etc/gitconfig"),
    ] {
        mounts.bind(&mut args, &run_dir.join(name), Path::new(destination), false);
    }
    add_git_mounts(options, &mut args, &mut mounts)?;
    append_path_mounts(
        &mut args,
        &mut mounts,
        env::split_paths(&env::var_os("PATH").unwrap_or_default()),
    );
    args.extend([
        "--dir".to_string(),
        "/bin".to_string(),
        "--symlink".to_string(),
        shell.to_string_lossy().into_owned(),
        "/bin/sh".to_string(),
        "--dir".to_string(),
        "/usr/bin".to_string(),
        "--symlink".to_string(),
        env_tool.to_string_lossy().into_owned(),
        "/usr/bin/env".to_string(),
        "--chdir".to_string(),
        options.worktree.to_string_lossy().into_owned(),
        "--clearenv".to_string(),
    ]);
    for (key, value) in sandbox_environment(options, cert) {
        args.extend(["--setenv".to_string(), key, value]);
    }
    args.extend([
        "--json-status-fd".to_string(),
        status_fd.to_string(),
        "--block-fd".to_string(),
        gate_fd.to_string(),
        "--".to_string(),
    ]);
    if options.devenv {
        args.extend([
            "/bin/sh".to_string(),
            "-c".to_string(),
            GUEST_SCRIPT.to_string(),
            "remoter-bwrap".to_string(),
            "1".to_string(),
            options.worktree.to_string_lossy().into_owned(),
        ]);
    }
    args.extend(options.command.iter().cloned());
    Ok(args)
}

struct TrustedGit {
    gitdir: PathBuf,
    common: PathBuf,
    common_alias: PathBuf,
}

fn add_git_mounts(options: &LaunchOptions, args: &mut Vec<String>, mounts: &mut Mounts) -> anyhow::Result<()> {
    let canonical_state = options
        .state_root
        .canonicalize()
        .context("canonicalize bwrap state root")?;
    let workspace_root = canonical_state
        .parent()
        .context("bwrap state root has no workspace parent")?
        .to_path_buf();
    let workspace_alias = options
        .state_root
        .parent()
        .context("bwrap state root has no original workspace parent")?
        .to_path_buf();
    let main = trusted_main_git(options, &workspace_root, &workspace_alias)?;
    if validate_git_metadata(&options.worktree, &main)?.is_some() {
        bind_validated_git(args, mounts, &main, true);
    }

    let refs_dir = options.worktree.join(".refs");
    let entries = match fs::read_dir(&refs_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", refs_dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", refs_dir.display()))?;
        if !entry.file_type()?.is_symlink() {
            continue;
        }
        let link = entry.path();
        let target = link
            .canonicalize()
            .with_context(|| format!("resolve reference worktree symlink {}", link.display()))?;
        let (reference, target_alias) = trusted_reference_git(&target, &workspace_root, &workspace_alias)?;
        mounts.bind(args, &target, &target, false);
        mounts.bind(args, &target, &target_alias, false);
        if validate_git_metadata(&target, &reference)?.is_none() {
            bail!("reference worktree {} has no Git metadata", target.display());
        }
        bind_validated_git(args, mounts, &reference, false);
    }
    Ok(())
}

fn trusted_main_git(
    options: &LaunchOptions,
    workspace_root: &Path,
    workspace_alias: &Path,
) -> anyhow::Result<TrustedGit> {
    let worktree = options
        .worktree
        .canonicalize()
        .context("canonicalize main worktree for Git validation")?;
    let relative = worktree.strip_prefix(workspace_root).with_context(|| {
        format!(
            "main worktree {} is outside trusted workspace {}",
            worktree.display(),
            workspace_root.display()
        )
    })?;
    let components: Vec<&OsStr> = relative.components().map(|component| component.as_os_str()).collect();
    let [project, leaf] = components.as_slice() else {
        bail!(
            "main worktree {} does not match p<ID>/wt-<task> or p<ID>/repo",
            worktree.display()
        );
    };
    if !valid_layout_component(project, "p") {
        bail!("main worktree {} has an invalid project directory", worktree.display());
    }
    let central = *leaf == OsStr::new("repo");
    if !central && !valid_layout_component(leaf, "wt-") {
        bail!(
            "main worktree {} does not match p<ID>/wt-<task> or p<ID>/repo",
            worktree.display()
        );
    }
    let common = workspace_root.join(project).join("repo/.git");
    let common_alias = workspace_alias.join(project).join("repo/.git");
    let gitdir = if central {
        common.clone()
    } else {
        common.join("worktrees").join(leaf)
    };
    Ok(TrustedGit {
        gitdir,
        common,
        common_alias,
    })
}

fn trusted_reference_git(
    target: &Path,
    workspace_root: &Path,
    workspace_alias: &Path,
) -> anyhow::Result<(TrustedGit, PathBuf)> {
    if !target.is_dir() {
        bail!("reference worktree {} is not a directory", target.display());
    }
    let relative = target.strip_prefix(workspace_root).with_context(|| {
        format!(
            "reference worktree {} is outside trusted workspace {}",
            target.display(),
            workspace_root.display()
        )
    })?;
    let components: Vec<&OsStr> = relative.components().map(|component| component.as_os_str()).collect();
    let [project, refs, mount, leaf] = components.as_slice() else {
        bail!(
            "reference worktree {} does not match p<ID>/refs/<mount>/wt-<task>",
            target.display()
        );
    };
    if !valid_layout_component(project, "p") || *refs != OsStr::new("refs") || !valid_layout_component(leaf, "wt-") {
        bail!(
            "reference worktree {} does not match p<ID>/refs/<mount>/wt-<task>",
            target.display()
        );
    }
    let common = workspace_root.join(project).join("refs").join(mount).join("repo/.git");
    let common_alias = workspace_alias.join(project).join("refs").join(mount).join("repo/.git");
    let gitdir = common.join("worktrees").join(leaf);
    Ok((
        TrustedGit {
            gitdir,
            common,
            common_alias,
        },
        workspace_alias.join(relative),
    ))
}

fn valid_layout_component(component: &OsStr, prefix: &str) -> bool {
    component
        .to_str()
        .and_then(|component| component.strip_prefix(prefix))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

fn validate_git_metadata(worktree: &Path, expected: &TrustedGit) -> anyhow::Result<Option<PathBuf>> {
    let dot_git = worktree.join(".git");
    let metadata = match fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", dot_git.display())),
    };
    let (gitdir, commondir) = if metadata.is_dir() {
        (
            dot_git.canonicalize().context("resolve repository Git directory")?,
            None,
        )
    } else if metadata.is_file() {
        let content = fs::read_to_string(&dot_git).with_context(|| format!("read Git file {}", dot_git.display()))?;
        let path = content
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .with_context(|| format!("invalid Git file {}", dot_git.display()))?;
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            worktree.join(path)
        };
        let gitdir = path
            .canonicalize()
            .with_context(|| format!("resolve Git directory from {}", dot_git.display()))?;
        let commondir = match fs::read_to_string(gitdir.join("commondir")) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("read commondir in {}", gitdir.display()));
            }
        };
        (gitdir, commondir)
    } else {
        bail!("unsupported Git metadata at {}", dot_git.display());
    };
    if gitdir != expected.gitdir || !gitdir.is_dir() {
        bail!(
            "Git directory {} does not match trusted {}",
            gitdir.display(),
            expected.gitdir.display()
        );
    }
    let common = match commondir {
        Some(content) => {
            let common = Path::new(content.trim());
            if common.as_os_str().is_empty() {
                bail!("empty commondir in {}", gitdir.display());
            }
            let common = if common.is_absolute() {
                common.to_path_buf()
            } else {
                gitdir.join(common)
            };
            common.canonicalize().context("resolve Git commondir")?
        }
        None if gitdir
            .parent()
            .is_some_and(|parent| parent.file_name() == Some(OsStr::new("worktrees"))) =>
        {
            gitdir.parent().and_then(Path::parent).unwrap_or(&gitdir).to_path_buf()
        }
        None => gitdir.clone(),
    };
    let canonical_expected = expected
        .common
        .canonicalize()
        .with_context(|| format!("resolve trusted Git directory {}", expected.common.display()))?;
    if common != expected.common || canonical_expected != expected.common || !common.is_dir() {
        bail!(
            "Git common directory {} does not match trusted {}",
            common.display(),
            expected.common.display()
        );
    }
    Ok(Some(canonical_expected))
}

fn bind_validated_git(args: &mut Vec<String>, mounts: &mut Mounts, git: &TrustedGit, writable: bool) {
    mounts.bind(args, &git.common, &git.common, writable);
    mounts.bind(args, &git.common, &git.common_alias, writable);
}

fn append_path_mounts<I>(args: &mut Vec<String>, mounts: &mut Mounts, entries: I)
where
    I: IntoIterator<Item = PathBuf>,
{
    for entry in entries {
        if !entry.is_absolute() || !entry.is_dir() {
            continue;
        }
        let Ok(canonical) = entry.canonicalize() else {
            continue;
        };
        if entry.starts_with("/nix/store")
            || matches!(canonical.to_str(), Some("/" | "/etc" | "/usr" | "/bin" | "/usr/bin"))
            || mounts.rw_roots.iter().any(|root| {
                entry.starts_with(root)
                    || canonical.starts_with(root)
                    || root.starts_with(&entry)
                    || root.starts_with(&canonical)
            })
        {
            continue;
        }
        mounts.bind(args, &entry, &entry, false);
    }
}

struct ProcessGuard {
    bwrap: Child,
    slirp: Option<Child>,
    slirp_exit: Option<OwnedFd>,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.bwrap.try_wait().ok().flatten().is_none() {
            let _ = self.bwrap.kill();
        }
        let _ = self.bwrap.wait();
        drop(self.slirp_exit.take());
        if let Some(network) = &mut self.slirp {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match network.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                    _ => {
                        let _ = network.kill();
                        let _ = network.wait();
                        break;
                    }
                }
            }
        }
    }
}

fn cloexec_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` points to two writable descriptors; successful pipe2 initializes both.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two new, distinct descriptors, each now owned exactly once.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn pass_fds(command: &mut Command, fds: [RawFd; 2]) {
    // SAFETY: the closure only invokes async-signal-safe fcntl and captures plain descriptor values.
    unsafe {
        command.pre_exec(move || {
            for fd in fds {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

fn wait_readable(fd: RawFd, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for sandbox network");
        }
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: pollfd is a valid one-element array for the duration of the call.
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            bail!("timed out waiting for sandbox network");
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("wait for sandbox file descriptor");
        }
    }
}

fn read_child_pid(status: &OwnedFd) -> anyhow::Result<i32> {
    wait_readable(status.as_raw_fd(), NETWORK_TIMEOUT).context("wait for bubblewrap namespaces")?;
    let mut line = String::new();
    BufReader::new(File::from(status.try_clone()?))
        .read_line(&mut line)
        .context("read bubblewrap namespace status")?;
    if line.is_empty() {
        bail!("bubblewrap exited before reporting its namespaces");
    }
    let value: serde_json::Value = serde_json::from_str(&line).context("parse bubblewrap namespace status")?;
    value["child-pid"]
        .as_i64()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .context("bubblewrap status did not contain a valid child-pid")
}

fn wait_network_ready(ready: OwnedFd) -> anyhow::Result<()> {
    wait_readable(ready.as_raw_fd(), NETWORK_TIMEOUT)?;
    let mut byte = [0];
    let read = File::from(ready)
        .read(&mut byte)
        .context("read slirp4netns readiness")?;
    if read == 0 {
        bail!("slirp4netns exited before becoming ready");
    }
    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

fn display_status(status: ExitStatus) -> String {
    status.code().map_or_else(
        || format!("signal {}", status.signal().unwrap_or(0)),
        |code| code.to_string(),
    )
}

fn check_version(program: &str, description: &str) -> anyhow::Result<()> {
    let output = Command::new(program)
        .arg("--version")
        .output()
        .with_context(|| format!("{description} is not executable: {program}"))?;
    if !output.status.success() {
        bail!("{description} --version failed with {}", display_status(output.status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    fn valid_args() -> Vec<String> {
        strings(&[
            "--bwrap",
            "bwrap",
            "--slirp4netns",
            "slirp4netns",
            "--agent-home",
            "/home/agent",
            "--state-root",
            "/state",
            "--worktree",
            "/worktree",
            "--devenv",
            "0",
            "--",
            "agent",
            "--flag",
        ])
    }

    fn options() -> LaunchOptions {
        parse_args(&valid_args()).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let sequence = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("remoter-bwrap-test-{tag}-{}-{sequence}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn api_url_rewrites_only_supported_loopback_urls() {
        for (input, expected) in [
            ("http://localhost", "http://10.0.2.2"),
            ("https://localhost:8181", "https://10.0.2.2:8181"),
            ("ws://127.0.0.1:8181", "ws://10.0.2.2:8181"),
            ("wss://0.0.0.0:8181/api", "wss://10.0.2.2:8181/api"),
            ("http://[::1]:8181/api", "http://10.0.2.2:8181/api"),
            ("HTTP://LOCALHOST:8181", "HTTP://10.0.2.2:8181"),
            (
                "https://user:pass@0.0.0.0:8181/api?q=1#x",
                "https://user:pass@10.0.2.2:8181/api?q=1#x",
            ),
            ("https://localhost?q=1", "https://10.0.2.2?q=1"),
        ] {
            assert_eq!(api_url(input), expected);
        }
        for unchanged in [
            "ftp://localhost:8181",
            "http://localhost.evil.test",
            "https://127.0.0.1.evil.test",
            "http://0.0.0.0.evil.test",
            "http://[::2]:8181",
            "http://example.test/localhost",
            "not a URL",
        ] {
            assert_eq!(api_url(unchanged), unchanged);
        }
    }

    #[test]
    fn run_env_has_exact_bwrap_contract() {
        assert_eq!(
            run_env(42),
            vec![
                ("CI".to_string(), "1".to_string()),
                ("REMOTER_EXECUTION".to_string(), "bwrap".to_string()),
                ("REMOTER_AGENT_TASK_ID".to_string(), "42".to_string()),
            ]
        );
    }

    #[test]
    fn cli_parses_command_and_env_without_eating_agent_flags() {
        let mut args = valid_args();
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        args.splice(
            separator..separator,
            strings(&[
                "--retain",
                "/nix/store/remoter-mcp/bin/remoter-mcp",
                "--retain",
                "late-tool",
                "--env",
                "A=x=y",
            ])
            .iter()
            .cloned(),
        );
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.command, strings(&["agent", "--flag"]));
        assert_eq!(
            parsed.retain,
            strings(&["/nix/store/remoter-mcp/bin/remoter-mcp", "late-tool"])
        );
        assert_eq!(parsed.env, vec![("A".to_string(), "x=y".to_string())]);
        assert!(!parsed.devenv);
    }

    #[test]
    fn cli_rejects_missing_invalid_and_relative_values() {
        let cases = [
            vec![],
            valid_args()[..valid_args().len() - 3].to_vec(),
            strings(&["--bwrap", "--worktree", "/x"]),
            valid_args()
                .into_iter()
                .map(|arg| {
                    if arg == "/worktree" {
                        "relative".to_string()
                    } else {
                        arg
                    }
                })
                .collect(),
            valid_args()
                .into_iter()
                .map(|arg| if arg == "0" { "yes".to_string() } else { arg })
                .collect(),
            strings(&["--unknown", "x"]),
        ];
        for args in cases {
            assert!(parse_args(&args).is_err(), "unexpectedly accepted {args:?}");
        }
        let mut no_program = valid_args();
        no_program.truncate(no_program.len() - 2);
        assert!(parse_args(&no_program).is_err());
        let mut bad_env = valid_args();
        let separator = bad_env.iter().position(|arg| arg == "--").unwrap();
        bad_env.splice(separator..separator, strings(&["--env", "=bad"]).iter().cloned());
        assert!(parse_args(&bad_env).is_err());
    }

    #[test]
    fn sandbox_env_filters_host_mode_secrets_and_database_urls() {
        let mut parsed = options();
        parsed.env = vec![
            ("REMOTER_TOKEN".to_string(), "secret".to_string()),
            ("REMOTER_CONTAINER".to_string(), "1".to_string()),
            ("REMOTER_AGENT_PORT_BASE".to_string(), "10000".to_string()),
            ("DATABASE_URL".to_string(), "postgres://host".to_string()),
            ("TEST_DATABASE_URL".to_string(), "postgres://host".to_string()),
            ("CUSTOM".to_string(), "yes".to_string()),
            ("CI".to_string(), "0".to_string()),
        ];
        let env = sandbox_environment(&parsed, Path::new("/nix/store/ca-bundle.crt"));
        assert!(env.iter().any(|(key, value)| key == "CUSTOM" && value == "yes"));
        assert!(env.iter().any(|(key, value)| key == "CI" && value == "1"));
        assert!(
            env.iter()
                .any(|(key, value)| key == "NIX_REMOTE" && value == "local?read-only=true")
        );
        for forbidden in [
            "REMOTER_TOKEN",
            "REMOTER_CONTAINER",
            "REMOTER_AGENT_PORT_BASE",
            "DATABASE_URL",
            "TEST_DATABASE_URL",
        ] {
            assert!(env.iter().all(|(key, _)| key != forbidden));
        }
    }

    #[test]
    fn sweep_preserves_active_reused_malformed_and_foreign_state() {
        let root = scratch("sweep");
        let pid = std::process::id();
        let starttime = process_starttime(pid).unwrap();
        let active = root.join(format!("{RUN_PREFIX}{pid}-1"));
        let reused = root.join(format!("{RUN_PREFIX}{pid}-2"));
        let malformed = root.join(format!("{RUN_PREFIX}{pid}-3"));
        let stale_pid = 2_000_000_000_u32;
        let stale = root.join(format!("{RUN_PREFIX}{stale_pid}-4"));
        let foreign = root.join("foreign");
        let unrecognized = root.join(format!("{RUN_PREFIX}not-a-pid"));
        for path in [&active, &reused, &malformed, &stale, &foreign, &unrecognized] {
            fs::create_dir(path).unwrap();
        }
        write_owner(&active, Owner { pid, starttime }).unwrap();
        write_owner(
            &reused,
            Owner {
                pid,
                starttime: starttime + 1,
            },
        )
        .unwrap();
        fs::write(malformed.join(OWNER_FILE), "not metadata").unwrap();
        write_owner(
            &stale,
            Owner {
                pid: stale_pid,
                starttime: 1,
            },
        )
        .unwrap();
        sweep(&root);
        assert!(active.exists());
        assert!(!reused.exists(), "a reused PID must not preserve stale state");
        assert!(malformed.exists());
        assert!(!stale.exists());
        assert!(foreign.exists());
        assert!(unrecognized.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn online_backup_includes_committed_wal_state() {
        let root = scratch("sqlite");
        let source_path = root.join("source.sqlite");
        let schema = root.join("schema");
        fs::write(&schema, "schema-version").unwrap();
        let source = Connection::open(&source_path).unwrap();
        source
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE values_table(value TEXT);")
            .unwrap();
        source
            .execute("INSERT INTO values_table VALUES ('from-wal')", [])
            .unwrap();
        let destination = root.join("snapshot");
        snapshot_nix_db(&source_path, &schema, &destination).unwrap();
        let snapshot = Connection::open(destination.join("db.sqlite")).unwrap();
        let value: String = snapshot
            .query_row("SELECT value FROM values_table", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "from-wal");
        assert_eq!(
            fs::read_to_string(destination.join("schema")).unwrap(),
            "schema-version"
        );
        assert!(destination.join("reserved").exists());
        assert!(destination.join("big-lock").exists());
        drop(snapshot);
        drop(source);
        fs::remove_dir_all(root).unwrap();
    }

    fn trusted_options(root: &Path) -> LaunchOptions {
        let mut parsed = options();
        parsed.worktree = root.join("p1/wt-7");
        parsed.state_root = root.join("bwrap");
        parsed.agent_home = root.join("home");
        fs::create_dir_all(&parsed.worktree).unwrap();
        fs::create_dir_all(&parsed.state_root).unwrap();
        parsed
    }

    fn link_worktree(worktree: &Path, common: &Path, task: u32) -> PathBuf {
        let linked = common.join("worktrees").join(format!("wt-{task}"));
        fs::create_dir_all(&linked).unwrap();
        fs::write(worktree.join(".git"), format!("gitdir: {}\n", linked.display())).unwrap();
        fs::write(linked.join("commondir"), "../..\n").unwrap();
        linked
    }

    #[test]
    fn mounts_validated_central_worktree_and_reference_git_metadata() {
        let root = scratch("mounts");
        let parsed = trusted_options(&root);
        let common_git = root.join("p1/repo/.git");
        let linked_git = link_worktree(&parsed.worktree, &common_git, 7);
        let reference = root.join("p9/refs/dependency/wt-7");
        let reference_common = root.join("p9/refs/dependency/repo/.git");
        fs::create_dir_all(&reference).unwrap();
        link_worktree(&reference, &reference_common, 7);
        fs::create_dir(parsed.worktree.join(".refs")).unwrap();
        symlink(&reference, parsed.worktree.join(".refs/dependency")).unwrap();
        let run_dir = root.join("run");
        let args = bwrap_arguments(
            &parsed,
            &run_dir,
            Path::new("/nix/store/ca"),
            Path::new("/nix/store/bash"),
            Path::new("/nix/store/env"),
            10,
            11,
        )
        .unwrap();
        assert!(has_bind(&args, "--bind", &common_git, &common_git));
        assert!(!has_bind(&args, "--bind", &linked_git, &linked_git));
        assert!(has_bind(&args, "--ro-bind", &reference, &reference));
        assert!(has_bind(&args, "--ro-bind", &reference_common, &reference_common));
        assert!(args.windows(2).any(|pair| pair == ["--json-status-fd", "10"]));
        assert!(args.windows(2).any(|pair| pair == ["--block-fd", "11"]));

        let mut central = trusted_options(&root);
        central.worktree = root.join("p3/repo");
        let central_git = central.worktree.join(".git");
        fs::create_dir_all(&central_git).unwrap();
        let args = bwrap_arguments(
            &central,
            &run_dir,
            Path::new("/nix/store/ca"),
            Path::new("/nix/store/bash"),
            Path::new("/nix/store/env"),
            10,
            11,
        )
        .unwrap();
        assert!(has_bind(&args, "--bind", &central_git, &central_git));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_mounts_reject_untrusted_gitdir_and_commondir() {
        let root = scratch("malicious-git");
        let parsed = trusted_options(&root);
        fs::write(parsed.worktree.join(".git"), "gitdir: /\n").unwrap();
        let error = bwrap_arguments(
            &parsed,
            &root.join("run"),
            Path::new("/nix/store/ca"),
            Path::new("/nix/store/bash"),
            Path::new("/nix/store/env"),
            10,
            11,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not match trusted"), "{error:#}");

        let common = root.join("p1/repo/.git");
        let linked = link_worktree(&parsed.worktree, &common, 7);
        fs::write(linked.join("commondir"), "/\n").unwrap();
        let error = bwrap_arguments(
            &parsed,
            &root.join("run"),
            Path::new("/nix/store/ca"),
            Path::new("/nix/store/bash"),
            Path::new("/nix/store/env"),
            10,
            11,
        )
        .unwrap_err();
        assert!(error.to_string().contains("common directory"), "{error:#}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_mounts_reject_untrusted_reference_targets() {
        let root = scratch("malicious-ref");
        let parsed = trusted_options(&root);
        fs::create_dir(parsed.worktree.join(".refs")).unwrap();
        symlink("/", parsed.worktree.join(".refs/evil")).unwrap();
        let error = bwrap_arguments(
            &parsed,
            &root.join("run"),
            Path::new("/nix/store/ca"),
            Path::new("/nix/store/bash"),
            Path::new("/nix/store/env"),
            10,
            11,
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside trusted workspace"), "{error:#}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlinked_workspace_preserves_cwd_and_mounts_git_aliases() {
        let root = scratch("symlinked-workspace");
        let real_root = root.join("real");
        let alias_root = root.join("alias");
        fs::create_dir_all(&real_root).unwrap();
        symlink(&real_root, &alias_root).unwrap();
        let mut parsed = trusted_options(&alias_root);
        let original_worktree = parsed.worktree.clone();
        let original_state = parsed.state_root.clone();
        let common_alias = alias_root.join("p1/repo/.git");
        let common_real = real_root.join("p1/repo/.git");
        link_worktree(&parsed.worktree, &common_alias, 7);
        prepare_launch_paths(&mut parsed).unwrap();
        assert_eq!(parsed.worktree, original_worktree);
        assert_eq!(parsed.state_root, original_state);
        let args = bwrap_arguments(
            &parsed,
            &original_state.join("run"),
            Path::new("/nix/store/ca"),
            Path::new("/nix/store/bash"),
            Path::new("/nix/store/env"),
            10,
            11,
        )
        .unwrap();
        assert!(has_bind(&args, "--bind", &original_worktree, &original_worktree));
        assert!(has_bind(&args, "--bind", &common_real, &common_real));
        assert!(has_bind(&args, "--bind", &common_real, &common_alias));
        assert!(
            args.windows(2)
                .any(|pair| { pair[0] == "--chdir" && pair[1] == original_worktree.to_string_lossy() })
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_mounts_exclude_store_root_etc_and_rw_subtrees() {
        let root = scratch("path-mounts");
        let work = root.join("work");
        let external = root.join("tools");
        fs::create_dir_all(work.join("bin")).unwrap();
        fs::create_dir(&external).unwrap();
        let mut mounts = Mounts::new();
        mounts.rw_roots.push(work.clone());
        let mut args = Vec::new();
        append_path_mounts(
            &mut args,
            &mut mounts,
            vec![
                PathBuf::from("/"),
                PathBuf::from("/etc"),
                PathBuf::from("/nix/store"),
                root.clone(),
                work.join("bin"),
                external.clone(),
            ],
        );
        assert!(!args.iter().any(|arg| arg == "/"));
        assert!(!args.iter().any(|arg| arg == "/etc"));
        assert!(!args.iter().any(|arg| arg == "/nix/store"));
        assert!(!has_bind(&args, "--ro-bind", &root, &root));
        assert!(!has_bind(&args, "--ro-bind", &work.join("bin"), &work.join("bin")));
        assert!(has_bind(&args, "--ro-bind", &external, &external));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guest_script_uses_devenv_shell_and_preserves_agent_args() {
        let root = scratch("script");
        let mut parsed = trusted_options(&root);
        parsed.devenv = true;
        parsed.command = strings(&["agent", "arg with spaces", "$HOME;echo no"]);
        let args = bwrap_arguments(
            &parsed,
            &root,
            Path::new("/nix/store/ca"),
            Path::new("/nix/store/sh"),
            Path::new("/nix/store/env"),
            10,
            11,
        )
        .unwrap();
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        let command = &args[separator + 1..];
        assert_eq!(&command[..4], ["/bin/sh", "-c", GUEST_SCRIPT, "remoter-bwrap"]);
        assert!(command.ends_with(&parsed.command));
        assert!(GUEST_SCRIPT.contains("shell --no-tui -- \"$@\""));
        assert!(GUEST_SCRIPT.contains("status=$?"));
        assert!(GUEST_SCRIPT.contains("exit \"$status\""));
        assert!(GUEST_SCRIPT.contains("trap 'devenv"));
        fs::remove_dir_all(root).unwrap();
    }

    fn has_bind(args: &[String], kind: &str, source: &Path, destination: &Path) -> bool {
        args.windows(3).any(|triple| {
            triple[0] == kind && triple[1] == source.to_string_lossy() && triple[2] == destination.to_string_lossy()
        })
    }
}
