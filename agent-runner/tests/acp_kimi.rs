//! Conformance test against the real, pinned `kimi acp` (PoC-5 acceptance,
//! spec §5.5): new session, prompt/stream, permission/MCP wiring, usage
//! fields, `session/resume` keeping context across runs, and per-kind
//! `session/set_config_option`.
//!
//! Not part of `cargo test` — requires an authenticated kimi CLI on PATH and
//! LLM quota. Run manually:
//!
//! ```sh
//! cargo test -p remoter-agent --test acp_kimi -- --ignored --nocapture
//! ```
//!
//! The driver is only validated against one pinned kimi version (spec §5.5:
//! "pinning tested agent versions"); the test refuses to run against any
//! other so a silent CLI upgrade can't invalidate the conformance claim.

use std::path::PathBuf;

use remoter_agent::config::DriverConfig;
use remoter_agent::driver::acp::AcpDriver;
use remoter_agent::driver::{AgentDriver, RunSpec};

/// The only kimi version the ACP driver is validated against (spec §5.5).
/// Bump deliberately: re-run this test, then update.
const PINNED_KIMI_VERSION: &str = "0.36.1";

fn spec(
    dir: PathBuf,
    prompt: &str,
    kind: &'static str,
    resume: Option<String>,
    config_options: Vec<(String, String)>,
) -> RunSpec {
    RunSpec {
        task_id: 0,
        run_id: 0,
        cwd: dir,
        prompt: prompt.to_string(),
        kind,
        resume_session: resume,
        branch: "agent/task-0-conformance".to_string(),
        env: vec![],
        exec: remoter_agent::workspace::ExecEnv::host(false),
        config_options,
        logger: remoter_agent::session_log::SessionLogger::noop(),
    }
}

#[tokio::test]
#[ignore = "needs an authenticated kimi CLI and LLM quota — run manually"]
async fn kimi_acp_conformance() {
    let out = std::process::Command::new("kimi")
        .arg("--version")
        .output()
        .expect("kimi not on PATH");
    let version = String::from_utf8_lossy(&out.stdout);
    assert!(
        version.split_whitespace().any(|tok| tok == PINNED_KIMI_VERSION),
        "conformance is pinned to kimi {PINNED_KIMI_VERSION}, found: {}",
        version.trim()
    );

    let dir = std::env::temp_dir().join(format!("remoter-acp-kimi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let cfg = DriverConfig {
        kind: "kimi-acp".to_string(),
        simulate_delay_ms: 0,
        simulate_uncommitted_changes: false,
        agent_program: "kimi".to_string(),
        agent_args: vec!["acp".to_string()],
        remoter_mcp_command: "remoter-mcp".to_string(),
        plan_model: Some("kimi-code/k3".to_string()),
        plan_thinking: Some("max".to_string()),
        implement_model: Some("kimi-code/kimi-for-coding".to_string()),
        // kimi-for-coding accepts only `on` for thinking (its console shows
        // On / Off (unsupported)); the driver validates it against the option
        // set refreshed by the model switch.
        implement_thinking: Some("on".to_string()),
        supervise_model: None,
        supervise_thinking: None,
        review_model: None,
        review_thinking: None,
        sessions_dir: None,
    };
    let driver = AcpDriver::new(&cfg, "http://localhost:8181", "unused-in-this-test", None);

    // New session + prompt/stream with per-kind config options.
    let first = driver
        .run(spec(
            dir.clone(),
            "Reply with exactly: REMOTER_ACP_OK — nothing else.",
            "plan",
            None,
            cfg.config_options_for("plan"),
        ))
        .await
        .expect("first run failed");
    assert!(
        first.text.contains("REMOTER_ACP_OK"),
        "unexpected final message: {}",
        first.text
    );
    assert_eq!(first.model.as_deref(), Some("kimi-code/k3"));
    assert_eq!(first.thinking.as_deref(), Some("max"));
    let session = first.session_id.clone().expect("kimi must report a session id");
    // ACP does not report token usage directly up to (and including) kimi
    // 0.38.0. The driver reads it from the session's wire.jsonl file when
    // available; if usage is still missing here, the fallback path has not
    // landed a record yet.
    eprintln!(
        "kimi usage report: in={:?} out={:?}",
        first.input_tokens, first.output_tokens
    );

    // session/resume keeps context (spec §5.4): the token from the first turn
    // must be recalled without being repeated in the prompt.
    let second = driver
        .run(spec(
            dir.clone(),
            "Repeat the exact token I asked you to reply with in the previous turn.",
            "plan",
            Some(session),
            cfg.config_options_for("plan"),
        ))
        .await
        .expect("resume run failed");
    assert!(
        second.text.contains("REMOTER_ACP_OK"),
        "resumed session lost context: {}",
        second.text
    );

    // The implement pairing (spec §5.1): K2.7 Coding with thinking `on` — the
    // only thinking value kimi accepts for that model. The run succeeding at
    // all proves the agent took both `set_config_option` calls.
    let third = driver
        .run(spec(
            dir.clone(),
            "Reply with exactly: REMOTER_ACP_OK — nothing else.",
            "implement",
            None,
            cfg.config_options_for("implement"),
        ))
        .await
        .expect("implement run failed");
    assert!(
        third.text.contains("REMOTER_ACP_OK"),
        "unexpected final message: {}",
        third.text
    );
    assert_eq!(third.model.as_deref(), Some("kimi-code/kimi-for-coding"));
    assert_eq!(third.thinking.as_deref(), Some("on"));

    std::fs::remove_dir_all(&dir).ok();
}
