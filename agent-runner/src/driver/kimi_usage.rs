//! Reads per-session LLM usage from kimi's local `wire.jsonl` files.
//!
//! ACP does not report token usage directly up to (and including) kimi 0.38.0,
//! but kimi writes a `usage.record` line to
//! `<sessions_dir>/*/<session_id>/agents/<agent>/wire.jsonl` after each LLM
//! request. The driver polls that file after a turn ends and sums the records
//! that appeared since the baseline line count.
//!
//! One `usage.record` corresponds to one LLM request. The "input" count is the
//! sum of `inputOther`, `inputCacheRead`, and `inputCacheCreation`; the
//! "output" count is `output`.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Default time the driver waits for usage records to land after the ACP
/// response came back without token counts.
const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize)]
struct UsageRecord {
    #[serde(rename = "type")]
    _ty: String,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(rename = "inputOther", default)]
    input_other: i64,
    #[serde(default)]
    output: i64,
    #[serde(rename = "inputCacheRead", default)]
    input_cache_read: i64,
    #[serde(rename = "inputCacheCreation", default)]
    input_cache_creation: i64,
}

impl UsageRecord {
    fn input(&self) -> i64 {
        self.usage.input_other + self.usage.input_cache_read + self.usage.input_cache_creation
    }

    fn output(&self) -> i64 {
        self.usage.output
    }
}

/// Returns the total number of lines in all `wire.jsonl` files for the session,
/// or `0` if the session directory does not exist yet.
pub fn usage_line_count(sessions_dir: &Path, session_id: &str) -> usize {
    wire_files(sessions_dir, session_id)
        .iter()
        .map(|p| count_lines(p))
        .sum()
}

/// Sums `usage.record` lines that appear at or after the global `since_line`
/// index across all agent `wire.jsonl` files for this session. Returns `None`
/// if no `wire.jsonl` files exist yet (so the caller can keep polling).
pub fn session_usage(sessions_dir: &Path, session_id: &str, since_line: usize) -> Option<(i64, i64)> {
    let files = wire_files(sessions_dir, session_id);
    if files.is_empty() {
        return None;
    }

    let mut input = 0i64;
    let mut output = 0i64;
    let mut line_no = 0usize;

    for path in &files {
        let Ok(file) = std::fs::File::open(path) else { continue };
        let reader = std::io::BufReader::new(file);
        for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
            if line_no >= since_line
                && let Ok(rec) = serde_json::from_str::<UsageRecord>(&line)
            {
                input += rec.input();
                output += rec.output();
            }
            line_no += 1;
        }
    }

    Some((input, output))
}

/// Polls the session usage file until new records appear or the timeout elapses.
/// Returns `Some(...)` counts if the file exists, even when the delta is zero.
pub async fn poll_session_usage(
    sessions_dir: &Path,
    session_id: &str,
    since_line: usize,
) -> (Option<i64>, Option<i64>) {
    poll_session_usage_with(
        sessions_dir,
        session_id,
        since_line,
        DEFAULT_POLL_TIMEOUT,
        DEFAULT_POLL_INTERVAL,
    )
    .await
}

