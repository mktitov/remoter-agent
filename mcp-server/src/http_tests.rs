//! HTTP-level tests for the MCP tools: each test spins up a [`wiremock`]
//! server standing in for the Remoter backend and drives the real tool
//! handlers (params → HTTP request → result mapping), including the
//! daemon-mode supervise scoping. Complements the pure unit tests in
//! `tools.rs` by covering the client and the handler wiring.

use rmcp::handler::server::wrapper::Parameters;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path, query_param},
};

use crate::{
    Role,
    client::{RemoterClient, WhoAmI, WorkspaceMembership},
    tools::{
        AssignTaskParams, CreateTaskParams, ListBoardParams, ListGoalsParams, ListMyTasksParams, ListUsersParams,
        RemoterMcp, ReviewVerdictParam, SearchTasksParams, SetTaskGoalParams, SetTaskReviewParams, UpdateTaskParams,
    },
};

fn test_mcp(base_url: String, role: Role, agent_task_id: Option<i32>) -> RemoterMcp {
    RemoterMcp::new(
        RemoterClient::new(base_url, "dummy".into(), None),
        WhoAmI {
            id: 3,
            name: "supervisor".into(),
            kind: "agent".into(),
            workspaces: vec![WorkspaceMembership {
                id: 1,
                name: "My".into(),
                role: "member".into(),
            }],
            workspace_id: Some(1),
            workspace_name: Some("My".into()),
            role: Some("member".into()),
        },
        role,
        agent_task_id,
    )
}

/// Extract the JSON text block from a successful tool result.
fn result_text(result: rmcp::model::CallToolResult) -> String {
    result.content[0].as_text().expect("text content block").text.clone()
}

/// A full `TaskDetail` JSON fixture (camelCase) with the given task links.
fn task_detail_json(id: i32, links: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "projectId": 1,
        "projectName": "remoter",
        "featureId": 3,
        "featureDescription": "feature",
        "title": format!("task {id}"),
        "description": "d",
        "taskStatus": "backlog",
        "taskKind": "task",
        "taskPriority": "medium",
        "actionsTotal": 0,
        "actionsCompleted": 0,
        "actionsRejected": 0,
        "timeSpent": 0,
        "blocked": false,
        "assigneeId": null,
        "assigneeName": null,
        "completedAt": null,
        "actions": [],
        "report": null,
        "reportOutcomeId": null,
        "plan": null,
        "planOutcomeId": null,
        "review": null,
        "reviewOutcomeId": null,
        "reviewVerdict": null,
        "prUrl": null,
        "links": links,
    })
}

/// One `TaskLinkRef` JSON fixture (camelCase).
fn link_json(relation: &str, task_id: i32) -> serde_json::Value {
    serde_json::json!({
        "linkId": task_id * 10,
        "relation": relation,
        "taskId": task_id,
        "title": format!("task {task_id}"),
        "taskStatus": "backlog",
        "featureId": 3,
        "projectId": 1,
    })
}

#[tokio::test]
async fn list_users_returns_workspace_members() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/assignable"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": 2, "name": "Alice"},
            {"id": 3, "name": "Bob"},
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let result = mcp.list_users(Parameters(ListUsersParams {})).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("\"id\": 2"), "{text}");
    assert!(text.contains("Alice"), "{text}");
    assert!(text.contains("Bob"), "{text}");
}

#[tokio::test]
async fn assign_task_posts_assign_with_assignee_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/7/assign"))
        .and(body_json(serde_json::json!({"assigneeId": 3})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7, "assignee_id": 3,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: AssignTaskParams = serde_json::from_value(serde_json::json!({"taskId": 7, "assigneeId": 3})).unwrap();
    let result = mcp.assign_task(Parameters(p)).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("\"id\": 7"), "{text}");
}

