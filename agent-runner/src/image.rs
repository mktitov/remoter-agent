//! Per-project agent images (docs/specs/remoter-agent-containers.md, PoC-2).
//!
//! The project's central clone (`p<id>/repo`) carries `.remoter/` — the
//! image build context: `agent.Dockerfile` (required), the optional
//! `agent-init.sh`, and any project nix configuration the init step installs
//! (`agent-flake.nix`/`agent-configuration.nix` in this repo). The image tag
//! is content-keyed — `<image_tag_prefix>:p<id>-<sha256-16>` over **every
//! file in `.remoter/`** (sorted relative paths + bytes) — so a changed
//! build input naturally produces a new tag while unchanged files hit the
//! local image cache (`docker image inspect`).
//!
//! Cache freshness against inputs **outside** the build context (e.g. the
//! remote flake the init script installs `remoter-mcp` from — master can
//! move while the context stays put) is checked per turn by the optional `.remoter/check-image.sh` hook: when the tag is
//! already built, the daemon runs the hook on the host (cwd = the central
//! clone, env `REMOTER_IMAGE_TAG` / `REMOTER_IMAGE_BUILT_REV` /
//! `REMOTER_DOCKER`). Exit 0 keeps the cache; the reserved exit 42 rebuilds
//! the image under the same tag; any other failure (non-zero, spawn error,
//! timeout) is logged and the cache is kept — a failed *check* must never
//! escalate a ticket. The committed image carries the label
//! `remoter.image_flake_rev` = the clone HEAD the init step built from,
//! handed back to the hook as `REMOTER_IMAGE_BUILT_REV` (empty for
//! pre-label images, which the hook should treat as stale — one self-healing
//! rebuild on rollout).
//!
//! The init container gets GitHub SSH access (agent forwarding when
//! `SSH_AUTH_SOCK` points at a live agent socket, else a read-only mount of
//! the daemon user's `~/.ssh`; the host's `known_hosts` read-only in both
//! cases, its absence a permanent error) and
//! `REMOTER_IMAGE_FLAKE_REV` (the clone's synced HEAD) so the init script can
//! build from the remote `git+ssh` flake at exactly the hashed revision.
//!
//! On a cache miss: `docker build` the Dockerfile, then run an **init
//! container** with the repo mounted read-only at `/repo` (a throwaway tmpfs
//! over `/repo/.devenv` — `devenv shell` must write its state somewhere) —
//! inside, the project's devenv shell is entered once (warming the nix store
//! and building the devshell profile into the image) and the optional
//! `agent-init.sh` runs (project-specific setup: toolchain prefetch, agent
//! CLI install, …) — and `docker commit` freezes the result as the tagged
//! image.
//!
//! Fail closed: a project without `.remoter/agent.Dockerfile` is a permanent
//! error — never a silent fallback to host mode, which would strip the
//! isolation the operator asked for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::Digest;

use crate::client::ProjectRepoConfig;
use crate::config::ExecutionConfig;
use crate::container::{PROJECT_ID_LABEL, docker};
use crate::driver::DriverError;

/// Repo-relative path of the agent image Dockerfile.
const DOCKERFILE_REL: &str = ".remoter/agent.Dockerfile";
/// Repo-relative path of the image build context — the content key covers
/// every file in it.
const IMAGE_CONTEXT_REL: &str = ".remoter";
/// Repo-relative path of the optional init script (runs once per image build).
const INIT_SCRIPT_REL: &str = ".remoter/agent-init.sh";
/// Where the init script lands inside the image (the Dockerfile COPYs it).
const INIT_SCRIPT_CONTAINER: &str = "/opt/agent-init.sh";
/// Repo-relative path of the optional per-turn freshness hook — see the
/// module docs for the exit-code contract.
const CHECK_SCRIPT_REL: &str = ".remoter/check-image.sh";
/// Exit code the freshness hook uses to say "the image is stale, rebuild".
const CHECK_EXIT_STALE: i32 = 42;
/// Image label recording the clone HEAD the init step built from — read back
/// per turn and handed to the freshness hook as `REMOTER_IMAGE_BUILT_REV`.
const IMAGE_FLAKE_REV_LABEL: &str = "remoter.image_flake_rev";
/// How long the freshness hook may run before it is killed (fail open).
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Serializes image builds per project — two parallel runs of one project
/// must not race the same `docker build`/`docker commit`.
#[derive(Clone, Default)]
pub struct ImageLocks {
    locks: Arc<Mutex<HashMap<i32, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ImageLocks {
    fn for_project(&self, project_id: i32) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .expect("image locks poisoned")
            .entry(project_id)
            .or_default()
            .clone()
    }
}

