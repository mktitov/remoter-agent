//! Workspace layout helpers (spec §5.1): pure path/branch computation shared by
//! the run lifecycle and (from PoC-4) the git worktree management.
//!
//! ```text
//! <workspace_root>/
//!   p<project_id>/
//!     repo/                      # full clone (worktree source)       — PoC-4
//!     wt-<task_id>/              # per-ticket worktree + branch       — PoC-4
//!     refs/<mount>/repo/         # shared clone of a reference repo   — #169
//!     refs/<mount>/wt-<task_id>/ # detached ref worktree of a ticket  — #169
//! ```

use std::path::{Path, PathBuf};

use crate::client::ReferenceRepo;

/// `<workspace_root>/p<project_id>`
pub fn project_dir(root: &Path, project_id: i32) -> PathBuf {
    root.join(format!("p{project_id}"))
}

/// `<workspace_root>/p<project_id>/repo`
pub fn repo_dir(root: &Path, project_id: i32) -> PathBuf {
    project_dir(root, project_id).join("repo")
}

/// `<workspace_root>/p<project_id>/wt-<task_id>` — the per-ticket worktree the
/// agent works in.
pub fn worktree_dir(root: &Path, project_id: i32, task_id: i32) -> PathBuf {
    project_dir(root, project_id).join(format!("wt-{task_id}"))
}

/// `<workspace_root>/p<project_id>/refs/<mount>/repo` — the shared clone of one
/// project reference repo (docs/specs/cross-repo-projects.md). Cloned once,
/// re-fetched on every prepare, kept by cleanup.
pub fn refs_repo_dir(root: &Path, project_id: i32, mount: &str) -> PathBuf {
    project_dir(root, project_id).join("refs").join(mount).join("repo")
}

/// `<workspace_root>/p<project_id>/refs/<mount>/wt-<task_id>` — the per-ticket
/// detached worktree of a reference repo, surfaced inside the ticket worktree
/// as `.refs/<mount>`.
pub fn refs_worktree_dir(root: &Path, project_id: i32, mount: &str, task_id: i32) -> PathBuf {
    project_dir(root, project_id)
        .join("refs")
        .join(mount)
        .join(format!("wt-{task_id}"))
}

/// `agent/task-<id>-<slug>` — one branch per ticket attempt (spec §5.4). The
/// slug comes from the task *title* (the short summary), not the description.
pub fn branch_name(task_id: i32, title: &str) -> String {
    format!("agent/task-{task_id}-{}", slugify(title))
}

/// The branch an existing ticket worktree is on, or `None` when the worktree
/// doesn't exist yet (or is detached). Run setup must prefer this over
/// `branch_name(...)`: a ticket renamed between runs keeps the branch its
/// worktree was created with, and pushing a freshly recomputed name would
/// fail the refspec.
pub async fn existing_branch(root: &Path, project_id: i32, task_id: i32) -> Option<String> {
    let wt = worktree_dir(root, project_id, task_id);
    if !wt.exists() {
        return None;
    }
    worktree_branch(&wt).await.ok()
}

// ── git worktree lifecycle (spec §5.4) ───────────────────────────────────────

/// A prepared per-ticket worktree.
#[derive(Debug, Clone)]
pub struct PreparedWorktree {
    pub dir: PathBuf,
    pub branch: String,
    /// The worktree contains `devenv.nix` / `devenv.yaml` (spec §5.8).
    pub devenv: bool,
}

#[derive(Debug)]
pub struct WorkspaceError(pub String);

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WorkspaceError {}

/// Prepares the ticket worktree (spec §5.4 step 1), idempotently:
/// - clone `<repo_url>` into `p<project_id>/repo` on first use;
/// - on every later use, repoint `origin` at `<repo_url>` — the project's
///   Repository URL may have been edited since the clone was made;
/// - always `fetch origin` so a *new* worktree branches off fresh upstream state;
/// - reuse an existing `wt-<task_id>` as-is (the implement run continues the
///   plan run's worktree) and report the branch it is actually on — a ticket
///   renamed mid-flight keeps the branch its worktree was created with;
///   create it off `origin/<base_branch>` (or `origin/HEAD` when no base
///   branch is configured) otherwise.
pub async fn prepare(
    root: &Path,
    project_id: i32,
    task_id: i32,
    title: &str,
    repo_url: &str,
    base_branch: Option<&str>,
) -> Result<PreparedWorktree, WorkspaceError> {
    prepare_with_stack(root, project_id, task_id, title, repo_url, base_branch, None).await
}

/// `prepare` plus branch stacking for supervised child tickets: when
/// `stack_base` is the parent's local branch, a *new* child worktree branches
/// off that branch's tip instead of the fetched base — the child's commits
/// stack on the parent's branch and ride its PR. A reused worktree/branch is
/// untouched (stacking applies only at creation), and a reopened ticket whose
/// remote branch still exists keeps the remote-reattach precedence.
/// Clone `<repo_url>` into `p<project_id>/repo` on first use (with clone-race
/// tolerance: two parallel tickets of a brand-new project race this clone — the
/// loser's `git clone` fails with "destination path already exists", which is a
/// win as long as the repo is really there); on every later use repoint
/// `origin` at `<repo_url>` — the project's Repository URL may have been edited
/// since the clone was made. Returns the clone path. No fetch: callers fetch
/// the refs they need.
pub async fn ensure_repo_clone(root: &Path, project_id: i32, repo_url: &str) -> Result<PathBuf, WorkspaceError> {
    let repo = repo_dir(root, project_id);
    if !repo.exists() {
        if let Some(parent) = repo.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WorkspaceError(format!("mkdir {parent:?}: {e}")))?;
        }
        if let Err(e) = git(root, &["clone", repo_url, &repo.to_string_lossy()]).await {
            if !repo.exists() {
                return Err(e);
            }
            tracing::info!(repo = ?repo, "clone race lost; using the winner's clone");
        }
    } else {
        // The clone outlives the project's Repository URL setting: repoint
        // origin every prepare (idempotent, cheap) so an edited URL stops the
        // daemon fetching from / pushing to the stale remote (spec §5.4).
        git(&repo, &["remote", "set-url", "origin", repo_url]).await?;
    }
    Ok(repo)
}

pub async fn prepare_with_stack(
    root: &Path,
    project_id: i32,
    task_id: i32,
    title: &str,
    repo_url: &str,
    base_branch: Option<&str>,
    stack_base: Option<&str>,
) -> Result<PreparedWorktree, WorkspaceError> {
    let repo = ensure_repo_clone(root, project_id, repo_url).await?;
    let wt = worktree_dir(root, project_id, task_id);
    let branch = branch_name(task_id, title);

    let start_point = match base_branch {
        Some(b) => {
            git(&repo, &["fetch", "origin", b]).await?;
            format!("origin/{b}")
        }
        None => {
            git(&repo, &["fetch", "origin"]).await?;
            "origin/HEAD".to_string()
        }
    };

    let branch = if wt.exists() {
        // The worktree outlives the ticket title: reuse continues on the
        // branch the worktree was created with. Recomputing from the current
        // title would name a branch that doesn't exist locally and the push
        // at finish time would fail — the same reason `cleanup` takes the
        // recorded branch instead of recomputing it.
        worktree_branch(&wt).await.unwrap_or(branch)
    } else {
        let branch_exists = git_output(&repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .await
            .is_ok();
        if branch_exists {
            // The branch survived a worktree cleanup — reattach it.
            git(&repo, &["worktree", "add", &wt.to_string_lossy(), &branch]).await?;
        } else {
            // The local branch is gone (the completion cleanup deletes only
            // the local one; or a fresh workspace) but the remote branch may
            // still carry the pushed, reviewed commits: a reopened ticket
            // must continue from `origin/<branch>` — branching off base again
            // would fork that history and the finish-time push would be
            // rejected as non-fast-forward.
            let _ = git(&repo, &["fetch", "origin", &branch]).await;
            let remote_ref = format!("refs/remotes/origin/{branch}");
            let remote_exists = git_output(&repo, &["rev-parse", "--verify", &remote_ref]).await.is_ok();
            // `worktree add -b` auto-tracks a remote-tracking start point.
            // Tracking `origin/<branch>` itself (reopened ticket) is right;
            // tracking the *base* branch is not — a human running bare
            // `git push` in the worktree would hit "upstream branch does not
            // match" against `origin/master`.
            let (start, track) = if remote_exists {
                (format!("origin/{branch}"), "--track")
            } else if let Some(stack) = stack_base {
                // Supervised child: branch off the parent's local branch tip.
                // Never tracked — the parent branch has no upstream to track.
                (stack.to_string(), "--no-track")
            } else {
                (start_point, "--no-track")
            };
            git(
                &repo,
                &["worktree", "add", &wt.to_string_lossy(), "-b", &branch, track, &start],
            )
            .await?;
        }
        branch
    };

    Ok(PreparedWorktree {
        devenv: uses_devenv(&wt),
        dir: wt,
        branch,
    })
}

// ── reference repos (docs/specs/cross-repo-projects.md, #169) ────────────────

/// One reference repo mounted for a ticket run.
#[derive(Debug, Clone)]
pub struct RefMount {
    /// The mount name — surfaced as `.refs/<mount_name>` in the ticket
    /// worktree (symlink in host mode, read-only bind mount in container mode).
    pub mount_name: String,
    /// `refs/<mount>/wt-<task_id>` — the per-ticket detached worktree.
    pub worktree: PathBuf,
    /// `refs/<mount>/repo` — the shared ref clone. Container mode also mounts
    /// its `.git` (at the same absolute path, like the main repo's) so git
    /// commands work inside `/work/.refs/<mount_name>`.
    pub repo: PathBuf,
}

/// The result of [`prepare_reference_repos`] — fail-open: an unavailable ref
/// (network down, no keys, bad config) never fails the run; it lands in
/// `failed` for a warning log + ticket-thread note and the run continues
/// without that ref.
#[derive(Debug, Default)]
pub struct RefsOutcome {
    pub mounted: Vec<RefMount>,
    /// `(mount_name, error)` per ref that could not be prepared.
    pub failed: Vec<(String, String)>,
}

/// Prepares the project's reference repos for one ticket run, mirroring the
/// [`prepare_with_stack`] lifecycle per ref:
/// - clone `<repo_url>` into `refs/<mount>/repo` on first use (with the same
///   clone-race tolerance as the main repo), repoint `origin` on every later
///   use;
/// - always `fetch origin <base_branch>` (or bare `fetch origin` when the ref
///   has no base branch) so ref worktrees branch off fresh upstream state;
/// - `git worktree add refs/<mount>/wt-<task_id> --detach origin/<base>`
///   (`origin/HEAD` when no base branch). Detached, always: the daemon never
///   creates branches in — and never pushes to — a reference repo. An
///   existing ref worktree is reused as-is (the fetch above already refreshed
///   the clone; re-pointing a live worktree mid-ticket would confuse a run
///   that already read it).
///
/// When at least one ref mounted, `.refs/` is appended to the ticket
/// worktree's `info/exclude` so the mounts never dirty `git status
/// --porcelain` (the commit guard must stay clean). Per-ref failures are
/// collected in [`RefsOutcome::failed`], never returned.
pub async fn prepare_reference_repos(
    root: &Path,
    project_id: i32,
    task_id: i32,
    ticket_worktree: &Path,
    refs: &[ReferenceRepo],
) -> RefsOutcome {
    let mut outcome = RefsOutcome::default();
    for r in refs {
        match prepare_reference_repo(root, project_id, task_id, r).await {
            Ok(mount) => outcome.mounted.push(mount),
            Err(e) => {
                tracing::warn!(
                    mount = %r.mount_name,
                    error = %e,
                    "reference repo unavailable; continuing without it"
                );
                outcome.failed.push((r.mount_name.clone(), e.to_string()));
            }
        }
    }
    if !outcome.mounted.is_empty()
        && let Err(e) = exclude_refs_dir(ticket_worktree).await
    {
        tracing::warn!(
            dir = ?ticket_worktree,
            error = %e,
            "could not add .refs/ to git info/exclude — the commit guard may flag the ref mounts"
        );
    }
    outcome
}

async fn prepare_reference_repo(
    root: &Path,
    project_id: i32,
    task_id: i32,
    r: &ReferenceRepo,
) -> Result<RefMount, WorkspaceError> {
    let (repo, wt, name) = refs_paths(root, project_id, task_id, &r.mount_name)?;
    ensure_refs_clone(root, &repo, &r.repo_url).await?;

    let start_point = match r.base_branch.as_deref() {
        Some(b) => {
            git(&repo, &["fetch", "origin", b]).await?;
            format!("origin/{b}")
        }
        None => {
            git(&repo, &["fetch", "origin"]).await?;
            "origin/HEAD".to_string()
        }
    };

    if !wt.exists() {
        git(
            &repo,
            &["worktree", "add", &wt.to_string_lossy(), "--detach", &start_point],
        )
        .await?;
    }

    Ok(RefMount {
        mount_name: name,
        worktree: wt,
        repo,
    })
}

/// Branch-mode reference mount (docs/specs/cross-repo-projects.md): a
/// supervise/review run of a parent gets a read-only checkout of a supervised
/// child's branch — for a cross-project child the branch lives in the child
/// project's repo, which the parent's own clone never sees. Behaves like
/// [`prepare_reference_repo`] with `branch` as the start point, except that an
/// existing worktree is re-pointed at the fresh `origin/<branch>` tip: the
/// child branch advances between supervision cycles (unlike a ref's base), and
/// re-pointing is safe because prepare always runs before the run starts.
pub async fn prepare_branch_reference(
    root: &Path,
    project_id: i32,
    task_id: i32,
    mount_name: &str,
    repo_url: &str,
    branch: &str,
) -> Result<RefMount, WorkspaceError> {
    let (repo, wt, name) = refs_paths(root, project_id, task_id, mount_name)?;
    ensure_refs_clone(root, &repo, repo_url).await?;
    git(&repo, &["fetch", "origin", branch]).await?;
    let start_point = format!("origin/{branch}");
    if wt.exists() {
        git(&wt, &["checkout", "--detach", &start_point]).await?;
    } else {
        git(
            &repo,
            &["worktree", "add", &wt.to_string_lossy(), "--detach", &start_point],
        )
        .await?;
    }
    Ok(RefMount {
        mount_name: name,
        worktree: wt,
        repo,
    })
}

/// Validated mount name + the refs-clone and per-ticket worktree paths for it.
fn refs_paths(
    root: &Path,
    project_id: i32,
    task_id: i32,
    mount_name: &str,
) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
    // The mount name becomes a path segment — refuse anything that could
    // escape `refs/` (it comes from project settings, but defense in depth).
    let name = mount_name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(WorkspaceError(format!("invalid mount name {mount_name:?}")));
    }
    Ok((
        refs_repo_dir(root, project_id, name),
        refs_worktree_dir(root, project_id, name, task_id),
        name.to_string(),
    ))
}