#[tokio::test]
async fn set_task_review_puts_review_with_verdict() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/tasks/7/review"))
        .and(body_json(
            serde_json::json!({"body": "looks good", "verdict": "approve"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 77, "task_id": 7, "kind": "review", "body": "looks good", "verdict": "approve",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: SetTaskReviewParams =
        serde_json::from_value(serde_json::json!({"taskId": 7, "markdown": "looks good", "verdict": "approve"}))
            .unwrap();
    assert_eq!(p.verdict, ReviewVerdictParam::Approve);
    let result = mcp.set_task_review(Parameters(p)).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("\"kind\": \"review\""), "{text}");
}

#[tokio::test]
async fn set_task_review_rejects_unknown_verdict() {
    let p: Result<SetTaskReviewParams, _> =
        serde_json::from_value(serde_json::json!({"taskId": 7, "markdown": "x", "verdict": "lgtm"}));
    assert!(p.is_err(), "unknown verdict must fail param parsing");
}

#[tokio::test]
async fn assign_task_without_assignee_id_posts_unassign() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/7/unassign"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7, "assignee_id": null,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    // Standalone-style params (no assigneeId) — the daemon-mode child gate is
    // covered by the dedicated tests below; here the ticket 70 owns task 7.
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/7"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(task_detail_json(7, serde_json::json!([link_json("subtask", 70)]))),
        )
        .mount(&server)
        .await;

    let p: AssignTaskParams = serde_json::from_value(serde_json::json!({"taskId": 7})).unwrap();
    let result = mcp.assign_task(Parameters(p)).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("\"id\": 7"), "{text}");
}

#[tokio::test]
async fn assign_task_in_daemon_mode_rejects_non_child() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/71"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(71, serde_json::json!([]))))
        .mount(&server)
        .await;
    // The assign endpoint must not be hit for a non-child task.
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/71/assign"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: AssignTaskParams = serde_json::from_value(serde_json::json!({"taskId": 71, "assigneeId": 3})).unwrap();
    let err = mcp.assign_task(Parameters(p)).await.unwrap_err();
    assert!(err.message.contains("not a child"), "{err:?}");
}

#[tokio::test]
async fn assign_task_in_daemon_mode_assigns_child() {
    let server = MockServer::start().await;
    // From the child's perspective its link to the ticket carries "subtask".
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/71"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(task_detail_json(71, serde_json::json!([link_json("subtask", 70)]))),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/71/assign"))
        .and(body_json(serde_json::json!({"assigneeId": 3})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 71, "assignee_id": 3,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: AssignTaskParams = serde_json::from_value(serde_json::json!({"taskId": 71, "assigneeId": 3})).unwrap();
    let result = mcp.assign_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 71"));
}

#[tokio::test]
async fn create_task_standalone_with_assignee_id_creates_then_assigns() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/features/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 5, "projectId": 1, "description": "f", "tasksTotal": 0, "tasksCompleted": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks"))
        .and(body_json(serde_json::json!({
            "projectId": 1,
            "featureId": 5,
            "title": "Child",
            "description": "do it",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 42, "title": "Child"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/42/assign"))
        .and(body_json(serde_json::json!({"assigneeId": 9})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 42, "assignee_id": 9})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "featureId": 5,
        "assigneeId": 9,
    }))
    .unwrap();
    let result = mcp.create_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 42"));
}

#[tokio::test]
async fn create_task_without_assignee_id_does_not_assign() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/features/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 5, "projectId": 1, "description": "f", "tasksTotal": 0, "tasksCompleted": 0,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 43})))
        .mount(&server)
        .await;
    // No assign endpoint is mocked: a stray assign call would get a 404 and
    // fail the tool, so a green result proves assignment was skipped.
    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "featureId": 5,
    }))
    .unwrap();
    let result = mcp.create_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 43"));
}

#[tokio::test]
async fn create_task_daemon_mode_with_assignee_id_links_and_assigns() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/70"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(70, serde_json::json!([]))))
        .expect(1) // project/feature lookup for the child body
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks"))
        .and(body_json(serde_json::json!({
            "projectId": 1,
            "featureId": 3,
            "title": "Child",
            "description": "do it",
            "parentTaskId": 70,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 44})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/44/assign"))
        .and(body_json(serde_json::json!({"assigneeId": 9})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 44, "assignee_id": 9})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "assigneeId": 9,
    }))
    .unwrap();
    let result = mcp.create_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 44"));
}

#[tokio::test]
async fn create_task_forwards_task_kind() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/features/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 5, "projectId": 1, "description": "f", "tasksTotal": 0, "tasksCompleted": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks"))
        .and(body_json(serde_json::json!({
            "projectId": 1,
            "featureId": 5,
            "title": "Crash on startup",
            "description": "repro steps",
            "taskKind": "bug",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 45})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Crash on startup",
        "description": "repro steps",
        "featureId": 5,
        "taskKind": "bug",
    }))
    .unwrap();
    let result = mcp.create_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 45"));
}

