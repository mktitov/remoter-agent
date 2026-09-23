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
//!
//! ## Shared nix binary cache (`[execution] nix_binary_cache_dir`)
//!
//! Baking a nix-based image from scratch takes tens of minutes (the devenv
//! warm plus the crane build of `remoter-mcp` inside the init container).
//! When the operator points `nix_binary_cache_dir` at a host directory, the
//! daemon treats it as a shared `file://` binary cache — but only for
//! nix-based projects ([`nix_cache_applicable`]): the Dockerfile builds `FROM`
//! a nix image, or the repo carries `devenv.nix`/`devenv.yaml`. The init
//! container then mounts the dir rw at [`NIX_CACHE_CONTAINER_DIR`] with
//! `NIX_CONFIG` adding it as an extra substituter, exports its gcroot
//! closures back into it after the bake (best-effort — a failed export never
//! fails the build), and run containers mount it read-only
//! ([`nix_cache_run_args`]). When the host has nix and the image matches the
//! host's OS/arch, the optional `.remoter/seed-nix-cache.sh` hook runs on the
//! host before the build to seed the cache from the host's warm store
//! (fail-open, like the freshness hook). The store itself is never mounted:
//! `docker commit` must produce a self-contained image, and the substituter
//! model guarantees that — substituted paths materialize into the container's
//! own store and are committed.
//!
//! Signing: with nix on the host the daemon generates a binary-cache keypair
//! once, next to the cache dir (`remoter-nix-cache-key.{secret,pub}` — never
//! inside it, so the read-only run-container mount cannot leak the secret).
//! Containers trust the public key; the secret is bind-mounted read-only into
//! the init container only (bind mounts never reach `docker commit`) and used
//! to sign the export. Without host nix there is no key and containers get
//! `require-sigs = false` — acceptable because the cache dir is private to
//! this docker host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::Digest;

use crate::client::ProjectRepoConfig;
use crate::config::ExecutionConfig;
use crate::container::{IMAGE_INIT_LABEL, PROJECT_ID_LABEL, TASK_ID_LABEL, docker};
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
/// Repo-relative path of the optional host-side cache seeding hook — run
/// before the bake when the host has nix and the image matches the host's
/// OS/arch. See the module docs for the env contract.
const SEED_SCRIPT_REL: &str = ".remoter/seed-nix-cache.sh";
/// How long the seeding hook may run before it is killed (fail open).
const SEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// Mount point of the shared nix binary cache inside init and run containers.
const NIX_CACHE_CONTAINER_DIR: &str = "/nix-cache";
/// Where the signing key (secret) is mounted inside the init container —
/// read-only, only for the bake, so it never reaches the committed image.
const NIX_CACHE_KEY_CONTAINER: &str = "/remoter-nix-cache-key.secret";
/// Basename of the cache signing keypair, kept NEXT TO the cache dir (never
/// inside it — run containers mount the cache dir itself).
const NIX_CACHE_KEY_NAME: &str = "remoter-nix-cache-key";
/// Prune policy (spec §9): how many of a project's newest image tags survive
/// the post-build prune — the just-built image plus one rollback. Fixed by
/// the spec; deliberately not configurable.
const IMAGE_TAGS_TO_KEEP: usize = 2;

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
/// two parallel runs cannot race git's `index.lock`. `task_id` labels the
/// init container with the ticket that triggered the build
/// (`remoter.task_id` — provenance for operators).
pub async fn ensure_project_image(
    cfg: &ExecutionConfig,
    locks: &ImageLocks,
    repo: &Path,
    project: &ProjectRepoConfig,
    task_id: i32,
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

    // Pin the remote-flake fetch to the exact revision the content hash was
    // computed from (master could move between hashing and building).
    let flake_rev = clone_head_rev(repo)?;
    let ssh_args = ssh_access_args(
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from).as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )?;

    let result = build_and_commit(BuildSpec {
        cfg,
        repo,
        project_id,
        task_id,
        hash: &hash,
        tag: &tag,
        build_tag: &build_tag,
        init_container: &init_container,
        flake_rev: &flake_rev,
        ssh_args,
    })
    .await;

    // Best-effort cleanup of the intermediate artifacts — the tagged image is
    // the deliverable; the build-stage image only wastes disk.
    let _ = docker(docker_bin, &["rmi".to_string(), "-f".to_string(), build_tag]).await;
    let _ = docker(docker_bin, &["rm".to_string(), "-f".to_string(), init_container]).await;
    if result.is_ok() {
        // Spec §9 prune policy: after a successful build, drop the project's
        // superseded tags (keep current + previous). Best-effort — it never
        // affects the returned tag.
        prune_superseded_images(docker_bin, &cfg.image_tag_prefix, project_id, &tag).await;
    }
    result
}

/// Everything [`build_and_commit`] needs — bundled so unit tests can inject
/// `flake_rev`/`ssh_args` directly instead of mutating the process
/// environment (`HOME`/`SSH_AUTH_SOCK`).
struct BuildSpec<'a> {
    cfg: &'a ExecutionConfig,
    repo: &'a Path,
    project_id: i32,
    task_id: i32,
    hash: &'a str,
    tag: &'a str,
    build_tag: &'a str,
    init_container: &'a str,
    flake_rev: &'a str,
    ssh_args: Vec<String>,
}