/// Ensures the project's agent image exists locally and returns its tag.
///
/// `repo` is the central clone (`p<id>/repo`) — the Dockerfile is read from
/// the clone's checkout after syncing it to the fetched base branch tip
/// (`origin/<base_branch>`, or `origin/HEAD` when unset; the daemon otherwise
/// only fetches, never updates the clone's working tree — worktrees branch
/// off the remote-tracking refs). The sync runs under the per-project lock so
/// two parallel runs cannot race git's `index.lock`.
pub async fn ensure_project_image(
    cfg: &ExecutionConfig,
    locks: &ImageLocks,
    repo: &Path,
    project: &ProjectRepoConfig,
) -> Result<String, DriverError> {
    let lock = locks.for_project(project.project_id);
    let _guard = lock.lock().await;

    crate::workspace::sync_clone_checkout(repo, project.base_branch.as_deref())
        .await
        .map_err(|e| DriverError::Transient(format!("sync clone checkout: {e}")))?;

    let dockerfile = repo.join(DOCKERFILE_REL);
    if !dockerfile.is_file() {
        return Err(DriverError::Permanent(format!(
            "container mode requires {DOCKERFILE_REL} in the project clone ({}), but it is absent — \
             add it to the repository or switch execution.mode back to \"host\"",
            repo.display()
        )));
    }
    let hash = content_hash(repo)?;
    let project_id = project.project_id;
    let tag = format!("{}:p{project_id}-{hash}", cfg.image_tag_prefix);
    let docker_bin = &cfg.docker_binary;

    // Fast path: the exact content hash is already built (also where the loser
    // of a build race lands — the per-project lock is already held). Even on
    // a hit the optional freshness hook gets a say: the tag covers only the
    // build context, not its external inputs (the remote flake's master).
    if image_exists(docker_bin, &tag).await {
        match run_freshness_hook(docker_bin, repo, &tag, CHECK_TIMEOUT).await {
            Freshness::NoHook | Freshness::Fresh | Freshness::CheckFailed => return Ok(tag),
            Freshness::Stale => {
                tracing::info!(
                    tag,
                    "freshness hook marked the agent image stale; rebuilding under the same tag"
                )
            }
        }
    }

    let build_tag = format!("{tag}-build");
    let init_container = format!("remoter-image-init-p{project_id}-{hash}");

    let result = build_and_commit(cfg, repo, project_id, &hash, &tag, &build_tag, &init_container).await;

    // Best-effort cleanup of the intermediate artifacts — the tagged image is
    // the deliverable; the build-stage image only wastes disk.
    let _ = docker(docker_bin, &["rmi".to_string(), "-f".to_string(), build_tag]).await;
    let _ = docker(docker_bin, &["rm".to_string(), "-f".to_string(), init_container]).await;
    result
}