/// Clone `<repo_url>` into the refs clone `repo` on first use (with the same
/// clone-race tolerance as [`ensure_repo_clone`]: two parallel tickets of one
/// project race this clone; the loser's failure is a win as long as the clone
/// is really there), repoint `origin` on every later use — the ref clone
/// outlives the settings it was created from.
async fn ensure_refs_clone(root: &Path, repo: &Path, repo_url: &str) -> Result<(), WorkspaceError> {
    if !repo.exists() {
        if let Some(parent) = repo.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WorkspaceError(format!("mkdir {parent:?}: {e}")))?;
        }
        if let Err(e) = git(root, &["clone", repo_url, &repo.to_string_lossy()]).await {
            if !repo.exists() {
                return Err(e);
            }
            tracing::info!(repo = ?repo, "clone race lost; using the winner's clone");
        }
    } else {
        git(repo, &["remote", "set-url", "origin", repo_url]).await?;
    }
    Ok(())
}

/// Appends `.refs/` to the worktree's `info/exclude` (idempotent). Resolved
/// via `git rev-parse --git-path` so linked worktrees get the right shared
/// location.
async fn exclude_refs_dir(worktree: &Path) -> Result<(), WorkspaceError> {
    let path = git_output(worktree, &["rev-parse", "--git-path", "info/exclude"]).await?;
    let path = {
        let p = PathBuf::from(&path);
        if p.is_absolute() { p } else { worktree.join(p) }
    };
    let mut content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| WorkspaceError(format!("read {path:?}: {e}")))?;
    if content.lines().any(|l| l.trim() == ".refs/") {
        return Ok(());
    }
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(".refs/\n");
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| WorkspaceError(format!("write {path:?}: {e}")))
}

/// Host mode: expose each mounted ref inside the ticket worktree as a
/// `.refs/<mount_name>` symlink to the ref worktree. Idempotent (an existing
/// symlink is replaced — the ref worktree path never changes for a ticket,
/// but a stale link must not block a re-run). Fail-open per ref, like
/// [`prepare_reference_repos`]: returns `(mount_name, error)` for the caller
/// to note. Container mode does not call this — refs appear via bind mounts.
pub async fn link_refs_into(worktree: &Path, mounted: &[RefMount]) -> Vec<(String, String)> {
    let mut failed = Vec::new();
    if mounted.is_empty() {
        return failed;
    }
    let refs_dir = worktree.join(".refs");
    if let Err(e) = tokio::fs::create_dir_all(&refs_dir).await {
        let e = format!("mkdir {refs_dir:?}: {e}");
        return mounted.iter().map(|m| (m.mount_name.clone(), e.clone())).collect();
    }
    for m in mounted {
        let link = refs_dir.join(&m.mount_name);
        if let Err(e) = symlink_ref(&m.worktree, &link) {
            tracing::warn!(link = ?link, error = %e, "could not symlink reference repo into the worktree");
            failed.push((m.mount_name.clone(), e.to_string()));
        }
    }
    failed
}

#[cfg(unix)]
fn symlink_ref(target: &Path, link: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(md) if md.file_type().is_symlink() => std::fs::remove_file(link)?,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{link:?} exists and is not a symlink"),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::os::unix::fs::symlink(target, link)
}

/// Removes this ticket's reference-repo worktrees (`refs/*/wt-<task_id>`),
/// best-effort per ref — the housekeeping twin of [`cleanup`]'s worktree
/// removal. Enumerates the `refs/` directory rather than the project config:
/// a ref removed from the settings since the run must still be cleaned up.
/// The shared `refs/<mount>/repo` clones stay — they are reused across
/// tickets.
async fn cleanup_reference_worktrees(root: &Path, project_id: i32, task_id: i32) {
    let refs_root = project_dir(root, project_id).join("refs");
    let mounts = match std::fs::read_dir(&refs_root) {
        Ok(mounts) => mounts,
        Err(_) => return, // no refs ever mounted (or the project dir is gone)
    };
    for mount in mounts.flatten() {
        let wt = mount.path().join(format!("wt-{task_id}"));
        if !wt.exists() {
            continue;
        }
        let repo = mount.path().join("repo");
        let removed = repo.exists()
            && git(&repo, &["worktree", "remove", "--force", &wt.to_string_lossy()])
                .await
                .is_ok();
        if !removed {
            // Same stale-leftover rule as the main cleanup: a dir that
            // outlived its worktree registration is deleted directly.
            if repo.exists()
                && let Err(e) = git(&repo, &["worktree", "prune"]).await
            {
                tracing::debug!(error = %e, "ref worktree prune failed");
            }
            if wt.exists()
                && let Err(e) = tokio::fs::remove_dir_all(&wt).await
            {
                tracing::warn!(dir = ?wt, error = %e, "ref worktree removal failed; will retry on the next poll");
            }
        }
    }
}

/// Removes the ticket worktree and its branch (after the human accepts the
/// work). The branch name comes from the run row (recorded at finish time), not
/// recomputed from the title — a ticket renamed mid-flight would
/// otherwise leak its branch. Missing pieces are ignored — cleanup must be
/// idempotent.
///
/// Ordering matters: the branch is deleted only after the worktree is actually
/// gone. Housekeeping retries cleanup on every poll while `wt-<task_id>`
/// exists, and a still-present worktree whose branch was already deleted used
/// to WARN "branch not found" on every poll. A failed worktree remove (busy
/// dir — an open terminal/editor, or devenv processes still running) is a WARN
/// and skips the branch delete, leaving both pieces for the next poll's retry.
///
/// That retry-on-failure applies only to a *registered* worktree. A dir can
/// outlive its registration: an earlier cleanup removed the worktree, then a
/// still-running devenv/MinIO process recreated `.devenv/state/...` under it.
/// `git worktree remove` on such a leftover fails permanently ("not a working
/// tree") — treating that as "busy, retry later" retries forever. A present
/// but unregistered `wt-<task_id>` is a stale leftover in the daemon-owned
/// workspace and is deleted directly.
///
/// `keep_branch` retains the local branch: a supervised child's commits live
/// only there until the supervision loop absorbs them into the parent's
/// branch, so accepting the child (review → completed) must not destroy
/// unmerged work while the parent is not `completed` (review #106). The
/// worktree is still removed.
pub async fn cleanup(
    root: &Path,
    project_id: i32,
    task_id: i32,
    branch: &str,
    keep_branch: bool,
) -> Result<(), WorkspaceError> {
    // The ticket's reference-repo worktrees go with it (same housekeeping
    // rules); the shared refs clones stay for reuse (#169).
    cleanup_reference_worktrees(root, project_id, task_id).await;
    let repo = repo_dir(root, project_id);
    let wt = worktree_dir(root, project_id, task_id);
    if repo.exists() {
        if wt.exists() {
            let registered = match is_registered_worktree(&repo, &wt).await {
                Ok(registered) => registered,
                Err(e) => {
                    // Conservative fallback: without the worktree list we can't
                    // tell a stale leftover from a real checkout, so keep the
                    // old path and let `git worktree remove` decide.
                    tracing::warn!(
                        dir = ?wt,
                        error = %e,
                        "worktree list failed; falling back to git worktree remove"
                    );
                    true
                }
            };
            if registered {
                // `--force`: the agent's uncommitted leftovers must not block cleanup.
                if let Err(e) = git(&repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]).await {
                    tracing::warn!(
                        dir = ?wt,
                        error = %e,
                        "worktree remove failed — a terminal/editor open in it, or devenv processes \
                         still running? skipping the branch delete; will retry on the next poll"
                    );
                    return Ok(());
                }
            } else {
                tracing::info!(dir = ?wt, "leftover dir is not a registered worktree; removing it directly");
                // Hygiene, best-effort: clear any stale worktree metadata
                // before the branch delete below.
                if let Err(e) = git(&repo, &["worktree", "prune"]).await {
                    tracing::debug!(error = %e, "worktree prune failed");
                }
            }
        } else if let Err(e) = git(&repo, &["worktree", "prune"]).await {
            // The dir was deleted out from under git: prune clears the stale
            // worktree metadata so the branch delete below isn't blocked by a
            // phantom checkout. Best-effort — a failure is invisible noise.
            tracing::debug!(error = %e, "worktree prune failed");
        }
        let branch_ref = format!("refs/heads/{branch}");
        if keep_branch {
            tracing::info!(
                branch,
                "keeping the branch — supervised child accepted before its commits were absorbed"
            );
        } else if git_output(&repo, &["rev-parse", "--verify", &branch_ref]).await.is_ok() {
            if let Err(e) = git(&repo, &["branch", "-D", branch]).await {
                tracing::warn!(branch, error = %e, "branch cleanup failed");
            }
        } else {
            tracing::debug!(branch, "branch already gone — cleanup idempotent");
        }
    }
    if wt.exists() {
        tokio::fs::remove_dir_all(&wt)
            .await
            .map_err(|e| WorkspaceError(format!("remove {wt:?}: {e}")))?;
    }
    Ok(())
}

/// How `push` delivered the branch to origin (spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The initial `git push` fast-forwarded without any recovery.
    Plain,
    /// The remote moved ahead; the daemon fetched, rebased, and pushed.
    RebasedAndPushed,
    /// The remote held the agent's own rewritten history; the daemon used
    /// `git push --force-with-lease` safely (no remote-only commits lost).
    ForcePushed,
}

/// `git push origin <branch>` from the ticket worktree (spec §4.4) — the
/// successful implement run's branch leaves the daemon host here.
///
/// A non-fast-forward rejection gets one recovery attempt. First a provenance
/// fast path: if the remote tip is exactly what a push from this clone last
/// set it to (`remote_tip_is_own_push`), the divergence can only be the agent
/// rewriting its own pushed history, so a safe `--force-with-lease` is used
/// directly. This catches the common rebase-onto-master case even when
/// conflict resolution changed the rewritten commits' patch-ids — the
/// patch-equivalence test below cannot see that equivalence and would
/// misroute the old commits as somebody else's into a rebase that conflicts
/// with itself.
///
/// Otherwise: fetch, then decide whether the remote's extra commits are
/// already present locally by patch-equivalence (`git cherry`). If every
/// remote-unique commit has a local patch-equivalent, the agent rewrote its
/// own history, so a safe `--force-with-lease` is used. If any remote-unique
/// commit lacks a local equivalent, a human or another agent pushed to the
/// branch; rebasing integrates those commits. A conflicting rebase is aborted
/// and the original push error returned, so the run still succeeds with the
/// failure riding its row as `pr_error` (§4.4).
pub async fn push(dir: &Path, branch: &str) -> Result<PushOutcome, WorkspaceError> {
    match git(dir, &["push", "origin", branch]).await {
        Ok(()) => Ok(PushOutcome::Plain),
        Err(e) if e.0.contains("non-fast-forward") || e.0.contains("fetch first") => {
            if remote_tip_is_own_push(dir, branch).await {
                tracing::info!(
                    branch,
                    "remote tip is this daemon's own last push; the agent rewrote its own history — \
                     force-pushing with lease"
                );
                git(dir, &["push", "--force-with-lease", "origin", branch]).await?;
                return Ok(PushOutcome::ForcePushed);
            }
            tracing::info!(
                branch,
                "push rejected (non-fast-forward); checking whether remote commits are already present locally"
            );
            if let Err(pe) = git(dir, &["fetch", "origin", branch]).await {
                return Err(WorkspaceError(format!("{e}; fetch origin {branch} failed: {pe}")));
            }
            let cherry = match git_output(dir, &["cherry", "HEAD", &format!("origin/{branch}")]).await {
                Ok(out) => out,
                Err(pe) => {
                    return Err(WorkspaceError(format!(
                        "{e}; git cherry HEAD origin/{branch} failed: {pe}"
                    )));
                }
            };
            let has_remote_only = cherry.lines().any(|l| l.starts_with("+ "));
            if has_remote_only {
                tracing::info!(
                    branch,
                    "remote has commits without local patch-equivalent; rebasing onto origin"
                );
                if let Err(pe) = git(dir, &["rebase", &format!("origin/{branch}")]).await {
                    // Never leave an in-progress rebase behind — it would poison
                    // the next run's worktree.
                    let _ = git(dir, &["rebase", "--abort"]).await;
                    return Err(WorkspaceError(format!(
                        "{e}; recovery rebase onto origin/{branch} failed: {pe}"
                    )));
                }
                git(dir, &["push", "origin", branch]).await?;
                Ok(PushOutcome::RebasedAndPushed)
            } else {
                tracing::info!(
                    branch,
                    "all remote-unique commits have local patch-equivalents; force-pushing with lease"
                );
                git(dir, &["push", "--force-with-lease", "origin", branch]).await?;
                Ok(PushOutcome::ForcePushed)
            }
        }
        Err(e) => Err(e),
    }
}

