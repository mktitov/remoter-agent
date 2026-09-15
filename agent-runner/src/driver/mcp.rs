//! MCP server injection (spec §5.6): every ACP session gets the daemon's own
//! `remoter` server (the dev-agent's progress channel), merged with the project
//! repo's `.mcp.json` so the dev-agent has the same tooling a human has.
//!
//! Merge rules (§5.6):
//! - the daemon's `remoter` entry **wins on name conflict** — repo files may
//!   carry stale or personal tokens, never trust them;
//! - `${WORKTREE}` in `command`/`args`/`env`/`url`/header values expands to the
//!   ticket worktree path; absolute paths hardcoded in a repo config are passed
//!   through unchanged.

use std::path::Path;

use agent_client_protocol::schema::v1::{EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio};

use crate::run::RunKind;

/// The daemon-owned MCP server name (spec §5.6).
pub const REMOTER_MCP_NAME: &str = "remoter";

/// `${WORKTREE}` placeholder in repo `.mcp.json` values.
const WORKTREE_PLACEHOLDER: &str = "${WORKTREE}";

/// The session's MCP server list: repo `.mcp.json` entries (if any) plus the
/// daemon's `remoter` entry, which always wins on a name conflict.
/// `${WORKTREE}` expands to the path the *agent* sees: the worktree on the
/// host, or `/work` inside the run container.
#[allow(clippy::too_many_arguments)]
pub fn merged_mcp_servers(
    worktree: &Path,
    mcp_command: &str,
    api_url: &str,
    token: &str,
    workspace_id: Option<i32>,
    kind: RunKind,
    task_id: i32,
    exec: &crate::workspace::ExecEnv,
) -> Vec<McpServer> {
    let expand_as = match exec {
        crate::workspace::ExecEnv::Container(c) => c.work_dir.clone(),
        crate::workspace::ExecEnv::Host { .. } => worktree.to_path_buf(),
    };
    let mut servers = repo_mcp_servers(worktree, &expand_as);
    servers.retain(|s| server_name(s) != REMOTER_MCP_NAME);
    servers.push(remoter_mcp(mcp_command, api_url, token, workspace_id, kind, task_id));
    servers
}

/// The daemon's own `remoter` server: the same API URL + agent token the
/// daemon itself uses (spec §5.6).
fn remoter_mcp(
    command: &str,
    api_url: &str,
    token: &str,
    workspace_id: Option<i32>,
    kind: RunKind,
    task_id: i32,
) -> McpServer {
    let role = match kind {
        RunKind::Plan => "dev-agent-plan",
        RunKind::Implement => "dev-agent-implement",
        RunKind::Supervise => "dev-agent-supervise",
        RunKind::Review => "dev-agent-review",
    };
    let mut env = vec![
        EnvVariable::new("REMOTER_API_URL", api_url),
        EnvVariable::new("REMOTER_AGENT_TOKEN", token),
        EnvVariable::new("REMOTER_AGENT_TASK_ID", task_id.to_string()),
    ];
    if let Some(id) = workspace_id {
        env.push(EnvVariable::new("REMOTER_WORKSPACE_ID", id.to_string()));
    }
    McpServer::Stdio(
        McpServerStdio::new(REMOTER_MCP_NAME, command)
            .args(vec!["--role".to_string(), role.to_string()])
            .env(env),
    )
}

fn server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Http(s) => &s.name,
        McpServer::Sse(s) => &s.name,
        McpServer::Stdio(s) => &s.name,
        _ => "",
    }
}

/// Reads and parses `<worktree>/.mcp.json` (host-side path — the daemon reads
/// the file). A missing or malformed file is not an error — the repo simply
/// has no project tooling of its own. `expand_as` is what `${WORKTREE}`
/// becomes in the parsed entries.
fn repo_mcp_servers(worktree: &Path, expand_as: &Path) -> Vec<McpServer> {
    let path = worktree.join(".mcp.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match parse_repo_mcp(&text, expand_as) {
        Ok(servers) => servers,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ignoring malformed repo .mcp.json");
            Vec::new()
        }
    }
}

/// Pure parse+expand, separated for tests. Accepts the widespread
/// `{ "mcpServers": { name: { command, args?, env? } | { url, headers? } } }`
/// shape (e.g. planner's).
fn parse_repo_mcp(text: &str, worktree: &Path) -> Result<Vec<McpServer>, String> {
    let root: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Some(map) = root.get("mcpServers").and_then(|m| m.as_object()) else {
        return Err("no \"mcpServers\" object".to_string());
    };
    let wt = worktree.to_string_lossy();
    let mut servers = Vec::new();
    for (name, entry) in map {
        servers.push(parse_entry(name, entry, &wt)?);
    }
    Ok(servers)
}

