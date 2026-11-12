//! Real NixOS integration coverage for the `__bwrap-launch` daemon entry point.
//!
//! Run serially on a NixOS host with rootless user namespaces:
//!
//! ```sh
//! cargo test --test bwrap_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! All tests are ignored by default and skip with a diagnostic when their
//! executables or Nix paths are unavailable. The network tests need outbound
//! DNS/HTTPS access. The PostgreSQL test needs devenv with flakes, enough host
//! network access/disk to pre-warm two small PostgreSQL closures, and no
//! existing listener on host `127.0.0.1:5432`. Guests use the read-only Nix
//! store and delegate builds/substitution to the host Nix daemon.

use remoter_agent::bwrap::{INTERNAL_LAUNCH_ARG, prewarm, run_env};
use remoter_agent::project::ProjectCommands;
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const DEVENV_READY_TIMEOUT: Duration = Duration::from_secs(240);

struct Tools {
    bwrap: PathBuf,
    slirp4netns: PathBuf,
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(tag: &str) -> Self {
        let root = project_root().join("target/tmp/bwrap-live");
        fs::create_dir_all(&root).unwrap();
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("{tag}-{}-{sequence}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn launch_spec(&self, name: &str, devenv: bool) -> LaunchSpec {
        let base = self.path.join(name);
        let worktree = base.join("p249/wt-249");
        let agent_home = base.join("agent-home");
        let state_root = base.join("bwrap");
        for path in [&worktree, &agent_home, &state_root] {
            fs::create_dir_all(path).unwrap();
        }
        let commands = if devenv {
            ProjectCommands {
                env_wrap: devenv_env_wrap(),
                services_down: devenv_services_down(),
                ..ProjectCommands::default()
            }
        } else {
            ProjectCommands::default()
        };
        LaunchSpec {
            worktree,
            agent_home,
            state_root,
            commands,
            retain: Vec::new(),
            env: run_env(249),
        }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct LaunchSpec {
    worktree: PathBuf,
    agent_home: PathBuf,
    state_root: PathBuf,
    commands: ProjectCommands,
    retain: Vec<String>,
    env: Vec<(String, String)>,
}

#[derive(Debug)]
struct LiveOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

struct LiveChild {
    child: Option<Child>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl LiveChild {
    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.as_mut().unwrap().try_wait().unwrap()
    }

    fn logs(&self) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            fs::read_to_string(&self.stdout_path).unwrap_or_default(),
            fs::read_to_string(&self.stderr_path).unwrap_or_default()
        )
    }

    fn wait(mut self) -> LiveOutput {
        let status = self.child.take().unwrap().wait().unwrap();
        LiveOutput {
            status,
            stdout: fs::read_to_string(&self.stdout_path).unwrap_or_default(),
            stderr: fs::read_to_string(&self.stderr_path).unwrap_or_default(),
        }
    }

    fn wait_success(self) -> LiveOutput {
        let output = self.wait();
        assert!(
            output.status.success(),
            "launcher failed with {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            output.stdout,
            output.stderr
        );
        output
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    starttime: u64,
    comm: String,
}

#[test]
#[ignore = "real NixOS bwrap/slirp4netns integration — run manually"]
fn sandbox_filesystem_env_and_nix_daemon() {
    let Some(tools) = prerequisites("sandbox_filesystem_env_and_nix_daemon") else {
        return;
    };
    let dir = TestDir::new("sandbox");
    let mut spec = dir.launch_spec("run", false);
    spec.env.extend([
        ("CUSTOM_VISIBLE".to_string(), "yes".to_string()),
        ("REMOTER_CONTAINER".to_string(), "1".to_string()),
        ("REMOTER_AGENT_PORT_BASE".to_string(), "10000".to_string()),
        ("DATABASE_URL".to_string(), "postgres://should-not-leak".to_string()),
        (
            "TEST_DATABASE_URL".to_string(),
            "postgres://should-not-leak".to_string(),
        ),
    ]);
    let script = r#"
set -eu
worktree=$1
home=$2
outside=$3
printf 'worktree-rw\n' > "$worktree/worktree-file"
printf 'work-symlink-rw\n' > /work/work-symlink-file
printf 'home-rw\n' > "$home/home-file"
test -f /etc/pki/tls/certs/ca-bundle.crt
if printf 'must-fail\n' > /nix/store/remoter-bwrap-live-forbidden 2>/dev/null; then
    echo '/nix/store was writable' >&2
    exit 1
fi
printf 'must-not-reach-host\n' > "$outside" 2>/dev/null || :
/usr/bin/env > "$worktree/environment"
"#;
    let outside = spec.worktree.parent().unwrap().join("outside-host-write");
    let started = Instant::now();
    run_launch(
        &tools,
        &spec,
        "/bin/sh",
        strings(&["-c", script, "sh"])
            .into_iter()
            .chain([
                spec.worktree.to_string_lossy().into_owned(),
                spec.agent_home.to_string_lossy().into_owned(),
                outside.to_string_lossy().into_owned(),
            ])
            .collect(),
        "sandbox",
    )
    .wait_success();
    eprintln!("bwrap live: warm sandbox launch+command took {:?}", started.elapsed());

    assert_eq!(
        fs::read_to_string(spec.worktree.join("worktree-file")).unwrap(),
        "worktree-rw\n"
    );
    assert_eq!(
        fs::read_to_string(spec.worktree.join("work-symlink-file")).unwrap(),
        "work-symlink-rw\n"
    );
    assert_eq!(
        fs::read_to_string(spec.agent_home.join("home-file")).unwrap(),
        "home-rw\n"
    );
    assert!(
        !outside.exists(),
        "a write outside the writable mounts reached the host"
    );
    let environment = fs::read_to_string(spec.worktree.join("environment")).unwrap();
    assert!(environment.lines().any(|line| line == "CI=1"), "{environment}");
    assert!(
        environment.lines().any(|line| line == "REMOTER_EXECUTION=bwrap"),
        "{environment}"
    );
    assert!(
        environment.lines().any(|line| line == "NIX_REMOTE=daemon"),
        "{environment}"
    );
    assert!(
        environment.lines().any(|line| line == "REMOTER_AGENT_TASK_ID=249"),
        "{environment}"
    );
    assert!(
        environment.lines().any(|line| line == "CUSTOM_VISIBLE=yes"),
        "{environment}"
    );
    for key in ["REMOTER_CONTAINER", "REMOTER_AGENT_PORT_BASE"] {
        assert!(
            environment.lines().all(|line| !line.starts_with(&format!("{key}="))),
            "{environment}"
        );
    }
    assert!(
        environment
            .lines()
            .all(|line| !line.starts_with("DATABASE_URL=") && !line.contains("_DATABASE_URL=")),
        "{environment}"
    );
    assert!(
        run_dirs(&spec.state_root).is_empty(),
        "normal launch state was not cleaned"
    );
}

#[test]
#[ignore = "real network integration; needs outbound DNS/HTTPS to GitHub"]
fn network_dns_https_git_and_host_loopback() {
    let Some(tools) = prerequisites("network_dns_https_git_and_host_loopback") else {
        return;
    };
    if !has_tools("network_dns_https_git_and_host_loopback", &["bash", "git"]) {
        return;
    }
    let dir = TestDir::new("network");
    let spec = dir.launch_spec("run", false);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut request = [0; 2048];
                    let _ = stream.read(&mut request);
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\nbwrap-loopback-ok",
                        )
                        .unwrap();
                    let _ = stream.shutdown(Shutdown::Both);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    panic!("guest did not connect to the host HTTP listener: {error}")
                }
                Err(error) => panic!("host HTTP listener failed: {error}"),
            }
        }
    });
    let script = r#"