#[tokio::test]
async fn create_task_without_task_kind_omits_the_field() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/features/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 5, "projectId": 1, "description": "f", "tasksTotal": 0, "tasksCompleted": 0,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks"))
        .and(body_json(serde_json::json!({
            "projectId": 1,
            "featureId": 5,
            "title": "Plain task",
            "description": "no kind",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 46})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Plain task",
        "description": "no kind",
        "featureId": 5,
    }))
    .unwrap();
    let result = mcp.create_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 46"));
}

#[tokio::test]
async fn create_task_daemon_cross_project_creates_in_target_project() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/70"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(70, serde_json::json!([]))))
        .expect(1) // current ticket lookup for its project id
        .mount(&server)
        .await;
    // The cross-project contract (remoter#182): the MCP server verifies
    // feature↔project ownership via `GET /features/{id}` before POSTing.
    Mock::given(method("GET"))
        .and(path("/api/v1/features/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 9, "projectId": 2, "description": "f", "tasksTotal": 0, "tasksCompleted": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks"))
        .and(body_json(serde_json::json!({
            "projectId": 2,
            "featureId": 9,
            "title": "Child",
            "description": "do it",
            "parentTaskId": 70,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 44})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "projectId": 2,
        "featureId": 9,
    }))
    .unwrap();
    let result = mcp.create_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 44"));
}

#[tokio::test]
async fn create_task_daemon_cross_project_requires_feature_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/70"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(70, serde_json::json!([]))))
        .expect(1)
        .mount(&server)
        .await;
    // No POST /tasks mock: the call must fail before any creation happens.

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "projectId": 2,
    }))
    .unwrap();
    let err = mcp.create_task(Parameters(p)).await.unwrap_err();
    assert!(err.message.contains("featureId is required"), "{err:?}");
}

#[tokio::test]
async fn create_task_daemon_cross_project_feature_mismatch_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/70"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(70, serde_json::json!([]))))
        .expect(1)
        .mount(&server)
        .await;
    // Feature 9 belongs to project 1, not the requested project 2.
    Mock::given(method("GET"))
        .and(path("/api/v1/features/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 9, "projectId": 1, "description": "f", "tasksTotal": 0, "tasksCompleted": 0,
        })))
        .expect(1)
        .mount(&server)
        .await;
    // No POST /tasks mock: the mismatch must fail before any creation happens.

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "projectId": 2,
        "featureId": 9,
    }))
    .unwrap();
    let err = mcp.create_task(Parameters(p)).await.unwrap_err();
    assert!(err.message.contains("belongs to project 1"), "{err:?}");
}

#[tokio::test]
async fn create_task_daemon_project_id_equal_to_current_behaves_as_before() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/70"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(70, serde_json::json!([]))))
        .expect(2) // both create_task calls below look up the current ticket
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks"))
        .and(body_json(serde_json::json!({
            "projectId": 1,
            "featureId": 3,
            "title": "Child",
            "description": "do it",
            "parentTaskId": 70,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 44})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    // projectId equal to the current ticket's project is a no-op; featureId
    // stays banned in that case.
    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "projectId": 1,
    }))
    .unwrap();
    let result = mcp.create_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 44"));

    let p: CreateTaskParams = serde_json::from_value(serde_json::json!({
        "title": "Child",
        "description": "do it",
        "projectId": 1,
        "featureId": 3,
    }))
    .unwrap();
    let err = mcp.create_task(Parameters(p)).await.unwrap_err();
    assert!(err.message.contains("featureId must not be provided"), "{err:?}");
}

/// Gating contract: `create_task` stays available in implement/supervise and
/// gated away in plan/review — cross-project support changes nothing here.
#[test]
fn create_task_role_gates_are_unchanged() {
    for role in [Role::DevAgentPlan, Role::DevAgentReview] {
        assert!(role.is_gated("create_task"), "create_task must be gated in {role}");
    }
    for role in [Role::Full, Role::DevAgentImplement, Role::DevAgentSupervise] {
        assert!(!role.is_gated("create_task"), "create_task must be open in {role}");
    }
}