async fn poll_session_usage_with(
    sessions_dir: &Path,
    session_id: &str,
    since_line: usize,
    timeout: Duration,
    interval: Duration,
) -> (Option<i64>, Option<i64>) {
    let start = std::time::Instant::now();
    loop {
        if let Some((input, output)) = session_usage(sessions_dir, session_id, since_line) {
            return (Some(input), Some(output));
        }
        if start.elapsed() >= timeout {
            return (None, None);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Tool-name prefix of the remoter MCP server in the model's tool list.
const REMOTER_TOOL_PREFIX: &str = "mcp__remoter__";

/// Reads the latest `llm.tools_snapshot` record in the session's wire files
/// and reports whether the remoter MCP tools (`mcp__remoter__*`) were offered
/// to the model — `Some(false)` means remoter-mcp failed to start and the
/// agent ran the whole turn without the ticket tools. Returns `None` when no
/// snapshot record exists (unknown or changed kimi wire format); callers must
/// not warn in that case.
pub fn session_has_remoter_tools(sessions_dir: &Path, session_id: &str) -> Option<bool> {
    let mut snapshot: Option<serde_json::Value> = None;
    for path in wire_files(sessions_dir, session_id) {
        let Ok(file) = std::fs::File::open(&path) else { continue };
        let reader = std::io::BufReader::new(file);
        for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
            if line.contains("\"llm.tools_snapshot\"")
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && v.get("type").and_then(|t| t.as_str()) == Some("llm.tools_snapshot")
            {
                snapshot = Some(v);
            }
        }
    }
    let snapshot = snapshot?;
    let tools = snapshot.get("tools")?.as_array()?;
    Some(
        tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .any(|name| name.starts_with(REMOTER_TOOL_PREFIX)),
    )
}

/// Finds all `<sessions_dir>/*/<session_id>/agents/<agent>/wire.jsonl` files.
fn wire_files(sessions_dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(top) = std::fs::read_dir(sessions_dir) else {
        return out;
    };

    for entry in top.flatten() {
        let candidate = entry.path().join(session_id);
        if !candidate.is_dir() {
            continue;
        }
        let agents_dir = candidate.join("agents");
        let Ok(agents) = std::fs::read_dir(&agents_dir) else {
            continue;
        };
        for agent in agents.flatten() {
            let wire = agent.path().join("wire.jsonl");
            if wire.is_file() {
                out.push(wire);
            }
        }
    }

    out
}

fn count_lines(path: &Path) -> usize {
    let Ok(file) = std::fs::File::open(path) else { return 0 };
    let reader = std::io::BufReader::new(file);
    reader.lines().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_line(input_other: i64, output: i64) -> String {
        format!(
            r#"{{"type":"usage.record","agentId":"main","model":"kimi-code/k3-256k","usage":{{"inputOther":{input_other},"output":{output},"inputCacheRead":0,"inputCacheCreation":0}},"usageScope":"turn","time":0}}"#
        )
    }

    fn write_wire(dir: &Path, session_id: &str, lines: &[String]) -> PathBuf {
        let path = dir.join("wd").join(session_id).join("agents/main/wire.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            lines.iter().map(|l| format!("{l}\n")).collect::<Vec<_>>().join(""),
        )
        .unwrap();
        path
    }

    /// Per-test temp dir — tests run in parallel within one process, so a
    /// shared `<pid>`-only dir lets concurrent tests wipe each other's
    /// fixtures.
    fn test_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("remoter-usage-test-{}-{tag}", std::process::id()))
    }

    #[test]
    fn sums_usage_records() {
        let dir = test_dir("sums");
        let _ = std::fs::remove_dir_all(&dir);
        write_wire(&dir, "sess-a", &[fixture_line(10, 5), fixture_line(20, 7)]);
        let (input, output) = session_usage(&dir, "sess-a", 0).unwrap();
        assert_eq!(input, 30);
        assert_eq!(output, 12);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delta_from_baseline_skips_earlier_lines() {
        let dir = test_dir("delta");
        let _ = std::fs::remove_dir_all(&dir);
        write_wire(
            &dir,
            "sess-b",
            &[fixture_line(1, 1), fixture_line(2, 2), fixture_line(3, 3)],
        );
        let (input, output) = session_usage(&dir, "sess-b", 2).unwrap();
        assert_eq!(input, 3);
        assert_eq!(output, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_session_returns_none() {
        let dir = test_dir("missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(session_usage(&dir, "no-such-session", 0).is_none());
    }

    #[test]
    fn line_count_matches_file() {
        let dir = test_dir("line-count");
        let _ = std::fs::remove_dir_all(&dir);
        let lines = vec![fixture_line(1, 1), fixture_line(2, 2)];
        write_wire(&dir, "sess-c", &lines);
        assert_eq!(usage_line_count(&dir, "sess-c"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sums_across_multiple_agents() {
        let dir = test_dir("multi-agent");
        let _ = std::fs::remove_dir_all(&dir);
        let a = dir.join("wd").join("sess-d").join("agents/main/wire.jsonl");
        let b = dir.join("wd").join("sess-d").join("agents/side/wire.jsonl");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&a, format!("{}\n", fixture_line(10, 5))).unwrap();
        std::fs::write(&b, format!("{}\n{}\n", fixture_line(7, 3), fixture_line(2, 1))).unwrap();
        let (input, output) = session_usage(&dir, "sess-d", 0).unwrap();
        assert_eq!(input, 19);
        assert_eq!(output, 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn snapshot_line(tool_names: &[&str]) -> String {
        let tools = tool_names
            .iter()
            .map(|n| format!(r#"{{"name":"{n}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"type":"llm.tools_snapshot","agentId":"main","tools":[{tools}]}}"#)
    }

    #[test]
    fn remoter_tools_present_in_snapshot() {
        let dir = test_dir("tools-present");
        let _ = std::fs::remove_dir_all(&dir);
        write_wire(
            &dir,
            "sess-e",
            &[
                fixture_line(1, 1),
                snapshot_line(&["Read", "Bash", "mcp__remoter__add_action"]),
            ],
        );
        assert_eq!(session_has_remoter_tools(&dir, "sess-e"), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remoter_tools_missing_from_snapshot() {
        let dir = test_dir("tools-missing");
        let _ = std::fs::remove_dir_all(&dir);
        write_wire(&dir, "sess-f", &[fixture_line(1, 1), snapshot_line(&["Read", "Bash"])]);
        assert_eq!(session_has_remoter_tools(&dir, "sess-f"), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_snapshot_wins() {
        let dir = test_dir("tools-latest");
        let _ = std::fs::remove_dir_all(&dir);
        write_wire(
            &dir,
            "sess-g",
            &[snapshot_line(&["mcp__remoter__get_task"]), snapshot_line(&["Read"])],
        );
        assert_eq!(session_has_remoter_tools(&dir, "sess-g"), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_snapshot_record_returns_none() {
        let dir = test_dir("tools-unknown");
        let _ = std::fs::remove_dir_all(&dir);
        write_wire(&dir, "sess-h", &[fixture_line(1, 1)]);
        assert_eq!(session_has_remoter_tools(&dir, "sess-h"), None);
        assert_eq!(session_has_remoter_tools(&dir, "no-such-session"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