/// `true` when the remote branch tip is exactly what a push from this clone
/// last set it to: the remote-tracking ref's newest reflog entry is
/// `update by push` (any fetch that observed someone else's push since would
/// have overwritten that entry) and `git ls-remote` confirms the remote has
/// not moved. Any divergence from local HEAD is then the agent rewriting its
/// own pushed history, so a force-with-lease loses nothing — regardless of
/// whether the rewrite preserved patch-ids. Any doubt (no tracking ref, no
/// reflog, a moved remote) yields `false` and the caller falls back to the
/// patch-equivalence recovery.
async fn remote_tip_is_own_push(dir: &Path, branch: &str) -> bool {
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let Ok(tracking) = git_output(dir, &["rev-parse", "--verify", &remote_ref]).await else {
        return false;
    };
    let Ok(subject) = git_output(dir, &["reflog", "show", "-1", "--format=%gs", &remote_ref]).await else {
        return false;
    };
    if subject != "update by push" {
        return false;
    }
    match git_output(dir, &["ls-remote", "origin", &format!("refs/heads/{branch}")]).await {
        Ok(out) => out.split_whitespace().next() == Some(tracking.as_str()),
        Err(_) => false,
    }
}

/// `origin/HEAD`'s branch name (`origin/main` → `main`) — the PR target when
/// the project has no explicit base branch (spec §4.4).
pub async fn remote_default_branch(dir: &Path) -> Result<String, WorkspaceError> {
    let out = git_output(dir, &["rev-parse", "--abbrev-ref", "origin/HEAD"]).await?;
    Ok(out.strip_prefix("origin/").unwrap_or(&out).to_string())
}

/// `git rev-list --count <base>..HEAD` — how many commits the worktree branch
/// adds on top of the base ref (`origin/<base_branch>` or `origin/HEAD`). The
/// implement-success guard (spec §5.4 step 5) uses it to reject runs the agent
/// finished without committing.
pub async fn commits_ahead(dir: &Path, base: &str) -> Result<u32, WorkspaceError> {
    let range = format!("{base}..HEAD");
    let out = git_output(dir, &["rev-list", "--count", &range]).await?;
    out.parse::<u32>()
        .map_err(|e| WorkspaceError(format!("git rev-list --count {range}: not a number: {e}")))
}

/// `git rev-list --count <range>` for an arbitrary range — the supervise run
/// counts a child branch's commits ahead of the parent's branch.
pub async fn commit_count(dir: &Path, range: &str) -> Result<u32, WorkspaceError> {
    let out = git_output(dir, &["rev-list", "--count", range]).await?;
    out.parse::<u32>()
        .map_err(|e| WorkspaceError(format!("git rev-list --count {range}: not a number: {e}")))
}

/// `git diff <range>` output, uncapped — the caller bounds it for the prompt
/// (the supervise run embeds the child-branch diff, spec §5.4).
pub async fn diff(dir: &Path, range: &str) -> Result<String, WorkspaceError> {
    git_output(dir, &["diff", range]).await
}

/// `git status --porcelain` output, empty when the worktree is clean. Tracked
/// modifications and untracked files are reported; ignored files (build
/// artifacts, `.devenv/` state) are omitted by `porcelain` and do not count.
/// The dirty-worktree guard uses this to send the agent back to finish its
/// turn (spec §5.4 step 5).
pub async fn dirty_status(dir: &Path) -> Result<String, WorkspaceError> {
    git_output(dir, &["status", "--porcelain"]).await
}

/// Outcome of absorbing a child branch into the parent's branch (the supervise
/// run's accept path, spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merge committed cleanly (or the branch was already merged — a
    /// re-triggered supervise run must be idempotent).
    Clean,
    /// The merge stopped on conflicts: the worktree carries unmerged entries
    /// and conflict markers for the agent to resolve.
    Conflicted,
}

/// `git merge --no-edit --no-ff <branch>` in `dir` (the parent's worktree).
/// `--no-ff` keeps a traceable "Merge child #N" commit even when the child is
/// a fast-forward ahead. `Conflicted` is only returned when git actually left
/// unmerged entries — any other failure (missing branch, dirty worktree) is an
/// `Err` for the caller to report.
pub async fn merge(dir: &Path, branch: &str, message: &str) -> Result<MergeOutcome, WorkspaceError> {
    // The merge commit is created by the daemon (not the agent), so it needs
    // an explicit identity — prepared worktrees don't configure one.
    let out = git_allow_fail(
        dir,
        &[
            "-c",
            "user.email=remoter-agent@localhost",
            "-c",
            "user.name=remoter-agent",
            "merge",
            "--no-edit",
            "--no-ff",
            "-m",
            message,
            branch,
        ],
    )
    .await?;
    if out.status.success() {
        return Ok(MergeOutcome::Clean);
    }
    if !unmerged_paths(dir).await?.is_empty() {
        return Ok(MergeOutcome::Conflicted);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (true, false) => stderr.to_string(),
        (false, true) => stdout.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    };
    Err(WorkspaceError(format!("git merge {branch} failed: {detail}")))
}

/// Paths with unmerged (conflicted) entries, one per line of
/// `git diff --name-only --diff-filter=U` — empty when no merge conflict is
/// in progress.
pub async fn unmerged_paths(dir: &Path) -> Result<Vec<String>, WorkspaceError> {
    let out = git_output(dir, &["diff", "--name-only", "--diff-filter=U"]).await?;
    Ok(out.lines().map(str::to_string).collect())
}