/// `docker build` → init container (repo RO at `/repo`) → `docker commit`
/// with the provenance labels.
async fn build_and_commit(
    cfg: &ExecutionConfig,
    repo: &Path,
    project_id: i32,
    hash: &str,
    tag: &str,
    build_tag: &str,
    init_container: &str,
) -> Result<String, DriverError> {
    let docker_bin = &cfg.docker_binary;
    let context = repo.join(".remoter");

    tracing::info!(tag, "agent image cache miss; building");
    docker(
        docker_bin,
        &[
            "build".to_string(),
            "-t".to_string(),
            build_tag.to_string(),
            "-f".to_string(),
            context.join("agent.Dockerfile").to_string_lossy().into_owned(),
            context.to_string_lossy().into_owned(),
        ],
    )
    .await
    .map_err(image_build_error)?;

    // `devenv shell` writes its state under `.devenv` — impossible on the
    // read-only `/repo` bind (run #942). Give the init container a throwaway
    // tmpfs there; the warm that matters (the nix store) lives on the
    // container's own layer and survives `docker commit`. runc cannot create
    // the mountpoint inside a read-only bind, so the host side must exist.
    tokio::fs::create_dir_all(repo.join(".devenv"))
        .await
        .map_err(|e| DriverError::Transient(format!("mkdir {}/.devenv: {e}", repo.display())))?;

    // The init step warms the project's devenv shell into the image (the run
    // container's first `devenv shell --` must not pay a full nix download)
    // and runs the project's optional init script inside that shell.
    let init_cmd = if repo.join(INIT_SCRIPT_REL).is_file() {
        format!("cd /repo && devenv shell --no-tui --no-eval-cache -- sh {INIT_SCRIPT_CONTAINER}")
    } else {
        "cd /repo && devenv shell --no-tui --no-eval-cache -- true".to_string()
    };
    // Pin the remote-flake fetch to the exact revision the content hash was
    // computed from (master could move between hashing and building).
    let flake_rev = clone_head_rev(repo)?;
    let ssh_args = ssh_access_args(
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from).as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )?;
    let run_args = init_run_args(init_container, repo, build_tag, &init_cmd, &flake_rev, ssh_args);
    docker(docker_bin, &run_args).await.map_err(image_build_error)?;

    docker(
        docker_bin,
        &[
            "commit".to_string(),
            "-c".to_string(),
            format!("LABEL {PROJECT_ID_LABEL}=\"{project_id}\""),
            "-c".to_string(),
            format!("LABEL remoter.image_hash=\"{hash}\""),
            "-c".to_string(),
            format!("LABEL {IMAGE_FLAKE_REV_LABEL}=\"{flake_rev}\""),
            init_container.to_string(),
            tag.to_string(),
        ],
    )
    .await
    .map_err(image_build_error)?;
    tracing::info!(tag, "agent image built and committed");
    Ok(tag.to_string())
}

/// A build failure is permanent: retrying the same content hash cannot
/// succeed, and the ticket must escalate rather than burn attempts.
fn image_build_error(e: DriverError) -> DriverError {
    match e {
        DriverError::Transient(msg) | DriverError::Permanent(msg) | DriverError::Stalled(msg) => {
            DriverError::Permanent(format!("agent image build failed (not retryable): {msg}"))
        }
    }
}

/// `sha256` over the whole `.remoter/` build context — every file, sorted
/// context-relative path + bytes — first 16 hex chars, the tag's content
/// key. Inputs beyond the context (the remote flake's master) are the
/// freshness hook's job, not the tag's.
fn content_hash(repo: &Path) -> Result<String, DriverError> {
    let context = repo.join(IMAGE_CONTEXT_REL);
    let mut files = Vec::new();
    let mut visited = std::collections::HashSet::new();
    collect_context_files(&context, &mut visited, &mut files)?;
    files.sort();
    let mut hasher = sha2::Sha256::new();
    for f in files {
        let rel = f
            .strip_prefix(&context)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace('\\', "/");
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(read_file(&f)?);
        hasher.update([0]);
    }
    let hex = format!("{:x}", hasher.finalize());
    Ok(hex[..16].to_string())
}