set -eu
port=$1
exec 3<>"/dev/tcp/10.0.2.2/$port"
printf 'GET /api/health HTTP/1.0\r\nHost: localhost\r\n\r\n' >&3
cat <&3
"#;
    let output = run_launch(
        &tools,
        &spec,
        "bash",
        strings(&["-c", script, "bash", &port.to_string()]),
        "host-http",
    )
    .wait_success();
    server.join().unwrap();
    assert!(output.stdout.contains("bwrap-loopback-ok"), "{}", output.stdout);

    let git = run_launch(
        &tools,
        &spec,
        "git",
        strings(&["ls-remote", "https://github.com/git/git.git", "HEAD"]),
        "git-https",
    )
    .wait_success();
    assert!(git.stdout.trim_end().ends_with("HEAD"), "{}", git.stdout);
}

#[test]
#[ignore = "real Git integration; creates a commit inside Bubblewrap"]
fn git_add_and_commit_use_provisioned_identity() {
    let Some(tools) = prerequisites("git_add_and_commit_use_provisioned_identity") else {
        return;
    };
    if !has_tools("git_add_and_commit_use_provisioned_identity", &["git"]) {
        return;
    }
    let dir = TestDir::new("git-commit");
    let mut spec = dir.launch_spec("run", false);
    spec.worktree = spec.worktree.parent().unwrap().join("repo");
    fs::create_dir_all(&spec.worktree).unwrap();
    let init = Command::new("git")
        .arg("-C")
        .arg(&spec.worktree)
        .arg("init")
        .output()
        .unwrap();
    assert!(init.status.success(), "git init failed: {init:?}");

    let script = r#"
set -eu
printf 'committed inside bwrap\n' > tracked.txt
git add tracked.txt
git commit -m 'Bubblewrap live commit'
git log -1 --format='%an <%ae>'
"#;
    let output = run_launch(&tools, &spec, "/bin/sh", strings(&["-c", script]), "git-commit").wait_success();
    assert_eq!(
        output.stdout.lines().last(),
        Some("remoter-agent <remoter-agent@localhost>")
    );
    let count = Command::new("git")
        .arg("-C")
        .arg(&spec.worktree)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    assert!(count.status.success(), "host Git could not read the commit: {count:?}");
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
}