/// `docker build` → init container (repo RO at `/repo`) → `docker commit`
/// with the provenance labels.
async fn build_and_commit(spec: BuildSpec<'_>) -> Result<String, DriverError> {
    let BuildSpec {
        cfg,
        repo,
        project_id,
        task_id,
        hash,
        tag,
        build_tag,
        init_container,
        flake_rev,
        ssh_args,
    } = spec;
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

    // The shared nix binary cache (module docs § "Shared nix binary cache"):
    // prepared after `docker build` — seeding needs the built image's
    // OS/arch — and entirely fail-open (every failure degrades to no cache).
    let nix_cache = prepare_nix_cache(cfg, repo, docker_bin, build_tag, flake_rev).await;

    // The init step warms the project's devenv shell into the image (the run
    // container's first `devenv shell --` must not pay a full nix download)
    // and runs the project's optional init script inside that shell.
    let mut init_cmd = if repo.join(INIT_SCRIPT_REL).is_file() {
        format!("cd /repo && devenv shell --no-tui --no-eval-cache -- sh {INIT_SCRIPT_CONTAINER}")
    } else {
        "cd /repo && devenv shell --no-tui --no-eval-cache -- true".to_string()
    };
    if nix_cache.is_some() {
        // Pay the warm forward: export the closures of every gcroot (system
        // profile, devenv shell roots on the /repo/.devenv tmpfs) into the
        // shared cache so the next bake — and same-arch run containers —
        // substitute instead of rebuilding. Best-effort: the export must
        // never fail the bake.
        init_cmd = format!(
            "{init_cmd}; ( {} ) || echo 'remoter: nix cache export failed; continuing' >&2",
            cache_populate_cmd()
        );
    }
    // Best-effort pre-clean: the deterministic init-container name may still
    // be held by an orphan from a dropped build future or a crashed daemon —
    // possibly still *running* (ticket #204) — and `docker run --name` fails
    // permanently on the conflict. The orphan's work is useless (nobody will
    // commit its result) and the per-project lock rules out a live namesake
    // in this daemon, so force-removing it is always safe.
    let _ = docker(
        docker_bin,
        &["rm".to_string(), "-f".to_string(), init_container.to_string()],
    )
    .await;
    let run_args = init_run_args(
        init_container,
        repo,
        build_tag,
        &init_cmd,
        flake_rev,
        project_id,
        task_id,
        ssh_args,
        nix_cache.as_ref(),
    );
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

/// Assembles the `docker run` arguments for the init container. The
/// `remoter.image_init` marker lets the startup sweep (`container::sweep`)
/// reap orphans; `project_id`/`task_id` record provenance. With a prepared
/// nix cache the container additionally gets the cache dir mounted rw with
/// `NIX_CONFIG` pointing at it (and, when signing is active, the secret key
/// read-only — bind mounts never reach `docker commit`).
#[allow(clippy::too_many_arguments)]
fn init_run_args(
    init_container: &str,
    repo: &Path,
    build_tag: &str,
    init_cmd: &str,
    flake_rev: &str,
    project_id: i32,
    task_id: i32,
    ssh_args: Vec<String>,
    nix_cache: Option<&NixCachePlan>,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--name".to_string(),
        init_container.to_string(),
        "--label".to_string(),
        format!("{IMAGE_INIT_LABEL}=1"),
        "--label".to_string(),
        format!("{PROJECT_ID_LABEL}={project_id}"),
        "--label".to_string(),
        format!("{TASK_ID_LABEL}={task_id}"),
        "-v".to_string(),
        format!("{}:/repo:ro", repo.display()),
        "--mount".to_string(),
        "type=tmpfs,destination=/repo/.devenv".to_string(),
        "-e".to_string(),
        format!("REMOTER_IMAGE_FLAKE_REV={flake_rev}"),
    ];
    if let Some(cache) = nix_cache {
        args.extend([
            "-v".to_string(),
            format!("{}:{NIX_CACHE_CONTAINER_DIR}:rw", cache.dir.display()),
            // NIX_CONFIG replaces the image's ENV wholesale, so the compose
            // includes the base image's own experimental-features line.
            "-e".to_string(),
            format!("NIX_CONFIG={}", nix_config_with_cache(cache.public_key.as_deref())),
        ]);
        if let Some(secret) = &cache.secret_file {
            args.extend([
                "-v".to_string(),
                format!("{}:{NIX_CACHE_KEY_CONTAINER}:ro", secret.display()),
            ]);
        }
    }
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

/// Post-build prune (spec §9): removes the project's superseded image tags,
/// keeping the [`IMAGE_TAGS_TO_KEEP`] newest — the just-built image plus one
/// rollback for a bad Dockerfile. Entirely best-effort: a failed listing or
/// `rmi` (e.g. the image is still used by a running container) only logs a
/// warning; the build result is never affected. Runs under the per-project
/// image lock, so no concurrent build of the same project can race it.
async fn prune_superseded_images(docker_bin: &str, image_tag_prefix: &str, project_id: i32, current_tag: &str) {
    let listing = docker(
        docker_bin,
        &[
            "images".to_string(),
            "--format".to_string(),
            "{{.Repository}}:{{.Tag}} {{.CreatedAt}}".to_string(),
            "--filter".to_string(),
            format!("label={PROJECT_ID_LABEL}={project_id}"),
            "--filter".to_string(),
            format!("reference={image_tag_prefix}:p{project_id}-*"),
        ],
    )
    .await;
    let listing = match listing {
        Ok(listing) => listing,
        Err(e) => {
            tracing::warn!(project_id, error = %e, "agent image prune: cannot list images; keeping everything");
            return;
        }
    };
    let images: Vec<(String, String)> = listing
        .lines()
        .filter_map(|line| {
            let (tag, created_at) = line.split_once(' ')?;
            Some((tag.to_string(), created_at.to_string()))
        })
        .collect();
    for tag in superseded_tags(images, current_tag) {
        match docker(docker_bin, &["rmi".to_string(), tag.clone()]).await {
            Ok(_) => tracing::info!(tag, project_id, "agent image prune: removed superseded image"),
            Err(e) => {
                tracing::warn!(tag, project_id, error = %e, "agent image prune: cannot remove superseded image; skipping")
            }
        }
    }
}

/// From `(tag, created_at)` pairs (the `docker images --format
/// '{{.Repository}}:{{.Tag}} {{.CreatedAt}}'` rendering — the timestamp is
/// lexicographically sortable), pick the tags to delete under the
/// keep-current+previous policy (spec §9): everything but the
/// [`IMAGE_TAGS_TO_KEEP`] newest. `-build` staging tags are excluded (the
/// build's own cleanup handles them), and `current_tag` is never returned,
/// even if its timestamp would sort it out of the kept window.
fn superseded_tags(mut images: Vec<(String, String)>, current_tag: &str) -> Vec<String> {
    images.retain(|(tag, _)| !tag.ends_with("-build"));
    // Newest first; the tag breaks timestamp ties to keep the choice
    // deterministic.
    images.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    images
        .into_iter()
        .skip(IMAGE_TAGS_TO_KEEP)
        .map(|(tag, _)| tag)
        .filter(|tag| tag != current_tag)
        .collect()
}

/// A prepared shared nix binary cache for one image build (module docs §
/// "Shared nix binary cache").
struct NixCachePlan {
    /// Host directory bind-mounted at [`NIX_CACHE_CONTAINER_DIR`].
    dir: PathBuf,
    /// Public key the containers trust; `None` => `require-sigs = false`.
    public_key: Option<String>,
    /// Host secret key file, mounted read-only into the init container only.
    secret_file: Option<PathBuf>,
}

/// Prepares the shared nix binary cache for one bake: applicability gate,
/// directory creation, signing key, host seeding. Every failure degrades to
/// "no cache" (logged) — the cache is an accelerator and must never fail or
/// delay a build beyond its hook timeouts.
async fn prepare_nix_cache(
    cfg: &ExecutionConfig,
    repo: &Path,
    docker_bin: &str,
    build_tag: &str,
    flake_rev: &str,
) -> Option<NixCachePlan> {
    let dir = cfg.nix_binary_cache_dir.as_deref()?;
    if !nix_cache_applicable(repo) {
        tracing::debug!(
            "nix_binary_cache_dir is set but the project is not nix-based \
             (no nix FROM image, no devenv.nix/devenv.yaml); building without the cache"
        );
        return None;
    }
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!(dir = %dir.display(), error = %e, "cannot create the nix binary cache dir; building without the cache");
        return None;
    }
    let host_nix = host_nix_available();
    let (secret_file, public_key) = match signing_key(dir, host_nix) {
        Some((secret, public)) => (Some(secret), Some(public)),
        None => (None, None),
    };
    // Host seeding helps only when the container can actually substitute
    // host-built paths: same OS and architecture as the image. The hook is
    // optional — without it the cache still pays forward across bakes.
    let image_os_arch = docker(
        docker_bin,
        &[
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{.Os}}/{{.Architecture}}".to_string(),
            build_tag.to_string(),
        ],
    )
    .await;
    match image_os_arch {
        Ok(os_arch) if should_seed_from_host(host_nix, repo, &os_arch) => {
            run_seed_hook(repo, dir, flake_rev, SEED_TIMEOUT).await;
        }
        Ok(os_arch) => {
            tracing::debug!(image = %os_arch, "nix cache host seeding skipped (no hook, no host nix, or OS/arch mismatch)")
        }
        Err(e) => tracing::warn!(error = %e, "cannot inspect the built image for nix cache seeding; skipping the hook"),
    }
    Some(NixCachePlan {
        dir: dir.to_path_buf(),
        public_key,
        secret_file,
    })
}

