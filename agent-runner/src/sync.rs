//! PR review feedback sync (PROD-8, spec §4.4) and PR mergeability sync
//! (PROD-9): while a ticket sits in `review`, the daemon watches the draft
//! PR/MR for human feedback and for mergeability conflicts. Both syncs are
//! read-only towards the forge and never error upward — a sync failure must not
//! kill the poll loop (same rule as `run.rs`).
//!
//! Dedup state lives in `<workspace_root>/pr-feedback-sync.json` (feedback ids)
//! and `<workspace_root>/pr-mergeable-sync.json` (`pr_url` → last rebase-bounced
//! `base_sha`) so a daemon restart doesn't re-inject stale state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::client::RemoterClient;
use crate::config::Config;
use crate::forge::{self, Mergeable, PrFeedback};
use crate::workspace;

const FEEDBACK_STATE_FILE: &str = "pr-feedback-sync.json";
const MERGEABLE_STATE_FILE: &str = "pr-mergeable-sync.json";

pub struct PrFeedbackSync {
    state_path: PathBuf,
    /// `pr_url` → feedback ids already mirrored into the thread.
    seen: HashMap<String, HashSet<String>>,
}

impl PrFeedbackSync {
    /// Loads the dedup state from `<workspace_root>/pr-feedback-sync.json`;
    /// a missing or corrupt file starts empty (lenient — worst case is a
    /// one-time re-injection of the PR history into the thread).
    pub fn new(workspace_root: &Path) -> Self {
        let state_path = workspace_root.join(FEEDBACK_STATE_FILE);
        let seen = load_state(&state_path);
        Self { state_path, seen }
    }

    /// One sync pass over every ticket of this agent sitting in `review`.
    /// Logs and continues on every failure — never errors upward. The cadence
    /// is the caller's job (`main.rs`), so tests drive this directly.
    pub async fn sync_once(&mut self, client: &RemoterClient, _config: &Config) {
        let tasks = match client.my_tasks(Some("review")).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "PR feedback sync: could not list review tasks");
                return;
            }
        };
        if tasks.is_empty() {
            return;
        }
        // repo_url per project (the forge API path derives from it).
        let repo_urls: HashMap<i32, String> = match client.projects().await {
            Ok(ps) => ps.into_iter().filter_map(|p| p.repo_url.map(|u| (p.id, u))).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "PR feedback sync: could not list projects");
                return;
            }
        };
        for task in tasks {
            self.sync_task(client, &repo_urls, task.id, task.project_id).await;
        }
    }

    async fn sync_task(
        &mut self,
        client: &RemoterClient,
        repo_urls: &HashMap<i32, String>,
        task_id: i32,
        project_id: i32,
    ) {
        // The newest run carrying a PR URL owns the conversation (spec §4.4).
        let runs = match client.list_runs(task_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "PR feedback sync: could not list runs");
                return;
            }
        };
        let Some(pr_url) = runs.iter().find_map(|r| r.pr_url.clone()) else {
            return; // no PR (no forge configured or push/PR failed) — nothing to sync
        };
        let pr_number = match forge::pr_number_from_url(&pr_url) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(task_id, pr_url, error = %e, "PR feedback sync: unparseable PR URL");
                return;
            }
        };
        let Some(repo_url) = repo_urls.get(&project_id) else {
            return; // project no longer agent-managed
        };
        // Same skip semantics as push_and_create_pr (run.rs): no forge kind or
        // token → nothing to call the forge with.
        let cfg = match client.forge_config(project_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "PR feedback sync: could not fetch forge config");
                return;
            }
        };
        let (Some(kind), Some(token)) = (cfg.forge_kind.as_deref(), cfg.forge_token.as_deref()) else {
            return;
        };
        let kind = match forge::ForgeKind::from_token(kind) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "PR feedback sync: unknown forge kind");
                return;
            }
        };

        let feedback = match forge::list_pr_feedback(
            client.http(),
            kind,
            cfg.forge_api_url.as_deref(),
            token,
            repo_url,
            pr_number,
        )
        .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(task_id, pr_url, error = %e, "PR feedback sync: forge API call failed");
                return;
            }
        };
        let seen = self.seen.entry(pr_url.clone()).or_default();
        let new_items: Vec<PrFeedback> = feedback.into_iter().filter(|f| !seen.contains(&f.id)).collect();
        if new_items.is_empty() {
            return;
        }
        // Mark seen and persist BEFORE posting: losing a comment beats a
        // bounce loop if the comment write fails (spec §4.4).
        for f in &new_items {
            seen.insert(f.id.clone());
        }
        self.persist(task_id);

        for f in &new_items {
            let mut body = format!("**PR review** @{}", f.author);
            if let Some(ctx) = &f.context {
                body.push_str(&format!(" `{ctx}`"));
            }
            body.push_str(&format!(": {}", f.body));
            if let Err(e) = client.add_comment(task_id, &body).await {
                tracing::warn!(task_id, error = %e, "PR feedback sync: could not post comment");
            }
        }
        tracing::info!(
            task_id,
            pr_url,
            count = new_items.len(),
            "PR feedback mirrored; bouncing → implement"
        );
        // Auto-bounce (PROD-8): the next poll claims the ticket for a new
        // implement run whose prompt carries the fresh comments.
        match client.set_task_status(task_id, "implement").await {
            Ok(()) => {}
            Err(e) if e.is_conflict() || e.is_forbidden() => {
                tracing::info!(task_id, "PR feedback sync: bounce skipped — ticket moved by a human")
            }
            Err(e) => tracing::warn!(task_id, error = %e, "PR feedback sync: bounce failed"),
        }
    }

    /// Atomic write (tmp + rename); a failure only means the next pass may
    /// re-see items — logged, never fatal.
    fn persist(&self, task_id: i32) {
        if let Err(e) = save_state(&self.state_path, &self.seen) {
            tracing::warn!(task_id, error = %e, "PR feedback sync: could not persist state");
        }
    }
}