#[test]
#[ignore = "real Nix GC integration; roots a late-started store executable"]
fn late_started_store_executable_has_per_run_gc_root() {
    let Some(tools) = prerequisites("late_started_store_executable_has_per_run_gc_root") else {
        return;
    };
    let env_tool = find_tool("env").unwrap().canonicalize().unwrap();
    assert!(env_tool.starts_with("/nix/store"));
    let dir = TestDir::new("late-gc-root");
    let mut spec = dir.launch_spec("run", false);
    spec.retain = vec!["env".to_string()];
    let ready = spec.worktree.join("ready");
    let stop = spec.worktree.join("stop");
    let mut child = spawn_launch(&tools, &spec, "/bin/sh", long_running_args(&spec), "late-gc-root");
    wait_ready(&mut child, &ready, READY_TIMEOUT);
    let run_dir = one_run_dir(&spec.state_root);
    let roots: Vec<PathBuf> = fs::read_dir(run_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("gc-root-"))
        })
        .collect();
    assert!(
        roots.iter().any(|root| {
            root.canonicalize()
                .is_ok_and(|target| env_tool == target || env_tool.starts_with(target))
        }),
        "late-started executable has no GC root: {roots:?}"
    );
    fs::write(stop, "stop").unwrap();
    child.wait_success();
    assert!(run_dirs(&spec.state_root).is_empty());
}

