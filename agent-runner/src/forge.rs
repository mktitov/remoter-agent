//! Forge integration (spec §4.4): after a successful implement run the daemon
//! pushes the ticket branch to origin and opens a draft PR/MR via the project's
//! forge config, then syncs human PR/MR feedback back into the ticket thread
//! (PROD-8). The URL parsing helpers are pure (unit-tested); `create_pr` and
//! `list_pr_feedback` are thin reqwest calls pointed at the project's
//! `forge_api_url` (or the forge's default), so tests aim them at a local stub
//! server.

use serde::Deserialize;

/// The supported forges — mirrors the backend's `forge_kind` JSON tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

impl ForgeKind {
    /// Parses the backend's `forgeKind` token (`"github"` / `"gitlab"`).
    pub fn from_token(token: &str) -> Result<Self, ForgeError> {
        match token {
            "github" => Ok(Self::GitHub),
            "gitlab" => Ok(Self::GitLab),
            other => Err(ForgeError::Parse(format!("unknown forge kind: {other}"))),
        }
    }

    /// The default API base when the project has no `forge_api_url`.
    fn default_api_base(self) -> &'static str {
        match self {
            Self::GitHub => "https://api.github.com",
            Self::GitLab => "https://gitlab.com/api/v4",
        }
    }
}

/// A push/PR failure. `Display` is a single line — it lands verbatim in the
/// ticket thread note (`push/PR failed: <error>`).
#[derive(Debug)]
pub enum ForgeError {
    /// The repo URL doesn't look like this forge's repo addresses.
    Parse(String),
    /// The forge API call failed (transport or non-2xx).
    Api(String),
    /// A 2xx response without the expected PR URL field.
    UnexpectedResponse(String),
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(m) | Self::Api(m) | Self::UnexpectedResponse(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ForgeError {}

/// `owner/repo` from a GitHub repo URL: `https://github.com/o/r[.git]`,
/// `git@github.com:o/r.git`, `ssh://git@github.com/o/r.git`.
pub fn github_owner_repo(repo_url: &str) -> Result<(String, String), ForgeError> {
    let (host, path) = host_and_path(repo_url)?;
    if host != "github.com" {
        return Err(ForgeError::Parse(format!("not a github.com URL: {repo_url}")));
    }
    let parts = path_segments(&path);
    let [owner, repo] = parts.as_slice() else {
        return Err(ForgeError::Parse(format!("cannot extract owner/repo from {repo_url}")));
    };
    Ok((owner.to_string(), repo.to_string()))
}

/// The URL-encoded project path (`group%2Fsub%2Frepo`) from a GitLab repo URL
/// — gitlab.com or any self-hosted host, in the https/ssh/scp-like forms.
pub fn gitlab_project_path(repo_url: &str) -> Result<String, ForgeError> {
    let (_host, path) = host_and_path(repo_url)?;
    let parts = path_segments(&path);
    if parts.len() < 2 {
        return Err(ForgeError::Parse(format!(
            "cannot extract project path from {repo_url}"
        )));
    }
    Ok(percent_encode(&parts.join("/")))
}

/// Splits a repo URL into (host, path). Handles `https://`/`http://`,
/// `ssh://[user@]host[:port]/path`, and the scp-like `user@host:path` form.
fn host_and_path(repo_url: &str) -> Result<(String, String), ForgeError> {
    let u = repo_url.trim();
    let err = || ForgeError::Parse(format!("unrecognized repo URL: {repo_url}"));
    if let Some(rest) = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) {
        let (host, path) = rest.split_once('/').ok_or_else(err)?;
        Ok((host.to_string(), path.to_string()))
    } else if let Some(rest) = u.strip_prefix("ssh://") {
        let rest = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
        let (host, path) = rest.split_once('/').ok_or_else(err)?;
        let host = host.split(':').next().unwrap_or(host); // drop :port
        Ok((host.to_string(), path.to_string()))
    } else if let Some((user_host, path)) = u.split_once(':') {
        // scp-like: git@host:path
        let (_, host) = user_host.rsplit_once('@').ok_or_else(err)?;
        Ok((host.to_string(), path.to_string()))
    } else {
        Err(err())
    }
}

/// Strips `.git`/slashes and splits the path into non-empty segments.
fn path_segments(path: &str) -> Vec<&str> {
    let p = path.trim_end_matches('/');
    let p = p.strip_suffix(".git").unwrap_or(p);
    p.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect()
}

/// Percent-encodes everything outside RFC 3986 unreserved — the GitLab API
/// wants the project path's `/` as `%2F`.
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// What to open: the source branch, its target, the PR title, and the PR body.
/// Both strings are clamped to the forge limits inside `create_pr` — callers
/// pass the task title as `title` and may pass the raw ticket description as
/// `body`. The PR/MR title is prefixed with `task-<task_id>:` and the body
/// with a `Task: task-<task_id>` line.
pub struct DraftPr<'a> {
    pub branch: &'a str,
    pub base: &'a str,
    /// The daemon's task id — surfaces in the title prefix (and body header)
    /// so a PR/MR is traceable back to its ticket, same as the branch name
    /// `agent/task-<id>-<slug>`.
    pub task_id: i32,
    pub title: &'a str,
    pub body: &'a str,
}