pub struct PrMergeableSync {
    workspace_root: PathBuf,
    state_path: PathBuf,
    /// `pr_url` → `base_sha` for which a rebase bounce has already been sent.
    seen: HashMap<String, String>,
}

impl PrMergeableSync {
    /// Loads the dedup state from `<workspace_root>/pr-mergeable-sync.json`;
    /// a missing or corrupt file starts empty.
    pub fn new(workspace_root: &Path) -> Self {
        let workspace_root = workspace_root.to_path_buf();
        let state_path = workspace_root.join(MERGEABLE_STATE_FILE);
        let seen = load_state(&state_path);
        Self {
            workspace_root,
            state_path,
            seen,
        }
    }

    /// One sync pass over every ticket of this agent sitting in `review`.
    /// Logs and continues on every failure — never errors upward. The cadence
    /// is shared with `PrFeedbackSync` (`pr_sync_interval_secs`, §5.1).
    pub async fn sync_once(&mut self, client: &RemoterClient, _config: &Config) {
        let tasks = match client.my_tasks(Some("review")).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "PR mergeability sync: could not list review tasks");
                return;
            }
        };
        if tasks.is_empty() {
            return;
        }
        let projects = match client.projects().await {
            Ok(ps) => ps,
            Err(e) => {
                tracing::warn!(error = %e, "PR mergeability sync: could not list projects");
                return;
            }
        };
        let repo_urls: HashMap<i32, String> = projects
            .iter()
            .filter_map(|p| p.repo_url.clone().map(|u| (p.id, u)))
            .collect();
        let base_branches: HashMap<i32, Option<String>> =
            projects.iter().map(|p| (p.id, p.base_branch.clone())).collect();
        for task in tasks {
            self.sync_task(client, &repo_urls, &base_branches, task.id, task.project_id)
                .await;
        }
    }

    async fn sync_task(
        &mut self,
        client: &RemoterClient,
        repo_urls: &HashMap<i32, String>,
        base_branches: &HashMap<i32, Option<String>>,
        task_id: i32,
        project_id: i32,
    ) {
        let runs = match client.list_runs(task_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "PR mergeability sync: could not list runs");
                return;
            }
        };
        let Some(pr_url) = runs.iter().find_map(|r| r.pr_url.clone()) else {
            return;
        };
        let pr_number = match forge::pr_number_from_url(&pr_url) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(task_id, pr_url, error = %e, "PR mergeability sync: unparseable PR URL");
                return;
            }
        };
        let Some(repo_url) = repo_urls.get(&project_id) else {
            return;
        };
        let cfg = match client.forge_config(project_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "PR mergeability sync: could not fetch forge config");
                return;
            }
        };
        let (Some(kind), Some(token)) = (cfg.forge_kind.as_deref(), cfg.forge_token.as_deref()) else {
            return;
        };
        let kind = match forge::ForgeKind::from_token(kind) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "PR mergeability sync: unknown forge kind");
                return;
            }
        };

        let state = match forge::pr_mergeable(
            client.http(),
            kind,
            cfg.forge_api_url.as_deref(),
            token,
            repo_url,
            pr_number,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(task_id, pr_url, error = %e, "PR mergeability sync: forge API call failed");
                return;
            }
        };

        match state.mergeable {
            Mergeable::Yes => {
                if self.seen.remove(&pr_url).is_some() {
                    self.persist(task_id);
                }
                tracing::info!(task_id, pr_url, "PR is mergeable");
            }
            Mergeable::No => {
                let Some(sha) = state.base_sha else {
                    tracing::warn!(task_id, pr_url, "PR mergeability sync: no base_sha in forge response");
                    return;
                };
                if self.seen.get(&pr_url) == Some(&sha) {
                    tracing::info!(
                        task_id,
                        pr_url,
                        "PR mergeability sync: already bounced for base_sha {sha}"
                    );
                    return;
                }

                let base = match resolve_base_branch(&self.workspace_root, project_id, base_branches).await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(task_id, error = %e, "PR mergeability sync: could not resolve base branch");
                        return;
                    }
                };

                // Persist BEFORE posting: losing the comment is better than a
                // bounce loop if the status write fails (PROD-9).
                self.seen.insert(pr_url.clone(), sha.clone());
                self.persist(task_id);

                let body = format!(
                    "**PR mergeability**: the PR conflicts with `{base}`. Rebase onto `origin/{base}`, resolve the conflicts, run the project checks (`just lint` / `just test`), commit everything and leave the worktree clean — do not push, the daemon pushes after the run."
                );
                if let Err(e) = client.add_comment(task_id, &body).await {
                    tracing::warn!(task_id, error = %e, "PR mergeability sync: could not post comment");
                    return;
                }
                tracing::info!(
                    task_id,
                    pr_url,
                    base_sha = sha,
                    "PR conflicts with base; bouncing → implement"
                );
                match client.set_task_status(task_id, "implement").await {
                    Ok(()) => {}
                    Err(e) if e.is_conflict() || e.is_forbidden() => {
                        tracing::info!(
                            task_id,
                            "PR mergeability sync: bounce skipped — ticket moved by a human"
                        )
                    }
                    Err(e) => tracing::warn!(task_id, error = %e, "PR mergeability sync: bounce failed"),
                }
            }
            Mergeable::Unknown => {
                tracing::info!(task_id, pr_url, "PR mergeability sync: state still unknown");
            }
        }
    }

    fn persist(&self, task_id: i32) {
        if let Err(e) = save_state(&self.state_path, &self.seen) {
            tracing::warn!(task_id, error = %e, "PR mergeability sync: could not persist state");
        }
    }
}