#[test]
#[ignore = "real process lifecycle integration; sends SIGTERM and SIGKILL"]
fn launcher_normal_and_signal_lifecycle() {
    let Some(tools) = prerequisites("launcher_normal_and_signal_lifecycle") else {
        return;
    };
    let dir = TestDir::new("lifecycle-normal");
    let spec = dir.launch_spec("run", false);
    let foreign = spec.state_root.join("foreign-state");
    let malformed = spec.state_root.join("bwrap-run-not-a-pid");
    fs::create_dir_all(&foreign).unwrap();
    fs::create_dir_all(&malformed).unwrap();
    fs::write(foreign.join("keep"), "foreign").unwrap();
    let ready = spec.worktree.join("ready");
    let stop = spec.worktree.join("stop");
    let mut child = spawn_launch(&tools, &spec, "/bin/sh", long_running_args(&spec), "normal");
    wait_ready(&mut child, &ready, READY_TIMEOUT);
    let active_state = one_run_dir(&spec.state_root);
    let descendants = descendant_processes(child.pid());
    assert_has_helpers(&descendants);
    run_launch(&tools, &spec, "/bin/sh", strings(&["-c", "true"]), "normal-reconcile").wait_success();
    assert!(active_state.exists(), "sweep removed an active launch directory");
    assert!(foreign.exists() && malformed.exists(), "sweep removed foreign state");
    fs::write(stop, "stop").unwrap();
    child.wait_success();
    assert_processes_gone(&descendants);
    assert!(run_dirs(&spec.state_root).is_empty());
    assert!(foreign.join("keep").exists() && malformed.exists());

    for signal in [libc::SIGTERM, libc::SIGKILL] {
        let dir = TestDir::new(&format!("lifecycle-{signal}"));
        let spec = dir.launch_spec("run", false);
        let foreign = spec.state_root.join("foreign-state");
        fs::create_dir_all(&foreign).unwrap();
        let ready = spec.worktree.join("ready");
        let mut child = spawn_launch(&tools, &spec, "/bin/sh", long_running_args(&spec), "signal");
        wait_ready(&mut child, &ready, READY_TIMEOUT);
        let descendants = descendant_processes(child.pid());
        assert_has_helpers(&descendants);
        // SAFETY: the signal is sent only to the launcher child created above.
        assert_eq!(unsafe { libc::kill(child.pid() as i32, signal) }, 0);
        let output = child.wait();
        assert_eq!(
            output.status.signal(),
            Some(signal),
            "unexpected launcher status: {output:?}"
        );
        assert_processes_gone(&descendants);
        let stale = run_dirs(&spec.state_root);
        assert!(!stale.is_empty(), "hard-killed launcher left no state to reconcile");
        run_launch(&tools, &spec, "/bin/sh", strings(&["-c", "true"]), "signal-reconcile").wait_success();
        assert!(
            stale.iter().all(|path| !path.exists()),
            "stale launch state survived reconciliation"
        );
        assert!(foreign.exists(), "reconciliation removed foreign state");
        eprintln!(
            "bwrap live: signal {signal} cleaned {} descendant(s)",
            descendants.len()
        );
    }
}

#[test]
#[ignore = "real daemon-crash integration; SIGKILLs the launcher's parent"]
fn daemon_death_terminates_launcher_and_sandbox_before_reconcile() {
    let Some(tools) = prerequisites("daemon_death_terminates_launcher_and_sandbox_before_reconcile") else {
        return;
    };
    let dir = TestDir::new("daemon-death");
    let spec = dir.launch_spec("run", false);
    let ready = spec.worktree.join("ready");
    let mut daemon = spawn_daemon_launch(&tools, &spec, "/bin/sh", long_running_args(&spec), "daemon-death");
    wait_ready(&mut daemon, &ready, READY_TIMEOUT);
    let descendants = descendant_processes(daemon.pid());
    assert_has_helpers(&descendants);
    assert!(
        descendants.iter().any(|process| process.comm.contains("remoter-agent")),
        "launcher is not a daemon descendant: {descendants:?}"
    );

    // SAFETY: the signal is sent only to the intermediate daemon created above.
    assert_eq!(unsafe { libc::kill(daemon.pid() as i32, libc::SIGKILL) }, 0);
    let output = daemon.wait();
    assert_eq!(output.status.signal(), Some(libc::SIGKILL));
    assert_processes_gone(&descendants);

    let stale = run_dirs(&spec.state_root);
    assert!(!stale.is_empty(), "daemon crash left no launch state to reconcile");
    run_launch(&tools, &spec, "/bin/sh", strings(&["-c", "true"]), "daemon-reconcile").wait_success();
    assert!(
        stale.iter().all(|path| !path.exists()),
        "stale launch state survived reconciliation"
    );
}

#[tokio::test]
#[ignore = "real devenv integration; host pre-warm may build or substitute"]
async fn devenv_shell_without_processes() {
    let Some(tools) = prerequisites("devenv_shell_without_processes") else {
        return;
    };
    if !has_tools("devenv_shell_without_processes", &["devenv"]) {
        return;
    }
    let dir = TestDir::new("shell-only");
    let spec = dir.launch_spec("run", true);
    write_shell_only_devenv_project(&spec.worktree);
    prewarm(&spec.worktree, &spec.agent_home, &spec.env, &spec.commands.env_wrap)
        .await
        .unwrap_or_else(|error| panic!("host devenv pre-warm failed: {error}"));
    let output = run_launch(
        &tools,
        &spec,
        "/bin/sh",
        strings(&["-c", "printf shell-ok"]),
        "shell-only",
    )
    .wait_success();
    assert_eq!(output.stdout, "shell-ok");
}