/// Forge title limits: GitHub 256 chars, GitLab 255 (and we prepend
/// "Draft: " there). A single conservative cap covers both forges.
const MAX_PR_TITLE_CHARS: usize = 240;

/// Cap for the PR/MR body — far below the forges' real limits (GitHub
/// 65536, GitLab ~1MB), generous for a ticket description.
const MAX_PR_BODY_CHARS: usize = 4096;

/// Derives a single-line PR/MR title from the task title: the first non-empty
/// line, whitespace collapsed, prefixed with `task-<id>:` (the same id the
/// branch name carries), char-safe truncation with an ellipsis — the clamp
/// budget shrinks by the prefix so the total stays within the forge limits.
/// Falls back to a placeholder for an empty title.
fn pr_title(task_id: i32, title: &str) -> String {
    let line = title
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let prefix = format!("task-{task_id}: ");
    let line = clamp_chars(&line, MAX_PR_TITLE_CHARS - prefix.chars().count());
    if line.is_empty() {
        return format!("{prefix}remoter ticket");
    }
    format!("{prefix}{line}")
}

/// Char-safe truncation to `max` chars, appending `…` when anything was cut.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// GitHub 403s API calls without a User-Agent ("Request forbidden by
/// administrative rules"); reqwest sends none by default. GitLab doesn't
/// require one, but self-hosted proxies may — send it on both.
const USER_AGENT: &str = "remoter-agent";

/// Opens a draft PR/MR for `pr.branch` (targeting `pr.base`) and returns its
/// web URL (spec §4.4): GitHub `POST {api}/repos/{o}/{r}/pulls` (draft, bearer
/// auth), GitLab `POST {api}/projects/{path}/merge_requests` ("Draft: " title,
/// PRIVATE-TOKEN auth). `api_base` falls back to the forge's default. The
/// title is `task-<id>: ` + the task title clamped to the forge limit; the
/// body is a `Task: task-<id>` header line followed by the full description,
/// clamped to `MAX_PR_BODY_CHARS`.
pub async fn create_pr(
    http: &reqwest::Client,
    kind: ForgeKind,
    api_base: Option<&str>,
    token: &str,
    repo_url: &str,
    pr: &DraftPr<'_>,
) -> Result<String, ForgeError> {
    let api_base = api_base
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| kind.default_api_base())
        .trim_end_matches('/');
    let title = pr_title(pr.task_id, pr.title);
    let body = clamp_chars(&format!("Task: task-{}\n\n{}", pr.task_id, pr.body), MAX_PR_BODY_CHARS);
    match kind {
        ForgeKind::GitHub => {
            let (owner, repo) = github_owner_repo(repo_url)?;
            let resp = http
                .post(format!("{api_base}/repos/{owner}/{repo}/pulls"))
                .bearer_auth(token)
                .header("User-Agent", USER_AGENT)
                .header("Accept", "application/vnd.github+json")
                .json(&serde_json::json!({
                    "title": title,
                    "body": body,
                    "head": pr.branch,
                    "base": pr.base,
                    "draft": true,
                }))
                .send()
                .await
                .map_err(|e| ForgeError::Api(format!("POST pulls: {e}")))?;
            decode::<PrResponse>(resp)
                .await?
                .html_url
                .ok_or_else(|| ForgeError::UnexpectedResponse("pulls response has no html_url".to_string()))
        }
        ForgeKind::GitLab => {
            let path = gitlab_project_path(repo_url)?;
            let resp = http
                .post(format!("{api_base}/projects/{path}/merge_requests"))
                .header("PRIVATE-TOKEN", token)
                .header("User-Agent", USER_AGENT)
                .json(&serde_json::json!({
                    "source_branch": pr.branch,
                    "target_branch": pr.base,
                    "title": format!("Draft: {title}"),
                    "description": body,
                }))
                .send()
                .await
                .map_err(|e| ForgeError::Api(format!("POST merge_requests: {e}")))?;
            decode::<PrResponse>(resp)
                .await?
                .web_url
                .ok_or_else(|| ForgeError::UnexpectedResponse("merge_requests response has no web_url".to_string()))
        }
    }
}

