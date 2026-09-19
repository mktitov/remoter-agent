//! `kimi-acp` driver tests against a fake ACP agent (`fake_acp_agent.py`):
//! the full `initialize` → `session/new` → `session/prompt` flow with streamed
//! updates, permission auto-approval, elicitation auto-answer, usage capture,
//! session resume + fallback, MCP injection, config option application, and
//! kill-on-drop (spec §5.5).

use std::path::{Path, PathBuf};
use std::time::Duration;

use remoter_agent::config::DriverConfig;
use remoter_agent::driver::acp::AcpDriver;
use remoter_agent::driver::{AgentDriver, DriverError, RunSpec};

const FAKE_AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_acp_agent.py");

fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remoter-acp-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn driver() -> AcpDriver {
    driver_with_stall(300, 30)
}

fn driver_with_stall(stall_idle_secs: u64, stall_grace_secs: u64) -> AcpDriver {
    let cfg = DriverConfig {
        kind: "kimi-acp".to_string(),
        simulate_delay_ms: 0,
        simulate_uncommitted_changes: false,
        agent_program: "python3".to_string(),
        agent_args: vec![FAKE_AGENT.to_string()],
        remoter_mcp_command: "remoter-mcp".to_string(),
        plan_model: None,
        plan_thinking: None,
        implement_model: None,
        implement_thinking: None,
        supervise_model: None,
        supervise_thinking: None,
        review_model: None,
        review_thinking: None,
        sessions_dir: None,
    };
    AcpDriver::new(&cfg, "http://api.test", "tok", None, stall_idle_secs, stall_grace_secs)
}

fn driver_with_config() -> AcpDriver {
    let cfg = DriverConfig {
        kind: "kimi-acp".to_string(),
        simulate_delay_ms: 0,
        simulate_uncommitted_changes: false,
        agent_program: "python3".to_string(),
        agent_args: vec![FAKE_AGENT.to_string()],
        remoter_mcp_command: "remoter-mcp".to_string(),
        plan_model: Some("kimi-code/k3".to_string()),
        plan_thinking: Some("max".to_string()),
        implement_model: Some("kimi-code/kimi-for-coding".to_string()),
        implement_thinking: Some("on".to_string()),
        supervise_model: None,
        supervise_thinking: None,
        review_model: None,
        review_thinking: None,
        sessions_dir: None,
    };
    AcpDriver::new(&cfg, "http://api.test", "tok", None, 300, 30)
}

fn spec(
    dir: &Path,
    kind: &'static str,
    resume: Option<String>,
    env: Vec<(String, String)>,
    config_options: Vec<(String, String)>,
) -> RunSpec {
    RunSpec {
        task_id: 1,
        run_id: 1,
        cwd: dir.to_path_buf(),
        prompt: "test prompt".to_string(),
        kind,
        resume_session: resume,
        branch: "agent/task-1-test".to_string(),
        env,
        exec: remoter_agent::workspace::ExecEnv::host(false),
        config_options,
        logger: remoter_agent::session_log::SessionLogger::noop(),
        phase_tx: None,
    }
}