#[tokio::test]
#[ignore = "real devenv integration; guest realizes a new derivation through the host Nix daemon"]
async fn devenv_shell_change_uses_host_nix_daemon() {
    let Some(tools) = prerequisites("devenv_shell_change_uses_host_nix_daemon") else {
        return;
    };
    if !has_tools("devenv_shell_change_uses_host_nix_daemon", &["devenv"]) {
        return;
    }
    let dir = TestDir::new("shell-change");
    let spec = dir.launch_spec("run", true);
    write_shell_only_devenv_project(&spec.worktree);
    prewarm(&spec.worktree, &spec.agent_home, &spec.env, &spec.commands.env_wrap)
        .await
        .unwrap_or_else(|error| panic!("host devenv pre-warm failed: {error}"));
    write_devenv_config(&spec.worktree, "{ pkgs, ... }:\n{\n  packages = [ pkgs.hello ];\n}\n");
    let output = run_launch(&tools, &spec, "/bin/sh", strings(&["-c", "hello"]), "shell-change").wait_success();
    assert_eq!(output.stdout.lines().last(), Some("Hello, world!"));
}

#[test]
#[ignore = "real Flutter/pub integration; needs outbound DNS/HTTPS to pub.dev"]
fn flutter_pub_get_uses_system_ca_bundle() {
    let Some(tools) = prerequisites("flutter_pub_get_uses_system_ca_bundle") else {
        return;
    };
    if !has_tools("flutter_pub_get_uses_system_ca_bundle", &["flutter"]) {
        return;
    }
    let dir = TestDir::new("flutter-pub");
    let mut spec = dir.launch_spec("run", false);
    spec.env.push((
        "PUB_CACHE".to_string(),
        spec.worktree.join("pub-cache").to_string_lossy().into_owned(),
    ));
    fs::write(
        spec.worktree.join("pubspec.yaml"),
        r#"name: bwrap_tls_live
description: Bubblewrap Dart CA integration test
publish_to: none
version: 1.0.0
environment:
  sdk: ^3.0.0
dependencies:
  flutter:
    sdk: flutter
  http: ^1.2.0
flutter:
  uses-material-design: false
"#,
    )
    .unwrap();
    run_launch(
        &tools,
        &spec,
        "flutter",
        strings(&["--suppress-analytics", "pub", "get"]),
        "flutter-pub",
    )
    .wait_success();
    assert!(spec.worktree.join(".dart_tool/package_config.json").is_file());
    assert!(
        fs::read_to_string(spec.worktree.join("pubspec.lock"))
            .unwrap()
            .contains("http")
    );
}

