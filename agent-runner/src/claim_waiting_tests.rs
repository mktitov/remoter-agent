use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

use crate::{
    claim::Daemon,
    client::{AgentRunDto, CommentDto, LinkDto, RemoterClient},
    config::Config,
    driver::{AgentDriver, RunOutcome, RunSpec, stub::StubDriver},
    logstore::LogStore,
};

const AGENT_ID: i32 = 3;
const HUMAN_ID: i32 = 1;
const WAIT_PREFIX: &str = "⏳ Ожидание действия человека:";

#[derive(Clone)]
struct TaskState {
    id: i32,
    title: String,
    status: String,
    supervision_enabled: bool,
    links: Vec<LinkDto>,
    comments: Vec<CommentDto>,
    runs: Vec<AgentRunDto>,
}

impl TaskState {
    fn new(id: i32, title: &str, status: &str) -> Self {
        Self {
            id,
            title: title.to_string(),
            status: status.to_string(),
            supervision_enabled: true,
            links: vec![],
            comments: vec![],
            runs: vec![],
        }
    }
}

struct BackendState {
    tasks: HashMap<i32, TaskState>,
    repo_url: String,
    next_run_id: i32,
    next_comment_id: i32,
    status_writes: Vec<(i32, String)>,
}

#[derive(Clone)]
struct FakeBackend {
    url: String,
    state: Arc<Mutex<BackendState>>,
}

impl FakeBackend {
    async fn spawn(tasks: Vec<TaskState>, repo_url: String) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(BackendState {
            tasks: tasks.into_iter().map(|task| (task.id, task)).collect(),
            repo_url,
            next_run_id: 1,
            next_comment_id: 1,
            status_writes: vec![],
        }));
        let shared = state.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let shared = shared.clone();
                tokio::spawn(async move {
                    let Some((method, path, body)) = read_request(&mut socket).await else {
                        return;
                    };
                    let (status, response) = route(shared, &method, &path, &body).await;
                    let response = response.to_string();
                    let headers = format!(
                        "HTTP/1.1 {status} Reason\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response.len()
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self { url, state }
    }

    async fn human_done(&self, task_id: i32, comment: Option<&str>) {
        let mut state = self.state.lock().await;
        if let Some(body) = comment {
            add_comment(&mut state, task_id, HUMAN_ID, body);
        }
        state.tasks.get_mut(&task_id).unwrap().status = "implement".into();
    }

    async fn status(&self, task_id: i32) -> String {
        self.state.lock().await.tasks[&task_id].status.clone()
    }

    async fn comments(&self, task_id: i32) -> Vec<CommentDto> {
        self.state.lock().await.tasks[&task_id].comments.clone()
    }

    async fn run_count(&self, task_id: i32) -> usize {
        self.state.lock().await.tasks[&task_id].runs.len()
    }

    async fn status_writes(&self) -> Vec<(i32, String)> {
        self.state.lock().await.status_writes.clone()
    }
}

fn add_comment(state: &mut BackendState, task_id: i32, author_id: i32, body: &str) {
    let id = state.next_comment_id;
    state.next_comment_id += 1;
    state.tasks.get_mut(&task_id).unwrap().comments.push(CommentDto {
        id,
        task_id,
        author_id,
        body: body.to_string(),
        created_at: format!("2026-11-21T00:00:{id:02}Z"),
    });
}

fn run_json(run: &AgentRunDto) -> Value {
    json!({
        "id": run.id,
        "task_id": run.task_id,
        "agent_user_id": run.agent_user_id,
        "kind": run.kind,
        "status": run.status,
        "session_id": run.session_id,
        "branch": run.branch,
        "plan": run.plan,
        "summary": run.summary,
        "error": run.error,
        "pr_url": run.pr_url,
        "pr_error": run.pr_error,
        "input_tokens": run.input_tokens,
        "output_tokens": run.output_tokens,
        "attempt": run.attempt,
        "phase": run.phase,
        "created_at": run.created_at,
    })
}

fn summary_json(task: &TaskState) -> Value {
    json!({
        "id": task.id,
        "projectId": 1,
        "projectName": "stateful",
        "featureId": 1,
        "featureDescription": "resume",
        "title": task.title,
        "description": "stateful waiting/resume regression",
        "taskStatus": task.status,
        "taskPriority": "medium",
        "actionsTotal": 0,
        "actionsCompleted": 0,
        "timeSpent": 0,
        "blocked": false,
        "agentReviewRequested": false,
        "goalId": null,
    })
}

fn detail_json(task: &TaskState) -> Value {
    let comments: Vec<Value> = task
        .comments
        .iter()
        .map(|comment| {
            json!({
                "id": comment.id,
                "task_id": comment.task_id,
                "author_id": comment.author_id,
                "body": comment.body,
                "created_at": comment.created_at,
            })
        })
        .collect();
    let links: Vec<Value> = task
        .links
        .iter()
        .map(|link| {
            json!({
                "relation": link.relation,
                "taskId": link.task_id,
                "createdBy": link.created_by,
            })
        })
        .collect();
    let runs: Vec<Value> = task.runs.iter().map(run_json).collect();
    let mut detail = summary_json(task);
    detail["assigneeId"] = json!(AGENT_ID);
    detail["assigneeName"] = json!("stateful-agent");
    detail["actions"] = json!([]);
    detail["runs"] = json!(runs);
    detail["comments"] = json!(comments);
    detail["attachments"] = json!([]);
    detail["links"] = json!(links);
    detail["questions"] = json!([]);
    detail["report"] = json!(null);
    detail["review"] = json!(null);
    detail["supervisionEnabled"] = json!(task.supervision_enabled);
    detail
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, String, Value)> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let count = socket.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let count = socket.read(&mut chunk).await.ok()?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
    }
    let mut first_line = headers.lines().next()?.split_whitespace();
    let method = first_line.next()?.to_string();
    let path = first_line.next()?.to_string();
    let body = if content_length == 0 {
        json!({})
    } else {
        serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap_or(json!({}))
    };
    Some((method, path, body))
}