/// The one field each forge's create response carries the PR URL in.
#[derive(Deserialize)]
struct PrResponse {
    html_url: Option<String>,
    web_url: Option<String>,
}

/// Replaces the body of an existing PR/MR (the cross-project integration
/// PR's body is refreshed on every accepted child, docs/specs/
/// cross-repo-projects.md): GitHub `PATCH {api}/repos/{o}/{r}/pulls/{n}`,
/// GitLab `PUT {api}/projects/{path}/merge_requests/{n}` — only the
/// body/description field is sent; GitLab's draft state lives in the title,
/// which is deliberately left untouched. Same URL/auth/header conventions as
/// [`create_pr`]. `body` must be the complete rendered body, including any
/// header line [`create_pr`] would have added — it is clamped to the same
/// limit.
pub async fn update_pr_body(
    http: &reqwest::Client,
    kind: ForgeKind,
    api_base: Option<&str>,
    token: &str,
    repo_url: &str,
    pr_number: u64,
    body: &str,
) -> Result<(), ForgeError> {
    let api_base = api_base
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| kind.default_api_base())
        .trim_end_matches('/');
    let body = clamp_chars(body, MAX_PR_BODY_CHARS);
    match kind {
        ForgeKind::GitHub => {
            let (owner, repo) = github_owner_repo(repo_url)?;
            let resp = http
                .patch(format!("{api_base}/repos/{owner}/{repo}/pulls/{pr_number}"))
                .bearer_auth(token)
                .header("User-Agent", USER_AGENT)
                .header("Accept", "application/vnd.github+json")
                .json(&serde_json::json!({ "body": body }))
                .send()
                .await
                .map_err(|e| ForgeError::Api(format!("PATCH pulls/{pr_number}: {e}")))?;
            decode::<serde_json::Value>(resp).await?;
        }
        ForgeKind::GitLab => {
            let path = gitlab_project_path(repo_url)?;
            let resp = http
                .put(format!("{api_base}/projects/{path}/merge_requests/{pr_number}"))
                .header("PRIVATE-TOKEN", token)
                .header("User-Agent", USER_AGENT)
                .json(&serde_json::json!({ "description": body }))
                .send()
                .await
                .map_err(|e| ForgeError::Api(format!("PUT merge_requests/{pr_number}: {e}")))?;
            decode::<serde_json::Value>(resp).await?;
        }
    }
    Ok(())
}

async fn decode<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, ForgeError> {
    let status = resp.status();
    if status.is_success() {
        resp.json()
            .await
            .map_err(|e| ForgeError::UnexpectedResponse(format!("response decode: {e}")))
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(ForgeError::Api(format!("HTTP {status}: {}", one_line(&body))))
    }
}

/// Collapses a forge error body to a single bounded line for the thread note.
fn one_line(body: &str) -> String {
    let line: String = body.chars().take(200).collect();
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── PR feedback (PROD-8, spec §4.4) ─────────────────────────────────────────

/// One piece of human feedback on the draft PR/MR: a GitHub issue comment,
/// review, or inline review comment, or a GitLab MR note. Read-side only —
/// the daemon never writes to the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFeedback {
    /// Source-scoped id for dedup: `issue:<id>` / `review:<id>` / `inline:<id>`
    /// (GitHub), `note:<id>` (GitLab).
    pub id: String,
    pub author: String,
    pub body: String,
    /// `path:line` for inline review comments / positioned notes, `None`
    /// otherwise.
    pub context: Option<String>,
}

/// The PR/MR number from its web URL: GitHub `…/pull/<n>`, GitLab
/// `…/merge_requests/<n>` (any host — github.com/GHE/gitlab.com/self-hosted).
pub fn pr_number_from_url(pr_url: &str) -> Result<u64, ForgeError> {
    for marker in ["/pull/", "/merge_requests/"] {
        if let Some(pos) = pr_url.find(marker) {
            let digits: String = pr_url[pos + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse::<u64>() {
                return Ok(n);
            }
        }
    }
    Err(ForgeError::Parse(format!("cannot extract PR/MR number from {pr_url}")))
}

/// Whether a PR/MR web URL belongs to the given repo — the create-or-reuse
/// guard in `push_and_create_pr` uses it to ignore a PR URL recorded by a
/// cross-project supervise run (that URL points at the CHILD repo's
/// integration PR, not at a PR in this project's repo). Compares host and
/// path segments: GitHub `https://host/owner/repo/pull/N` and GitLab
/// `https://host/group/proj/-/merge_requests/N` both embed the repo path
/// before the PR marker. Returns `false` on any parse failure — a URL we
/// can't attribute is never safe to reuse.
pub fn pr_url_matches_repo(pr_url: &str, repo_url: &str) -> bool {
    let (Ok((pr_host, pr_path)), Ok((repo_host, repo_path))) = (host_and_path(pr_url), host_and_path(repo_url)) else {
        return false;
    };
    if pr_host != repo_host {
        return false;
    }
    let pr_segs = path_segments(&pr_path);
    let repo_segs = path_segments(&repo_path);
    // The PR URL's path is `<repo path>/pull/<n>` or
    // `<repo path>/-/merge_requests/<n>` — the repo path is a strict prefix.
    pr_segs.len() > repo_segs.len() && pr_segs[..repo_segs.len()] == repo_segs[..]
}

// ── PR mergeability (PROD-9) ────────────────────────────────────────────────

/// Whether the forge considers the PR/MR mergeable into its target branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Yes,
    No,
    /// The forge has not finished computing the status, or the status is not
    /// about conflicts (e.g. GitLab `checking`, `ci_still_running`).
    Unknown,
}