#[tokio::test]
async fn update_task_forwards_task_kind() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/tasks/7"))
        .and(body_json(serde_json::json!({"taskKind": "research"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 7})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: UpdateTaskParams = serde_json::from_value(serde_json::json!({"taskId": 7, "taskKind": "research"})).unwrap();
    let result = mcp.update_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 7"));
}

#[tokio::test]
async fn update_task_forwards_task_kind_alongside_title() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/tasks/7"))
        .and(body_json(serde_json::json!({"title": "Retitled", "taskKind": "bug"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 7})))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: UpdateTaskParams =
        serde_json::from_value(serde_json::json!({"taskId": 7, "title": "Retitled", "taskKind": "bug"})).unwrap();
    let result = mcp.update_task(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 7"));
}

#[tokio::test]
async fn supervise_role_scopes_list_tools_to_ticket_and_children() {
    let server = MockServer::start().await;
    // Ticket 70 with one child (71) and one unrelated relates-link (99). From
    // the ticket's perspective the child link carries the "parent" relation.
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/70"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(
            70,
            serde_json::json!([link_json("parent", 71), link_json("relates", 99)]),
        )))
        .mount(&server)
        .await;

    let board_card = |id| {
        serde_json::json!({
            "id": id,
            "projectId": 1,
            "projectName": "remoter",
            "featureId": 3,
            "featureDescription": "feature",
            "title": format!("task {id}"),
            "taskStatus": "backlog",
            "assigneeId": null,
            "assigneeName": null,
            "actionsTotal": 0,
            "actionsCompleted": 0,
            "completedAt": null,
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/boards"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            board_card(70),
            board_card(71),
            board_card(72),
        ])))
        .mount(&server)
        .await;

    let search_item = |id| {
        serde_json::json!({
            "id": id,
            "title": format!("task {id}"),
            "taskStatus": "backlog",
            "projectId": 1,
            "featureId": 3,
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks"))
        .and(query_param("search", "task"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([search_item(71), search_item(82),])))
        .mount(&server)
        .await;

    let summary = |id| {
        serde_json::json!({
            "id": id,
            "projectId": 1,
            "projectName": "remoter",
            "featureId": 3,
            "featureDescription": "feature",
            "title": format!("task {id}"),
            "description": "d",
            "taskStatus": "backlog",
            "taskKind": "task",
            "taskPriority": "medium",
            "actionsTotal": 0,
            "actionsCompleted": 0,
            "actionsRejected": 0,
            "timeSpent": 0,
            "blocked": false,
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks"))
        .and(query_param("assignee", "me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([summary(70), summary(80),])))
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));

    // list_board: only the ticket and its child stay visible.
    let p: ListBoardParams = serde_json::from_value(serde_json::json!({})).unwrap();
    let text = result_text(mcp.list_board(Parameters(p)).await.unwrap());
    assert!(text.contains("\"id\": 70"), "{text}");
    assert!(text.contains("\"id\": 71"), "{text}");
    assert!(!text.contains("\"id\": 72"), "{text}");

    // search_tasks: the out-of-scope hit is filtered out.
    let p: SearchTasksParams = serde_json::from_value(serde_json::json!({"query": "task"})).unwrap();
    let text = result_text(mcp.search_tasks(Parameters(p)).await.unwrap());
    assert!(text.contains("\"id\": 71"), "{text}");
    assert!(!text.contains("\"id\": 82"), "{text}");

    // list_my_tasks: same filtering.
    let p: ListMyTasksParams = serde_json::from_value(serde_json::json!({})).unwrap();
    let text = result_text(mcp.list_my_tasks(Parameters(p)).await.unwrap());
    assert!(text.contains("\"id\": 70"), "{text}");
    assert!(!text.contains("\"id\": 80"), "{text}");
}

#[tokio::test]
async fn daemon_mode_without_supervise_role_is_not_scoped() {
    let server = MockServer::start().await;
    let board_card = |id| {
        serde_json::json!({
            "id": id,
            "projectId": 1,
            "projectName": "remoter",
            "featureId": 3,
            "featureDescription": "feature",
            "title": format!("task {id}"),
            "taskStatus": "backlog",
            "assigneeId": null,
            "assigneeName": null,
            "actionsTotal": 0,
            "actionsCompleted": 0,
            "completedAt": null,
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/v1/boards"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([board_card(70), board_card(72),])))
        .mount(&server)
        .await;
    // The ticket lookup used by supervise scoping must NOT happen for the
    // implement role; it is not mocked, so a scope fetch would 404 the tool.
    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(70));
    let p: ListBoardParams = serde_json::from_value(serde_json::json!({})).unwrap();
    let text = result_text(mcp.list_board(Parameters(p)).await.unwrap());
    assert!(text.contains("\"id\": 70"), "{text}");
    assert!(text.contains("\"id\": 72"), "{text}");
}

/// One `TaskQuestionWithOptions` JSON fixture (entity JSON is snake_case).
fn question_json(id: i32, status: &str, answer_text: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "task_id": 7,
        "author_id": 3,
        "body": format!("question {id}"),
        "multiple": false,
        "status": status,
        "answer_text": answer_text,
        "answerer_id": null,
        "answered_at": null,
        "created_at": "2026-01-01T00:00:00Z",
        "options": [
            {"id": id * 10, "question_id": id, "body": "yes", "position": 0, "selected": status == "answered"},
            {"id": id * 10 + 1, "question_id": id, "body": "no", "position": 1, "selected": false},
        ],
    })
}

#[tokio::test]
async fn add_task_question_posts_body_options_and_multiple() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/7/questions"))
        .and(body_json(serde_json::json!({
            "body": "Which backend?",
            "multiple": true,
            "options": ["postgres", "sqlite"],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(question_json(11, "open", serde_json::Value::Null)))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentPlan, Some(7));
    let p: crate::tools::AddTaskQuestionParams = serde_json::from_value(serde_json::json!({
        "taskId": 7,
        "body": "Which backend?",
        "options": ["postgres", "sqlite"],
        "multiple": true,
    }))
    .unwrap();
    let result = mcp.add_task_question(Parameters(p)).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("\"id\": 11"), "{text}");
    assert!(text.contains("\"status\": \"open\""), "{text}");
    // The body_json matcher above already proves `options` and `multiple`
    // were forwarded in the POST body.
}

#[tokio::test]
async fn add_task_question_defaults_to_no_options_single_choice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/7/questions"))
        .and(body_json(serde_json::json!({
            "body": "Free-form?",
            "multiple": false,
            "options": [],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(question_json(12, "open", serde_json::Value::Null)))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentPlan, Some(7));
    let p: crate::tools::AddTaskQuestionParams =
        serde_json::from_value(serde_json::json!({"taskId": 7, "body": "Free-form?"})).unwrap();
    let result = mcp.add_task_question(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 12"));
}

#[tokio::test]
async fn list_task_questions_returns_questions_with_answers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/7/questions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            question_json(11, "answered", serde_json::json!("use postgres")),
            question_json(12, "open", serde_json::Value::Null),
        ])))
        .expect(1)
        .mount(&server)
        .await;

    // Available in every role — supervise included.
    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(7));
    let p: crate::tools::ListTaskQuestionsParams = serde_json::from_value(serde_json::json!({"taskId": 7})).unwrap();
    let result = mcp.list_task_questions(Parameters(p)).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("\"status\": \"answered\""), "{text}");
    assert!(text.contains("use postgres"), "{text}");
    assert!(text.contains("\"selected\": true"), "{text}");
}