async fn route(state: Arc<Mutex<BackendState>>, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let mut state = state.lock().await;
    if method == "GET" && path == "/api/v1/auth/me" {
        return (
            200,
            json!({"id":AGENT_ID,"name":"stateful-agent","kind":"agent","workspaces":[{"id":1,"name":"Test","role":"owner"}]}),
        );
    }
    if method == "GET" && path == "/api/v1/projects" {
        return (
            200,
            json!([{"id":1,"repo_url":state.repo_url,"base_branch":"main","staging_auto_start":false,"reference_repos":[]}]),
        );
    }
    if method == "GET" && path.starts_with("/api/v1/projects/") && path.ends_with("/forge-config") {
        return (200, json!({"forgeKind":null,"forgeApiUrl":null,"forgeToken":null}));
    }
    if method == "GET" && path.starts_with("/api/v1/tasks?") {
        let status = path.split('&').find_map(|part| part.strip_prefix("status="));
        let mut tasks: Vec<&TaskState> = state
            .tasks
            .values()
            .filter(|task| status.is_none_or(|status| task.status == status))
            .collect();
        tasks.sort_by_key(|task| task.id);
        return (200, json!(tasks.into_iter().map(summary_json).collect::<Vec<_>>()));
    }
    if let Some(rest) = path.strip_prefix("/api/v1/agent-runs/") {
        let mut parts = rest.split('/');
        let run_id = parts.next().unwrap().parse::<i32>().unwrap();
        if method == "PATCH" && parts.next() == Some("phase") {
            for task in state.tasks.values_mut() {
                if let Some(run) = task.runs.iter_mut().find(|run| run.id == run_id) {
                    run.phase = body["phase"].as_str().map(str::to_string);
                    return (200, json!({}));
                }
            }
        }
        if method == "PATCH" {
            for task in state.tasks.values_mut() {
                if let Some(run) = task.runs.iter_mut().find(|run| run.id == run_id) {
                    run.status = body["status"].as_str().unwrap().to_string();
                    run.session_id = body["sessionId"].as_str().map(str::to_string);
                    run.branch = body["branch"].as_str().map(str::to_string);
                    run.summary = body["summary"].as_str().map(str::to_string);
                    run.plan = body["plan"].as_str().map(str::to_string);
                    run.error = body["error"].as_str().map(str::to_string);
                    run.input_tokens = body["inputTokens"].as_i64();
                    run.output_tokens = body["outputTokens"].as_i64();
                    return (200, run_json(run));
                }
            }
        }
    }
    let Some(rest) = path.strip_prefix("/api/v1/tasks/") else {
        return (404, json!({"error":"unknown route","method":method,"path":path}));
    };
    let mut parts = rest.split('/');
    let task_id = parts.next().unwrap().split('?').next().unwrap().parse::<i32>().unwrap();
    let operation = parts.next();
    match (method, operation) {
        ("GET", None) => (200, detail_json(&state.tasks[&task_id])),
        ("GET", Some("agent-runs")) => (
            200,
            json!(state.tasks[&task_id].runs.iter().map(run_json).collect::<Vec<_>>()),
        ),
        ("PATCH", Some("status")) => {
            let status = body["taskStatus"].as_str().unwrap().to_string();
            state.status_writes.push((task_id, status.clone()));
            let current = state.tasks[&task_id].status.clone();
            if current == "waiting" && status == "review" {
                return (403, json!({"error":"waiting is human-owned"}));
            }
            state.tasks.get_mut(&task_id).unwrap().status = status;
            (200, json!({}))
        }
        ("POST", Some("comments")) => {
            add_comment(&mut state, task_id, AGENT_ID, body["body"].as_str().unwrap());
            (200, json!({}))
        }
        ("POST", Some("wait")) => {
            let reason = body["reason"].as_str().unwrap();
            add_comment(&mut state, task_id, AGENT_ID, &format!("{WAIT_PREFIX} {reason}"));
            state.tasks.get_mut(&task_id).unwrap().status = "waiting".into();
            (200, json!({}))
        }
        ("POST", Some("agent-runs")) => {
            let id = state.next_run_id;
            state.next_run_id += 1;
            let run = AgentRunDto {
                id,
                task_id,
                agent_user_id: AGENT_ID,
                kind: body["kind"].as_str().unwrap().to_string(),
                status: "running".into(),
                session_id: None,
                branch: None,
                plan: None,
                summary: None,
                error: None,
                pr_url: None,
                pr_error: None,
                input_tokens: None,
                output_tokens: None,
                attempt: 1,
                phase: None,
                created_at: None,
            };
            state.tasks.get_mut(&task_id).unwrap().runs.insert(0, run.clone());
            (200, run_json(&run))
        }
        _ => (404, json!({"error":"unknown task route","method":method,"path":path})),
    }
}