/// Seeding runs only with host nix (to export from the host store), a hook to
/// run, and an image whose OS/arch matches the host — foreign-arch host paths
/// would never substitute inside the container.
fn should_seed_from_host(host_nix: bool, repo: &Path, image_os_arch: &str) -> bool {
    host_nix
        && repo.join(SEED_SCRIPT_REL).is_file()
        && image_os_arch_matches_host(std::env::consts::OS, std::env::consts::ARCH, image_os_arch)
}

/// Docker renders OS/arch as `linux/amd64`; Rust's `std::env::consts` as
/// `linux`/`x86_64` — map the arch names before comparing.
fn image_os_arch_matches_host(host_os: &str, host_arch: &str, image_os_arch: &str) -> bool {
    let docker_arch = match host_arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    image_os_arch == format!("{host_os}/{docker_arch}")
}

/// Is nix usable on the daemon host? Drives both key generation and seeding.
fn host_nix_available() -> bool {
    std::process::Command::new("nix-store")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Applicability gate (ticket #215): the shared cache only helps nix-based
/// projects — the image Dockerfile builds `FROM` a nix image (e.g.
/// `nixos/nix`), or the repo carries `devenv.nix`/`devenv.yaml`.
fn nix_cache_applicable(repo: &Path) -> bool {
    if repo.join("devenv.nix").is_file() || repo.join("devenv.yaml").is_file() {
        return true;
    }
    match std::fs::read_to_string(repo.join(DOCKERFILE_REL)) {
        Ok(dockerfile) => dockerfile_from_is_nix(&dockerfile),
        Err(_) => false,
    }
}

/// Any `FROM` line whose image reference mentions nix (`nixos/nix`,
/// `nixpkgs/...`, a custom `…/nix-base`). Flags (`--platform=…`) are skipped;
/// comments never reach here as their first token is `#`.
fn dockerfile_from_is_nix(dockerfile: &str) -> bool {
    dockerfile.lines().any(|line| {
        let mut tokens = line.split_whitespace();
        match tokens.next() {
            Some(kw) if kw.eq_ignore_ascii_case("from") => tokens
                .find(|t| !t.starts_with("--"))
                .map(|image| image.to_ascii_lowercase().contains("nix"))
                .unwrap_or(false),
            _ => false,
        }
    })
}

/// The full `NIX_CONFIG` for a container using the cache: it replaces the
/// base image's ENV wholesale, so it re-states `experimental-features` and
/// then adds the cache as an extra substituter — trusted public key when the
/// cache is signed, `require-sigs = false` otherwise.
fn nix_config_with_cache(public_key: Option<&str>) -> String {
    let mut cfg =
        format!("experimental-features = nix-command flakes\nextra-substituters = file://{NIX_CACHE_CONTAINER_DIR}");
    match public_key {
        Some(key) => {
            cfg.push_str("\nextra-trusted-public-keys = ");
            cfg.push_str(key.trim());
        }
        None => cfg.push_str("\nrequire-sigs = false"),
    }
    cfg
}

/// The cache keypair lives NEXT TO the cache dir, never inside it: run
/// containers mount the cache dir itself (read-only) and must not see the
/// secret. `None` when the cache dir is a filesystem root.
fn signing_key_paths(cache_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let parent = cache_dir.parent()?;
    Some((
        parent.join(format!("{NIX_CACHE_KEY_NAME}.secret")),
        parent.join(format!("{NIX_CACHE_KEY_NAME}.pub")),
    ))
}

/// Returns the signing keypair (secret path + public key content), generating
/// it once via `nix-store --generate-binary-cache-key` when the host has nix
/// and the pair is missing. A pre-existing pair is used even without host
/// nix — the signing itself happens inside the init container. Best-effort:
/// any failure means an unsigned cache (`require-sigs = false`).
fn signing_key(cache_dir: &Path, host_nix: bool) -> Option<(PathBuf, String)> {
    let (secret, public) = signing_key_paths(cache_dir)?;
    if !(secret.is_file() && public.is_file()) {
        if !host_nix {
            return None;
        }
        // Temp files + rename: two parallel bakes must not read a
        // half-written key.
        let tmp_secret = secret.with_extension("secret.tmp");
        let tmp_public = public.with_extension("pub.tmp");
        let out = std::process::Command::new("nix-store")
            .arg("--generate-binary-cache-key")
            .arg("remoter-nix-cache")
            .arg(&tmp_secret)
            .arg(&tmp_public)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&tmp_secret, std::fs::Permissions::from_mode(0o600));
                }
                if std::fs::rename(&tmp_secret, &secret).is_err() || std::fs::rename(&tmp_public, &public).is_err() {
                    tracing::warn!("cannot install the generated nix cache signing key; using an unsigned cache");
                    let _ = std::fs::remove_file(&tmp_secret);
                    let _ = std::fs::remove_file(&tmp_public);
                    return None;
                }
            }
            _ => {
                tracing::warn!("nix-store --generate-binary-cache-key failed; using an unsigned nix cache");
                let _ = std::fs::remove_file(&tmp_secret);
                let _ = std::fs::remove_file(&tmp_public);
                return None;
            }
        }
    }
    let public_key = std::fs::read_to_string(&public).ok()?.trim().to_string();
    if public_key.is_empty() {
        return None;
    }
    Some((secret, public_key))
}