/// The mergeability state of one PR/MR.
#[derive(Debug, Clone)]
pub struct PrMergeState {
    pub mergeable: Mergeable,
    pub base_sha: Option<String>,
}

const MERGEABLE_POLL_ATTEMPTS: u32 = 5;
const MERGEABLE_POLL_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

/// Reads the PR/MR mergeability state from the forge API. A single GET, no
/// retries — callers that run inside the daemon poll loop should rely on the
/// loop's cadence instead of sleeping.
pub async fn pr_mergeable(
    http: &reqwest::Client,
    kind: ForgeKind,
    api_base: Option<&str>,
    token: &str,
    repo_url: &str,
    pr_number: u64,
) -> Result<PrMergeState, ForgeError> {
    let api_base = api_base
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| kind.default_api_base())
        .trim_end_matches('/');
    match kind {
        ForgeKind::GitHub => {
            let (owner, repo) = github_owner_repo(repo_url)?;
            let resp = http
                .get(format!("{api_base}/repos/{owner}/{repo}/pulls/{pr_number}"))
                .bearer_auth(token)
                .header("User-Agent", USER_AGENT)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|e| ForgeError::Api(format!("GET pulls/{pr_number}: {e}")))?;
            let pr: GhPull = decode(resp).await?;
            let mergeable = match pr.mergeable {
                Some(true) => Mergeable::Yes,
                Some(false) => Mergeable::No,
                None => Mergeable::Unknown,
            };
            Ok(PrMergeState {
                mergeable,
                base_sha: pr.base.sha,
            })
        }
        ForgeKind::GitLab => {
            let path = gitlab_project_path(repo_url)?;
            let resp = http
                .get(format!("{api_base}/projects/{path}/merge_requests/{pr_number}"))
                .header("PRIVATE-TOKEN", token)
                .header("User-Agent", USER_AGENT)
                .send()
                .await
                .map_err(|e| ForgeError::Api(format!("GET merge_requests/{pr_number}: {e}")))?;
            let mr: GlMergeRequest = decode(resp).await?;
            let status = mr
                .detailed_merge_status
                .as_deref()
                .or(mr.merge_status.as_deref())
                .unwrap_or("unchecked");
            let mergeable = match status {
                "mergeable" | "can_be_merged" => Mergeable::Yes,
                "conflict" | "cannot_be_merged" => Mergeable::No,
                _ => Mergeable::Unknown,
            };
            Ok(PrMergeState {
                mergeable,
                base_sha: mr.diff_refs.and_then(|r| r.base_sha),
            })
        }
    }
}

/// Polls `pr_mergeable` until the forge has resolved to Yes/No or an error
/// occurs. Returns `Unknown` only when every poll came back still computing
/// (GitHub `mergeable: null`) — callers should treat `Unknown` as "not a
/// confirmed conflict right now".
pub async fn wait_for_pr_mergeable(
    http: &reqwest::Client,
    kind: ForgeKind,
    api_base: Option<&str>,
    token: &str,
    repo_url: &str,
    pr_number: u64,
) -> Result<PrMergeState, ForgeError> {
    for attempt in 1..=MERGEABLE_POLL_ATTEMPTS {
        let state = pr_mergeable(http, kind, api_base, token, repo_url, pr_number).await?;
        if matches!(state.mergeable, Mergeable::Yes | Mergeable::No) {
            return Ok(state);
        }
        if attempt < MERGEABLE_POLL_ATTEMPTS {
            tokio::time::sleep(MERGEABLE_POLL_DELAY).await;
        }
    }
    Ok(PrMergeState {
        mergeable: Mergeable::Unknown,
        base_sha: None,
    })
}

#[derive(Deserialize)]
struct GhBase {
    sha: Option<String>,
}