enum Step {
    Wait(String),
    Commit,
}

struct ScriptedDriver {
    api_url: String,
    steps: Mutex<VecDeque<Step>>,
    specs: Mutex<Vec<RunSpec>>,
}

impl ScriptedDriver {
    fn new(api_url: &str, steps: Vec<Step>) -> Self {
        Self {
            api_url: api_url.to_string(),
            steps: Mutex::new(steps.into()),
            specs: Mutex::new(vec![]),
        }
    }

    async fn specs(&self) -> Vec<RunSpec> {
        self.specs.lock().await.clone()
    }
}

#[async_trait]
impl AgentDriver for ScriptedDriver {
    async fn run(&self, spec: RunSpec) -> Result<RunOutcome, crate::driver::RunFailure> {
        self.specs.lock().await.push(spec.clone());
        match self
            .steps
            .lock()
            .await
            .pop_front()
            .expect("unexpected extra driver turn")
        {
            Step::Wait(reason) => {
                let response = reqwest::Client::new()
                    .post(format!("{}/api/v1/tasks/{}/wait", self.api_url, spec.task_id))
                    .bearer_auth("tok")
                    .json(&json!({"reason":reason}))
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success(), "POST /wait failed: {response:?}");
                Ok(RunOutcome {
                    session_id: Some(format!("session-{}", spec.run_id)),
                    text: "paused for a human".into(),
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    model: None,
                    thinking: None,
                })
            }
            Step::Commit => StubDriver::new(0, false).run(spec).await,
        }
    }
}

struct Harness {
    daemon: Daemon,
    backend: FakeBackend,
    driver: Arc<ScriptedDriver>,
    client: RemoterClient,
    root: PathBuf,
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(cwd).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_origin(test_name: &str) -> (PathBuf, PathBuf) {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("remoter-waiting-{test_name}-{}-{unique}", std::process::id()));
    let seed = root.join("seed");
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "stateful test\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(
        &seed,
        &[
            "-c",
            "user.email=stateful@example.test",
            "-c",
            "user.name=stateful-test",
            "commit",
            "-m",
            "initial",
        ],
    );
    git(&root, &["clone", "--bare", "seed", "origin.git"]);
    git(
        &root,
        &["--git-dir=origin.git", "symbolic-ref", "HEAD", "refs/heads/main"],
    );
    (root, origin)
}