#[tokio::test]
async fn delete_task_question_sends_delete() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/tasks/7/questions/11"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(7));
    let p: crate::tools::DeleteTaskQuestionParams =
        serde_json::from_value(serde_json::json!({"taskId": 7, "questionId": 11})).unwrap();
    let result = mcp.delete_task_question(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("deleted"));
}

#[tokio::test]
async fn get_task_requests_and_returns_questions() {
    let server = MockServer::start().await;
    let mut detail = task_detail_json(7, serde_json::json!([]));
    detail["questions"] = serde_json::json!([question_json(11, "answered", serde_json::json!("yes"))]);
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/7"))
        .and(query_param("include", "actions,links,questions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(detail))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentSupervise, Some(70));
    let p: crate::tools::TaskIdParams = serde_json::from_value(serde_json::json!({"taskId": 7})).unwrap();
    let result = mcp.get_task(Parameters(p)).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("\"questions\""), "{text}");
    assert!(text.contains("\"status\": \"answered\""), "{text}");
}

/// Gating contract (spec mcp-server-for-agents): `add_task_question` is
/// plan-only, `delete_task_question` is plan+implement, `list_task_questions`
/// is available in every role.
#[test]
fn task_question_tools_are_role_gated() {
    for role in [Role::Full, Role::DevAgentImplement, Role::DevAgentSupervise] {
        assert!(
            role.is_gated("add_task_question"),
            "add_task_question must be gated in {role}"
        );
    }
    assert!(!Role::DevAgentPlan.is_gated("add_task_question"));
    for role in [Role::Full, Role::DevAgentSupervise] {
        assert!(
            role.is_gated("delete_task_question"),
            "delete_task_question must be gated in {role}"
        );
    }
    assert!(!Role::DevAgentPlan.is_gated("delete_task_question"));
    assert!(!Role::DevAgentImplement.is_gated("delete_task_question"));
    for role in [
        Role::Full,
        Role::DevAgentPlan,
        Role::DevAgentImplement,
        Role::DevAgentSupervise,
    ] {
        assert!(
            !role.is_gated("list_task_questions"),
            "list_task_questions must be open in {role}"
        );
    }
    // A gated call is rejected before any HTTP happens.
    let mcp = test_mcp("http://localhost:9999".into(), Role::DevAgentImplement, Some(7));
    let err = mcp.gate_tool("add_task_question").unwrap_err();
    assert!(err.message.contains("not available"), "{err:?}");
    let mcp = test_mcp("http://localhost:9999".into(), Role::DevAgentSupervise, Some(70));
    assert!(mcp.gate_tool("delete_task_question").is_err());
}

/// Regression: schemars 0.8 (via rmcp 0.6) emits the OpenAPI-only `nullable`
/// keyword — `{"type": T, "nullable": true}` for optional scalars and an anyOf
/// branch `{"const": null, "nullable": true}` for optional enums (taskKind on
/// create_task/update_task). Strict MCP clients reject those tools' schemas
/// outright. Every schema advertised by `list_tools` must be clean after the
/// `schema::sanitize_tools` rewrite.
#[test]
fn advertised_tool_schemas_contain_no_nullable() {
    let mcp = test_mcp("http://localhost".into(), Role::Full, None);
    for tool in crate::schema::sanitize_tools(mcp.tool_router.list_all()) {
        assert_no_nullable(&serde_json::Value::Object((*tool.input_schema).clone()), &tool.name);
    }
}

fn assert_no_nullable(value: &serde_json::Value, path: &str) {
    match value {
        serde_json::Value::Object(map) => {
            assert!(
                !map.contains_key("nullable"),
                "{path}: `nullable` survived sanitization: {map:?}"
            );
            for (key, item) in map {
                assert_no_nullable(item, &format!("{path}.{key}"));
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_no_nullable(item, path);
            }
        }
        _ => {}
    }
}

/// Contract: MCP clients learn which text fields accept Markdown and which
/// are plain text from the advertised `tools/list` schemas alone — the field
/// doc comments become JSON Schema descriptions
/// (docs/specs/mcp-server-for-agents.md §3, "Text fields: Markdown vs plain
/// text").
#[test]
fn advertised_tool_schemas_document_markdown_and_plain_text_fields() {
    let mcp = test_mcp("http://localhost".into(), Role::Full, None);
    let tools = mcp.tool_router.list_all();
    let find_tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("tool {name} is not advertised"))
    };
    let prop_desc = |tool: &str, prop: &str| -> String {
        find_tool(tool).input_schema["properties"][prop]["description"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}.{prop} has no description in the advertised schema"))
            .to_string()
    };
    for (tool, prop) in [
        ("create_task", "description"),
        ("update_task", "description"),
        ("add_task_comment", "body"),
        ("add_task_question", "body"),
        ("set_task_report", "report"),
        ("set_task_review", "markdown"),
    ] {
        let desc = prop_desc(tool, prop);
        assert!(
            desc.to_lowercase().contains("markdown"),
            "{tool}.{prop} must advertise Markdown support, got: {desc}"
        );
    }
    for (tool, prop) in [
        ("add_action", "description"),
        ("update_action", "description"),
        ("reject_action", "reason"),
    ] {
        let desc = prop_desc(tool, prop);
        assert!(
            desc.to_lowercase().contains("plain text"),
            "{tool}.{prop} must advertise plain-text-only, got: {desc}"
        );
    }
    for name in ["create_task", "update_task"] {
        let desc = find_tool(name).description.as_deref().unwrap_or("");
        assert!(
            desc.to_lowercase().contains("markdown"),
            "{name} tool description must mention Markdown, got: {desc}"
        );
    }
}