#[derive(Deserialize)]
struct GhPull {
    mergeable: Option<bool>,
    base: GhBase,
}

#[derive(Deserialize)]
struct GlDiffRefs {
    base_sha: Option<String>,
}

#[derive(Deserialize)]
struct GlMergeRequest {
    #[serde(rename = "detailed_merge_status")]
    detailed_merge_status: Option<String>,
    #[serde(rename = "merge_status")]
    merge_status: Option<String>,
    diff_refs: Option<GlDiffRefs>,
}

/// Fetches all human feedback on the PR/MR (spec §4.4, PROD-8). GitHub: the
/// issue comments, inline review comments, and reviews (3 GETs). GitLab: the
/// MR notes, system notes excluded. Lists come back oldest-first per source.
pub async fn list_pr_feedback(
    http: &reqwest::Client,
    kind: ForgeKind,
    api_base: Option<&str>,
    token: &str,
    repo_url: &str,
    pr_number: u64,
) -> Result<Vec<PrFeedback>, ForgeError> {
    let api_base = api_base
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| kind.default_api_base())
        .trim_end_matches('/');
    match kind {
        ForgeKind::GitHub => {
            let (owner, repo) = github_owner_repo(repo_url)?;
            let mut out = Vec::new();
            // Issue (conversation) comments.
            for c in get_json::<Vec<GhComment>>(
                http,
                kind,
                &format!("{api_base}/repos/{owner}/{repo}/issues/{pr_number}/comments"),
                token,
            )
            .await?
            {
                out.push(PrFeedback {
                    id: format!("issue:{}", c.id),
                    author: c.user.login,
                    body: c.body,
                    context: None,
                });
            }
            // Inline review comments (path:line context).
            for c in get_json::<Vec<GhInlineComment>>(
                http,
                kind,
                &format!("{api_base}/repos/{owner}/{repo}/pulls/{pr_number}/comments"),
                token,
            )
            .await?
            {
                let context = match c.line {
                    Some(line) => Some(format!("{}:{line}", c.path)),
                    None => Some(c.path.clone()),
                };
                out.push(PrFeedback {
                    id: format!("inline:{}", c.id),
                    author: c.user.login,
                    body: c.body,
                    context,
                });
            }
            // Reviews: empty bodies carry no text (e.g. a bare approval), and a
            // non-COMMENTED state is part of the message ("[CHANGES_REQUESTED] …").
            for r in get_json::<Vec<GhReview>>(
                http,
                kind,
                &format!("{api_base}/repos/{owner}/{repo}/pulls/{pr_number}/reviews"),
                token,
            )
            .await?
            {
                if r.body.trim().is_empty() {
                    continue;
                }
                let body = match r.state.as_str() {
                    "COMMENTED" => r.body,
                    state => format!("[{state}] {}", r.body),
                };
                out.push(PrFeedback {
                    id: format!("review:{}", r.id),
                    author: r.user.login,
                    body,
                    context: None,
                });
            }
            Ok(out)
        }
        ForgeKind::GitLab => {
            let path = gitlab_project_path(repo_url)?;
            let notes = get_json::<Vec<GlNote>>(
                http,
                kind,
                &format!("{api_base}/projects/{path}/merge_requests/{pr_number}/notes?sort=asc&order_by=created_at"),
                token,
            )
            .await?;
            Ok(notes
                .into_iter()
                .filter(|n| !n.system)
                .map(|n| {
                    let context = n.position.and_then(|p| match (p.new_path, p.new_line) {
                        (Some(path), Some(line)) => Some(format!("{path}:{line}")),
                        (Some(path), None) => Some(path),
                        _ => None,
                    });
                    PrFeedback {
                        id: format!("note:{}", n.id),
                        author: n.author.username,
                        body: n.body,
                        context,
                    }
                })
                .collect())
        }
    }
}