#[tokio::test]
#[ignore = "real devenv/PostgreSQL integration; host pre-warm may build or substitute"]
async fn devenv_postgres_parallel_and_restart() {
    let Some(tools) = prerequisites("devenv_postgres_parallel_and_restart") else {
        return;
    };
    if !has_tools("devenv_postgres_parallel_and_restart", &["devenv"]) {
        return;
    }
    if host_port_5432_open() {
        eprintln!("SKIP devenv_postgres_parallel_and_restart: host 127.0.0.1:5432 is already in use");
        return;
    }
    let dir = TestDir::new("postgres-parallel");
    let mut one = dir.launch_spec("one", true);
    let mut two = dir.launch_spec("two", true);
    for (spec, task_id) in [(&mut one, 251), (&mut two, 252)] {
        write_devenv_project(&spec.worktree);
        spec.commands.services_up = devenv_services_up();
        spec.commands.services_wait = devenv_services_wait();
        spec.env.clone_from(&run_env(task_id));
        prewarm(&spec.worktree, &spec.agent_home, &spec.env, &spec.commands.env_wrap)
            .await
            .unwrap_or_else(|error| panic!("host devenv pre-warm failed for task {task_id}: {error}"));
    }
    let mut children = Vec::new();
    let mut descendants = Vec::new();
    let mut startup = Vec::new();
    for (name, spec, marker) in [("one", &one, "sandbox-one"), ("two", &two, "sandbox-two")] {
        let started = Instant::now();
        let child = spawn_launch(&tools, spec, "/bin/sh", postgres_args(spec, marker), name);
        children.push((child, started));
    }
    for (index, (child, started)) in children.iter_mut().enumerate() {
        let spec = if index == 0 { &one } else { &two };
        wait_ready(child, &spec.worktree.join("sql-result"), DEVENV_READY_TIMEOUT);
        startup.push(started.elapsed());
        descendants.push(descendant_processes(child.pid()));
    }
    assert_eq!(
        fs::read_to_string(one.worktree.join("sql-result")).unwrap().trim(),
        "sandbox-one"
    );
    assert_eq!(
        fs::read_to_string(two.worktree.join("sql-result")).unwrap().trim(),
        "sandbox-two"
    );
    assert!(
        !host_port_5432_open(),
        "sandbox PostgreSQL exposed a host listening port"
    );
    for (index, _) in [&one, &two].into_iter().enumerate() {
        eprintln!(
            "bwrap live: sandbox {} warm launch-to-SQL {:?}",
            index + 1,
            startup[index]
        );
    }
    fs::write(one.worktree.join("stop"), "stop").unwrap();
    fs::write(two.worktree.join("stop"), "stop").unwrap();
    for (child, _) in children {
        child.wait_success();
    }
    for processes in descendants {
        assert_processes_gone(&processes);
    }
    assert!(!host_port_5432_open(), "PostgreSQL remained on the host after teardown");
    assert!(run_dirs(&one.state_root).is_empty() && run_dirs(&two.state_root).is_empty());
}

fn prerequisites(test: &str) -> Option<Tools> {
    let bwrap = find_tool("bwrap");
    let slirp4netns = find_tool("slirp4netns");
    let missing: Vec<&str> = [
        ("bwrap", bwrap.is_none()),
        ("slirp4netns", slirp4netns.is_none()),
        ("nix", find_tool("nix").is_none()),
        ("nix-store", find_tool("nix-store").is_none()),
        ("/nix/store", !Path::new("/nix/store").is_dir()),
        (
            "/nix/var/nix/daemon-socket/socket",
            !Path::new("/nix/var/nix/daemon-socket/socket").exists(),
        ),
        (
            "/etc/ssl/certs/ca-bundle.crt",
            !Path::new("/etc/ssl/certs/ca-bundle.crt").is_file(),
        ),
    ]
    .into_iter()
    .filter_map(|(name, missing)| missing.then_some(name))
    .collect();
    if !missing.is_empty() {
        eprintln!("SKIP {test}: missing live prerequisites: {}", missing.join(", "));
        return None;
    }
    let tools = Tools {
        bwrap: bwrap.unwrap(),
        slirp4netns: slirp4netns.unwrap(),
    };
    remoter_agent::bwrap::validate_prerequisites(&tools.bwrap.to_string_lossy(), &tools.slirp4netns.to_string_lossy())
        .unwrap_or_else(|error| panic!("{test}: startup validation failed: {error:#}"));
    Some(tools)
}

fn has_tools(test: &str, names: &[&str]) -> bool {
    let missing: Vec<&str> = names.iter().copied().filter(|name| find_tool(name).is_none()).collect();
    if missing.is_empty() {
        true
    } else {
        eprintln!("SKIP {test}: missing live prerequisites: {}", missing.join(", "));
        false
    }
}

fn find_tool(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|entry| entry.join(name))
        .find(|candidate| candidate.is_file())
}

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn devenv_env_wrap() -> Vec<String> {
    strings(&["devenv", "--no-eval-cache", "shell", "--no-tui", "--"])
}

fn devenv_services_up() -> Vec<String> {
    strings(&["devenv", "--no-eval-cache", "up", "-d"])
}

fn devenv_services_wait() -> Vec<String> {
    strings(&["devenv", "--no-eval-cache", "processes", "wait", "--timeout", "120"])
}