/// Gating contract (remoter#162, spec remoter-agent.md §4.3):
/// `request_human_action` is the working agent's pause button — gated in
/// Full (humans move tickets in the UI), Supervise (not the supervisor's
/// ticket), and Review (read-only); open in Plan and Implement.
#[test]
fn request_human_action_is_role_gated() {
    for role in [Role::Full, Role::DevAgentSupervise, Role::DevAgentReview] {
        assert!(
            role.is_gated("request_human_action"),
            "request_human_action must be gated in {role}"
        );
    }
    for role in [Role::DevAgentPlan, Role::DevAgentImplement] {
        assert!(
            !role.is_gated("request_human_action"),
            "request_human_action must be open in {role}"
        );
    }
    // A gated call is rejected before any HTTP happens.
    let mcp = test_mcp("http://localhost:9999".into(), Role::DevAgentSupervise, Some(70));
    let err = mcp.gate_tool("request_human_action").unwrap_err();
    assert!(err.message.contains("not available"), "{err:?}");
}

#[tokio::test]
async fn request_human_action_posts_wait_with_reason() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/7/wait"))
        .and(body_json(serde_json::json!({"reason": "Create the public repository"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7, "taskStatus": "waiting",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(70));
    let p: crate::tools::RequestHumanActionParams =
        serde_json::from_value(serde_json::json!({"taskId": 7, "description": "Create the public repository"}))
            .unwrap();
    let result = mcp.request_human_action(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"waiting\""));
}

/// Daemon mode: an omitted taskId defaults to the current ticket.
#[tokio::test]
async fn request_human_action_daemon_mode_defaults_to_current_ticket() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/70/wait"))
        .and(body_json(serde_json::json!({"reason": "Rotate the deploy key"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 70, "taskStatus": "waiting",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentPlan, Some(70));
    let p: crate::tools::RequestHumanActionParams =
        serde_json::from_value(serde_json::json!({"description": "Rotate the deploy key"})).unwrap();
    let result = mcp.request_human_action(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 70"));
}

/// Standalone mode without taskId is a tool error before any HTTP happens.
#[tokio::test]
async fn request_human_action_standalone_requires_task_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, None);
    let p: crate::tools::RequestHumanActionParams =
        serde_json::from_value(serde_json::json!({"description": "d"})).unwrap();
    let err = mcp.request_human_action(Parameters(p)).await.unwrap_err();
    assert!(err.message.contains("taskId is required"), "{err:?}");
}

/// Backward compatibility (remoter#162): a backend older than the waiting
/// rollout answers 404 — the tool surfaces the HTTP error to the agent
/// instead of hanging or panicking.
#[tokio::test]
async fn request_human_action_surfaces_404_from_old_backend() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/tasks/7/wait"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "not found",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(7));
    let p: crate::tools::RequestHumanActionParams =
        serde_json::from_value(serde_json::json!({"description": "d"})).unwrap();
    let err = mcp.request_human_action(Parameters(p)).await.unwrap_err();
    assert!(err.message.to_lowercase().contains("not found"), "{err:?}");
}

fn whoami_json() -> serde_json::Value {
    serde_json::json!({
        "id": 3,
        "name": "agent",
        "kind": "agent",
        "workspaces": [{"id": 1, "name": "My", "role": "member"}],
    })
}

/// Startup validation survives a transient 5xx blip: 503, 503, then 200.
#[tokio::test]
async fn validate_with_retry_recovers_from_transient_503() {
    let server = MockServer::start().await;
    // Mounted first, matched first; after 2 hits wiremock falls through to 200.
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(whoami_json()))
        .expect(1)
        .mount(&server)
        .await;

    let client = RemoterClient::new(server.uri(), "dummy".into(), None);
    let whoami = crate::validate_with_retry(&client, 5, std::time::Duration::from_millis(1))
        .await
        .unwrap();
    assert_eq!(whoami.id, 3);
}