/// `true` when `ancestor` is an ancestor of HEAD — i.e. the merge of the
/// child's branch actually landed (the supervise run's post-merge check).
pub async fn contains_commit(dir: &Path, ancestor: &str) -> Result<bool, WorkspaceError> {
    let out = git_allow_fail(dir, &["merge-base", "--is-ancestor", ancestor, "HEAD"]).await?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(WorkspaceError(format!(
            "git merge-base --is-ancestor {ancestor} HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))),
    }
}

/// `git rev-parse <ref>` — the tip commit of a branch (the supervise run pins
/// the child tip before conflict nudges to verify the merge landed).
pub async fn git_rev_parse(dir: &Path, r#ref: &str) -> Result<String, WorkspaceError> {
    git_output(dir, &["rev-parse", r#ref]).await
}

/// `git merge --abort` — best-effort rollback of a half-open merge (the
/// supervise run uses it when a conflict-resolution nudge is cancelled).
pub async fn merge_abort(dir: &Path) -> Result<(), WorkspaceError> {
    git(dir, &["merge", "--abort"]).await
}

/// devenv auto-detection (spec §5.8): the worktree carries `devenv.nix` or
/// `devenv.yaml`.
pub fn uses_devenv(dir: &Path) -> bool {
    dir.join("devenv.nix").exists() || dir.join("devenv.yaml").exists()
}

// ── devenv command wrapping (spec §5.8) ──────────────────────────────────────

/// Where a driver-spawned command executes (docs/specs/remoter-agent-containers.md).
#[derive(Debug, Clone)]
pub enum ExecEnv {
    /// On the daemon host — directly, or wrapped in `devenv shell --` when the
    /// worktree uses devenv (spec §5.8).
    Host {
        /// The worktree carries `devenv.nix` / `devenv.yaml`.
        devenv: bool,
    },
    /// Inside the run container via `docker exec` (container mode).
    Container(ContainerExec),
}

/// Container-mode execution details threaded from the daemon to the driver.
#[derive(Debug, Clone)]
pub struct ContainerExec {
    /// The docker CLI binary (config `execution.docker_binary`).
    pub docker: String,
    /// The run container's name (`docker exec` target).
    pub container: String,
    /// The worktree mount point inside the container (`/work`).
    pub work_dir: PathBuf,
    /// The worktree uses devenv — wrap as `devenv shell -- …` *inside* the
    /// container (the image has the shell warmed, see `image.rs`).
    pub devenv: bool,
    /// Host-side kimi sessions directory (`<agent_home>/.kimi-code/sessions`)
    /// — where the wire files appear, since the container mounts the agent
    /// home at `/root`. Used by the token-usage fallback.
    pub sessions_dir: PathBuf,
    /// The backend API URL rewritten for in-container reachability
    /// (loopback → `host.docker.internal`).
    pub api_url: String,
}

impl ExecEnv {
    pub fn host(devenv: bool) -> Self {
        ExecEnv::Host { devenv }
    }

    /// Whether commands are wrapped in `devenv shell --` (either side).
    pub fn devenv(&self) -> bool {
        match self {
            ExecEnv::Host { devenv } => *devenv,
            ExecEnv::Container(c) => c.devenv,
        }
    }
}

/// Builds the command a driver spawns. Host mode: with devenv, the invocation
/// is wrapped as `devenv shell --no-tui --no-eval-cache -- <program> <args>`
/// (with `CI=1` / `DEVENV_NO_AI_AGENT=1`); without, it runs directly.
/// Container mode: `docker exec -i -w /work [-e K=V]… <run> [devenv shell …
/// --] <program> <args>` — the devenv shell is entered *inside* the container,
/// where the image has it warmed.
pub fn wrap_command(
    dir: &Path,
    exec: &ExecEnv,
    program: &str,
    args: &[&str],
    env: &[(String, String)],
) -> std::process::Command {
    match exec {
        ExecEnv::Container(c) => {
            let mut cmd = std::process::Command::new(&c.docker);
            cmd.arg("exec").arg("-i").arg("-w").arg(&c.work_dir);
            cmd.arg("-e").arg("CI=1");
            if c.devenv {
                cmd.arg("-e").arg("DEVENV_NO_AI_AGENT=1");
            }
            for (k, v) in env {
                cmd.arg("-e").arg(format!("{k}={v}"));
            }
            cmd.arg(&c.container);
            // Keep the legacy human token out of the agent process even when
            // it was baked into or inherited by the run container.
            cmd.arg("env").arg("-u").arg("REMOTER_TOKEN");
            if c.devenv {
                cmd.arg("devenv")
                    .arg("shell")
                    .arg("--no-tui")
                    .arg("--no-eval-cache")
                    .arg("--");
            }
            cmd.arg(program).args(args);
            // `docker exec` runs on the host; `current_dir` is meaningless
            // for the in-container process but harmless for the CLI itself.
            cmd
        }
        ExecEnv::Host { devenv } => {
            let mut cmd = if *devenv {
                let mut c = std::process::Command::new("devenv");
                c.arg("shell")
                    .arg("--no-tui")
                    .arg("--no-eval-cache")
                    .arg("--")
                    .arg(program)
                    .args(args);
                c.env("DEVENV_NO_AI_AGENT", "1");
                c
            } else {
                let mut c = std::process::Command::new(program);
                c.args(args);
                c
            };
            cmd.current_dir(dir);
            // Do not leak the pre-agent legacy human token into the child
            // process. The injected remoter MCP server receives the daemon's
            // agent token explicitly below.
            cmd.env_remove("REMOTER_TOKEN");
            cmd.env("CI", "1");
            cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            cmd
        }
    }
}

/// `devenv --no-eval-cache up -d` + `devenv --no-eval-cache processes wait
/// --timeout 120` before a run (R§5). Self-healing: a best-effort
/// `processes down` runs first (a stale supervisor from a crashed previous
/// attempt would otherwise fail `up -d` with "Processes already running"),
/// and once more when `up -d`/`wait` fails, so a partially started supervisor
/// is never leaked into the caller's retry loop.
pub async fn services_up(dir: &Path, env: &[(String, String)]) -> Result<(), WorkspaceError> {
    services_down(dir, env).await;
    let result = async {
        devenv(dir, env, &["up", "-d"]).await?;
        devenv(dir, env, &["processes", "wait", "--timeout", "120"]).await
    }
    .await;
    if result.is_err() {
        services_down(dir, env).await;
    }
    result
}

/// `devenv --no-eval-cache processes down` — fully best-effort: a non-zero
/// exit (commonly "nothing running") or a spawn failure is logged at debug
/// level and never returned, so a down can never mask the run's real outcome
/// or block a subsequent up.
pub async fn services_down(dir: &Path, env: &[(String, String)]) {
    if let Err(e) = devenv(dir, env, &["processes", "down"]).await {
        tracing::debug!(dir = %dir.display(), error = %e, "devenv processes down failed (ignored)");
    }
}

/// Startup sweep (spec §5.8): best-effort `services_down` in every existing
/// devenv worktree under `root` (`p*/wt-*`), cleaning up processes leaked by
/// a daemon crash or an older daemon version. Never fails the caller.
pub async fn down_all_worktrees(root: &Path) {
    let projects = match std::fs::read_dir(root) {
        Ok(projects) => projects,
        Err(e) => {
            tracing::debug!(root = %root.display(), error = %e, "startup sweep: cannot list workspace root");
            return;
        }
    };
    for project in projects.flatten() {
        let name = project.file_name();
        if !name.to_string_lossy().starts_with('p') {
            continue;
        }
        let worktrees = match std::fs::read_dir(project.path()) {
            Ok(worktrees) => worktrees,
            Err(e) => {
                tracing::debug!(dir = %project.path().display(), error = %e, "startup sweep: cannot list project dir");
                continue;
            }
        };
        for wt in worktrees.flatten() {
            let wt_name = wt.file_name();
            if !wt_name.to_string_lossy().starts_with("wt-") {
                continue;
            }
            let dir = wt.path();
            if !dir.is_dir() || !uses_devenv(&dir) {
                tracing::trace!(dir = %dir.display(), "startup sweep: skipping non-devenv worktree");
                continue;
            }
            tracing::info!(dir = %dir.display(), "startup sweep: stopping devenv processes");
            services_down(&dir, &[]).await;
        }
    }
}

async fn devenv(dir: &Path, env: &[(String, String)], args: &[&str]) -> Result<(), WorkspaceError> {
    let mut cmd = std::process::Command::new("devenv");
    cmd.arg("--no-eval-cache")
        .args(args)
        .current_dir(dir)
        .env("CI", "1")
        .env("DEVENV_NO_AI_AGENT", "1")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    run(cmd, &format!("devenv --no-eval-cache {}", args.join(" "))).await
}

// ── git helpers ───────────────────────────────────────────────────────────────

/// Whether `wt` is still a registered worktree of `repo`, per
/// `git worktree list --porcelain`. Both paths are canonicalized before
/// comparing — macOS resolves `/var` to `/private/var`, so a textual compare
/// would miss a real registration. Never classify by the text of a git error
/// instead: that output is localized.
async fn is_registered_worktree(repo: &Path, wt: &Path) -> Result<bool, WorkspaceError> {
    let out = git_output(repo, &["worktree", "list", "--porcelain"]).await?;
    let wt = std::fs::canonicalize(wt).unwrap_or_else(|_| wt.to_path_buf());
    Ok(out.lines().filter_map(|line| line.strip_prefix("worktree ")).any(|p| {
        let p = Path::new(p);
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()) == wt
    }))
}

/// `true` when `refs/heads/<branch>` exists in the clone at `repo` — the gate
/// for stacking a supervised child on its parent's branch: the tip to branch
/// off must be present locally.
pub async fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    git_output(repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .await
        .is_ok()
}

/// `true` when `refs/remotes/origin/<branch>` exists in the clone at `repo`
/// — a remote-tracking copy the supervise run can fall back to as its diff
/// base when the parent's local branch is gone.
pub async fn remote_tracking_branch_exists(repo: &Path, branch: &str) -> bool {
    git_output(
        repo,
        &["rev-parse", "--verify", &format!("refs/remotes/origin/{branch}")],
    )
    .await
    .is_ok()
}

/// Best-effort `git fetch origin <branch>` — refreshes the remote-tracking
/// ref so a missing local branch can still be diffed against `origin/<branch>`.
pub async fn fetch_branch(repo: &Path, branch: &str) -> Result<(), WorkspaceError> {
    git(repo, &["fetch", "origin", branch]).await
}

/// Sync the central clone's checkout to the fetched base branch tip
/// (`origin/<base>`, or `origin/HEAD` when no base branch is configured).
/// `prepare` only ever fetches — it never touches the clone's working tree,
/// so without this the checkout stays at the clone-time revision forever and
/// anything read from it (the agent image's `.remoter/agent.Dockerfile`, the
/// devenv warm at image build) silently goes stale. The clone is daemon-owned
/// and never committed to, so a fast-forward always applies.
pub async fn sync_clone_checkout(repo: &Path, base_branch: Option<&str>) -> Result<(), WorkspaceError> {
    let base = match base_branch {
        Some(b) => b.to_string(),
        None => remote_default_branch(repo).await?,
    };
    git(repo, &["fetch", "origin", &base]).await?;
    git(repo, &["checkout", &base]).await?;
    git(repo, &["merge", "--ff-only", &format!("origin/{base}")]).await
}

/// Whether `ensure_local_branch` found the branch locally, recreated it from
/// `origin/<branch>`, or confirmed it missing everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalBranchState {
    Present,
    Reattached,
    Missing,
}

/// Make `branch` available as a local ref: already present → `Present` (no
/// fetch at all); otherwise a best-effort fetch, then — when
/// `refs/remotes/origin/<branch>` exists (just fetched, or left over from an
/// earlier fetch when the network is down) — recreate the local branch from
/// it without a checkout (`Reattached`). `Missing` only when the ref is
/// confirmed absent both locally and on origin. `Err` only when the recreate
/// itself failed — the branch exists on origin but could not be reattached.
pub async fn ensure_local_branch(repo: &Path, branch: &str) -> Result<LocalBranchState, WorkspaceError> {
    if local_branch_exists(repo, branch).await {
        return Ok(LocalBranchState::Present);
    }
    // Best-effort: a failed fetch (network down, unknown ref) must not mask a
    // remote-tracking ref an earlier fetch already left behind.
    let _ = fetch_branch(repo, branch).await;
    if !remote_tracking_branch_exists(repo, branch).await {
        return Ok(LocalBranchState::Missing);
    }
    git(repo, &["branch", branch, &format!("origin/{branch}")]).await?;
    Ok(LocalBranchState::Reattached)
}

/// `true` when `origin` already has `refs/heads/<branch>` (a copy pushed by an
/// earlier run). A supervised child skips push/PR only while no remote copy
/// exists — an existing one must keep being updated, or its open PR would
/// silently go stale (review #106). Any `ls-remote` failure (no match, no
/// network) yields `false`.
pub async fn remote_branch_exists(dir: &Path, branch: &str) -> bool {
    git_output(
        dir,
        &["ls-remote", "--exit-code", "origin", &format!("refs/heads/{branch}")],
    )
    .await
    .is_ok()
}

/// `agent/feature-<parentId>-<slug>` — the integration branch that collects a
/// cross-project feature's child work inside the *child* project's repo
/// (docs/specs/cross-repo-projects.md): the child's work never reaches that
/// repo's base branch before a human merges the integration PR. The branch
/// lives until the parent ticket completes.
pub fn integration_branch_name(parent_id: i32, parent_title: &str) -> String {
    format!("agent/feature-{parent_id}-{}", slugify(parent_title))
}

/// Ensure the parent ticket's integration branch exists in `repo` (the child
/// project's clone) and on its origin. Idempotent: a present local branch is
/// returned as-is, a branch that exists only on origin is reattached locally;
/// otherwise it is branched off `origin/<base_branch>` (or `origin/HEAD` when
/// the child project has no base branch configured) and pushed, so the
/// isolation invariant holds even if the daemon host dies right after.
pub async fn ensure_integration_branch(
    repo: &Path,
    parent_id: i32,
    parent_title: &str,
    base_branch: Option<&str>,
) -> Result<String, WorkspaceError> {
    let branch = integration_branch_name(parent_id, parent_title);
    match ensure_local_branch(repo, &branch).await? {
        LocalBranchState::Present | LocalBranchState::Reattached => return Ok(branch),
        LocalBranchState::Missing => {}
    }
    let start_point = match base_branch {
        Some(b) => {
            git(repo, &["fetch", "origin", b]).await?;
            format!("origin/{b}")
        }
        None => {
            git(repo, &["fetch", "origin"]).await?;
            "origin/HEAD".to_string()
        }
    };
    git(repo, &["branch", &branch, &start_point]).await?;
    git(repo, &["push", "origin", &branch]).await?;
    Ok(branch)
}

/// Delete the parent ticket's integration branch locally and on origin — the
/// housekeeping twin of [`ensure_integration_branch`], run when the parent
/// completes. Best-effort and idempotent: already-gone pieces are skipped, a
/// failed delete is a warning for the next poll's retry, never an error.
pub async fn delete_integration_branch(repo: &Path, branch: &str) {
    if local_branch_exists(repo, branch).await
        && let Err(e) = git(repo, &["branch", "-D", branch]).await
    {
        tracing::warn!(branch, error = %e, "integration branch cleanup failed");
    }
    if remote_branch_exists(repo, branch).await
        && let Err(e) = git(repo, &["push", "origin", "--delete", branch]).await
    {
        tracing::warn!(branch, error = %e, "integration branch remote cleanup failed");
    }
}

/// Housekeeping sweep for a completed parent ticket: delete its integration
/// branches (`agent/feature-<parentId>-*`) in every project clone under
/// `root`. Enumerates the local clones instead of asking the API which
/// projects had cross-project children — cheap, and covers projects whose
/// config changed since the branches were created.
pub async fn cleanup_integration_branches(root: &Path, parent_id: i32) {
    let projects = match std::fs::read_dir(root) {
        Ok(projects) => projects,
        Err(_) => return,
    };
    let pattern = format!("agent/feature-{parent_id}-*");
    for project in projects.flatten() {
        let repo = project.path().join("repo");
        if !repo.exists() {
            continue;
        }
        let Ok(branches) = git_output(&repo, &["branch", "--list", &pattern, "--format=%(refname:short)"]).await else {
            continue;
        };
        for branch in branches.lines().filter(|l| !l.is_empty()) {
            delete_integration_branch(&repo, branch).await;
        }
    }
}

/// `git ls-remote <repo_url> refs/heads/<branch>` tip commit — `None` when the
/// branch is absent or the remote is unreachable. Prompt-context use: never
/// fatal. `dir` is any existing directory (ls-remote needs no repository).
pub async fn ls_remote_tip(dir: &Path, repo_url: &str, branch: &str) -> Option<String> {
    git_output(dir, &["ls-remote", repo_url, &format!("refs/heads/{branch}")])
        .await
        .ok()
        .and_then(|out| out.split_whitespace().next().map(str::to_string))
        .filter(|s| !s.is_empty())
}

/// `git symbolic-ref --short HEAD` — the branch a worktree is on (`Err` when
/// detached).
async fn worktree_branch(dir: &Path) -> Result<String, WorkspaceError> {
    git_output(dir, &["symbolic-ref", "--short", "HEAD"]).await
}

async fn git(dir: &Path, args: &[&str]) -> Result<(), WorkspaceError> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir).args(args);
    run(cmd, &format!("git {}", args.join(" "))).await
}