/// One authenticated GET against the forge API — same auth style and
/// User-Agent as `create_pr` (GitHub bearer, GitLab PRIVATE-TOKEN).
async fn get_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    kind: ForgeKind,
    url: &str,
    token: &str,
) -> Result<T, ForgeError> {
    let req = http.get(url).header("User-Agent", USER_AGENT);
    let req = match kind {
        ForgeKind::GitHub => req.bearer_auth(token).header("Accept", "application/vnd.github+json"),
        ForgeKind::GitLab => req.header("PRIVATE-TOKEN", token),
    };
    let resp = req
        .send()
        .await
        .map_err(|e| ForgeError::Api(format!("GET feedback: {e}")))?;
    decode(resp).await
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct GhComment {
    id: u64,
    user: GhUser,
    body: String,
}

#[derive(Deserialize)]
struct GhInlineComment {
    id: u64,
    user: GhUser,
    body: String,
    path: String,
    line: Option<u64>,
}

#[derive(Deserialize)]
struct GhReview {
    id: u64,
    user: GhUser,
    body: String,
    state: String,
}

#[derive(Deserialize)]
struct GlAuthor {
    username: String,
}

#[derive(Deserialize)]
struct GlNotePosition {
    new_path: Option<String>,
    new_line: Option<u64>,
}

#[derive(Deserialize)]
struct GlNote {
    id: u64,
    author: GlAuthor,
    body: String,
    #[serde(default)]
    system: bool,
    position: Option<GlNotePosition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_https_forms() {
        assert_eq!(
            github_owner_repo("https://github.com/o/r").unwrap(),
            ("o".to_string(), "r".to_string())
        );
        assert_eq!(
            github_owner_repo("https://github.com/o/r.git").unwrap(),
            ("o".to_string(), "r".to_string())
        );
        assert_eq!(
            github_owner_repo("https://github.com/o/r/").unwrap(),
            ("o".to_string(), "r".to_string())
        );
    }

    #[test]
    fn github_ssh_forms() {
        assert_eq!(
            github_owner_repo("git@github.com:o/r.git").unwrap(),
            ("o".to_string(), "r".to_string())
        );
        assert_eq!(
            github_owner_repo("git@github.com:o/r").unwrap(),
            ("o".to_string(), "r".to_string())
        );
        assert_eq!(
            github_owner_repo("ssh://git@github.com/o/r.git").unwrap(),
            ("o".to_string(), "r".to_string())
        );
        assert_eq!(
            github_owner_repo("ssh://git@github.com:2222/o/r.git").unwrap(),
            ("o".to_string(), "r".to_string())
        );
    }

    #[test]
    fn github_rejects_other_hosts_and_shapes() {
        assert!(github_owner_repo("https://gitlab.com/o/r.git").is_err());
        assert!(github_owner_repo("https://github.com/o").is_err());
        assert!(github_owner_repo("https://github.com/o/r/extra").is_err());
        assert!(github_owner_repo("/tmp/local-repo").is_err());
        assert!(github_owner_repo("not a url at all").is_err());
        assert!(github_owner_repo("").is_err());
    }

    #[test]
    fn gitlab_https_forms() {
        assert_eq!(
            gitlab_project_path("https://gitlab.com/group/repo").unwrap(),
            "group%2Frepo"
        );
        assert_eq!(
            gitlab_project_path("https://gitlab.com/group/sub/repo.git").unwrap(),
            "group%2Fsub%2Frepo"
        );
        // Self-hosted host.
        assert_eq!(
            gitlab_project_path("https://gitlab.example.com/grp/proj.git").unwrap(),
            "grp%2Fproj"
        );
    }

    #[test]
    fn gitlab_ssh_forms() {
        assert_eq!(
            gitlab_project_path("git@gitlab.com:group/repo.git").unwrap(),
            "group%2Frepo"
        );
        assert_eq!(
            gitlab_project_path("git@gitlab.example.com:group/sub/repo.git").unwrap(),
            "group%2Fsub%2Frepo"
        );
        assert_eq!(
            gitlab_project_path("ssh://git@gitlab.example.com/group/repo.git").unwrap(),
            "group%2Frepo"
        );
        assert_eq!(
            gitlab_project_path("ssh://git@gitlab.example.com:2222/group/repo.git").unwrap(),
            "group%2Frepo"
        );
    }

    #[test]
    fn gitlab_rejects_junk() {
        assert!(gitlab_project_path("https://gitlab.com/onlyone").is_err());
        assert!(gitlab_project_path("/tmp/local-repo").is_err());
        assert!(gitlab_project_path("git@gitlab.com:").is_err());
        assert!(gitlab_project_path("").is_err());
    }

    #[test]
    fn forge_kind_tokens() {
        assert_eq!(ForgeKind::from_token("github").unwrap(), ForgeKind::GitHub);
        assert_eq!(ForgeKind::from_token("gitlab").unwrap(), ForgeKind::GitLab);
        assert!(ForgeKind::from_token("bitbucket").is_err());
    }

    /// Short single-line titles pass through untouched, prefixed with the
    /// task id.
    #[test]
    fn pr_title_short_title() {
        assert_eq!(pr_title(42, "Fix the bug"), "task-42: Fix the bug");
    }

    /// Multi-line input yields its first non-empty line, collapsed.
    #[test]
    fn pr_title_first_line_only() {
        assert_eq!(
            pr_title(42, "\n  Fix  the\tbug  \nmore context\n"),
            "task-42: Fix the bug"
        );
        assert_eq!(pr_title(42, "Fix the bug\nmore context"), "task-42: Fix the bug");
    }

    /// Long titles are char-safe truncated with an ellipsis so the prefixed
    /// total stays within the cap — and within the GitLab limit once the
    /// "Draft: " prefix is added.
    #[test]
    fn pr_title_truncates_to_forge_limit() {
        let long: String = "ё".repeat(500);
        let title = pr_title(42, &long);
        assert_eq!(title.chars().count(), MAX_PR_TITLE_CHARS);
        assert!(title.starts_with("task-42: "));
        assert!(title.ends_with('…'));
        assert!("Draft: ".chars().count() + title.chars().count() <= 255);
    }

    /// An empty title still yields a valid (non-empty) PR title.
    #[test]
    fn pr_title_empty_title_fallback() {
        assert_eq!(pr_title(42, ""), "task-42: remoter ticket");
        assert_eq!(pr_title(42, "  \n\n  "), "task-42: remoter ticket");
    }

    /// Bodies at or under the cap pass through; longer ones are char-safe
    /// truncated to the cap with an ellipsis.
    #[test]
    fn body_clamp() {
        let short = "line one\nline two";
        assert_eq!(clamp_chars(short, MAX_PR_BODY_CHARS), short);

        let long: String = "я".repeat(MAX_PR_BODY_CHARS + 10);
        let clamped = clamp_chars(&long, MAX_PR_BODY_CHARS);
        assert_eq!(clamped.chars().count(), MAX_PR_BODY_CHARS);
        assert!(clamped.ends_with('…'));
    }

    #[test]
    fn pr_number_from_all_pr_url_forms() {
        // github.com + GHE.
        assert_eq!(pr_number_from_url("https://github.com/o/r/pull/123").unwrap(), 123);
        assert_eq!(pr_number_from_url("https://ghe.acme.io/o/r/pull/7").unwrap(), 7);
        // gitlab.com + self-hosted (subgroups, trailing slash).
        assert_eq!(
            pr_number_from_url("https://gitlab.com/grp/sub/proj/-/merge_requests/3").unwrap(),
            3
        );
        assert_eq!(
            pr_number_from_url("https://gitlab.example.com/grp/proj/-/merge_requests/42/").unwrap(),
            42
        );
    }

    #[test]
    fn pr_number_rejects_non_pr_urls() {
        assert!(pr_number_from_url("https://github.com/o/r").is_err());
        assert!(pr_number_from_url("https://github.com/o/r/pull/").is_err());
        assert!(pr_number_from_url("https://github.com/o/r/pull/abc").is_err());
        assert!(pr_number_from_url("https://gitlab.com/o/r/-/issues/5").is_err());
        assert!(pr_number_from_url("not a url").is_err());
        assert!(pr_number_from_url("").is_err());
    }

    #[test]
    fn pr_url_matches_repo_same_repo() {
        // GitHub.
        assert!(pr_url_matches_repo(
            "https://github.com/o/r/pull/18",
            "git@github.com:o/r.git"
        ));
        // GitLab with subgroups.
        assert!(pr_url_matches_repo(
            "https://gitlab.com/grp/sub/proj/-/merge_requests/3",
            "https://gitlab.com/grp/sub/proj.git"
        ));
        // GHE / self-hosted host match.
        assert!(pr_url_matches_repo(
            "https://ghe.acme.io/o/r/pull/7",
            "ssh://git@ghe.acme.io/o/r.git"
        ));
    }

    #[test]
    fn pr_url_matches_repo_rejects_other_repo() {
        // The #220 bug: a cross-project supervise run records the CHILD repo's
        // integration PR on the parent's run history; the parent's own
        // implement run must not reuse it.
        assert!(!pr_url_matches_repo(
            "https://github.com/mktitov/remoter-agent/pull/18",
            "git@github.com:mktitov/remoter.git"
        ));
        // Different host.
        assert!(!pr_url_matches_repo(
            "https://gitlab.com/o/r/-/merge_requests/1",
            "git@github.com:o/r.git"
        ));
        // Repo path is a prefix of a different repo's path (r vs r-extra).
        assert!(!pr_url_matches_repo(
            "https://github.com/o/r-extra/pull/1",
            "git@github.com:o/r.git"
        ));
        // Unparseable URL is never reused.
        assert!(!pr_url_matches_repo("not a url", "git@github.com:o/r.git"));
        assert!(!pr_url_matches_repo("https://github.com/o/r/pull/1", "not a url"));
    }

    /// GitHub issue (conversation) comment fixture.
    #[test]
    fn decodes_github_issue_comment() {
        let json = serde_json::json!([{
            "id": 101,
            "user": { "login": "alice" },
            "body": "please add tests"
        }]);
        let comments: Vec<GhComment> = serde_json::from_value(json).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, 101);
        assert_eq!(comments[0].user.login, "alice");
        assert_eq!(comments[0].body, "please add tests");
    }

    /// GitHub inline review comment fixture (path + line context).
    #[test]
    fn decodes_github_inline_comment() {
        let json = serde_json::json!([{
            "id": 202,
            "user": { "login": "bob" },
            "body": "off by one",
            "path": "src/lib.rs",
            "line": 42
        }]);
        let comments: Vec<GhInlineComment> = serde_json::from_value(json).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/lib.rs");
        assert_eq!(comments[0].line, Some(42));
    }

    /// GitHub review fixture (state rides along; bodies may be empty).
    #[test]
    fn decodes_github_review() {
        let json = serde_json::json!([
            { "id": 303, "user": { "login": "carol" }, "body": "needs work", "state": "CHANGES_REQUESTED" },
            { "id": 304, "user": { "login": "carol" }, "body": "", "state": "APPROVED" }
        ]);
        let reviews: Vec<GhReview> = serde_json::from_value(json).unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].state, "CHANGES_REQUESTED");
        assert_eq!(reviews[1].body, "");
    }

    /// GitLab MR note fixture: human note with position, system note.
    #[test]
    fn decodes_gitlab_notes() {
        let json = serde_json::json!([
            {
                "id": 55,
                "author": { "username": "dave" },
                "body": "rename this field",
                "system": false,
                "position": { "new_path": "src/main.rs", "new_line": 10 }
            },
            {
                "id": 56,
                "author": { "username": "gitlab" },
                "body": "added 1 commit",
                "system": true,
                "position": null
            }
        ]);
        let notes: Vec<GlNote> = serde_json::from_value(json).unwrap();
        assert_eq!(notes.len(), 2);
        assert!(!notes[0].system);
        let pos = notes[0].position.as_ref().unwrap();
        assert_eq!(pos.new_path.as_deref(), Some("src/main.rs"));
        assert_eq!(pos.new_line, Some(10));
        assert!(notes[1].system);
        assert!(notes[1].position.is_none());
    }

    /// GitHub mergeability parsing: true/false/null → Yes/No/Unknown, base_sha
    /// extracted from `base.sha`.
    #[test]
    fn github_mergeable_parsing() {
        let yes: GhPull = serde_json::from_value(serde_json::json!({
            "mergeable": true,
            "base": { "sha": "base-yes" }
        }))
        .unwrap();
        assert_eq!(yes.mergeable, Some(true));
        assert_eq!(yes.base.sha.as_deref(), Some("base-yes"));

        let no: GhPull = serde_json::from_value(serde_json::json!({
            "mergeable": false,
            "base": { "sha": "base-no" }
        }))
        .unwrap();
        assert_eq!(no.mergeable, Some(false));

        let unknown: GhPull = serde_json::from_value(serde_json::json!({
            "mergeable": null,
            "base": { "sha": "base-unknown" }
        }))
        .unwrap();
        assert_eq!(unknown.mergeable, None);
    }

    /// GitLab mergeability parsing: detailed_merge_status drives Yes/No/Unknown;
    /// legacy `merge_status` is a fallback; `diff_refs.base_sha` is read.
    #[test]
    fn gitlab_merge_status_parsing() {
        let mergeable: GlMergeRequest = serde_json::from_value(serde_json::json!({
            "detailed_merge_status": "mergeable",
            "diff_refs": { "base_sha": "base-ok" }
        }))
        .unwrap();
        assert_eq!(mergeable.detailed_merge_status.as_deref(), Some("mergeable"));
        assert_eq!(
            mergeable.diff_refs.as_ref().unwrap().base_sha.as_deref(),
            Some("base-ok")
        );

        let conflict: GlMergeRequest = serde_json::from_value(serde_json::json!({
            "detailed_merge_status": "conflict",
            "diff_refs": { "base_sha": "base-conflict" }
        }))
        .unwrap();
        assert_eq!(conflict.detailed_merge_status.as_deref(), Some("conflict"));

        let legacy_no: GlMergeRequest = serde_json::from_value(serde_json::json!({
            "merge_status": "cannot_be_merged"
        }))
        .unwrap();
        assert_eq!(legacy_no.merge_status.as_deref(), Some("cannot_be_merged"));
    }

    /// The Mergeable enum classifies GitHub and GitLab status tokens as expected.
    #[test]
    fn mergeable_enum_classification() {
        assert_eq!(Mergeable::Yes, Mergeable::Yes);
        assert_ne!(Mergeable::Yes, Mergeable::No);
        assert_ne!(Mergeable::No, Mergeable::Unknown);
    }
}