async fn harness(test_name: &str, tasks: Vec<TaskState>, steps: Vec<Step>, supervised: bool) -> Harness {
    let (root, origin) = init_origin(test_name);
    if supervised {
        let project = root.join("p1");
        std::fs::create_dir_all(&project).unwrap();
        git(&project, &["clone", "../origin.git", "repo"]);
        git(&project.join("repo"), &["branch", "agent/task-10-parent"]);
    }
    let backend = FakeBackend::spawn(tasks, origin.to_string_lossy().into_owned()).await;
    // SAFETY: the explicit test api_url must win over the daemon's environment.
    unsafe { std::env::remove_var("REMOTER_API_URL") };
    let toml = format!(
        r#"
api_url = "{}"
workspace_root = "{}"
max_attempts = 1
cancel_grace_secs = 0

[driver]
kind = "stub"
"#,
        backend.url,
        root.display()
    );
    let config = Arc::new(Config::from_toml_str(&toml, Some("tok".into())).unwrap());
    let log_store = LogStore::new(&config.logs).unwrap();
    let client = RemoterClient::new(&backend.url, "tok", None);
    let driver = Arc::new(ScriptedDriver::new(&backend.url, steps));
    let daemon = Daemon::new(config, client.clone(), driver.clone(), log_store);
    Harness {
        daemon,
        backend,
        driver,
        client,
        root,
    }
}

async fn poll_run(harness: &mut Harness) {
    harness.daemon.poll_once().await;
    harness.daemon.wait_idle().await;
}

fn cleanup(harness: &Harness) {
    std::fs::remove_dir_all(&harness.root).ok();
}

#[tokio::test]
async fn equivalent_wait_after_human_done_stops_the_daemon_resume_loop() {
    let mut harness = harness(
        "equivalent",
        vec![TaskState::new(42, "waiting loop", "implement")],
        vec![
            Step::Wait("Approve deployment".into()),
            Step::Wait("  approve   deployment ".into()),
        ],
        false,
    )
    .await;

    poll_run(&mut harness).await;
    assert_eq!(harness.backend.status(42).await, "waiting");
    assert_eq!(harness.backend.run_count(42).await, 1);

    harness.backend.human_done(42, Some("Approved; continue.")).await;
    poll_run(&mut harness).await;

    assert_eq!(harness.backend.status(42).await, "waiting");
    assert_eq!(harness.backend.run_count(42).await, 2);
    let specs = harness.driver.specs().await;
    assert_eq!(specs.len(), 2, "a paused run must receive no cleanup/action nudges");
    assert_eq!(specs[1].resume_session.as_deref(), Some("session-1"));
    assert!(specs[1].prompt.contains("## Human action confirmed"));
    assert!(specs[1].prompt.contains("Approve deployment"));
    assert!(specs[1].prompt.contains("Approved; continue."));
    let comments = harness.backend.comments(42).await;
    let diagnostic = comments
        .iter()
        .find(|comment| comment.body.contains("[waiting-loop-stop]"))
        .expect("anti-loop diagnostic");
    assert!(diagnostic.body.contains("тикетом #42"));
    assert!(diagnostic.body.contains("run #2"));
    assert!(diagnostic.body.contains("run #1"));
    assert!(diagnostic.body.contains("новыми фактами"));
    assert!(diagnostic.body.contains("«Готово — продолжить»"));

    poll_run(&mut harness).await;
    poll_run(&mut harness).await;
    assert_eq!(harness.backend.run_count(42).await, 2, "waiting must not be re-claimed");
    assert_eq!(harness.driver.specs().await.len(), 2);
    assert!(
        !harness.backend.status_writes().await.contains(&(42, "review".into())),
        "a paused ticket must never be moved to review"
    );
    cleanup(&harness);
}

#[tokio::test]
async fn waiting_loop_stop_preserves_resume_context_for_the_next_human_done() {
    let mut harness = harness(
        "loop-stop-resume",
        vec![TaskState::new(45, "resume after loop stop", "implement")],
        vec![
            Step::Wait("Approve deployment".into()),
            Step::Wait("Approve deployment".into()),
            Step::Wait("Approve deployment".into()),
        ],
        false,
    )
    .await;

    poll_run(&mut harness).await;
    harness.backend.human_done(45, Some("Approved; continue.")).await;
    poll_run(&mut harness).await;
    assert!(
        harness
            .backend
            .comments(45)
            .await
            .iter()
            .any(|comment| { comment.body == "run #2 paused: waiting for human action" })
    );

    harness
        .backend
        .human_done(
            45,
            Some("The approval is now recorded externally; verify it and continue."),
        )
        .await;
    poll_run(&mut harness).await;

    let specs = harness.driver.specs().await;
    assert_eq!(specs.len(), 3);
    assert_eq!(specs[2].resume_session.as_deref(), Some("session-2"));
    let resume = specs[2]
        .prompt
        .split("## Human action confirmed\n")
        .nth(1)
        .expect("resume context after anti-loop stop");
    assert!(resume.starts_with("Previous implement run #2"), "{resume}");
    assert!(resume.contains("Approve deployment"), "{resume}");
    assert!(
        resume.contains("The approval is now recorded externally; verify it and continue."),
        "{resume}"
    );

    let comments = harness.backend.comments(45).await;
    assert!(comments.iter().any(|comment| {
        comment.body.contains("[waiting-loop-stop]")
            && comment.body.contains("run #3")
            && comment.body.contains("run #2")
    }));
    poll_run(&mut harness).await;
    assert_eq!(harness.backend.run_count(45).await, 3);
    cleanup(&harness);
}