/// The best-effort export appended to the init command after the devenv warm
/// and the init script: every gcroot's closure (the system profile — devenv,
/// the agent profile — and the devenv shell roots under the /repo/.devenv
/// tmpfs) is signed when the key is mounted and copied into the shared cache.
/// Runs outside the devenv shell; nix is on the base image's PATH. `$roots`
/// word-splitting is intentional (one store path per line).
fn cache_populate_cmd() -> String {
    format!(
        "roots=$(nix-store --gc --print-roots 2>/dev/null | grep -oE '/nix/store/[^ ]+' | sort -u); \
         if [ -n \"$roots\" ]; then \
         if [ -f {NIX_CACHE_KEY_CONTAINER} ]; then \
         printf '%s\\n' $roots | xargs nix store sign --key-file {NIX_CACHE_KEY_CONTAINER}; \
         fi; \
         printf '%s\\n' $roots | xargs nix copy --to file://{NIX_CACHE_CONTAINER_DIR}; \
         fi"
    )
}

/// Runs the optional host-side seeding hook (`.remoter/seed-nix-cache.sh`)
/// with cwd = the central clone and the documented env contract; the daemon's
/// own environment (`SSH_AUTH_SOCK`, `HOME`, nix) is inherited. Fail-open
/// exactly like the freshness hook: timeout, spawn failure and non-zero exit
/// only log — an unseeded cache just means a slower bake.
async fn run_seed_hook(repo: &Path, cache_dir: &Path, flake_rev: &str, timeout: std::time::Duration) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg(SEED_SCRIPT_REL)
        .current_dir(repo)
        .env("REMOTER_NIX_CACHE_DIR", cache_dir)
        .env("REMOTER_IMAGE_FLAKE_REV", flake_rev)
        // Dropping the future on timeout must kill the script, not orphan it.
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => tracing::info!("nix cache seeding hook finished"),
        Ok(Ok(out)) => tracing::warn!(
            status = %out.status,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "nix cache seeding hook failed; continuing with an unseeded cache"
        ),
        Ok(Err(e)) => tracing::warn!(error = %e, "nix cache seeding hook failed to spawn; continuing"),
        Err(_) => tracing::warn!("nix cache seeding hook timed out; continuing with an unseeded cache"),
    }
}