fn read_capture(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn full_turn_streams_approves_and_reports_usage() {
    let dir = test_dir("full");
    let capture = dir.join("capture.jsonl");
    std::fs::write(
        dir.join(".mcp.json"),
        r#"{ "mcpServers": { "git": { "command": "sh", "args": ["-c", "cd ${WORKTREE} && exec git-mcp"] } } }"#,
    )
    .unwrap();

    let outcome = driver()
        .run(spec(
            &dir,
            "plan",
            None,
            vec![("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string())],
            vec![],
        ))
        .await
        .expect("run should succeed");

    assert_eq!(outcome.session_id.as_deref(), Some("sess-1"));
    assert_eq!(outcome.text, "Hello, world");
    assert_eq!(outcome.input_tokens, Some(12));
    assert_eq!(outcome.output_tokens, Some(34));

    let events = read_capture(&capture);
    // MCP injection (spec §5.6): the repo entry (with ${WORKTREE} expanded) and
    // the daemon's own `remoter` server both land in session/new.
    let new = events.iter().find(|e| e.get("session_new").is_some()).unwrap();
    let servers = new["session_new"]["mcpServers"].as_array().unwrap();
    let git = servers.iter().find(|s| s["name"] == "git").unwrap();
    assert_eq!(
        git["args"][1].as_str().unwrap(),
        format!("cd {} && exec git-mcp", dir.display())
    );
    let remoter = servers.iter().find(|s| s["name"] == "remoter").unwrap();
    let env = remoter["env"].as_array().unwrap();
    assert!(
        env.iter()
            .any(|v| v["name"] == "REMOTER_API_URL" && v["value"] == "http://api.test")
    );
    assert!(
        env.iter()
            .any(|v| v["name"] == "REMOTER_AGENT_TOKEN" && v["value"] == "tok")
    );

    // Permission auto-approval picks the allow option even though reject was
    // listed first; elicitation is declined (spec §5.5).
    let permission = events.iter().find(|e| e.get("permission").is_some()).unwrap();
    assert!(permission["permission"].to_string().contains("allow"), "{permission}");
    let elicitation = events.iter().find(|e| e.get("elicitation").is_some()).unwrap();
    assert!(
        elicitation["elicitation"].to_string().contains("decline"),
        "{elicitation}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn plan_run_applies_config_options_and_records_them() {
    let dir = test_dir("config");
    let capture = dir.join("capture.jsonl");

    let outcome = driver_with_config()
        .run(spec(
            &dir,
            "plan",
            None,
            vec![("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string())],
            vec![
                ("model".to_string(), "kimi-code/k3".to_string()),
                ("thinking".to_string(), "max".to_string()),
            ],
        ))
        .await
        .expect("run should succeed");

    assert_eq!(outcome.model.as_deref(), Some("kimi-code/k3"));
    assert_eq!(outcome.thinking.as_deref(), Some("max"));

    let events = read_capture(&capture);
    let set = events
        .iter()
        .filter(|e| e.get("config_option").is_some())
        .map(|e| &e["config_option"])
        .collect::<Vec<_>>();
    assert_eq!(set.len(), 2, "expected set_config_option calls: {set:?}");
    let ids: Vec<&str> = set.iter().map(|e| e["configId"].as_str().unwrap()).collect();
    assert!(ids.contains(&"model"), "{ids:?}");
    assert!(ids.contains(&"thinking"), "{ids:?}");
    assert!(
        set.iter()
            .any(|e| { e["configId"] == "model" && e["value"] == "kimi-code/k3" })
    );
    assert!(
        set.iter()
            .any(|e| { e["configId"] == "thinking" && e["value"] == "max" })
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unknown_config_option_is_permanent() {
    let dir = test_dir("bad-config");

    let err = driver_with_config()
        .run(spec(
            &dir,
            "plan",
            None,
            vec![],
            vec![("model".to_string(), "unknown-model".to_string())],
        ))
        .await
        .unwrap_err();

    assert!(matches!(err.source, DriverError::Permanent(_)), "{err}");
    assert!(err.to_string().contains("not a valid choice"), "{err}");
    // The error names the advertised choices so a misconfiguration is
    // fixable from the log alone.
    assert!(err.to_string().contains("kimi-code/k3"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unsupported_opencode_effort_is_optional() {
    let dir = test_dir("optional-effort");

    let outcome = driver()
        .run(spec(
            &dir,
            "implement",
            None,
            vec![],
            vec![("effort".to_string(), "high".to_string())],
        ))
        .await
        .expect("unsupported OpenCode effort should be skipped");

    assert!(!outcome.text.trim().is_empty(), "{}", outcome.text);
    std::fs::remove_dir_all(&dir).ok();
}

/// The fake agent mimics the real kimi CLI: the `thinking` choices depend on
/// the current `model` — for `kimi-code/kimi-for-coding` only `on` is offered
/// (console: Thinking On / Off (unsupported)). `on` is not among the initial
/// `session/new` choices (low/high/max), so this only passes if the driver
/// validates `thinking` against the option set refreshed by the
/// `set_config_option(model)` response.
#[tokio::test]
async fn model_dependent_thinking_validates_against_refreshed_options() {
    let dir = test_dir("refresh");
    let capture = dir.join("capture.jsonl");

    let outcome = driver()
        .run(spec(
            &dir,
            "implement",
            None,
            vec![("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string())],
            vec![
                ("model".to_string(), "kimi-code/kimi-for-coding".to_string()),
                ("thinking".to_string(), "on".to_string()),
            ],
        ))
        .await
        .expect("run should succeed");

    assert_eq!(outcome.model.as_deref(), Some("kimi-code/kimi-for-coding"));
    assert_eq!(outcome.thinking.as_deref(), Some("on"));

    // The model is set before thinking — the thinking value only exists once
    // the model switch has refreshed the advertised choices.
    let events = read_capture(&capture);
    let ids: Vec<&str> = events
        .iter()
        .filter(|e| e.get("config_option").is_some())
        .map(|e| e["config_option"]["configId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["model", "thinking"], "{ids:?}");

    std::fs::remove_dir_all(&dir).ok();
}

/// The flip side: a value valid for the default model but not for the selected
/// one must fail permanently once the refreshed choices no longer offer it.
#[tokio::test]
async fn thinking_value_dropped_by_model_switch_is_permanent() {
    let dir = test_dir("refresh-invalid");

    let err = driver()
        .run(spec(
            &dir,
            "implement",
            None,
            vec![],
            vec![
                ("model".to_string(), "kimi-code/kimi-for-coding".to_string()),
                ("thinking".to_string(), "max".to_string()),
            ],
        ))
        .await
        .unwrap_err();

    assert!(matches!(err.source, DriverError::Permanent(_)), "{err}");
    assert!(err.to_string().contains("not a valid choice"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn implement_run_resumes_known_session() {
    let dir = test_dir("resume");
    let capture = dir.join("capture.jsonl");

    let outcome = driver()
        .run(spec(
            &dir,
            "implement",
            Some("known-session".to_string()),
            vec![("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string())],
            vec![
                ("model".to_string(), "kimi-code/kimi-for-coding".to_string()),
                ("thinking".to_string(), "on".to_string()),
            ],
        ))
        .await
        .expect("run should succeed");

    assert_eq!(outcome.session_id.as_deref(), Some("known-session"));
    let events = read_capture(&capture);
    assert!(events.iter().any(|e| e.get("session_resume").is_some()));
    assert!(events.iter().all(|e| e.get("session_new").is_none()));
    // Config options are also applied to the resumed session.
    assert!(events.iter().any(|e| e.get("config_option").is_some()));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unknown_session_falls_back_to_fresh_session() {
    let dir = test_dir("fallback");
    let capture = dir.join("capture.jsonl");

    let outcome = driver()
        .run(spec(
            &dir,
            "implement",
            Some("gone-session".to_string()),
            vec![("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string())],
            vec![],
        ))
        .await
        .expect("run should succeed");

    // A fresh session replaces the unresumable one (spec §5.4).
    assert_eq!(outcome.session_id.as_deref(), Some("sess-1"));
    let events = read_capture(&capture);
    assert!(events.iter().any(|e| e.get("session_resume").is_some()));
    assert!(events.iter().any(|e| e.get("session_new").is_some()));

    std::fs::remove_dir_all(&dir).ok();
}

/// In-attempt fallback (spec §5.4): the resume itself succeeded, but the
/// first prompt on the resumed session fails (task 11: the provider rejects
/// the ~60 MB session's compaction payload) — the driver retries the prompt
/// on a fresh session within the same attempt, without spending the retry
/// budget. The context-limit wording must not prevent the fallback.
#[tokio::test]
async fn resumed_session_prompt_failure_falls_back_to_fresh() {
    let dir = test_dir("prompt-fallback");
    let capture = dir.join("capture.jsonl");

    let outcome = driver()
        .run(spec(
            &dir,
            "implement",
            Some("known-session".to_string()),
            vec![
                ("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string()),
                (
                    "FAKE_AGENT_FAIL_PROMPT_ON_SESSION".to_string(),
                    "known-session".to_string(),
                ),
            ],
            vec![],
        ))
        .await
        .expect("run should succeed on the fresh session");

    // The fresh session replaces the poisoned resumed one (spec §5.4).
    assert_eq!(outcome.session_id.as_deref(), Some("sess-1"));
    assert_eq!(outcome.text, "Hello, world");
    let events = read_capture(&capture);
    let pos = |key: &str| events.iter().position(|e| e.get(key).is_some()).unwrap();
    let resume_at = pos("session_resume");
    let failed_at = pos("prompt_failed");
    let new_at = pos("session_new");
    assert!(resume_at < failed_at && failed_at < new_at, "{events:?}");
    // Both prompt attempts are recorded: the failed one on the resumed
    // session, then the successful one on the fresh session.
    assert_eq!(events.iter().filter(|e| e.get("prompt").is_some()).count(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

/// When the fresh session's prompt fails too, the error propagates and is
/// classified as before — the context-limit wording is Permanent (§5.7).
#[tokio::test]
async fn fresh_prompt_failure_after_fallback_is_permanent() {
    let dir = test_dir("prompt-fallback-perm");

    let err = driver()
        .run(spec(
            &dir,
            "implement",
            Some("known-session".to_string()),
            vec![(
                "FAKE_AGENT_FAIL_PROMPT_ON_SESSION".to_string(),
                "known-session,sess-1".to_string(),
            )],
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(matches!(err.source, DriverError::Permanent(_)), "{err}");
    assert!(err.to_string().contains("total message size exceeds limit"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn missing_agent_binary_is_permanent() {
    let dir = test_dir("nobin");
    let cfg = DriverConfig {
        kind: "kimi-acp".to_string(),
        simulate_delay_ms: 0,
        simulate_uncommitted_changes: false,
        agent_program: "/nonexistent/remoter-agent-test-binary".to_string(),
        agent_args: vec![],
        remoter_mcp_command: "remoter-mcp".to_string(),
        plan_model: None,
        plan_thinking: None,
        implement_model: None,
        implement_thinking: None,
        supervise_model: None,
        supervise_thinking: None,
        review_model: None,
        review_thinking: None,
        sessions_dir: None,
    };
    let driver = AcpDriver::new(&cfg, "http://api.test", "tok", None, 300, 30);
    let err = driver.run(spec(&dir, "plan", None, vec![], vec![])).await.unwrap_err();
    assert!(matches!(err.source, DriverError::Permanent(_)), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

/// Stall detection (#154): a hung turn with no ACP activity is detected after
/// `stall_idle_secs`, cancelled via `session/cancel`, and — the fake agent
/// honors the cancel and ends the turn inside the grace window — the attempt
/// fails Stalled, far short of any run timeout. The idle limit is 10s rather
/// than the 2s minimum: under a loaded nix sandbox the fake agent's startup
/// can eat a 2s window before the prompt lands, failing the test for reasons
/// unrelated to stall detection.
#[tokio::test]
async fn idle_turn_is_stalled_cancelled_and_fails_fast() {
    let dir = test_dir("stall");
    let capture = dir.join("capture.jsonl");

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        driver_with_stall(10, 2).run(spec(
            &dir,
            "plan",
            None,
            vec![
                ("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string()),
                ("FAKE_AGENT_HANG".to_string(), "1".to_string()),
            ],
            vec![],
        )),
    )
    .await
    .expect("driver hung past the stall watchdog");
    let elapsed = started.elapsed();

    let err = result.unwrap_err();
    assert!(matches!(err.source, DriverError::Stalled(_)), "{err}");
    assert!(err.to_string().contains("grace"), "{err}");
    // idle (~10s, 1s poll granularity) + honored cancel — an order of
    // magnitude below the default 300s idle limit / 60min run timeout.
    assert!(elapsed < Duration::from_secs(30), "took {elapsed:?}");

    let events = read_capture(&capture);
    assert!(events.iter().any(|e| e.get("prompt_hang").is_some()));
    assert!(
        events.iter().any(|e| e.get("cancel").is_some()),
        "watchdog must send session/cancel: {events:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Stall with an agent that ignores `session/cancel` (#154): after the grace
/// window the driver drops the connection and the ChildGuard's
/// SIGTERM → SIGKILL escalation removes the whole process group — the hung
/// agent AND its grandchild holding the stdout pipe. The 10s idle limit (see
/// the sibling stall test) leaves room for a slow sandboxed fake-agent start.
#[tokio::test]
async fn stalled_agent_ignoring_cancel_is_killed_with_its_process_group() {
    let dir = test_dir("stall-kill");
    let capture = dir.join("capture.jsonl");
    let pidfile = dir.join("agent.pid");

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        driver_with_stall(10, 2).run(spec(
            &dir,
            "plan",
            None,
            vec![
                ("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string()),
                ("FAKE_AGENT_PIDFILE".to_string(), pidfile.to_string_lossy().to_string()),
                ("FAKE_AGENT_HANG".to_string(), "1".to_string()),
                ("FAKE_AGENT_IGNORE_CANCEL".to_string(), "1".to_string()),
                ("FAKE_AGENT_HANG_GRANDCHILD".to_string(), "1".to_string()),
            ],
            vec![],
        )),
    )
    .await
    .expect("driver hung past the stall watchdog + grace");
    let elapsed = started.elapsed();

    let err = result.unwrap_err();
    assert!(matches!(err.source, DriverError::Stalled(_)), "{err}");
    // idle (~10s) + grace (2s) — the turn never ends on its own.
    assert!(elapsed < Duration::from_secs(30), "took {elapsed:?}");

    let events = read_capture(&capture);
    // The cancel was sent and observed — and deliberately ignored.
    assert!(events.iter().any(|e| e.get("cancel").is_some()), "{events:?}");

    // Agent pid on line 1, grandchild pid on line 2 (FAKE_AGENT_HANG_GRANDCHILD).
    let pids: Vec<i32> = std::fs::read_to_string(&pidfile)
        .unwrap()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2, "agent + grandchild pids: {pids:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if pids.iter().all(|pid| process_dead(*pid)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "process group survived the ChildGuard TERM→KILL escalation: {pids:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// kill(pid, 0) existence check that also treats zombies as dead: the
/// grandchild is orphaned when the agent dies, and a minimal init (containers,
/// CI) may not reap it — a zombie proves the TERM→KILL escalation worked.
#[cfg(unix)]
fn process_dead(pid: i32) -> bool {
    // SAFETY: signal 0 is a pure existence check.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| {
            stat.rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(true)
}

/// A provider quota rejection (#154) is permanent: no retry budget is burned.
#[tokio::test]
async fn quota_rejection_is_permanent_and_not_retried() {
    let dir = test_dir("quota");

    let err = driver()
        .run(spec(
            &dir,
            "plan",
            None,
            vec![("FAKE_AGENT_QUOTA".to_string(), "1".to_string())],
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(matches!(err.source, DriverError::Permanent(_)), "{err}");
    assert!(err.to_string().contains("quota/authentication failure"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

/// The task-48 failure mode: the agent process dies mid-turn without ever
/// answering `session/prompt` (kimi: a provider-side turn failure that ends
/// the turn with no JSON-RPC response). The driver must notice the exit and
/// fail the attempt as transient, not wait for the outer run timeout.
#[tokio::test]
async fn agent_exit_before_prompt_answer_is_transient() {
    let dir = test_dir("exit-mid-turn");
    let capture = dir.join("capture.jsonl");

    // The timeout makes the regression explicit: without exit detection this
    // future never resolves on its own.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        driver().run(spec(
            &dir,
            "plan",
            None,
            vec![
                ("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string()),
                ("FAKE_AGENT_EXIT_ON_PROMPT".to_string(), "1".to_string()),
            ],
            vec![],
        )),
    )
    .await
    .expect("driver hung after the agent process exited");

    let err = result.unwrap_err();
    assert!(matches!(err.source, DriverError::Transient(_)), "{err}");
    assert!(err.to_string().contains("agent process exited"), "{err}");

    let events = read_capture(&capture);
    assert!(events.iter().any(|e| e.get("prompt_exit").is_some()));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn zero_output_turn_is_permanent() {
    let dir = test_dir("empty");

    // FAKE_AGENT_EMPTY=1: the agent ends the turn without any text — the
    // kimi #1485 failure class (spec §5.7) must fail loudly, not report an
    // empty plan as success.
    let err = driver()
        .run(spec(
            &dir,
            "plan",
            None,
            vec![("FAKE_AGENT_EMPTY".to_string(), "1".to_string())],
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(matches!(err.source, DriverError::Permanent(_)), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn permission_request_without_allow_option_is_cancelled() {
    let dir = test_dir("noallow");
    let capture = dir.join("capture.jsonl");

    // FAKE_AGENT_NO_ALLOW=1: the only option is reject — the driver must
    // cancel the request rather than auto-pick whatever came first.
    driver()
        .run(spec(
            &dir,
            "plan",
            None,
            vec![
                ("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string()),
                ("FAKE_AGENT_NO_ALLOW".to_string(), "1".to_string()),
            ],
            vec![],
        ))
        .await
        .expect("run should still succeed");

    let events = read_capture(&capture);
    let permission = events.iter().find(|e| e.get("permission").is_some()).unwrap();
    assert!(
        permission["permission"].to_string().contains("cancelled"),
        "{permission}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn dropped_run_future_kills_the_agent_process() {
    let dir = test_dir("kill");
    let capture = dir.join("capture.jsonl");
    let pidfile = dir.join("agent.pid");

    let driver = std::sync::Arc::new(driver());
    let task = {
        let driver = driver.clone();
        let dir = dir.clone();
        let capture = capture.clone();
        let pidfile = pidfile.clone();
        tokio::spawn(async move {
            driver
                .run(spec(
                    &dir,
                    "plan",
                    None,
                    vec![
                        ("FAKE_AGENT_CAPTURE".to_string(), capture.to_string_lossy().to_string()),
                        ("FAKE_AGENT_PIDFILE".to_string(), pidfile.to_string_lossy().to_string()),
                        ("FAKE_AGENT_HANG".to_string(), "1".to_string()),
                    ],
                    vec![],
                ))
                .await
        })
    };

    // Wait until the fake agent is inside the hanging prompt call.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let events = read_capture(&capture);
        if events.iter().any(|e| e.get("prompt_hang").is_some()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fake agent never reached the prompt"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();

    // Cancellation = dropping the run future (timeout / human cancel, §5.5).
    task.abort();
    let _ = task.await;

    // The process must be gone — TERM immediately, KILL after the grace, then
    // reaped (a zombie still answers kill(pid, 0), so allow the full window).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        // SAFETY: signal 0 is a pure existence check.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agent process {pid} survived kill-on-drop"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    std::fs::remove_dir_all(&dir).ok();
}