/// 401 is permanent: exactly one request, no retries.
#[tokio::test]
async fn validate_with_retry_fails_fast_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "Unauthorized",
            "message": "token revoked",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = RemoterClient::new(server.uri(), "dummy".into(), None);
    let err = crate::validate_with_retry(&client, 5, std::time::Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(!err.is_retryable());
}

/// A persistent 503 exhausts all attempts and returns the last error.
#[tokio::test]
async fn validate_with_retry_exhausts_attempts_on_persistent_503() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;

    let client = RemoterClient::new(server.uri(), "dummy".into(), None);
    let err = crate::validate_with_retry(&client, 3, std::time::Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(err.is_retryable());
}

// ── Business goals (remoter#199 / remoter-agent#202) ─────────────────────

#[tokio::test]
async fn list_goals_with_explicit_project_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/projects/1/goals"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": 3, "projectId": 1, "title": "Grow revenue"},
            {"id": 4, "projectId": 1, "title": "Retention"},
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: ListGoalsParams = serde_json::from_value(serde_json::json!({"projectId": 1})).unwrap();
    let result = mcp.list_goals(Parameters(p)).await.unwrap();
    let text = result_text(result);
    assert!(text.contains("Grow revenue"), "{text}");
    assert!(text.contains("Retention"), "{text}");
}