/// Run-container wiring for the shared cache (read-only): the agent's own
/// nix/devenv commands inside the run container substitute from the cache the
/// bakes warmed. Empty when the cache is not configured, the project is not
/// nix-based, or the dir does not exist (e.g. no bake has run yet).
pub(crate) fn nix_cache_run_args(cfg: &ExecutionConfig, repo: &Path) -> Vec<String> {
    let Some(dir) = cfg.nix_binary_cache_dir.as_deref() else {
        return Vec::new();
    };
    if !nix_cache_applicable(repo) || !dir.is_dir() {
        return Vec::new();
    }
    let public_key = signing_key_paths(dir)
        .and_then(|(_, public)| std::fs::read_to_string(public).ok())
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty());
    vec![
        "-v".to_string(),
        format!("{}:{NIX_CACHE_CONTAINER_DIR}:ro", dir.display()),
        "-e".to_string(),
        format!("NIX_CONFIG={}", nix_config_with_cache(public_key.as_deref())),
    ]
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
    fn init_run_args_carry_flake_rev_ssh_and_labels() {
        let args = init_run_args(
            "init-ctr",
            Path::new("/srv/repos/p1/repo"),
            "img:build",
            "cd /repo && true",
            "deadbeef",
            1,
            204,
            vec!["-e".to_string(), "SSH_AUTH_SOCK=/x".to_string()],
            None,
        );
        assert!(args.contains(&"REMOTER_IMAGE_FLAKE_REV=deadbeef".to_string()));
        assert!(args.contains(&"SSH_AUTH_SOCK=/x".to_string()));
        assert!(args.contains(&format!("{IMAGE_INIT_LABEL}=1")));
        assert!(args.contains(&format!("{PROJECT_ID_LABEL}=1")));
        assert!(args.contains(&format!("{TASK_ID_LABEL}=204")));
        assert_eq!(args.last().unwrap(), "cd /repo && true");
    }

    /// Installs a stub `docker` that logs every invocation's argv (one line
    /// per call) to `<dir>/docker.log` and exits 0 — except `rm`, which exits
    /// `rm_exit`. Returns the execution config pointing at the stub.
    fn stub_docker(dir: &Path, rm_exit: i32) -> ExecutionConfig {
        let stub = dir.join("docker-stub.sh");
        crate::testutil::write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = rm ]; then exit {rm_exit}; fi\nexit 0\n",
                dir.join("docker.log").display()
            ),
        );
        ExecutionConfig {
            docker_binary: stub.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    fn docker_log(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("docker.log"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn test_build_spec<'a>(cfg: &'a ExecutionConfig, repo: &'a Path) -> BuildSpec<'a> {
        BuildSpec {
            cfg,
            repo,
            project_id: 1,
            task_id: 204,
            hash: "abc123",
            tag: "img:p1-abc123",
            build_tag: "img:p1-abc123-build",
            init_container: "remoter-image-init-p1-abc123",
            flake_rev: "deadbeef",
            ssh_args: vec![],
        }
    }

    #[tokio::test]
    async fn build_and_commit_removes_stale_init_container_before_run() {
        let repo = setup_repo("build-preclean");
        let cfg = stub_docker(&repo, 0);
        let tag = build_and_commit(test_build_spec(&cfg, &repo)).await.unwrap();
        assert_eq!(tag, "img:p1-abc123");
        let log = docker_log(&repo);
        let rm_idx = log
            .iter()
            .position(|l| l == "rm -f remoter-image-init-p1-abc123")
            .expect("pre-clean rm -f missing");
        let run_idx = log
            .iter()
            .position(|l| l.starts_with("run --name remoter-image-init-p1-abc123"))
            .expect("docker run missing");
        assert!(rm_idx < run_idx, "rm -f must precede docker run: {log:?}");
        let run = &log[run_idx];
        assert!(run.contains(&format!("--label {IMAGE_INIT_LABEL}=1")), "{run}");
        assert!(run.contains(&format!("--label {PROJECT_ID_LABEL}=1")), "{run}");
        assert!(run.contains(&format!("--label {TASK_ID_LABEL}=204")), "{run}");
        assert!(log.last().unwrap().starts_with("commit "), "{log:?}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn build_and_commit_tolerates_a_failing_preclean() {
        let repo = setup_repo("build-preclean-fail");
        let cfg = stub_docker(&repo, 1);
        let tag = build_and_commit(test_build_spec(&cfg, &repo)).await.unwrap();
        assert_eq!(tag, "img:p1-abc123");
        let log = docker_log(&repo);
        assert!(log.iter().any(|l| l.starts_with("run --name")), "{log:?}");
        assert!(log.last().unwrap().starts_with("commit "), "{log:?}");
        let _ = std::fs::remove_dir_all(&repo);
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

    // ── shared nix binary cache ────────────────────────────────────────────

    #[test]
    fn nix_cache_gate_accepts_nix_dockerfile_or_devenv() {
        let dir = setup_repo("gate");
        // setup_repo writes `FROM scratch` — not nix-based.
        assert!(!nix_cache_applicable(&dir));

        std::fs::write(dir.join(DOCKERFILE_REL), "FROM nixos/nix:2.30.2\n").unwrap();
        assert!(nix_cache_applicable(&dir));
        std::fs::write(
            dir.join(DOCKERFILE_REL),
            "FROM --platform=linux/amd64 nixpkgs/nix:latest\n",
        )
        .unwrap();
        assert!(nix_cache_applicable(&dir));
        std::fs::write(dir.join(DOCKERFILE_REL), "# FROM nixos/nix\nFROM ubuntu:24.04\n").unwrap();
        assert!(!nix_cache_applicable(&dir));

        // A devenv project qualifies regardless of the base image.
        std::fs::write(dir.join("devenv.nix"), "{ }\n").unwrap();
        assert!(nix_cache_applicable(&dir));
        std::fs::remove_file(dir.join("devenv.nix")).unwrap();
        std::fs::write(dir.join("devenv.yaml"), "inputs: {}\n").unwrap();
        assert!(nix_cache_applicable(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nix_config_with_cache_composes_full_config() {
        let unsigned = nix_config_with_cache(None);
        assert!(
            unsigned.contains("experimental-features = nix-command flakes"),
            "{unsigned}"
        );
        assert!(
            unsigned.contains("extra-substituters = file:///nix-cache"),
            "{unsigned}"
        );
        assert!(unsigned.contains("require-sigs = false"), "{unsigned}");
        assert!(!unsigned.contains("trusted-public-keys"), "{unsigned}");

        let signed = nix_config_with_cache(Some("remoter-nix-cache:BASE64==\n"));
        assert!(
            signed.contains("extra-trusted-public-keys = remoter-nix-cache:BASE64=="),
            "{signed}"
        );
        assert!(!signed.contains("require-sigs"), "{signed}");
    }

    #[test]
    fn signing_key_reads_preexisting_pair_without_host_nix() {
        let dir = std::env::temp_dir().join(format!("remoter-signing-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = dir.join("nix-cache");
        std::fs::create_dir_all(&cache).unwrap();
        // No pair, no host nix -> unsigned.
        assert!(signing_key(&cache, false).is_none());
        // A pre-existing pair is used even without host nix (the init
        // container does the signing).
        let (secret, public) = signing_key_paths(&cache).unwrap();
        std::fs::write(&secret, "remoter-nix-cache:SECRET\n").unwrap();
        std::fs::write(&public, "remoter-nix-cache:PUB==\n").unwrap();
        let (s, p) = signing_key(&cache, false).unwrap();
        assert_eq!(s, secret);
        assert_eq!(p, "remoter-nix-cache:PUB==");
        // The keypair lives next to the cache dir, never inside it.
        assert_eq!(secret.parent().unwrap(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_os_arch_matches_host_maps_docker_arch_names() {
        assert!(image_os_arch_matches_host("linux", "x86_64", "linux/amd64"));
        assert!(image_os_arch_matches_host("linux", "aarch64", "linux/arm64"));
        // Docker Desktop on macOS runs linux images — the host store (macOS
        // binaries) is useless to them.
        assert!(!image_os_arch_matches_host("macos", "aarch64", "linux/arm64"));
        assert!(!image_os_arch_matches_host("linux", "aarch64", "linux/amd64"));
        assert!(!image_os_arch_matches_host("linux", "x86_64", ""));
    }

    #[test]
    fn init_run_args_wire_the_nix_cache() {
        let signed = NixCachePlan {
            dir: PathBuf::from("/srv/cache"),
            public_key: Some("remoter-nix-cache:PUB==".to_string()),
            secret_file: Some(PathBuf::from("/srv/remoter-nix-cache-key.secret")),
        };
        let args = init_run_args(
            "c",
            Path::new("/r"),
            "img:build",
            "true",
            "rev",
            1,
            2,
            vec![],
            Some(&signed),
        );
        let joined = args.join(" ");
        assert!(joined.contains("/srv/cache:/nix-cache:rw"), "{joined}");
        assert!(joined.contains("extra-substituters = file:///nix-cache"), "{joined}");
        assert!(
            joined.contains("extra-trusted-public-keys = remoter-nix-cache:PUB=="),
            "{joined}"
        );
        assert!(
            joined.contains("/srv/remoter-nix-cache-key.secret:/remoter-nix-cache-key.secret:ro"),
            "{joined}"
        );

        let unsigned = NixCachePlan {
            dir: PathBuf::from("/srv/cache"),
            public_key: None,
            secret_file: None,
        };
        let args = init_run_args(
            "c",
            Path::new("/r"),
            "img:build",
            "true",
            "rev",
            1,
            2,
            vec![],
            Some(&unsigned),
        );
        let joined = args.join(" ");
        assert!(joined.contains("require-sigs = false"), "{joined}");
        assert!(!joined.contains("key.secret"), "{joined}");
    }

    #[test]
    fn cache_populate_cmd_is_best_effort_and_exports_gcroot_closures() {
        let cmd = cache_populate_cmd();
        assert!(cmd.contains("nix-store --gc --print-roots"), "{cmd}");
        assert!(cmd.contains("nix copy --to file:///nix-cache"), "{cmd}");
        assert!(
            cmd.contains("nix store sign --key-file /remoter-nix-cache-key.secret"),
            "{cmd}"
        );
    }

    #[tokio::test]
    async fn build_and_commit_wires_cache_and_best_effort_export() {
        let repo = setup_repo("build-cache");
        std::fs::write(repo.join(DOCKERFILE_REL), "FROM nixos/nix:2.30.2\n").unwrap();
        let cache_dir = repo.join("host-cache");
        let mut cfg = stub_docker(&repo, 0);
        cfg.nix_binary_cache_dir = Some(cache_dir.clone());
        let tag = build_and_commit(test_build_spec(&cfg, &repo)).await.unwrap();
        assert_eq!(tag, "img:p1-abc123");
        assert!(cache_dir.is_dir(), "the cache dir is created");
        let log = docker_log(&repo);
        // NIX_CONFIG embeds newlines, so one docker invocation spans several
        // log lines — assert against the full log, not a single line.
        let full = log.join("\n");
        let run_idx = log
            .iter()
            .position(|l| l.starts_with("run --name remoter-image-init-p1-abc123"))
            .expect("docker run missing");
        let run = &log[run_idx];
        assert!(run.contains(&format!("{}:/nix-cache:rw", cache_dir.display())), "{run}");
        assert!(full.contains("extra-substituters = file:///nix-cache"), "{full}");
        assert!(full.contains("nix copy --to file:///nix-cache"), "{full}");
        // The export is appended to the init command and must never fail the
        // bake: the whole thing is wrapped in `( … ) || echo …`.
        assert!(
            full.contains("|| echo 'remoter: nix cache export failed; continuing'"),
            "{full}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn build_and_commit_skips_cache_for_non_nix_projects() {
        let repo = setup_repo("build-nocache");
        // setup_repo's `FROM scratch` Dockerfile is not nix-based.
        let cache_dir = repo.join("host-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let mut cfg = stub_docker(&repo, 0);
        cfg.nix_binary_cache_dir = Some(cache_dir);
        build_and_commit(test_build_spec(&cfg, &repo)).await.unwrap();
        let log = docker_log(&repo);
        let run = log
            .iter()
            .find(|l| l.starts_with("run --name"))
            .expect("docker run missing");
        assert!(!run.contains("/nix-cache"), "{run}");
        assert!(!run.contains("NIX_CONFIG"), "{run}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn write_seed_hook(dir: &Path, body: &str) {
        std::fs::write(dir.join(SEED_SCRIPT_REL), body).unwrap();
    }

    #[tokio::test]
    async fn seed_hook_receives_env_contract() {
        let dir = setup_repo("seed-env");
        write_seed_hook(
            &dir,
            "printf '%s|%s' \"$REMOTER_NIX_CACHE_DIR\" \"$REMOTER_IMAGE_FLAKE_REV\" > captured\n",
        );
        run_seed_hook(
            &dir,
            Path::new("/srv/cache"),
            "deadbeef",
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(dir.join("captured")).unwrap(),
            "/srv/cache|deadbeef"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn seed_hook_failure_and_timeout_fail_open() {
        let dir = setup_repo("seed-fail");
        write_seed_hook(&dir, "exit 1\n");
        run_seed_hook(&dir, Path::new("/srv/cache"), "rev", std::time::Duration::from_secs(5)).await;
        write_seed_hook(&dir, "sleep 30\n");
        let start = std::time::Instant::now();
        run_seed_hook(
            &dir,
            Path::new("/srv/cache"),
            "rev",
            std::time::Duration::from_millis(200),
        )
        .await;
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nix_cache_run_args_mount_read_only_for_nix_projects() {
        let dir = setup_repo("run-args");
        std::fs::write(dir.join("devenv.nix"), "{ }\n").unwrap();
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cfg = ExecutionConfig {
            nix_binary_cache_dir: Some(cache.clone()),
            ..Default::default()
        };
        let args = nix_cache_run_args(&cfg, &dir);
        let joined = args.join(" ");
        assert!(
            joined.contains(&format!("{}:/nix-cache:ro", cache.display())),
            "{joined}"
        );
        assert!(joined.contains("extra-substituters = file:///nix-cache"), "{joined}");
        // No keypair next to the cache dir -> unsigned mode.
        assert!(joined.contains("require-sigs = false"), "{joined}");

        // With a public key the containers trust it instead.
        let (_, public) = signing_key_paths(&cache).unwrap();
        std::fs::write(&public, "remoter-nix-cache:PUB==\n").unwrap();
        let joined = nix_cache_run_args(&cfg, &dir).join(" ");
        assert!(
            joined.contains("extra-trusted-public-keys = remoter-nix-cache:PUB=="),
            "{joined}"
        );

        // Not configured / not nix-based / dir missing -> no wiring.
        assert!(nix_cache_run_args(&ExecutionConfig::default(), &dir).is_empty());
        let non_nix = setup_repo("run-args-nonix");
        assert!(nix_cache_run_args(&cfg, &non_nix).is_empty());
        std::fs::remove_dir_all(&cache).unwrap();
        assert!(nix_cache_run_args(&cfg, &dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&non_nix);
    }

    // ── image prune (spec §9) ────────────────────────────────────────────

    fn img(tag: &str, created_at: &str) -> (String, String) {
        (tag.to_string(), created_at.to_string())
    }

    #[test]
    fn superseded_tags_keeps_two_newest() {
        // 0 / 1 / 2 images: nothing is ever pruned.
        assert!(superseded_tags(vec![], "img:p1-a").is_empty());
        assert!(superseded_tags(vec![img("img:p1-a", "2026-09-20 10:00:00 +0000 UTC")], "img:p1-a").is_empty());
        assert!(
            superseded_tags(
                vec![
                    img("img:p1-a", "2026-09-20 10:00:00 +0000 UTC"),
                    img("img:p1-b", "2026-09-21 10:00:00 +0000 UTC"),
                ],
                "img:p1-b",
            )
            .is_empty()
        );
        // N images in arbitrary input order: the two newest by CreatedAt
        // survive; the rest come out newest-first.
        let pruned = superseded_tags(
            vec![
                img("img:p1-c", "2026-09-22 10:00:00 +0000 UTC"),
                img("img:p1-a", "2026-09-20 10:00:00 +0000 UTC"),
                img("img:p1-d", "2026-09-23 10:00:00 +0000 UTC"),
                img("img:p1-b", "2026-09-21 10:00:00 +0000 UTC"),
            ],
            "img:p1-d",
        );
        assert_eq!(pruned, vec!["img:p1-b".to_string(), "img:p1-a".to_string()]);
    }

    #[test]
    fn superseded_tags_excludes_build_tags_and_current() {
        // A leftover -build tag is never pruned here (the build's own cleanup
        // handles it), even when it is the oldest entry.
        let pruned = superseded_tags(
            vec![
                img("img:p1-d", "2026-09-23 10:00:00 +0000 UTC"),
                img("img:p1-c", "2026-09-22 10:00:00 +0000 UTC"),
                img("img:p1-x-build", "2026-09-19 10:00:00 +0000 UTC"),
                img("img:p1-b", "2026-09-21 10:00:00 +0000 UTC"),
            ],
            "img:p1-d",
        );
        assert_eq!(pruned, vec!["img:p1-b".to_string()]);
        // The just-built tag is never pruned, even with a bogus old timestamp
        // that would sort it out of the kept window.
        let pruned = superseded_tags(
            vec![
                img("img:p1-c", "2026-09-23 10:00:00 +0000 UTC"),
                img("img:p1-b", "2026-09-22 10:00:00 +0000 UTC"),
                img("img:p1-a", "2026-09-21 10:00:00 +0000 UTC"),
                img("img:p1-current", "2020-01-01 00:00:00 +0000 UTC"),
            ],
            "img:p1-current",
        );
        assert_eq!(pruned, vec!["img:p1-a".to_string()]);
    }

    #[test]
    fn superseded_tags_breaks_timestamp_ties_deterministically() {
        let images = || {
            vec![
                img("img:p1-b", "2026-09-22 10:00:00 +0000 UTC"),
                img("img:p1-c", "2026-09-22 10:00:00 +0000 UTC"),
                img("img:p1-a", "2026-09-22 10:00:00 +0000 UTC"),
            ]
        };
        // Equal timestamps: the tag name breaks the tie, so the choice is
        // stable across runs (a, b kept; c pruned).
        let pruned = superseded_tags(images(), "img:p1-d");
        assert_eq!(pruned, vec!["img:p1-c".to_string()]);
        assert_eq!(pruned, superseded_tags(images(), "img:p1-d"));
    }

    #[tokio::test]
    async fn prune_superseded_images_keeps_two_newest_and_is_best_effort() {
        let dir = std::env::temp_dir().join(format!("remoter-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Stub docker: logs argv; `images` answers five tags (including a
        // -build leftover); `rmi img:p1-busy` fails as if a container used it.
        let stub = dir.join("docker-stub.sh");
        crate::testutil::write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n\
                 if [ \"$1\" = images ]; then\n  \
                 printf '%s\\n' \\\n\
                 'img:p1-newest 2026-09-23 10:00:00 +0000 UTC' \\\n\
                 'img:p1-prev 2026-09-22 10:00:00 +0000 UTC' \\\n\
                 'img:p1-old 2026-09-21 10:00:00 +0000 UTC' \\\n\
                 'img:p1-busy 2026-09-20 10:00:00 +0000 UTC' \\\n\
                 'img:p1-stale-build 2026-09-19 10:00:00 +0000 UTC'\n  \
                 exit 0\nfi\n\
                 if [ \"$1\" = rmi ] && [ \"$2\" = img:p1-busy ]; then\n  \
                 echo 'image is being used by a running container' >&2\n  \
                 exit 1\nfi\n\
                 exit 0\n",
                dir.join("docker.log").display()
            ),
        );
        let docker_bin = stub.to_string_lossy().into_owned();
        prune_superseded_images(&docker_bin, "img", 1, "img:p1-newest").await;
        let log = std::fs::read_to_string(dir.join("docker.log")).unwrap();
        // The listing is scoped to the project's label and tag prefix.
        assert!(
            log.contains(&format!(
                "images --format {{{{.Repository}}}}:{{{{.Tag}}}} {{{{.CreatedAt}}}} \
                 --filter label={PROJECT_ID_LABEL}=1 --filter reference=img:p1-*"
            )),
            "{log}"
        );
        // Both superseded tags are attempted — the failing rmi does not stop
        // the other — while the kept tags and the -build tag are untouched.
        assert!(log.contains("rmi img:p1-old\n"), "{log}");
        assert!(log.contains("rmi img:p1-busy\n"), "{log}");
        assert!(!log.contains("rmi img:p1-newest"), "{log}");
        assert!(!log.contains("rmi img:p1-prev"), "{log}");
        assert!(!log.contains("rmi img:p1-stale-build"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prune_superseded_images_tolerates_a_failing_listing() {
        let dir = std::env::temp_dir().join(format!("remoter-prune-fail-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("docker-stub.sh");
        crate::testutil::write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nexit 1\n",
                dir.join("docker.log").display()
            ),
        );
        let docker_bin = stub.to_string_lossy().into_owned();
        // Must not panic or escalate: a failed `docker images` only logs.
        prune_superseded_images(&docker_bin, "img", 1, "img:p1-newest").await;
        let log = std::fs::read_to_string(dir.join("docker.log")).unwrap();
        assert!(!log.contains("rmi"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