/// Recursively collects the files under `dir`; a visited-set of canonical
/// directory paths guards against symlink loops.
fn collect_context_files(
    dir: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) -> Result<(), DriverError> {
    let canon = dir
        .canonicalize()
        .map_err(|e| DriverError::Permanent(format!("cannot canonicalize {}: {e}", dir.display())))?;
    if !visited.insert(canon) {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| DriverError::Permanent(format!("cannot list {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| DriverError::Permanent(format!("cannot list {}: {e}", dir.display())))?;
        let path = entry.path();
        if path.is_dir() {
            collect_context_files(&path, visited, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

fn read_file(p: &Path) -> Result<Vec<u8>, DriverError> {
    std::fs::read(p).map_err(|e| DriverError::Permanent(format!("cannot read {}: {e}", p.display())))
}

/// `git rev-parse HEAD` of the synced clone — passed to the init container as
/// `REMOTER_IMAGE_FLAKE_REV` so the init script builds the remote flake at
/// exactly the revision the content hash was computed from.
fn clone_head_rev(repo: &Path) -> Result<String, DriverError> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| DriverError::Transient(format!("git rev-parse HEAD in {}: {e}", repo.display())))?;
    if !out.status.success() {
        return Err(DriverError::Permanent(format!(
            "git rev-parse HEAD in {} failed: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Docker-run args giving the init container GitHub SSH access for the
/// `git+ssh` flake fetch: agent forwarding when `SSH_AUTH_SOCK` points at a
/// live agent socket (the private key never leaves the host), otherwise a
/// read-only mount of the daemon user's `~/.ssh`. A stale `SSH_AUTH_SOCK`
/// (agent restarted/exited) falls back to the `~/.ssh` mount. The host's
/// `known_hosts` is mounted read-only in both cases (GitHub's host key is
/// already trusted on the host) via a fixed path referenced from
/// `GIT_SSH_COMMAND`, since the image has no `/etc/ssh` — in agent mode
/// `~/.ssh` is deliberately not mounted, so a missing `known_hosts` is a
/// permanent error rather than a mid-build host-key verification failure.
fn ssh_access_args(ssh_auth_sock: Option<&Path>, home: Option<&Path>) -> Result<Vec<String>, DriverError> {
    let mut args = Vec::new();
    let agent = ssh_auth_sock.filter(|s| !s.as_os_str().is_empty() && agent_socket_live(s));
    match agent {
        Some(sock) => {
            args.extend([
                "-v".to_string(),
                format!("{}:/remoter-ssh-agent.sock", sock.display()),
                "-e".to_string(),
                "SSH_AUTH_SOCK=/remoter-ssh-agent.sock".to_string(),
            ]);
        }
        None => match home.map(|h| h.join(".ssh")).filter(|d| d.is_dir()) {
            Some(ssh_dir) => {
                args.extend(["-v".to_string(), format!("{}:/root/.ssh:ro", ssh_dir.display())]);
            }
            None => {
                return Err(DriverError::Permanent(
                    "agent image build needs GitHub SSH access for the git+ssh flake fetch, but \
                     SSH_AUTH_SOCK does not point at a live agent socket and the daemon user has \
                     no ~/.ssh directory"
                        .to_string(),
                ));
            }
        },
    }
    match home.map(|h| h.join(".ssh/known_hosts")).filter(|p| p.is_file()) {
        Some(known_hosts) => {
            args.extend([
                "-v".to_string(),
                format!("{}:/etc/ssh_known_hosts_remoter:ro", known_hosts.display()),
                "-e".to_string(),
                "GIT_SSH_COMMAND=ssh -o GlobalKnownHostsFile=/etc/ssh_known_hosts_remoter".to_string(),
            ]);
        }
        None => {
            return Err(DriverError::Permanent(
                "agent image build needs the daemon user's ~/.ssh/known_hosts (GitHub's host key \
                 must already be trusted on the host), but the file is absent — run \
                 `ssh-keyscan github.com >> ~/.ssh/known_hosts` as the daemon user"
                    .to_string(),
            ));
        }
    }
    Ok(args)
}

/// Is `SSH_AUTH_SOCK` a socket with a live listener? A daemon inheriting a
/// stale path (agent restarted or exited) must not select agent forwarding —
/// the container would get an unusable socket and the flake fetch would fail.
fn agent_socket_live(sock: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(sock).is_ok()
}

/// Assembles the `docker run` arguments for the init container.
fn init_run_args(
    init_container: &str,
    repo: &Path,
    build_tag: &str,
    init_cmd: &str,
    flake_rev: &str,
    ssh_args: Vec<String>,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--name".to_string(),
        init_container.to_string(),
        "-v".to_string(),
        format!("{}:/repo:ro", repo.display()),
        "--mount".to_string(),
        "type=tmpfs,destination=/repo/.devenv".to_string(),
        "-e".to_string(),
        format!("REMOTER_IMAGE_FLAKE_REV={flake_rev}"),
    ];
    args.extend(ssh_args);
    args.extend([
        build_tag.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        init_cmd.to_string(),
    ]);
    args
}

async fn image_exists(docker_bin: &str, tag: &str) -> bool {
    docker(
        docker_bin,
        &["image".to_string(), "inspect".to_string(), tag.to_string()],
    )
    .await
    .is_ok()
}

/// Reads one label off a built image; `None` when the image or label is
/// absent (`docker image inspect` renders a missing map key as `<no value>`).
async fn image_label(docker_bin: &str, tag: &str, label: &str) -> Option<String> {
    let format = format!("{{{{index .Config.Labels \"{label}\"}}}}");
    let value = docker(
        docker_bin,
        &[
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            format,
            tag.to_string(),
        ],
    )
    .await
    .ok()?;
    match value.as_str() {
        "" | "<no value>" => None,
        _ => Some(value),
    }
}

/// Outcome of the per-turn freshness hook (`.remoter/check-image.sh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freshness {
    /// No hook file — the content-keyed tag alone decides.
    NoHook,
    /// Exit 0: the cached image is fresh.
    Fresh,
    /// Exit 42 (reserved): the image is stale, rebuild it under the same tag.
    Stale,
    /// The check itself failed (other non-zero, spawn error, timeout) — keep
    /// the cache: a failed *check* must not escalate a ticket or trigger a
    /// pointless rebuild.
    CheckFailed,
}

/// Runs the optional freshness hook on the host (cwd = the central clone)
/// with the documented env contract; the daemon's own environment (notably
/// `SSH_AUTH_SOCK`/`HOME`) is inherited. Times out with a kill after
/// `timeout`.
async fn run_freshness_hook(docker_bin: &str, repo: &Path, tag: &str, timeout: std::time::Duration) -> Freshness {
    if !repo.join(CHECK_SCRIPT_REL).is_file() {
        return Freshness::NoHook;
    }
    let built_rev = image_label(docker_bin, tag, IMAGE_FLAKE_REV_LABEL)
        .await
        .unwrap_or_default();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg(CHECK_SCRIPT_REL)
        .current_dir(repo)
        .env("REMOTER_IMAGE_TAG", tag)
        .env("REMOTER_IMAGE_BUILT_REV", &built_rev)
        .env("REMOTER_DOCKER", docker_bin)
        // Dropping the future on timeout must kill the script, not orphan it.
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => {
            tracing::warn!(tag, "agent image freshness hook timed out; keeping the cached image");
            Freshness::CheckFailed
        }
        Ok(Err(e)) => {
            tracing::warn!(tag, error = %e, "agent image freshness hook failed to spawn; keeping the cached image");
            Freshness::CheckFailed
        }
        Ok(Ok(out)) if out.status.success() => Freshness::Fresh,
        Ok(Ok(out)) if out.status.code() == Some(CHECK_EXIT_STALE) => Freshness::Stale,
        Ok(Ok(out)) => {
            tracing::warn!(
                tag,
                status = %out.status,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "agent image freshness hook failed; keeping the cached image"
            );
            Freshness::CheckFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal repo layout: `.remoter/agent.Dockerfile` plus whatever the
    /// test adds on top.
    fn setup_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("remoter-image-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".remoter")).unwrap();
        std::fs::write(dir.join(".remoter/agent.Dockerfile"), "FROM scratch\n").unwrap();
        dir
    }

    #[test]
    fn content_hash_changes_with_init_script() {
        let dir = setup_repo("init");
        let without = content_hash(&dir).unwrap();
        std::fs::write(dir.join(".remoter/agent-init.sh"), "#!/bin/sh\n").unwrap();
        let with = content_hash(&dir).unwrap();
        assert_eq!(without.len(), 16);
        assert_ne!(without, with);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_hash_is_deterministic_and_keyed_on_dockerfile_content() {
        let dir = setup_repo("det");
        let first = content_hash(&dir).unwrap();
        assert_eq!(first, content_hash(&dir).unwrap());
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        std::fs::write(dir.join(".remoter/agent.Dockerfile"), "FROM alpine:3.23\n").unwrap();
        assert_ne!(first, content_hash(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_hash_covers_every_context_file() {
        let dir = setup_repo("ctx");
        let first = content_hash(&dir).unwrap();
        // Any file in the build context — not just the Dockerfile/init
        // script — is part of the content key (e.g. agent-flake.nix).
        std::fs::write(dir.join(".remoter/agent-flake.nix"), "{ }\n").unwrap();
        let second = content_hash(&dir).unwrap();
        assert_ne!(first, second);
        std::fs::write(dir.join(".remoter/agent-flake.nix"), "{ } # changed\n").unwrap();
        assert_ne!(second, content_hash(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Home dir with `.ssh/known_hosts`; returns (repo_dir, home).
    fn setup_home(name: &str) -> (PathBuf, PathBuf) {
        let repo = setup_repo(name);
        let home = repo.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/known_hosts"), "github.com ssh-ed25519 AAAA\n").unwrap();
        (repo, home)
    }

    /// Socket path for the bind tests: deliberately NOT under
    /// `std::env::temp_dir()` — in the nix sandbox TMPDIR is deep enough that
    /// the path exceeds SUN_LEN and the bind fails for unrelated reasons.
    fn test_sock(name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/remoter-image-sock-{name}-{}", std::process::id()))
    }

    #[test]
    fn ssh_access_args_prefers_agent_forwarding() {
        let (repo, home) = setup_home("ssh-agent");
        let sock = test_sock("agent");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let args = ssh_access_args(Some(&sock), Some(&home)).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains(&format!("{}:/remoter-ssh-agent.sock", sock.display())));
        assert!(joined.contains("SSH_AUTH_SOCK=/remoter-ssh-agent.sock"));
        assert!(joined.contains("ssh_known_hosts_remoter"));
        assert!(joined.contains("GIT_SSH_COMMAND"));
        assert!(!joined.contains("/root/.ssh"));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn ssh_access_args_falls_back_from_stale_agent_socket() {
        let (repo, home) = setup_home("ssh-stale");
        // A stale SSH_AUTH_SOCK: path does not exist / no listener on it.
        let stale = repo.join("gone.sock");
        let args = ssh_access_args(Some(&stale), Some(&home)).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains(&format!("{}:/root/.ssh:ro", home.join(".ssh").display())));
        assert!(joined.contains("ssh_known_hosts_remoter"));
        assert!(!joined.contains("SSH_AUTH_SOCK"));
        // A regular file is not a usable agent socket either.
        let not_a_sock = repo.join("file.sock");
        std::fs::write(&not_a_sock, "x").unwrap();
        assert_eq!(args, ssh_access_args(Some(&not_a_sock), Some(&home)).unwrap());
        // An empty SSH_AUTH_SOCK is the same as unset.
        assert_eq!(args, ssh_access_args(Some(Path::new("")), Some(&home)).unwrap());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn ssh_access_args_falls_back_to_ssh_dir_mount() {
        let (repo, home) = setup_home("ssh-key");
        let args = ssh_access_args(None, Some(&home)).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains(&format!("{}:/root/.ssh:ro", home.join(".ssh").display())));
        assert!(joined.contains("ssh_known_hosts_remoter"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn ssh_access_args_fail_closed_without_known_hosts() {
        let (repo, home) = setup_home("ssh-nokh");
        std::fs::remove_file(home.join(".ssh/known_hosts")).unwrap();
        // Key mode: ~/.ssh mounts but the GitHub host key is untrusted.
        assert!(ssh_access_args(None, Some(&home)).is_err());
        // Agent mode: ~/.ssh is deliberately not mounted, so the container
        // would have no trusted host key at all.
        let sock = test_sock("nokh");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(ssh_access_args(Some(&sock), Some(&home)).is_err());
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn ssh_access_args_fail_closed_without_any_source() {
        let repo = setup_repo("ssh-none");
        let home = repo.join("home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(ssh_access_args(None, Some(&home)).is_err());
        assert!(ssh_access_args(None, None).is_err());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn init_run_args_carry_flake_rev_and_ssh() {
        let args = init_run_args(
            "init-ctr",
            Path::new("/srv/repos/p1/repo"),
            "img:build",
            "cd /repo && true",
            "deadbeef",
            vec!["-e".to_string(), "SSH_AUTH_SOCK=/x".to_string()],
        );
        assert!(args.contains(&"REMOTER_IMAGE_FLAKE_REV=deadbeef".to_string()));
        assert!(args.contains(&"SSH_AUTH_SOCK=/x".to_string()));
        assert_eq!(args.last().unwrap(), "cd /repo && true");
    }

    /// A nonexistent docker binary: `image_label` cannot inspect anything and
    /// resolves to `None`, so the hook gets an empty `REMOTER_IMAGE_BUILT_REV`.
    const NO_DOCKER: &str = "remoter-test-no-such-docker";

    fn write_hook(dir: &Path, body: &str) {
        std::fs::write(dir.join(CHECK_SCRIPT_REL), body).unwrap();
    }

    #[tokio::test]
    async fn freshness_hook_absent_is_no_hook() {
        let dir = setup_repo("hook-none");
        let f = run_freshness_hook(NO_DOCKER, &dir, "img:tag", std::time::Duration::from_secs(5)).await;
        assert_eq!(f, Freshness::NoHook);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn freshness_hook_maps_exit_codes() {
        let dir = setup_repo("hook-codes");
        let t = std::time::Duration::from_secs(5);
        write_hook(&dir, "exit 0\n");
        assert_eq!(
            run_freshness_hook(NO_DOCKER, &dir, "img:tag", t).await,
            Freshness::Fresh
        );
        write_hook(&dir, "exit 42\n");
        assert_eq!(
            run_freshness_hook(NO_DOCKER, &dir, "img:tag", t).await,
            Freshness::Stale
        );
        write_hook(&dir, "exit 1\n");
        assert_eq!(
            run_freshness_hook(NO_DOCKER, &dir, "img:tag", t).await,
            Freshness::CheckFailed
        );
        // Death by signal is also a failed check, not a stale verdict.
        write_hook(&dir, "kill -9 $$\n");
        assert_eq!(
            run_freshness_hook(NO_DOCKER, &dir, "img:tag", t).await,
            Freshness::CheckFailed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn freshness_hook_receives_env_contract() {
        let dir = setup_repo("hook-env");
        write_hook(
            &dir,
            "printf '%s|%s|%s' \"$REMOTER_IMAGE_TAG\" \"$REMOTER_IMAGE_BUILT_REV\" \"$REMOTER_DOCKER\" > captured\n",
        );
        let f = run_freshness_hook(NO_DOCKER, &dir, "img:p1-abc", std::time::Duration::from_secs(5)).await;
        assert_eq!(f, Freshness::Fresh);
        // No docker -> no readable label -> empty REMOTER_IMAGE_BUILT_REV.
        assert_eq!(
            std::fs::read_to_string(dir.join("captured")).unwrap(),
            "img:p1-abc||remoter-test-no-such-docker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn freshness_hook_timeout_fails_open() {
        let dir = setup_repo("hook-timeout");
        write_hook(&dir, "sleep 30\n");
        let f = run_freshness_hook(NO_DOCKER, &dir, "img:tag", std::time::Duration::from_millis(200)).await;
        assert_eq!(f, Freshness::CheckFailed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
