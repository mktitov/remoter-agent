//! Optional conformance smoke test for the OpenCode ACP integration.
//!
//! Run manually with an authenticated OpenCode installation:
//!
//! ```sh
//! cargo test -p remoter-agent --test acp_opencode -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use remoter_agent::config::DriverConfig;
use remoter_agent::driver::acp::AcpDriver;
use remoter_agent::driver::{AgentDriver, RunSpec};

fn spec(dir: PathBuf) -> RunSpec {
    RunSpec {
        task_id: 0,
        run_id: 0,
        cwd: dir,
        prompt: "Reply with exactly: REMOTER_OPENCODE_OK".to_string(),
        kind: "implement",
        resume_session: None,
        branch: "agent/task-0-opencode-conformance".to_string(),
        env: vec![],
        exec: remoter_agent::workspace::ExecEnv::host(false),
        config_options: vec![],
        logger: remoter_agent::session_log::SessionLogger::noop(),
    }
}

#[tokio::test]
#[ignore = "needs an authenticated OpenCode installation and provider quota"]
async fn opencode_acp_conformance() {
    let version = std::process::Command::new("opencode")
        .arg("--version")
        .output()
        .expect("opencode not on PATH");
    assert!(version.status.success(), "opencode --version failed");

    let dir = std::env::temp_dir().join(format!("remoter-acp-opencode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let cfg = DriverConfig {
        kind: "opencode-acp".to_string(),
        simulate_delay_ms: 0,
        simulate_uncommitted_changes: false,
        agent_program: "opencode".to_string(),
        agent_args: vec!["acp".to_string()],
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
    let driver = AcpDriver::new(&cfg, "http://localhost:8181", "unused-in-this-test", None);
    let outcome = driver.run(spec(dir.clone())).await.expect("OpenCode ACP run failed");
    assert!(
        outcome.text.contains("REMOTER_OPENCODE_OK"),
        "unexpected output: {}",
        outcome.text
    );
    assert!(outcome.session_id.is_some(), "OpenCode must return an ACP session id");
    assert_eq!(outcome.model, None, "model is intentionally configured by OpenCode");

    std::fs::remove_dir_all(dir).ok();
}