/// Daemon mode without projectId resolves the current ticket's project first.
#[tokio::test]
async fn list_goals_daemon_mode_defaults_to_ticket_project() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/70"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(70, serde_json::json!([]))))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/projects/1/goals"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": 3, "projectId": 1, "title": "Grow revenue"},
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(70));
    let p: ListGoalsParams = serde_json::from_value(serde_json::json!({})).unwrap();
    let result = mcp.list_goals(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("Grow revenue"));
}

#[tokio::test]
async fn list_goals_standalone_requires_project_id() {
    let server = MockServer::start().await;
    // No goals mock: the call must fail before any HTTP request.
    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: ListGoalsParams = serde_json::from_value(serde_json::json!({})).unwrap();
    let err = mcp.list_goals(Parameters(p)).await.unwrap_err();
    assert!(err.message.contains("projectId is required"), "{err:?}");
}

#[tokio::test]
async fn set_task_goal_patches_goal_id() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/tasks/7/set-goal"))
        .and(body_json(serde_json::json!({"goalId": 3})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7, "goalId": 3,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: SetTaskGoalParams = serde_json::from_value(serde_json::json!({"taskId": 7, "goalId": 3})).unwrap();
    let result = mcp.set_task_goal(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"goalId\": 3"));
}

/// Absent goalId serializes as an explicit null — that is how the task is
/// unlinked from its goal.
#[tokio::test]
async fn set_task_goal_without_goal_id_unlinks() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/tasks/7/set-goal"))
        .and(body_json(serde_json::json!({"goalId": null})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7, "goalId": null,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::Full, None);
    let p: SetTaskGoalParams = serde_json::from_value(serde_json::json!({"taskId": 7})).unwrap();
    let result = mcp.set_task_goal(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"goalId\": null"));
}

/// Daemon mode: the current ticket itself is in scope (no child-link check).
#[tokio::test]
async fn set_task_goal_daemon_mode_allows_current_ticket() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/tasks/70/set-goal"))
        .and(body_json(serde_json::json!({"goalId": 3})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 70, "goalId": 3,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(70));
    let p: SetTaskGoalParams = serde_json::from_value(serde_json::json!({"taskId": 70, "goalId": 3})).unwrap();
    let result = mcp.set_task_goal(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 70"));
}

/// Daemon mode: a subtask of the current ticket is in scope (from the child's
/// perspective its link to the ticket carries "subtask").
#[tokio::test]
async fn set_task_goal_daemon_mode_allows_child() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/71"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(task_detail_json(71, serde_json::json!([link_json("subtask", 70)]))),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/tasks/71/set-goal"))
        .and(body_json(serde_json::json!({"goalId": 3})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 71, "goalId": 3,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(70));
    let p: SetTaskGoalParams = serde_json::from_value(serde_json::json!({"taskId": 71, "goalId": 3})).unwrap();
    let result = mcp.set_task_goal(Parameters(p)).await.unwrap();
    assert!(result_text(result).contains("\"id\": 71"));
}

/// Daemon mode: an unrelated task is rejected before any PATCH is sent.
#[tokio::test]
async fn set_task_goal_daemon_mode_rejects_unrelated_task() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/71"))
        .respond_with(ResponseTemplate::new(200).set_body_json(task_detail_json(71, serde_json::json!([]))))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/tasks/71/set-goal"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mcp = test_mcp(server.uri(), Role::DevAgentImplement, Some(70));
    let p: SetTaskGoalParams = serde_json::from_value(serde_json::json!({"taskId": 71, "goalId": 3})).unwrap();
    let err = mcp.set_task_goal(Parameters(p)).await.unwrap_err();
    assert!(err.message.contains("not the current ticket or its subtask"), "{err:?}");
}
