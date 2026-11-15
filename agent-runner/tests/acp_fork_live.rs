//! Shared live `session/fork` conformance for Kimi, OpenCode, Claude Code,
//! and Codex. These tests prove context inheritance and source-session
//! isolation; they deliberately make no prompt-cache claim without provider
//! usage metrics. Run one adapter at a time with authenticated CLIs:
//!
//! ```sh
//! cargo test -p remoter-agent --test acp_fork_live -- --ignored --nocapture --test-threads=1
//! ```

use std::path::{Path, PathBuf};

use remoter_agent::config::DriverConfig;
use remoter_agent::driver::acp::AcpDriver;
use remoter_agent::driver::{AgentDriver, RunSpec};
use remoter_agent::question::WorktreeSnapshot;

fn config(kind: &str, program: &str, args: &[&str]) -> DriverConfig {
    DriverConfig {
        kind: kind.to_string(),
        simulate_delay_ms: 0,
        simulate_uncommitted_changes: false,
        agent_program: program.to_string(),
        agent_args: args.iter().map(|s| s.to_string()).collect(),
        kimi_config: PathBuf::from("unused-kimi-config.toml"),
        remoter_mcp_command: "remoter-mcp".to_string(),
        plan_model: None,
        plan_thinking: None,
        implement_model: None,
        implement_thinking: None,
        supervise_model: None,
        supervise_thinking: None,
        review_model: None,
        review_thinking: None,
        question_model: None,
        question_thinking: None,
        sessions_dir: None,
    }
}

fn spec(dir: &Path, kind: &'static str, prompt: &str) -> RunSpec {
    RunSpec {
        task_id: 0,
        run_id: 0,
        cwd: dir.to_path_buf(),
        prompt: prompt.to_string(),
        kind,
        resume_session: None,
        fork_session: None,
        fallback_prompt: None,
        branch: "agent/task-0-fork-conformance".to_string(),
        env: vec![],
        exec: remoter_agent::workspace::ExecEnv::host(false),
        config_options: vec![],
        logger: remoter_agent::session_log::SessionLogger::noop(),
        phase_tx: None,
    }
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("README"), "fork conformance\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "README"],
        vec![
            "-c",
            "user.email=acp@test",
            "-c",
            "user.name=acp-test",
            "commit",
            "-qm",
            "initial",
        ],
    ] {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

fn print_version(program: &str) {
    let output = std::process::Command::new(program)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| panic!("{program} --version failed: {e}"));
    assert!(output.status.success(), "{program} --version failed");
    eprintln!(
        "tested adapter: {program} {}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
}

async fn exercise_fork(kind: &str, program: &str, args: &[&str], marker: &str) {
    print_version(program);
    let dir = std::env::temp_dir().join(format!("remoter-acp-fork-{kind}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    init_repo(&dir);
    let before = WorktreeSnapshot::capture(&dir).await.unwrap();
    let driver = AcpDriver::new(
        &config(kind, program, args),
        "http://localhost:8181",
        "unused-in-this-test",
        None,
        300,
        30,
    );

    let mut source_spec = spec(
        &dir,
        "implement",
        &format!("Remember the secret marker {marker}. Reply with exactly MARKER_STORED."),
    );
    source_spec.run_id = 1;
    let source = driver.run(source_spec).await.expect("source session");
    assert!(source.text.contains("MARKER_STORED"), "{}", source.text);
    let source_id = source.session_id.expect("source session id");

    let mut question = spec(
        &dir,
        "question",
        "What was the secret marker I asked you to remember? Return only that marker.",
    );
    question.run_id = 2;
    question.fork_session = Some(source_id.clone());
    question.fallback_prompt = Some("Fork is mandatory in this live conformance test.".to_string());
    let answer = driver.run(question).await.expect("fork question");
    let fork_id = answer.session_id.expect("fork session id");
    assert_ne!(fork_id, source_id, "fork reused the source session id");
    assert!(
        answer.text.contains(marker),
        "fork lost source context: {}",
        answer.text
    );

    let mut resume = spec(
        &dir,
        "implement",
        "Before this prompt, did any user turn in this source conversation ask for the secret marker to be repeated? Answer exactly SOURCE_ISOLATED if no, or CONTEXT_LEAK if yes.",
    );
    resume.run_id = 3;
    resume.resume_session = Some(source_id);
    let isolated = driver.run(resume).await.expect("source resume");
    assert!(
        isolated.text.contains("SOURCE_ISOLATED") && !isolated.text.contains("CONTEXT_LEAK"),
        "Q&A leaked into the source history: {}",
        isolated.text
    );
    assert_eq!(before, WorktreeSnapshot::capture(&dir).await.unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "needs authenticated kimi and provider quota"]
async fn kimi_fork_conformance() {
    let output = std::process::Command::new("kimi").arg("--version").output().unwrap();
    let version = String::from_utf8_lossy(&output.stdout);
    assert!(
        version.split_whitespace().any(|part| part == "0.41.0"),
        "fork conformance was validated with kimi 0.41.0, found: {version}"
    );
    exercise_fork("kimi-acp", "kimi", &["acp"], "SECRET-KIMI-FORK-7319").await;
}

#[tokio::test]
#[ignore = "needs authenticated opencode and provider quota"]
async fn opencode_fork_conformance() {
    exercise_fork("opencode-acp", "opencode", &["acp"], "SECRET-OPENCODE-FORK-7319").await;
}

#[tokio::test]
#[ignore = "needs claude-agent-acp, authenticated Claude Code, and provider quota"]
async fn claude_code_fork_conformance() {
    exercise_fork("acp", "claude-agent-acp", &[], "SECRET-CLAUDE-FORK-7319").await;
}

#[tokio::test]
#[ignore = "needs codex-acp, authenticated Codex, and provider quota"]
async fn codex_fork_conformance() {
    exercise_fork("acp", "codex-acp", &[], "SECRET-CODEX-FORK-7319").await;
}