async fn git_output(dir: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir).args(args);
    let out = tokio::process::Command::from(cmd)
        .output()
        .await
        .map_err(|e| WorkspaceError(format!("git {}: {e}", args.join(" "))))?;
    if !out.status.success() {
        return Err(WorkspaceError(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like `git_output` but returns the raw process result — for git commands
/// whose non-zero exit is meaningful (merge conflicts, `merge-base
/// --is-ancestor` returning 1).
async fn git_allow_fail(dir: &Path, args: &[&str]) -> Result<std::process::Output, WorkspaceError> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir).args(args);
    tokio::process::Command::from(cmd)
        .output()
        .await
        .map_err(|e| WorkspaceError(format!("git {}: {e}", args.join(" "))))
}

async fn run(cmd: std::process::Command, what: &str) -> Result<(), WorkspaceError> {
    let out = tokio::process::Command::from(cmd)
        .output()
        .await
        .map_err(|e| WorkspaceError(format!("{what}: {e}")))?;
    if !out.status.success() {
        return Err(WorkspaceError(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Lowercase alphanumeric + dashes, ≤ 40 chars, no trailing dash.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true; // avoids a leading dash
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "work".to_string()
    } else {
        slug.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_layout() {
        let root = Path::new("/ws");
        assert_eq!(project_dir(root, 7), PathBuf::from("/ws/p7"));
        assert_eq!(repo_dir(root, 7), PathBuf::from("/ws/p7/repo"));
        assert_eq!(worktree_dir(root, 7, 42), PathBuf::from("/ws/p7/wt-42"));
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Add login page!"), "add-login-page");
        assert_eq!(slugify("  --weird   spacing-- "), "weird-spacing");
        assert_eq!(slugify("!!!"), "work");
        assert_eq!(slugify(""), "work");
    }

    #[test]
    fn slugify_truncates_without_trailing_dash() {
        let long = "a very long task description that goes on and on and on";
        let slug = slugify(long);
        assert!(slug.len() <= 40);
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn branch_name_format() {
        assert_eq!(branch_name(42, "Add login page"), "agent/task-42-add-login-page");
    }

    /// A scratch dir under the OS temp dir, unique per test name + process.
    fn scratch(tag: &str) -> PathBuf {
        // Canonicalize: on macOS temp_dir() is /var/folders/... which is a
        // symlink to /private/var/folders/..., while commands that resolve
        // paths report the canonical form — comparisons would mismatch.
        let base = std::env::temp_dir().canonicalize().unwrap();
        let dir = base.join(format!("remoter-ws-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sh(dir: &Path, program: &str, args: &[&str]) {
        let out = std::process::Command::new(program)
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A bare repo with one commit — prepare's `fetch`/`origin/HEAD` need a
    /// real upstream state to branch off.
    fn make_bare_repo(parent: &Path, name: &str) -> PathBuf {
        let src = parent.join(format!("{name}-src"));
        std::fs::create_dir_all(&src).unwrap();
        sh(&src, "git", &["init"]);
        std::fs::write(src.join("README.md"), name).unwrap();
        sh(&src, "git", &["add", "."]);
        sh(
            &src,
            "git",
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "init"],
        );
        let bare = parent.join(format!("{name}.git"));
        sh(
            parent,
            "git",
            &["clone", "--bare", &src.to_string_lossy(), &bare.to_string_lossy()],
        );
        bare
    }

    /// §5.4: the clone outlives the project's Repository URL setting — a
    /// re-prepare with an edited URL must repoint `origin` before fetching.
    #[tokio::test]
    async fn merge_failure_without_conflicts_reports_git_output() {
        let base = scratch("merge-fail");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");
        prepare(&ws, 7, 42, "parent work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let wt = worktree_dir(&ws, 7, 42);

        // A branch that does not exist: git fails without leaving unmerged
        // entries — the error (not `Conflicted`) must carry git's message.
        let err = merge(&wt, "agent/task-99-gone", "merge missing child")
            .await
            .unwrap_err();
        let msg = err.0;
        assert!(msg.contains("git merge agent/task-99-gone failed"), "{msg}");
        assert!(msg.contains("agent/task-99-gone"), "{msg}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// #138 (a): the child's branch exists only on origin (standalone run,
    /// wiped workspace) — `ensure_local_branch` fetches and recreates the
    /// local branch from `origin/<branch>` at the same tip.
    #[tokio::test]
    async fn ensure_local_branch_reattaches_from_origin() {
        let base = scratch("reattach");
        let origin = make_bare_repo(&base, "origin");
        let src = base.join("origin-src");
        sh(&src, "git", &["checkout", "-b", "agent/task-72-child"]);
        std::fs::write(src.join("child.txt"), "work").unwrap();
        sh(&src, "git", &["add", "."]);
        sh(
            &src,
            "git",
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "child work",
            ],
        );
        sh(&src, "git", &["push", &origin.to_string_lossy(), "agent/task-72-child"]);

        let ws = base.join("ws");
        prepare(&ws, 7, 71, "parent work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);
        // Simulate a daemon that has never fetched the child's branch.
        sh(
            &repo,
            "git",
            &["update-ref", "-d", "refs/remotes/origin/agent/task-72-child"],
        );
        assert!(!local_branch_exists(&repo, "agent/task-72-child").await);

        let state = ensure_local_branch(&repo, "agent/task-72-child").await.unwrap();
        assert_eq!(state, LocalBranchState::Reattached);
        let tip = git_output(&repo, &["rev-parse", "agent/task-72-child"]).await.unwrap();
        let origin_tip = git_output(&repo, &["rev-parse", "refs/remotes/origin/agent/task-72-child"])
            .await
            .unwrap();
        assert_eq!(tip, origin_tip);
        // No checkout: HEAD of the clone did not move to the child's branch.
        assert_ne!(
            git_output(&repo, &["symbolic-ref", "--short", "HEAD"]).await.unwrap(),
            "agent/task-72-child"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// #138 (b): the branch is on neither the clone nor origin — confirmed
    /// `Missing` (the failed fetch must not be surfaced as an error).
    #[tokio::test]
    async fn ensure_local_branch_missing_everywhere() {
        let base = scratch("missing");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");
        prepare(&ws, 7, 71, "parent work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);

        let state = ensure_local_branch(&repo, "agent/task-99-gone").await.unwrap();
        assert_eq!(state, LocalBranchState::Missing);
        assert!(!local_branch_exists(&repo, "agent/task-99-gone").await);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// #138 (c): happy path — the branch exists locally, so no fetch is even
    /// attempted (origin is removed entirely; any fetch would fail).
    #[tokio::test]
    async fn ensure_local_branch_present_skips_fetch() {
        let base = scratch("present");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");
        prepare(&ws, 7, 71, "parent work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);
        sh(&repo, "git", &["remote", "remove", "origin"]);

        let state = ensure_local_branch(&repo, "agent/task-71-parent-work").await.unwrap();
        assert_eq!(state, LocalBranchState::Present);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Run #941: `prepare` only fetches — the clone's checkout stays at the
    /// clone-time revision, so files added upstream later (like
    /// `.remoter/agent.Dockerfile`) look absent to the image build.
    /// `sync_clone_checkout` must fast-forward the checkout to the base tip.
    #[tokio::test]
    async fn sync_clone_checkout_fast_forwards_to_base_tip() {
        let base = scratch("sync-checkout");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");
        prepare(&ws, 7, 42, "parent work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);
        assert!(!repo.join(".remoter/agent.Dockerfile").exists());

        // Upstream advances after the daemon's clone was made.
        let src = base.join("origin-src");
        std::fs::create_dir_all(src.join(".remoter")).unwrap();
        std::fs::write(src.join(".remoter/agent.Dockerfile"), "FROM scratch\n").unwrap();
        sh(&src, "git", &["add", "."]);
        sh(
            &src,
            "git",
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "add agent image",
            ],
        );
        let branch = git_output(&src, &["symbolic-ref", "--short", "HEAD"]).await.unwrap();
        sh(
            &src,
            "git",
            &["push", &origin.to_string_lossy(), &format!("HEAD:{branch}")],
        );

        sync_clone_checkout(&repo, None).await.unwrap();
        assert!(repo.join(".remoter/agent.Dockerfile").is_file());
        let tip = git_output(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        let upstream = git_output(&repo, &["rev-parse", &format!("origin/{branch}")])
            .await
            .unwrap();
        assert_eq!(tip, upstream);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §5.4: the clone outlives the project's Repository URL setting — a
    /// re-prepare with an edited URL must repoint `origin` before fetching.
    #[tokio::test]
    async fn prepare_repoints_origin_when_repo_url_changes() {
        let base = scratch("set-url");
        let repo_a = make_bare_repo(&base, "a");
        let repo_b = make_bare_repo(&base, "b");
        let ws = base.join("ws");

        prepare(&ws, 7, 42, "add login page", &repo_a.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);
        assert_eq!(
            git_output(&repo, &["remote", "get-url", "origin"]).await.unwrap(),
            repo_a.to_string_lossy()
        );

        prepare(&ws, 7, 42, "add login page", &repo_b.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(
            git_output(&repo, &["remote", "get-url", "origin"]).await.unwrap(),
            repo_b.to_string_lossy()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §5.4: a ticket renamed between runs keeps the branch its worktree was
    /// created with — re-preparing with a new title reports the worktree's
    /// actual branch, not a recomputed name that doesn't exist locally.
    #[tokio::test]
    async fn prepare_reuses_existing_worktree_branch_after_rename() {
        let base = scratch("rename");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        assert_eq!(existing_branch(&ws, 7, 42).await, None);

        let first = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(first.branch, "agent/task-42-add-login-page");
        // A fresh worktree branches off the base — it must NOT track it, or a
        // human's bare `git push` in the worktree hits "upstream branch does
        // not match" against origin/<base>.
        assert!(
            git_output(&first.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"])
                .await
                .is_err()
        );

        let second = prepare(&ws, 7, 42, "a totally different title", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(second.branch, "agent/task-42-add-login-page");
        assert_eq!(
            existing_branch(&ws, 7, 42).await.as_deref(),
            Some("agent/task-42-add-login-page")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The supervise run's accept path: a clean child merge lands a merge
    /// commit and `contains_commit` confirms absorption; a conflicting child
    /// merge reports `Conflicted` with the conflicted paths; and merging the
    /// same branch again is a clean no-op (re-triggered supervise runs must be
    /// idempotent).
    #[tokio::test]
    async fn merge_absorbs_child_and_reports_conflicts() {
        let base = scratch("merge");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        // Parent worktree on its own branch.
        let parent = prepare(&ws, 7, 100, "parent work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        std::fs::write(parent.dir.join("parent.txt"), "parent\n").unwrap();
        sh(&parent.dir, "git", &["add", "."]);
        sh(
            &parent.dir,
            "git",
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "parent work",
            ],
        );

        // Clean child: stacked on the parent, one commit.
        let child = prepare_with_stack(
            &ws,
            7,
            200,
            "child work",
            &origin.to_string_lossy(),
            None,
            Some(&parent.branch),
        )
        .await
        .unwrap();
        std::fs::write(child.dir.join("child.txt"), "child\n").unwrap();
        sh(&child.dir, "git", &["add", "."]);
        sh(
            &child.dir,
            "git",
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "child work",
            ],
        );

        let outcome = merge(&parent.dir, &child.branch, "Merge child #200").await.unwrap();
        assert_eq!(outcome, MergeOutcome::Clean);
        let child_tip = git_output(&child.dir, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(contains_commit(&parent.dir, &child_tip).await.unwrap());
        assert!(unmerged_paths(&parent.dir).await.unwrap().is_empty());

        // Re-merge is a clean no-op.
        let again = merge(&parent.dir, &child.branch, "Merge child #200").await.unwrap();
        assert_eq!(again, MergeOutcome::Clean);

        // Conflicting child: touches the same file with different content.
        let conflicting = prepare_with_stack(
            &ws,
            7,
            300,
            "conflicting work",
            &origin.to_string_lossy(),
            None,
            Some(&parent.branch),
        )
        .await
        .unwrap();
        std::fs::write(conflicting.dir.join("parent.txt"), "conflicting\n").unwrap();
        sh(&conflicting.dir, "git", &["add", "."]);
        sh(
            &conflicting.dir,
            "git",
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "conflicting work",
            ],
        );
        // The parent side changes the same file before the merge — a conflict
        // needs both sides to move relative to the merge base.
        std::fs::write(parent.dir.join("parent.txt"), "parent v2\n").unwrap();
        sh(&parent.dir, "git", &["add", "."]);
        sh(
            &parent.dir,
            "git",
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "parent v2"],
        );
        let outcome = merge(&parent.dir, &conflicting.branch, "Merge child #300")
            .await
            .unwrap();
        assert_eq!(outcome, MergeOutcome::Conflicted);
        assert_eq!(unmerged_paths(&parent.dir).await.unwrap(), vec!["parent.txt"]);
        // The conflicting tip is not absorbed until the conflicts resolve.
        let conflicting_tip = git_output(&conflicting.dir, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(!contains_commit(&parent.dir, &conflicting_tip).await.unwrap());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §5.4 step 5 guard: zero commits ahead of the base on a fresh worktree,
    /// one after the agent's commit.
    #[tokio::test]
    async fn commits_ahead_counts_branch_commits() {
        let base = scratch("ahead");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");
        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(commits_ahead(&prepared.dir, "origin/HEAD").await.unwrap(), 0);

        std::fs::write(prepared.dir.join("work.txt"), "x").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(
            &prepared.dir,
            "git",
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "work"],
        );
        assert_eq!(commits_ahead(&prepared.dir, "origin/HEAD").await.unwrap(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §5.4 step 5 extended: `dirty_status` reports tracked modifications and
    /// untracked files, but ignored files are omitted.
    #[tokio::test]
    async fn dirty_status_reports_leftovers_but_ignores_ignored_files() {
        let base = scratch("dirty");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        sh(&prepared.dir, "git", &["config", "user.email", "t@t"]);
        sh(&prepared.dir, "git", &["config", "user.name", "t"]);

        // Pristine worktree.
        assert!(dirty_status(&prepared.dir).await.unwrap().is_empty());

        // Tracked modification.
        std::fs::write(prepared.dir.join("README.md"), "changed").unwrap();
        let status = dirty_status(&prepared.dir).await.unwrap();
        assert!(status.contains("README.md"), "{status}");
        sh(&prepared.dir, "git", &["checkout", "--", "README.md"]);

        // Untracked file.
        std::fs::write(prepared.dir.join("leftover.txt"), "x").unwrap();
        let status = dirty_status(&prepared.dir).await.unwrap();
        assert!(status.contains("leftover.txt"), "{status}");
        std::fs::remove_file(prepared.dir.join("leftover.txt")).unwrap();

        // Ignored file is invisible to porcelain.
        std::fs::write(prepared.dir.join(".gitignore"), "*.ignored\n").unwrap();
        sh(&prepared.dir, "git", &["add", ".gitignore"]);
        sh(&prepared.dir, "git", &["commit", "-m", "ignorefile"]);
        std::fs::write(prepared.dir.join("build.ignored"), "x").unwrap();
        assert!(dirty_status(&prepared.dir).await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §4.4: a push rejected as non-fast-forward (the remote branch moved
    /// ahead — a completed-then-reopened ticket, or review fixes pushed to
    /// the PR branch) is recovered by fetch + rebase + retry, integrating the
    /// remote commits instead of clobbering them.
    #[tokio::test]
    async fn push_rebases_and_retries_on_non_fast_forward() {
        let base = scratch("push-retry");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        sh(&prepared.dir, "git", &["config", "user.email", "t@t"]);
        sh(&prepared.dir, "git", &["config", "user.name", "t"]);

        // The agent's first commit lands on the remote branch.
        std::fs::write(prepared.dir.join("work.txt"), "v1").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v1"]);
        push(&prepared.dir, &prepared.branch).await.unwrap();

        // The remote moves ahead (a human's review fix on the PR branch).
        let human = base.join("human");
        sh(
            &base,
            "git",
            &["clone", &origin.to_string_lossy(), &human.to_string_lossy()],
        );
        sh(&human, "git", &["config", "user.email", "h@h"]);
        sh(&human, "git", &["config", "user.name", "h"]);
        sh(&human, "git", &["checkout", &prepared.branch]);
        std::fs::write(human.join("review-fix.txt"), "fix").unwrap();
        sh(&human, "git", &["add", "."]);
        sh(&human, "git", &["commit", "-m", "review fix"]);
        sh(&human, "git", &["push", "origin", &prepared.branch]);

        // The agent commits again without the human's commit: a plain push is
        // rejected; the recovery must integrate and deliver all commits.
        std::fs::write(prepared.dir.join("work2.txt"), "v2").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v2"]);
        push(&prepared.dir, &prepared.branch).await.unwrap();

        // No rebase state left behind, and the remote branch has all three.
        assert!(
            git_output(&prepared.dir, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty()
        );
        let log = git_output(
            &prepared.dir,
            &["log", "--format=%s", &format!("origin/{}", prepared.branch)],
        )
        .await
        .unwrap();
        assert!(
            log.contains("v2") && log.contains("review fix") && log.contains("v1"),
            "{log}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §4.4 force-push path: when the remote branch's extra commits are
    /// patch-equivalent to local commits, the agent rewrote its own history
    /// (e.g. rebase onto master); `push` uses `--force-with-lease` instead of
    /// rebasing. The remote tip is then the local HEAD.
    #[tokio::test]
    async fn push_force_pushes_after_history_rewrite() {
        let base = scratch("push-force");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        sh(&prepared.dir, "git", &["config", "user.email", "t@t"]);
        sh(&prepared.dir, "git", &["config", "user.name", "t"]);

        // Agent pushes v1.
        std::fs::write(prepared.dir.join("work.txt"), "v1").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v1"]);
        push(&prepared.dir, &prepared.branch).await.unwrap();
        let remote_v1 = git_output(&prepared.dir, &["rev-parse", &format!("origin/{}", prepared.branch)])
            .await
            .unwrap();

        // Agent rewrites its own history: soft-reset v1, recommit with a
        // different message (same patch), then add v2. The old remote commit
        // has a patch-equivalent locally, so `git cherry` returns only `-` lines.
        sh(&prepared.dir, "git", &["reset", "--soft", "HEAD~1"]);
        sh(&prepared.dir, "git", &["commit", "-m", "v1 rewritten"]);
        std::fs::write(prepared.dir.join("work2.txt"), "v2").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v2"]);

        // The next push is non-fast-forward, but the recovery forces with lease.
        let outcome = push(&prepared.dir, &prepared.branch).await.unwrap();
        assert_eq!(outcome, PushOutcome::ForcePushed);
        assert!(
            git_output(&prepared.dir, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty()
        );
        let remote_tip = git_output(&prepared.dir, &["rev-parse", &format!("origin/{}", prepared.branch)])
            .await
            .unwrap();
        let local_head = git_output(&prepared.dir, &["rev-parse", "HEAD"]).await.unwrap();
        assert_eq!(remote_tip, local_head);
        assert_ne!(remote_tip, remote_v1, "remote tip advanced to the rewritten history");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §4.4 rebase path: when the remote has a commit that is NOT
    /// patch-equivalent locally (a human review fix), the daemon rebases. If
    /// the rebase conflicts, it is aborted, the remote is untouched, and the
    /// push fails.
    #[tokio::test]
    async fn push_does_not_force_when_remote_has_new_commits_and_rebase_conflicts() {
        let base = scratch("push-conflict");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        sh(&prepared.dir, "git", &["config", "user.email", "t@t"]);
        sh(&prepared.dir, "git", &["config", "user.name", "t"]);

        // Agent pushes v1.
        std::fs::write(prepared.dir.join("shared.txt"), "agent v1\n").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v1"]);
        push(&prepared.dir, &prepared.branch).await.unwrap();

        // Human pushes a conflicting change to the same file on the remote branch.
        let human = base.join("human");
        sh(
            &base,
            "git",
            &["clone", &origin.to_string_lossy(), &human.to_string_lossy()],
        );
        sh(&human, "git", &["config", "user.email", "h@h"]);
        sh(&human, "git", &["config", "user.name", "h"]);
        sh(&human, "git", &["checkout", &prepared.branch]);
        std::fs::write(human.join("shared.txt"), "human fix\n").unwrap();
        sh(&human, "git", &["add", "."]);
        sh(&human, "git", &["commit", "-m", "human fix"]);
        sh(&human, "git", &["push", "origin", &prepared.branch]);
        let remote_tip_before = git_output(&human, &["rev-parse", "HEAD"]).await.unwrap();

        // Agent commits a conflicting change.
        std::fs::write(prepared.dir.join("shared.txt"), "agent v2\n").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v2"]);

        // Recovery rebase conflicts; push returns the original error and aborts.
        let err = push(&prepared.dir, &prepared.branch).await.unwrap_err();
        assert!(
            err.0.contains("non-fast-forward") || err.0.contains("recovery rebase"),
            "{err}"
        );
        assert!(
            git_output(&prepared.dir, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty(),
            "no rebase state left behind"
        );
        let remote_tip_after = git_output(&prepared.dir, &["rev-parse", &format!("origin/{}", prepared.branch)])
            .await
            .unwrap();
        assert_eq!(remote_tip_after, remote_tip_before, "remote branch untouched");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §4.4 force-push fast path: the agent rebased its branch onto an
    /// advanced base and the conflict resolution changed the commit's patch,
    /// so `git cherry` no longer sees the old remote commit as
    /// patch-equivalent — the patch-equivalence recovery would misread it as
    /// somebody else's work and route into a rebase that conflicts with
    /// itself (ticket #18). But the remote tip is still this clone's own last
    /// push, so `push` must force-with-lease via the provenance check.
    #[tokio::test]
    async fn push_force_pushes_after_conflicting_rebase_onto_advanced_base() {
        let base = scratch("push-rebase-conflict");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        sh(&prepared.dir, "git", &["config", "user.email", "t@t"]);
        sh(&prepared.dir, "git", &["config", "user.name", "t"]);

        // Agent creates shared.txt and pushes v1.
        std::fs::write(prepared.dir.join("shared.txt"), "agent line\n").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v1: shared line"]);
        push(&prepared.dir, &prepared.branch).await.unwrap();

        // The base branch advances with a conflicting add of the same file.
        let other = base.join("other");
        sh(
            &base,
            "git",
            &["clone", &origin.to_string_lossy(), &other.to_string_lossy()],
        );
        sh(&other, "git", &["config", "user.email", "o@o"]);
        sh(&other, "git", &["config", "user.name", "o"]);
        let base_branch = remote_default_branch(&other).await.unwrap();
        std::fs::write(other.join("shared.txt"), "base line\n").unwrap();
        sh(&other, "git", &["add", "."]);
        sh(&other, "git", &["commit", "-m", "base advances"]);
        sh(&other, "git", &["push", "origin", &format!("HEAD:{base_branch}")]);

        // The agent rebases onto the advanced base; the add/add conflict on
        // shared.txt is resolved by keeping the agent's content, so the
        // rebased commit's patch (modify, not create) differs from the old
        // remote commit's — `git cherry` reports it as a `+` line.
        sh(&prepared.dir, "git", &["fetch", "origin", &base_branch]);
        let rebase = std::process::Command::new("git")
            .current_dir(&prepared.dir)
            .args(["rebase", &format!("origin/{base_branch}")])
            .output()
            .unwrap();
        assert!(
            !rebase.status.success(),
            "rebase must conflict for this test to mean anything"
        );
        std::fs::write(prepared.dir.join("shared.txt"), "agent line\n").unwrap();
        sh(&prepared.dir, "git", &["add", "shared.txt"]);
        sh(
            &prepared.dir,
            "git",
            &["-c", "core.editor=true", "rebase", "--continue"],
        );
        sh(&prepared.dir, "git", &["fetch", "origin", &prepared.branch]);
        let cherry = git_output(
            &prepared.dir,
            &["cherry", "HEAD", &format!("origin/{}", prepared.branch)],
        )
        .await
        .unwrap();
        assert!(cherry.lines().any(|l| l.starts_with("+ ")), "{cherry}");

        // The push is rejected as non-fast-forward, but the remote tip is the
        // daemon's own last push, so the recovery forces with lease.
        let outcome = push(&prepared.dir, &prepared.branch).await.unwrap();
        assert_eq!(outcome, PushOutcome::ForcePushed);
        assert!(
            git_output(&prepared.dir, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty(),
            "no rebase state left behind"
        );
        let remote_tip = git_output(&prepared.dir, &["rev-parse", &format!("origin/{}", prepared.branch)])
            .await
            .unwrap();
        let local_head = git_output(&prepared.dir, &["rev-parse", "HEAD"]).await.unwrap();
        assert_eq!(remote_tip, local_head);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// §5.4: the completion cleanup deletes only the *local* branch; a
    /// reopened ticket recreates the worktree from the surviving remote
    /// branch (the reviewed commits), not from base — otherwise the next
    /// push forks the history and is rejected as non-fast-forward.
    #[tokio::test]
    async fn prepare_continues_from_remote_branch_after_cleanup() {
        let base = scratch("reopen");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        sh(&prepared.dir, "git", &["config", "user.email", "t@t"]);
        sh(&prepared.dir, "git", &["config", "user.name", "t"]);
        std::fs::write(prepared.dir.join("work.txt"), "v1").unwrap();
        sh(&prepared.dir, "git", &["add", "."]);
        sh(&prepared.dir, "git", &["commit", "-m", "v1"]);
        push(&prepared.dir, &prepared.branch).await.unwrap();
        let pushed_tip = git_output(&prepared.dir, &["rev-parse", "HEAD"]).await.unwrap();

        // Ticket accepted → cleanup removes the worktree + local branch …
        cleanup(&ws, 7, 42, &prepared.branch, false).await.unwrap();
        assert!(!prepared.dir.exists());

        // … then the ticket is reopened: the new worktree branches off the
        // remote branch's tip, so the next push fast-forwards.
        let reopened = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(reopened.branch, prepared.branch);
        assert_eq!(
            git_output(&reopened.dir, &["rev-parse", "HEAD"]).await.unwrap(),
            pushed_tip
        );
        // … and the branch tracks its own remote branch, not the base.
        assert_eq!(
            git_output(&reopened.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"])
                .await
                .unwrap(),
            format!("origin/{}", reopened.branch)
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Branch stacking for supervised children (spec §5.4): a worktree
    /// prepared with `stack_base` branches off the parent's local branch tip
    /// — not off the fetched base — and does not track anything.
    #[tokio::test]
    async fn prepare_with_stack_branches_off_parent_tip() {
        let base = scratch("stack");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        // The parent's worktree + branch, with one commit on top of the base.
        let parent = prepare(&ws, 7, 41, "parent work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        sh(&parent.dir, "git", &["config", "user.email", "t@t"]);
        sh(&parent.dir, "git", &["config", "user.name", "t"]);
        std::fs::write(parent.dir.join("parent.txt"), "p").unwrap();
        sh(&parent.dir, "git", &["add", "."]);
        sh(&parent.dir, "git", &["commit", "-m", "parent work"]);
        let parent_tip = git_output(&parent.dir, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(local_branch_exists(&repo_dir(&ws, 7), &parent.branch).await);

        // The supervised child's worktree starts exactly at the parent's tip.
        let child = prepare_with_stack(
            &ws,
            7,
            42,
            "child work",
            &origin.to_string_lossy(),
            None,
            Some(&parent.branch),
        )
        .await
        .unwrap();
        let merge_base = git_output(&child.dir, &["merge-base", "HEAD", &parent.branch])
            .await
            .unwrap();
        assert_eq!(merge_base, parent_tip, "child branch must start at the parent's tip");
        // Only the parent's commit sits between the child branch and the base.
        assert_eq!(commits_ahead(&child.dir, "origin/HEAD").await.unwrap(), 1);
        // The parent's branch has no upstream — the child must not track it.
        assert!(
            git_output(&child.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"])
                .await
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `cleanup(keep_branch = true)` (review #106): a supervised child's
    /// commits live only on its local branch until the supervision loop
    /// absorbs them into the parent's — accepting the child must remove the
    /// worktree but keep the branch.
    #[tokio::test]
    async fn cleanup_keep_branch_removes_worktree_but_keeps_the_branch() {
        let base = scratch("keep-branch");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        cleanup(&ws, 7, 42, &prepared.branch, true).await.unwrap();
        assert!(!prepared.dir.exists(), "worktree removed");
        assert!(
            local_branch_exists(&repo_dir(&ws, 7), &prepared.branch).await,
            "branch kept for the supervision loop to absorb"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A leftover dir that is no longer a registered worktree (task 23: an
    /// earlier cleanup removed the worktree and branch, then a still-running
    /// devenv/MinIO process recreated `.devenv/state/...` under `wt-<id>`).
    /// `git worktree remove` on such a dir fails permanently with "not a
    /// working tree" — treating every remove failure as "busy, retry" retries
    /// forever. Cleanup must delete the leftover directly, remove the branch
    /// if it survives, and stay idempotent.
    #[tokio::test]
    async fn cleanup_removes_leftover_dir_that_is_not_a_registered_worktree() {
        let base = scratch("leftover");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);
        let branch_ref = format!("refs/heads/{}", prepared.branch);

        // Remove the worktree through git (the branch survives), then recreate
        // only the state dir — exactly the wt-23 shape.
        sh(
            &repo,
            "git",
            &["worktree", "remove", "--force", &prepared.dir.to_string_lossy()],
        );
        let state = prepared.dir.join(".devenv/state/minio");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("minio.log"), "x").unwrap();
        assert!(!prepared.dir.join(".git").exists());
        let list = git_output(&repo, &["worktree", "list", "--porcelain"]).await.unwrap();
        assert!(!list.contains("wt-42"), "{list}");

        cleanup(&ws, 7, 42, &prepared.branch, false).await.unwrap();
        assert!(!prepared.dir.exists());
        assert!(
            git_output(&repo, &["rev-parse", "--verify", &branch_ref])
                .await
                .is_err()
        );

        // Idempotent: a second cleanup of the same ticket is a no-op success.
        cleanup(&ws, 7, 42, &prepared.branch, false).await.unwrap();

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A busy worktree (here: `git worktree lock` — a locked worktree refuses
    /// removal even with `--force`, the deterministic stand-in for an open
    /// terminal or running devenv processes) must not strand cleanup in the
    /// "worktree present, branch gone" state: the branch survives, and the
    /// next poll's retry completes once the dir is free again.
    #[tokio::test]
    async fn cleanup_keeps_branch_when_worktree_is_busy() {
        let base = scratch("busy");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);
        let branch_ref = format!("refs/heads/{}", prepared.branch);
        sh(&repo, "git", &["worktree", "lock", &prepared.dir.to_string_lossy()]);

        cleanup(&ws, 7, 42, &prepared.branch, false).await.unwrap();
        assert!(prepared.dir.exists());
        assert!(
            git_output(&repo, &["rev-parse", "--verify", &branch_ref]).await.is_ok(),
            "the branch must survive while the worktree is busy"
        );

        // The terminal is closed: the next housekeeping poll retries.
        sh(&repo, "git", &["worktree", "unlock", &prepared.dir.to_string_lossy()]);
        cleanup(&ws, 7, 42, &prepared.branch, false).await.unwrap();
        assert!(!prepared.dir.exists());
        assert!(
            git_output(&repo, &["rev-parse", "--verify", &branch_ref])
                .await
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A missing local branch is a successful cleanup outcome (the branch may
    /// have been deleted by an earlier partial cleanup or by hand) — cleanup
    /// must not error, and housekeeping must not WARN on every poll.
    #[tokio::test]
    async fn cleanup_treats_missing_branch_as_success() {
        let base = scratch("missing-branch");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let repo = repo_dir(&ws, 7);
        sh(
            &repo,
            "git",
            &["worktree", "remove", "--force", &prepared.dir.to_string_lossy()],
        );
        sh(&repo, "git", &["branch", "-D", &prepared.branch]);

        cleanup(&ws, 7, 42, &prepared.branch, false).await.unwrap();
        assert!(!prepared.dir.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A human deleted the worktree dir out from under git: cleanup prunes the
    /// stale worktree metadata and still removes the branch.
    #[tokio::test]
    async fn cleanup_handles_manually_deleted_worktree_dir() {
        let base = scratch("manual-rm");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");

        let prepared = prepare(&ws, 7, 42, "add login page", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        std::fs::remove_dir_all(&prepared.dir).unwrap();

        cleanup(&ws, 7, 42, &prepared.branch, false).await.unwrap();

        let repo = repo_dir(&ws, 7);
        assert!(
            git_output(
                &repo,
                &["rev-parse", "--verify", &format!("refs/heads/{}", prepared.branch)]
            )
            .await
            .is_err()
        );
        let list = git_output(&repo, &["worktree", "list", "--porcelain"]).await.unwrap();
        assert!(!list.contains("wt-42"), "{list}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn devenv_detection() {
        let dir = std::env::temp_dir().join(format!("remoter-ws-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!uses_devenv(&dir));
        std::fs::write(dir.join("devenv.nix"), "{}").unwrap();
        assert!(uses_devenv(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrap_command_plain_runs_directly() {
        let env = vec![("REMOTER_AGENT_TASK_ID".to_string(), "42".to_string())];
        let cmd = wrap_command(Path::new("/wt"), &ExecEnv::host(false), "kimi", &["acp"], &env);
        assert_eq!(cmd.get_program(), "kimi");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["acp"]);
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/wt")));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(std::ffi::OsStr::new("CI"), Some(std::ffi::OsStr::new("1")))));
        assert!(envs.contains(&(
            std::ffi::OsStr::new("REMOTER_AGENT_TASK_ID"),
            Some(std::ffi::OsStr::new("42"))
        )));
    }

    #[test]
    fn wrap_command_removes_legacy_human_token() {
        let cmd = wrap_command(Path::new("/wt"), &ExecEnv::host(false), "kimi", &["acp"], &[]);
        let legacy = std::ffi::OsStr::new("REMOTER_TOKEN");
        assert!(
            cmd.get_envs().any(|(key, value)| key == legacy && value.is_none()),
            "legacy REMOTER_TOKEN must be explicitly removed from the child environment"
        );
    }

    #[test]
    fn wrap_command_devenv_wraps_in_shell() {
        let cmd = wrap_command(Path::new("/wt"), &ExecEnv::host(true), "kimi", &["acp", "--yolo"], &[]);
        assert_eq!(cmd.get_program(), "devenv");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            args,
            ["shell", "--no-tui", "--no-eval-cache", "--", "kimi", "acp", "--yolo"]
        );
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(
            std::ffi::OsStr::new("DEVENV_NO_AI_AGENT"),
            Some(std::ffi::OsStr::new("1"))
        )));
        assert!(envs.contains(&(std::ffi::OsStr::new("CI"), Some(std::ffi::OsStr::new("1")))));
    }

    #[test]
    fn wrap_command_container_uses_docker_exec() {
        let exec = ExecEnv::Container(ContainerExec {
            docker: "docker".to_string(),
            container: "remoter-run-7".to_string(),
            work_dir: PathBuf::from("/work"),
            devenv: true,
            sessions_dir: PathBuf::from("/agent-home/.kimi-code/sessions"),
            api_url: "http://host.docker.internal:8181".to_string(),
        });
        let env = vec![("REMOTER_CONTAINER".to_string(), "1".to_string())];
        let cmd = wrap_command(Path::new("/host/wt"), &exec, "kimi", &["acp"], &env);
        assert_eq!(cmd.get_program(), "docker");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            [
                "exec",
                "-i",
                "-w",
                "/work",
                "-e",
                "CI=1",
                "-e",
                "DEVENV_NO_AI_AGENT=1",
                "-e",
                "REMOTER_CONTAINER=1",
                "remoter-run-7",
                "env",
                "-u",
                "REMOTER_TOKEN",
                "devenv",
                "shell",
                "--no-tui",
                "--no-eval-cache",
                "--",
                "kimi",
                "acp",
            ]
        );
    }

    #[test]
    fn wrap_command_container_without_devenv_runs_the_program_directly() {
        let exec = ExecEnv::Container(ContainerExec {
            docker: "docker".to_string(),
            container: "remoter-run-9".to_string(),
            work_dir: PathBuf::from("/work"),
            devenv: false,
            sessions_dir: PathBuf::from("/agent-home/.kimi-code/sessions"),
            api_url: "http://host.docker.internal:8181".to_string(),
        });
        let env = vec![("REMOTER_CONTAINER".to_string(), "1".to_string())];
        let cmd = wrap_command(Path::new("/host/wt"), &exec, "just", &["db-up"], &env);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            [
                "exec",
                "-i",
                "-w",
                "/work",
                "-e",
                "CI=1",
                "-e",
                "REMOTER_CONTAINER=1",
                "remoter-run-9",
                "env",
                "-u",
                "REMOTER_TOKEN",
                "just",
                "db-up",
            ]
        );
    }

    #[test]
    fn wrap_command_container_env_is_argv_not_shell() {
        // Values with spaces or shell metacharacters travel as single `-e`
        // argv elements — there is no `sh -c` anywhere in the invocation, so
        // no quoting/escaping layer can mangle or reinterpret them.
        let exec = ExecEnv::Container(ContainerExec {
            docker: "docker".to_string(),
            container: "remoter-run-3".to_string(),
            work_dir: PathBuf::from("/work"),
            devenv: true,
            sessions_dir: PathBuf::from("/agent-home/.kimi-code/sessions"),
            api_url: "http://host.docker.internal:8181".to_string(),
        });
        let env = vec![
            ("GREETING".to_string(), "hello world".to_string()),
            ("PAYLOAD".to_string(), "$(rm -rf /); `id`".to_string()),
        ];
        let cmd = wrap_command(Path::new("/host/wt"), &exec, "kimi", &["acp"], &env);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.iter().any(|a| a == "GREETING=hello world"));
        assert!(args.iter().any(|a| a == "PAYLOAD=$(rm -rf /); `id`"));
        assert!(!args.iter().any(|a| a == "sh" || a == "-c"));
    }

    // ── devenv up/down via a PATH-stubbed fake `devenv` ──────────────────────
    //
    // The stub appends `<args> @ <cwd>` to `$DEVENV_STUB_LOG` and exits 1 when
    // `$DEVENV_STUB_FAIL` is a substring of its args. The tests mutate the
    // process-wide PATH, so they serialize on `ENV_LOCK`.

    static ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct DevenvStub {
        bin: PathBuf,
        log: PathBuf,
        orig_path: Option<std::ffi::OsString>,
    }

    impl DevenvStub {
        fn install(tag: &str) -> Self {
            let base = scratch(tag);
            let bin = base.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let log = base.join("devenv.log");
            std::fs::write(
                bin.join("devenv"),
                "#!/bin/sh\necho \"$* @ $(pwd)\" >> \"$DEVENV_STUB_LOG\"\nif [ -n \"$DEVENV_STUB_FAIL\" ]; then\n  case \"$*\" in\n    *\"$DEVENV_STUB_FAIL\"*) exit 1;;\n  esac\nfi\nexit 0\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(bin.join("devenv"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let orig_path = std::env::var_os("PATH");
            // SAFETY: serialized on ENV_LOCK; no other thread in this binary
            // touches PATH while these tests hold it.
            unsafe {
                std::env::set_var(
                    "PATH",
                    format!(
                        "{}:{}",
                        bin.display(),
                        orig_path.as_deref().unwrap_or_default().to_string_lossy()
                    ),
                );
                std::env::set_var("DEVENV_STUB_LOG", &log);
                std::env::remove_var("DEVENV_STUB_FAIL");
            }
            Self { bin, log, orig_path }
        }

        fn fail_on(&self, pattern: &str) {
            // SAFETY: see install().
            unsafe { std::env::set_var("DEVENV_STUB_FAIL", pattern) };
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for DevenvStub {
        fn drop(&mut self) {
            // SAFETY: see install().
            unsafe {
                match &self.orig_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
                std::env::remove_var("DEVENV_STUB_LOG");
                std::env::remove_var("DEVENV_STUB_FAIL");
            }
            let _ = std::fs::remove_dir_all(self.bin.parent().unwrap());
        }
    }

    /// The arg part of each logged call, without the cwd suffix.
    fn args_only(calls: &[String]) -> Vec<String> {
        calls
            .iter()
            .map(|c| c.split(" @ ").next().unwrap().to_string())
            .collect()
    }

    /// Two-level directory listing of a sweep root, for failure diagnostics.
    fn list_tree(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(projects) = std::fs::read_dir(root) {
            for p in projects.flatten() {
                out.push(p.path().display().to_string());
                if let Ok(wts) = std::fs::read_dir(p.path()) {
                    for w in wts.flatten() {
                        out.push(w.path().display().to_string());
                    }
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn services_up_downs_first_then_up_then_wait() {
        let _guard = ENV_LOCK.lock().await;
        let stub = DevenvStub::install("up-order");
        let dir = scratch("up-order-wt");
        services_up(&dir, &[]).await.unwrap();
        assert_eq!(
            args_only(&stub.calls()),
            [
                "--no-eval-cache processes down",
                "--no-eval-cache up -d",
                "--no-eval-cache processes wait --timeout 120"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn services_up_tolerates_failing_down() {
        let _guard = ENV_LOCK.lock().await;
        let stub = DevenvStub::install("down-fails");
        stub.fail_on("processes down");
        let dir = scratch("down-fails-wt");
        services_up(&dir, &[]).await.unwrap();
        assert_eq!(
            args_only(&stub.calls()),
            [
                "--no-eval-cache processes down",
                "--no-eval-cache up -d",
                "--no-eval-cache processes wait --timeout 120"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn services_up_downs_again_when_wait_fails() {
        let _guard = ENV_LOCK.lock().await;
        let stub = DevenvStub::install("wait-fails");
        stub.fail_on("processes wait");
        let dir = scratch("wait-fails-wt");
        let err = services_up(&dir, &[]).await.unwrap_err();
        assert!(err.0.contains("processes wait"), "{err}");
        assert_eq!(
            args_only(&stub.calls()),
            [
                "--no-eval-cache processes down",
                "--no-eval-cache up -d",
                "--no-eval-cache processes wait --timeout 120",
                "--no-eval-cache processes down"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn down_all_worktrees_visits_only_devenv_worktrees() {
        let _guard = ENV_LOCK.lock().await;
        let stub = DevenvStub::install("sweep");
        let root = scratch("sweep-root");
        let devenv_nix = root.join("p1/wt-1");
        let plain_wt = root.join("p1/wt-2");
        let devenv_yaml = root.join("p2/wt-3");
        let repo_clone = root.join("p1/repo");
        let non_project = root.join("other/wt-9");
        for d in [&devenv_nix, &plain_wt, &devenv_yaml, &repo_clone, &non_project] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(devenv_nix.join("devenv.nix"), "{}").unwrap();
        std::fs::write(devenv_yaml.join("devenv.yaml"), "").unwrap();
        // A devenv marker directly in the project dir must not be swept either.
        std::fs::write(root.join("p1/devenv.nix"), "{}").unwrap();

        down_all_worktrees(&root).await;

        let calls = stub.calls();
        // Include the sweep tree and stub state so a flaky miss (observed once
        // in a sandboxed nix build) distinguishes "stub never spawned" from
        // "traversal saw nothing".
        assert_eq!(
            calls.len(),
            2,
            "calls={calls:?}; tree={:?}; stub bin exists={}",
            list_tree(&root),
            stub.bin.exists(),
        );
        assert!(
            calls.iter().all(|c| c.starts_with("--no-eval-cache processes down @ ")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.ends_with(&format!("@ {}", devenv_nix.display()))),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.ends_with(&format!("@ {}", devenv_yaml.display()))),
            "{calls:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── reference repos (#169) ───────────────────────────────────────────

    fn reference_repo(url: &Path, base_branch: Option<&str>, mount: &str) -> ReferenceRepo {
        ReferenceRepo {
            repo_url: url.to_string_lossy().to_string(),
            base_branch: base_branch.map(str::to_string),
            mount_name: mount.to_string(),
        }
    }

    /// A bare repo's default branch (`HEAD` symref) — `master`/`main` depends
    /// on the environment's git, so tests must not hardcode either.
    fn bare_default_branch(bare: &Path) -> String {
        let out = std::process::Command::new("git")
            .current_dir(bare)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Refs mount into the ticket worktree as `.refs/<mount>` symlinks (host
    /// mode), `info/exclude` keeps `git status --porcelain` clean (the commit
    /// guard), the ref worktree is detached, and an unavailable remote is
    /// fail-open — it lands in `failed` while the rest still mount.
    #[tokio::test]
    async fn reference_repos_mount_detached_and_fail_open() {
        let base = scratch("refs-mount");
        let origin = make_bare_repo(&base, "origin");
        let ref_a = make_bare_repo(&base, "refa");
        let ref_b = make_bare_repo(&base, "refb");
        let ws = base.join("ws");
        let prepared = prepare(&ws, 7, 42, "ticket work", &origin.to_string_lossy(), None)
            .await
            .unwrap();

        let refs = vec![
            reference_repo(&ref_a, None, "refa"),
            reference_repo(&ref_b, Some(&bare_default_branch(&ref_b)), "refb"),
            // Unreachable remote: fail-open, the other refs still mount.
            reference_repo(&base.join("gone.git"), None, "gone"),
        ];
        let outcome = prepare_reference_repos(&ws, 7, 42, &prepared.dir, &refs).await;
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, "gone");
        assert_eq!(outcome.mounted.len(), 2);

        // The ref worktree carries the base-branch tip's content, detached.
        let wt_a = refs_worktree_dir(&ws, 7, "refa", 42);
        assert_eq!(std::fs::read_to_string(wt_a.join("README.md")).unwrap(), "refa");
        assert!(
            worktree_branch(&wt_a).await.is_err(),
            "ref worktree must be detached — the daemon never branches in a reference repo"
        );

        // Host mode: `.refs/<mount>` symlinks into the ticket worktree.
        let failed = link_refs_into(&prepared.dir, &outcome.mounted).await;
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(
            std::fs::read_to_string(prepared.dir.join(".refs/refa/README.md")).unwrap(),
            "refa"
        );
        assert_eq!(
            std::fs::read_link(prepared.dir.join(".refs/refb")).unwrap(),
            refs_worktree_dir(&ws, 7, "refb", 42)
        );

        // `.refs/` is excluded: the commit guard's status stays clean.
        let exclude = git_output(&prepared.dir, &["rev-parse", "--git-path", "info/exclude"])
            .await
            .unwrap();
        let exclude = prepared.dir.join(exclude);
        let content = std::fs::read_to_string(exclude).unwrap();
        assert!(content.lines().any(|l| l.trim() == ".refs/"), "{content}");
        assert_eq!(
            dirty_status(&prepared.dir).await.unwrap(),
            "",
            "ref mounts must not dirty the ticket worktree"
        );

        // Idempotent re-run: no duplicate exclude line, links still fine.
        let outcome = prepare_reference_repos(&ws, 7, 42, &prepared.dir, &refs[..2]).await;
        assert_eq!(outcome.mounted.len(), 2);
        assert!(outcome.failed.is_empty());
        let failed = link_refs_into(&prepared.dir, &outcome.mounted).await;
        assert!(failed.is_empty(), "{failed:?}");
        let content = std::fs::read_to_string(
            prepared.dir.join(
                git_output(&prepared.dir, &["rev-parse", "--git-path", "info/exclude"])
                    .await
                    .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(content.lines().filter(|l| l.trim() == ".refs/").count(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repeated prepare reuses the shared refs clone and fetches the new
    /// upstream base: a *new* ticket's ref worktree branches off the new tip
    /// (an existing ticket's ref worktree is reused as-is).
    #[tokio::test]
    async fn reference_repos_reprepare_fetches_new_base() {
        let base = scratch("refs-refresh");
        let origin = make_bare_repo(&base, "origin");
        let ref_a = make_bare_repo(&base, "refa");
        let ws = base.join("ws");
        let prepared = prepare(&ws, 7, 42, "ticket work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let refs = vec![reference_repo(&ref_a, None, "refa")];

        let outcome = prepare_reference_repos(&ws, 7, 42, &prepared.dir, &refs).await;
        assert_eq!(outcome.mounted.len(), 1);
        assert!(outcome.failed.is_empty());
        let clone = refs_repo_dir(&ws, 7, "refa");
        assert!(clone.exists());

        // Advance the upstream base branch.
        let branch = bare_default_branch(&ref_a);
        let src = base.join("refa-src");
        std::fs::write(src.join("NEW.md"), "new").unwrap();
        sh(&src, "git", &["add", "."]);
        sh(
            &src,
            "git",
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "new"],
        );
        sh(&src, "git", &["push", &ref_a.to_string_lossy(), &branch]);

        // Same ticket: the existing ref worktree is reused as-is…
        let outcome = prepare_reference_repos(&ws, 7, 42, &prepared.dir, &refs).await;
        assert_eq!(outcome.mounted.len(), 1);
        assert!(!refs_worktree_dir(&ws, 7, "refa", 42).join("NEW.md").exists());
        // …but the clone did fetch the new tip.
        let tip = git_output(&clone, &["rev-parse", &format!("origin/{branch}")])
            .await
            .unwrap();
        let src_tip = git_output(&src, &["rev-parse", "HEAD"]).await.unwrap();
        assert_eq!(tip, src_tip);

        // A new ticket's ref worktree branches off the fresh tip.
        let prepared_43 = prepare(&ws, 7, 43, "other ticket", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let outcome = prepare_reference_repos(&ws, 7, 43, &prepared_43.dir, &refs).await;
        assert_eq!(outcome.mounted.len(), 1);
        assert!(refs_worktree_dir(&ws, 7, "refa", 43).join("NEW.md").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `cleanup` removes the ticket's ref worktrees along with its worktree;
    /// the shared refs clones stay for reuse.
    #[tokio::test]
    async fn cleanup_removes_ref_worktrees_keeps_clones() {
        let base = scratch("refs-cleanup");
        let origin = make_bare_repo(&base, "origin");
        let ref_a = make_bare_repo(&base, "refa");
        let ws = base.join("ws");
        let prepared = prepare(&ws, 7, 42, "ticket work", &origin.to_string_lossy(), None)
            .await
            .unwrap();
        let refs = vec![reference_repo(&ref_a, None, "refa")];
        let outcome = prepare_reference_repos(&ws, 7, 42, &prepared.dir, &refs).await;
        assert_eq!(outcome.mounted.len(), 1);
        let failed = link_refs_into(&prepared.dir, &outcome.mounted).await;
        assert!(failed.is_empty(), "{failed:?}");
        assert!(refs_worktree_dir(&ws, 7, "refa", 42).exists());

        cleanup(&ws, 7, 42, "agent/task-42-ticket-work", false).await.unwrap();

        assert!(!refs_worktree_dir(&ws, 7, "refa", 42).exists());
        assert!(
            refs_repo_dir(&ws, 7, "refa").exists(),
            "the shared refs clone is reused"
        );
        assert!(!worktree_dir(&ws, 7, 42).exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    // ── integration branches (cross-project children, #184) ───────────────

    #[test]
    fn integration_branch_name_format() {
        assert_eq!(
            integration_branch_name(7, "Cross-repo feature!"),
            "agent/feature-7-cross-repo-feature"
        );
    }

    /// ensure_integration_branch creates the branch off the child clone's
    /// origin base and pushes it; a second call reuses it; a local-only
    /// deletion is healed from the pushed remote copy.
    #[tokio::test]
    async fn integration_branch_create_reuse_reattach() {
        let base = scratch("integration-branch");
        let origin = make_bare_repo(&base, "origin");
        let ws = base.join("ws");
        let repo = ensure_repo_clone(&ws, 2, &origin.to_string_lossy()).await.unwrap();

        let branch = ensure_integration_branch(&repo, 7, "Parent work", None).await.unwrap();
        assert_eq!(branch, "agent/feature-7-parent-work");
        assert!(local_branch_exists(&repo, &branch).await);
        assert!(
            remote_branch_exists(&repo, &branch).await,
            "the integration branch must be pushed — the isolation invariant survives a host death"
        );
        // Branched off the base tip, not off some other ref.
        let base_tip = git_output(&repo, &["rev-parse", "origin/HEAD"]).await.unwrap();
        assert_eq!(git_rev_parse(&repo, &branch).await.unwrap(), base_tip);

        // Idempotent reuse.
        let again = ensure_integration_branch(&repo, 7, "Parent work", None).await.unwrap();
        assert_eq!(again, branch);

        // Local copy lost (fresh daemon workspace would re-clone, but a
        // partially cleaned clone reattaches from origin).
        sh(&repo, "git", &["branch", "-D", &branch]);
        let reattached = ensure_integration_branch(&repo, 7, "Parent work", None).await.unwrap();
        assert_eq!(reattached, branch);
        assert!(local_branch_exists(&repo, &branch).await);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// delete_integration_branch removes the branch locally and on origin and
    /// is idempotent; cleanup_integration_branches sweeps every project clone
    /// under the workspace root for the parent's `agent/feature-<id>-*`.
    #[tokio::test]
    async fn integration_branch_cleanup_sweeps_project_clones() {
        let base = scratch("integration-cleanup");
        let origin_a = make_bare_repo(&base, "origin-a");
        let origin_b = make_bare_repo(&base, "origin-b");
        let ws = base.join("ws");
        let repo_a = ensure_repo_clone(&ws, 2, &origin_a.to_string_lossy()).await.unwrap();
        let repo_b = ensure_repo_clone(&ws, 3, &origin_b.to_string_lossy()).await.unwrap();
        let branch_a = ensure_integration_branch(&repo_a, 7, "Parent work", None)
            .await
            .unwrap();
        let branch_b = ensure_integration_branch(&repo_b, 7, "Parent work", None)
            .await
            .unwrap();
        // Another parent's branch in the same clone must survive the sweep.
        let other = ensure_integration_branch(&repo_a, 8, "Other parent", None)
            .await
            .unwrap();

        cleanup_integration_branches(&ws, 7).await;

        for (repo, branch) in [(&repo_a, &branch_a), (&repo_b, &branch_b)] {
            assert!(
                !local_branch_exists(repo, branch).await,
                "{branch} must be gone locally"
            );
            assert!(
                !remote_branch_exists(repo, branch).await,
                "{branch} must be gone on origin"
            );
        }
        assert!(local_branch_exists(&repo_a, &other).await);
        assert!(remote_branch_exists(&repo_a, &other).await);

        // Idempotent: a second sweep over the leftovers is a no-op.
        cleanup_integration_branches(&ws, 7).await;

        let _ = std::fs::remove_dir_all(&base);
    }

    /// prepare_branch_reference mounts a detached worktree at the child
    /// branch's tip and re-points an existing worktree when the branch
    /// advances (unlike base-mode refs, which are reused as-is).
    #[tokio::test]
    async fn branch_reference_follows_the_child_branch_tip() {
        let base = scratch("branch-ref");
        let child_origin = make_bare_repo(&base, "child");
        let ws = base.join("ws");
        let branch = "agent/task-42-child-work";
        // Push a child branch with one commit on top of the base.
        let src = base.join("child-src");
        sh(&src, "git", &["checkout", "-b", branch]);
        std::fs::write(src.join("CHILD.md"), "v1").unwrap();
        sh(&src, "git", &["add", "."]);
        sh(
            &src,
            "git",
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "child v1"],
        );
        sh(&src, "git", &["push", &child_origin.to_string_lossy(), branch]);

        let mount = prepare_branch_reference(&ws, 1, 70, "task-42", &child_origin.to_string_lossy(), branch)
            .await
            .unwrap();
        assert_eq!(mount.mount_name, "task-42");
        assert_eq!(std::fs::read_to_string(mount.worktree.join("CHILD.md")).unwrap(), "v1");
        assert!(
            worktree_branch(&mount.worktree).await.is_err(),
            "branch reference worktree must be detached"
        );

        // The child branch advances: the same mount re-points at the new tip.
        std::fs::write(src.join("CHILD.md"), "v2").unwrap();
        sh(&src, "git", &["add", "."]);
        sh(
            &src,
            "git",
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "child v2"],
        );
        sh(&src, "git", &["push", &child_origin.to_string_lossy(), branch]);
        let mount = prepare_branch_reference(&ws, 1, 70, "task-42", &child_origin.to_string_lossy(), branch)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(mount.worktree.join("CHILD.md")).unwrap(), "v2");

        let _ = std::fs::remove_dir_all(&base);
    }
}