async fn resolve_base_branch(
    workspace_root: &Path,
    project_id: i32,
    base_branches: &HashMap<i32, Option<String>>,
) -> Result<String, String> {
    if let Some(Some(b)) = base_branches.get(&project_id) {
        return Ok(b.clone());
    }
    let repo = workspace::repo_dir(workspace_root, project_id);
    workspace::remote_default_branch(&repo)
        .await
        .map_err(|e| format!("remote default branch: {e}"))
}

fn load_state<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    let Ok(text) = std::fs::read_to_string(path) else {
        return T::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!(path = %path.display(), error = %e, "sync state file corrupt; starting empty");
        T::default()
    })
}

fn save_state<T: Serialize>(path: &Path, state: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(state).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dedup state survives a reload; a missing or corrupt file starts empty.
    #[test]
    fn feedback_state_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("remoter-sync-state-{}-fb", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut sync = PrFeedbackSync::new(&dir);
        assert!(sync.seen.is_empty(), "missing file → empty state");
        sync.seen
            .entry("https://forge.example/o/r/pull/7".to_string())
            .or_default()
            .insert("issue:101".to_string());
        sync.persist(0);

        let sync2 = PrFeedbackSync::new(&dir);
        assert!(
            sync2
                .seen
                .get("https://forge.example/o/r/pull/7")
                .is_some_and(|s| s.contains("issue:101")),
            "reloaded state keeps the seen ids"
        );

        std::fs::write(dir.join(FEEDBACK_STATE_FILE), "{ not json").unwrap();
        let sync3 = PrFeedbackSync::new(&dir);
        assert!(sync3.seen.is_empty(), "corrupt file → empty state");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mergeable dedup state survives a reload; a missing or corrupt file
    /// starts empty.
    #[test]
    fn mergeable_state_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("remoter-sync-state-{}-mb", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut sync = PrMergeableSync::new(&dir);
        assert!(sync.seen.is_empty(), "missing file → empty state");
        sync.seen
            .insert("https://forge.example/o/r/pull/7".to_string(), "abc123".to_string());
        sync.persist(0);

        let sync2 = PrMergeableSync::new(&dir);
        assert_eq!(
            sync2.seen.get("https://forge.example/o/r/pull/7"),
            Some(&"abc123".to_string()),
            "reloaded state keeps the base sha"
        );

        std::fs::write(dir.join(MERGEABLE_STATE_FILE), "{ not json").unwrap();
        let sync3 = PrMergeableSync::new(&dir);
        assert!(sync3.seen.is_empty(), "corrupt file → empty state");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