fn parse_entry(name: &str, entry: &serde_json::Value, wt: &str) -> Result<McpServer, String> {
    let expand = |v: &serde_json::Value| v.as_str().map(|s| s.replace(WORKTREE_PLACEHOLDER, wt));

    if let Some(url) = entry.get("url").and_then(&expand) {
        let headers = entry
            .get("headers")
            .and_then(|h| h.as_object())
            .map(|h| {
                h.iter()
                    .filter_map(|(k, v)| expand(v).map(|v| HttpHeader::new(k.as_str(), v)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return Ok(McpServer::Http(
            McpServerHttp::new(name.to_string(), url).headers(headers),
        ));
    }

    let command = entry
        .get("command")
        .and_then(&expand)
        .ok_or_else(|| format!("entry {name:?} has neither \"command\" nor \"url\""))?;
    let args = entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(&expand).collect::<Vec<_>>())
        .unwrap_or_default();
    let env = entry
        .get("env")
        .and_then(|e| e.as_object())
        .map(|e| {
            e.iter()
                .filter_map(|(k, v)| expand(v).map(|v| EnvVariable::new(k.as_str(), v)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(McpServer::Stdio(
        McpServerStdio::new(name.to_string(), command).args(args).env(env),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(server: &McpServer) -> &McpServerStdio {
        match server {
            McpServer::Stdio(s) => s,
            other => panic!("expected stdio server, got {other:?}"),
        }
    }

    #[test]
    fn remoter_entry_carries_api_url_and_token() {
        let task_id = 42;
        let servers = merged_mcp_servers(
            Path::new("/nonexistent"),
            "remoter-mcp",
            "http://api",
            "tok",
            None,
            RunKind::Implement,
            task_id,
            &crate::workspace::ExecEnv::host(false),
        );
        assert_eq!(servers.len(), 1);
        let s = stdio(&servers[0]);
        assert_eq!(s.name, "remoter");
        assert_eq!(s.command, Path::new("remoter-mcp"));
        assert_eq!(s.args, ["--role", "dev-agent-implement"]);
        let env: Vec<(&str, &str)> = s.env.iter().map(|e| (e.name.as_str(), e.value.as_str())).collect();
        assert_eq!(
            env,
            [
                ("REMOTER_API_URL", "http://api"),
                ("REMOTER_AGENT_TOKEN", "tok"),
                ("REMOTER_AGENT_TASK_ID", &task_id.to_string()),
            ]
        );
    }

    #[test]
    fn merges_repo_entries_and_expands_worktree() {
        let json = r#"{
          "mcpServers": {
            "git": { "command": "sh", "args": ["-c", "cd ${WORKTREE} && exec git-mcp"], "env": { "REPO": "${WORKTREE}" } },
            "docs": { "url": "http://localhost:6006/mcp", "headers": { "X-Path": "${WORKTREE}" } }
          }
        }"#;
        let dir = std::env::temp_dir().join(format!("remoter-mcp-merge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".mcp.json"), json).unwrap();
        let wt = dir.to_string_lossy();

        let task_id = 42;
        let servers = merged_mcp_servers(
            &dir,
            "remoter-mcp",
            "http://api",
            "tok",
            None,
            RunKind::Implement,
            task_id,
            &crate::workspace::ExecEnv::host(false),
        );
        assert_eq!(servers.len(), 3);

        let git = stdio(&servers[0]);
        assert_eq!(git.name, "git");
        assert_eq!(git.args, ["-c".to_string(), format!("cd {wt} && exec git-mcp")]);
        assert_eq!(git.env[0].value, wt.as_ref());

        let McpServer::Http(docs) = &servers[1] else {
            panic!("expected http")
        };
        assert_eq!(docs.url, "http://localhost:6006/mcp");
        assert_eq!(docs.headers[0].value, wt.as_ref());

        let remoter = stdio(&servers[2]);
        assert_eq!(remoter.name, REMOTER_MCP_NAME);
        assert_eq!(remoter.args, ["--role", "dev-agent-implement"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn daemon_remoter_wins_on_name_conflict() {
        // Repo-carried remoter entries may hold stale personal tokens (§5.6).
        let dir = std::env::temp_dir().join(format!("remoter-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{ "mcpServers": { "remoter": { "command": "evil", "env": { "REMOTER_AGENT_TOKEN": "stale" } } } }"#,
        )
        .unwrap();

        let task_id = 42;
        let servers = merged_mcp_servers(
            &dir,
            "remoter-mcp",
            "http://api",
            "fresh",
            None,
            RunKind::Plan,
            task_id,
            &crate::workspace::ExecEnv::host(false),
        );
        assert_eq!(servers.len(), 1);
        let s = stdio(&servers[0]);
        assert_eq!(s.command, Path::new("remoter-mcp"));
        assert_eq!(s.args, ["--role", "dev-agent-plan"]);
        assert_eq!(s.env[1].value, "fresh");
        assert!(
            s.env
                .iter()
                .any(|e| e.name == "REMOTER_AGENT_TASK_ID" && e.value == task_id.to_string())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn review_kind_maps_to_dev_agent_review_role() {
        let servers = merged_mcp_servers(
            Path::new("/nonexistent"),
            "remoter-mcp",
            "http://api",
            "tok",
            None,
            RunKind::Review,
            42,
            &crate::workspace::ExecEnv::host(false),
        );
        assert_eq!(stdio(&servers[0]).args, ["--role", "dev-agent-review"]);
    }

    #[test]
    fn malformed_file_yields_no_repo_entries() {
        assert!(parse_repo_mcp("not json", Path::new("/wt")).is_err());
        assert!(parse_repo_mcp("{}", Path::new("/wt")).is_err());
        assert!(parse_repo_mcp(r#"{ "mcpServers": { "x": {} } }"#, Path::new("/wt")).is_err());
    }

    #[test]
    fn remoter_entry_forwards_workspace_id_to_child_env() {
        let task_id = 42;
        let servers = merged_mcp_servers(
            Path::new("/nonexistent"),
            "remoter-mcp",
            "http://api",
            "tok",
            Some(7),
            RunKind::Implement,
            task_id,
            &crate::workspace::ExecEnv::host(false),
        );
        let s = stdio(&servers[0]);
        assert!(s.env.iter().any(|e| e.name == "REMOTER_WORKSPACE_ID" && e.value == "7"));
    }
}