fn devenv_services_down() -> Vec<String> {
    strings(&["devenv", "--no-eval-cache", "processes", "down"])
}

fn launcher_command(tools: &Tools, spec: &LaunchSpec, program: &str, args: Vec<String>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_remoter-agent"));
    command
        .arg(INTERNAL_LAUNCH_ARG)
        .arg("--bwrap")
        .arg(&tools.bwrap)
        .arg("--slirp4netns")
        .arg(&tools.slirp4netns)
        .arg("--agent-home")
        .arg(&spec.agent_home)
        .arg("--state-root")
        .arg(&spec.state_root)
        .arg("--worktree")
        .arg(&spec.worktree);
    for (option, argv) in [
        ("--env-wrap", &spec.commands.env_wrap),
        ("--services-up", &spec.commands.services_up),
        ("--services-wait", &spec.commands.services_wait),
        ("--services-down", &spec.commands.services_down),
    ] {
        command
            .arg(option)
            .arg(serde_json::to_string(argv).expect("string argv serializes"));
    }
    for program in &spec.retain {
        command.arg("--retain").arg(program);
    }
    for (key, value) in &spec.env {
        command.arg("--env").arg(format!("{key}={value}"));
    }
    command.arg("--").arg(program).args(args);
    command
}

fn spawn_logged(mut command: Command, spec: &LaunchSpec, log_stem: &str) -> LiveChild {
    let log_dir = spec.worktree.parent().unwrap();
    let stdout_path = log_dir.join(format!("{log_stem}.stdout"));
    let stderr_path = log_dir.join(format!("{log_stem}.stderr"));
    let stdout = File::create(&stdout_path).unwrap();
    let stderr = File::create(&stderr_path).unwrap();
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    LiveChild {
        child: Some(child),
        stdout_path,
        stderr_path,
    }
}

fn spawn_launch(tools: &Tools, spec: &LaunchSpec, program: &str, args: Vec<String>, log_stem: &str) -> LiveChild {
    spawn_logged(launcher_command(tools, spec, program, args), spec, log_stem)
}

fn spawn_daemon_launch(
    tools: &Tools,
    spec: &LaunchSpec,
    program: &str,
    args: Vec<String>,
    log_stem: &str,
) -> LiveChild {
    let launcher = launcher_command(tools, spec, program, args);
    let mut daemon = Command::new("/bin/sh");
    daemon
        .arg("-c")
        .arg("\"$@\"\nstatus=$?\nexit \"$status\"")
        .arg("remoter-bwrap-test-daemon")
        .arg(launcher.get_program())
        .args(launcher.get_args());
    spawn_logged(daemon, spec, log_stem)
}

fn run_launch(tools: &Tools, spec: &LaunchSpec, program: &str, args: Vec<String>, log_stem: &str) -> LiveChild {
    spawn_launch(tools, spec, program, args, log_stem)
}

fn long_running_args(spec: &LaunchSpec) -> Vec<String> {
    strings(&[
        "-c",
        "set -eu\nprintf '%s\\n' \"$$\" > \"$1/ready\"\nwhile [ ! -f \"$1/stop\" ]; do sleep 0.1; done\n",
        "sh",
    ])
    .into_iter()
    .chain([spec.worktree.to_string_lossy().into_owned()])
    .collect()
}