#[tokio::test]
async fn reformulated_waits_without_human_input_are_bounded() {
    let mut harness = harness(
        "reformulated",
        vec![TaskState::new(43, "rephrased waiting loop", "implement")],
        vec![
            Step::Wait("Approve the parent PR".into()),
            Step::Wait("Personally review and approve the specification PR".into()),
            Step::Wait("The specification still requires personal approval".into()),
        ],
        false,
    )
    .await;

    poll_run(&mut harness).await;
    harness.backend.human_done(43, None).await;
    poll_run(&mut harness).await;
    assert_eq!(harness.backend.status(43).await, "waiting");
    assert!(
        !harness
            .backend
            .comments(43)
            .await
            .iter()
            .any(|comment| comment.body.contains("[waiting-loop-stop]")),
        "the first different action is a legitimate pause"
    );

    harness.backend.human_done(43, None).await;
    poll_run(&mut harness).await;
    poll_run(&mut harness).await;

    assert_eq!(harness.backend.run_count(43).await, 3);
    assert_eq!(harness.driver.specs().await.len(), 3);
    let comments = harness.backend.comments(43).await;
    assert!(comments.iter().any(|comment| {
        comment.body.contains("[waiting-loop-stop]")
            && comment.body.contains("run #3")
            && comment.body.contains("run #2")
    }));
    cleanup(&harness);
}

#[tokio::test]
async fn human_comment_and_a_different_action_allow_legitimate_repeated_waits() {
    let mut harness = harness(
        "legitimate",
        vec![TaskState::new(44, "legitimate repeated waits", "implement")],
        vec![
            Step::Wait("Approve deployment".into()),
            Step::Wait("Verify the recorded deployment approval".into()),
            Step::Wait("Configure the production DNS record".into()),
        ],
        false,
    )
    .await;

    poll_run(&mut harness).await;
    harness
        .backend
        .human_done(44, Some("Deployment approved; remaining checks can continue."))
        .await;
    poll_run(&mut harness).await;
    assert_eq!(harness.backend.status(44).await, "waiting");
    assert!(
        !harness
            .backend
            .comments(44)
            .await
            .iter()
            .any(|comment| comment.body.contains("[waiting-loop-stop]")),
        "new human input resets the reformulated-wait sequence"
    );

    harness.backend.human_done(44, None).await;
    poll_run(&mut harness).await;
    assert_eq!(harness.backend.status(44).await, "waiting");
    assert_eq!(harness.backend.run_count(44).await, 3);
    assert!(
        !harness
            .backend
            .comments(44)
            .await
            .iter()
            .any(|comment| comment.body.contains("[waiting-loop-stop]")),
        "a different human-only action remains legal"
    );
    cleanup(&harness);
}

#[tokio::test]
async fn supervised_child_reaches_review_and_only_then_triggers_supervisor() {
    let parent = TaskState::new(10, "parent", "review");
    let mut child = TaskState::new(11, "child", "implement");
    child.links.push(LinkDto {
        relation: "subtask".into(),
        task_id: parent.id,
        created_by: Some(AGENT_ID),
    });
    let mut parent = parent;
    parent.links.push(LinkDto {
        relation: "parent".into(),
        task_id: child.id,
        created_by: Some(AGENT_ID),
    });
    let mut harness = harness(
        "supervised",
        vec![parent, child],
        vec![Step::Wait("Perform the external approval".into()), Step::Commit],
        true,
    )
    .await;

    poll_run(&mut harness).await;
    assert_eq!(harness.backend.status(11).await, "waiting");
    assert!(harness.daemon.supervisor_mut().poll(&harness.client).await.is_empty());

    harness
        .backend
        .human_done(11, Some("External approval completed."))
        .await;
    poll_run(&mut harness).await;
    assert_eq!(harness.backend.status(11).await, "review");

    let triggers = harness.daemon.supervisor_mut().poll(&harness.client).await;
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].parent.id, 10);
    assert_eq!(triggers[0].review_children, vec![11]);
    cleanup(&harness);
}