fn wait_ready(child: &mut LiveChild, path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        if let Some(status) = child.try_wait() {
            panic!("launcher exited with {status} before readiness\n{}", child.logs());
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}\n{}",
            path.display(),
            child.logs()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_dirs(state_root: &Path) -> Vec<PathBuf> {
    fs::read_dir(state_root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("bwrap-run-"))
                .and_then(|suffix| suffix.split_once('-'))
                .is_some_and(|(pid, nonce)| {
                    !pid.is_empty()
                        && pid.bytes().all(|byte| byte.is_ascii_digit())
                        && !nonce.is_empty()
                        && nonce.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
        .collect()
}

fn one_run_dir(state_root: &Path) -> PathBuf {
    let dirs = run_dirs(state_root);
    assert_eq!(dirs.len(), 1, "expected one active run directory, got {dirs:?}");
    dirs.into_iter().next().unwrap()
}

fn process_table() -> Vec<ProcessInfo> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(comm_start) = stat.find('(') else {
            continue;
        };
        let Some(comm_end) = stat.rfind(") ") else {
            continue;
        };
        let fields: Vec<&str> = stat[comm_end + 2..].split_whitespace().collect();
        if fields.len() <= 19 {
            continue;
        }
        let (Ok(ppid), Ok(starttime)) = (fields[1].parse(), fields[19].parse()) else {
            continue;
        };
        processes.push(ProcessInfo {
            pid,
            ppid,
            starttime,
            comm: stat[comm_start + 1..comm_end].to_string(),
        });
    }
    processes
}

fn descendant_processes(root: u32) -> Vec<ProcessInfo> {
    let table = process_table();
    let mut descendants = HashSet::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for process in table.iter().filter(|process| process.ppid == parent) {
            if descendants.insert(process.pid) {
                frontier.push(process.pid);
            }
        }
    }
    table
        .into_iter()
        .filter(|process| descendants.contains(&process.pid))
        .collect()
}

fn assert_has_helpers(processes: &[ProcessInfo]) {
    assert!(
        processes.iter().any(|process| process.comm.contains("bwrap")),
        "{processes:?}"
    );
    assert!(
        processes.iter().any(|process| process.comm.contains("slirp")),
        "{processes:?}"
    );
}

fn assert_processes_gone(processes: &[ProcessInfo]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let live: Vec<u32> = processes
            .iter()
            .filter(|process| process_alive(process))
            .map(|process| process.pid)
            .collect();
        if live.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "sandbox processes still alive: {live:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn process_alive(expected: &ProcessInfo) -> bool {
    let path = Path::new("/proc").join(expected.pid.to_string()).join("stat");
    let Ok(stat) = fs::read_to_string(path) else {
        return false;
    };
    stat.rfind(") ")
        .and_then(|end| stat[end + 2..].split_whitespace().nth(19))
        .and_then(|start| start.parse::<u64>().ok())
        == Some(expected.starttime)
}

fn host_port_5432_open() -> bool {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5432);
    TcpStream::connect_timeout(&address.into(), Duration::from_millis(250)).is_ok()
}

fn write_shell_only_devenv_project(worktree: &Path) {
    write_devenv_config(worktree, "{ }\n");
}

fn write_devenv_project(worktree: &Path) {
    write_devenv_config(
        worktree,
        r#"{ pkgs, ... }:
{
  packages = [ pkgs.postgresql ];
  services.postgres = {
    enable = true;
    listen_addresses = "127.0.0.1";
    port = 5432;
    initialDatabases = [ { name = "bwrap_live"; } ];
  };
}
"#,
    );
}

fn write_devenv_config(worktree: &Path, config: &str) {
    fs::write(
        worktree.join("devenv.yaml"),
        "inputs:\n  nixpkgs:\n    url: github:cachix/devenv-nixpkgs/rolling\n",
    )
    .unwrap();
    fs::write(worktree.join("devenv.nix"), config).unwrap();
    let lock = project_root().join("devenv.lock");
    if lock.exists() {
        fs::copy(lock, worktree.join("devenv.lock")).unwrap();
    }
}

fn postgres_args(spec: &LaunchSpec, marker: &str) -> Vec<String> {
    let script = r#"
set -eu
worktree=$1
create=$2
insert=$3
command -v psql >/dev/null
devenv --no-eval-cache processes down >&2
devenv --no-eval-cache up -d >&2
devenv --no-eval-cache processes wait --timeout 120 >&2
psql -h localhost -p 5432 -d bwrap_live -v ON_ERROR_STOP=1 -c "$create" -c "$insert"
psql -h localhost -p 5432 -d bwrap_live -tA -v ON_ERROR_STOP=1 -c 'SELECT value FROM bwrap_marker' > "$worktree/sql-result"
while [ ! -f "$worktree/stop" ]; do sleep 0.1; done
"#;
    vec![
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
        spec.worktree.to_string_lossy().into_owned(),
        "CREATE TABLE bwrap_marker(value TEXT NOT NULL)".to_string(),
        format!("INSERT INTO bwrap_marker VALUES ('{marker}')"),
    ]
}
