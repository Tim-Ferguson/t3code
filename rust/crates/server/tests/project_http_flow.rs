//! `GET /api/projects` and `POST /api/projects/mutate` (`project/http.ts`) over a real socket: the active projects, a mutation answered
//! with the project it left, the scopes each needs, and the errors the original names.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_are_listed_and_changed_over_http() {
    let h = harness().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://{}", h.address);
    let call = |method: reqwest::Method, path: &str, token: Option<&str>, body: Option<Value>| {
        let mut request = client.request(method, format!("{base}{path}"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.body(body.to_string()).header("content-type", "application/json");
        }
        async move {
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            let text = response.text().await.unwrap();
            (status, serde_json::from_str::<Value>(&text).unwrap_or(Value::Null), text)
        }
    };
    let workspace = h.base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let create = json!({"type":"project.create","commandId":"create","projectId":"p1","title":"One","workspaceRoot":workspace});

    assert_eq!(call(reqwest::Method::GET, "/api/projects", None, None).await.0, 401);
    // Reading needs read; changing needs operate.
    let (status, error, _) = call(reqwest::Method::POST, "/api/projects/mutate", Some(&h.reader), Some(create.clone())).await;
    assert_eq!((status, &error["requiredScope"]), (403, &json!("orchestration:operate")));
    // A body that is no mutation is an empty 400; a mutation the project service refuses is `invalid_command`.
    let (status, _, text) = call(reqwest::Method::POST, "/api/projects/mutate", Some(&h.operator), Some(json!({"type":"project.nonsense"}))).await;
    assert_eq!((status, text.as_str()), (400, "null"));
    let (status, error, _) = call(reqwest::Method::POST, "/api/projects/mutate", Some(&h.operator), Some(json!({"type":"project.update","commandId":"missing","projectId":"nobody","title":"x"}))).await;
    assert_eq!((status, &error["_tag"], &error["reason"]), (400, &json!("EnvironmentRequestInvalidError"), &json!("invalid_command")));

    let (status, project, _) = call(reqwest::Method::POST, "/api/projects/mutate", Some(&h.operator), Some(create)).await;
    assert_eq!(status, 200, "{project}");
    assert_eq!((&project["id"], &project["title"], &project["deletedAt"]), (&json!("p1"), &json!("One"), &Value::Null));
    assert!(project["scripts"].is_array() && project["createdAt"].is_string());
    let (_, snapshot, _) = call(reqwest::Method::GET, "/api/projects", Some(&h.reader), None).await;
    assert_eq!(snapshot["projects"].as_array().unwrap().len(), 1);
    assert!(snapshot["updatedAt"].is_string() && snapshot["projects"][0]["deletedAt"].is_null());
    let (_, renamed, _) = call(reqwest::Method::POST, "/api/projects/mutate", Some(&h.operator), Some(json!({"type":"project.update","commandId":"rename","projectId":"p1","title":"Renamed"}))).await;
    assert_eq!(renamed["title"], "Renamed");
    let (_, deleted, _) = call(reqwest::Method::POST, "/api/projects/mutate", Some(&h.operator), Some(json!({"type":"project.delete","commandId":"delete","projectId":"p1"}))).await;
    assert!(deleted["deletedAt"].is_string(), "{deleted}");
    let (_, snapshot, _) = call(reqwest::Method::GET, "/api/projects", Some(&h.reader), None).await;
    assert_eq!(snapshot["projects"], json!([]));
    h.finish().await;
}

/// `ProjectService.create` and `update` normalize the root a project is made in or moved to (`WorkspacePaths.normalizeWorkspaceRoot`)
/// before they commit: a folder that is not there, or that is a file, is refused unless `createWorkspaceRootIfMissing` makes it, and
/// the root kept is the absolute one. Over the socket and over HTTP alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_that_is_not_there_is_refused_unless_the_request_makes_it() {
    let h = harness().await;
    let mut socket = h.connect(&h.operator).await;
    let missing = h.base.join("missing");
    let file = h.base.join("a-file");
    std::fs::write(&file, "text").unwrap();
    let create = |id: &str, root: &std::path::Path, extra: Value| {
        let mut command = json!({"type":"project.create","commandId":id,"projectId":id,"title":"One","workspaceRoot":root});
        command.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        command
    };
    let kept = || t3_server::project::ProjectService::new(h.store.clone()).list().unwrap();

    let refused = rpc(&mut socket, 1, "projects.mutate", create("p1", &missing, json!({"createWorkspaceRootIfMissing":false}))).await;
    let error = failure(&refused);
    assert_eq!((&error["_tag"], &error["commandId"], &error["message"]), (&json!("ProjectMutationError"), &json!("p1"), &json!("Failed to mutate project.")));
    assert_eq!((&error["cause"]["_tag"], &error["cause"]["operation"]), (&json!("ProjectOperationError"), &json!("normalize-workspace")));
    assert_eq!(error["cause"]["cause"]["message"], format!("Workspace root does not exist: {}", missing.display()));
    // Not asking to make it is not asking: the flag is optional and false by default.
    let refused = rpc(&mut socket, 2, "projects.mutate", create("p2", &missing, json!({}))).await;
    assert_eq!(failure(&refused)["cause"]["operation"], "normalize-workspace");
    let refused = rpc(&mut socket, 3, "projects.mutate", create("p3", &file, json!({}))).await;
    assert_eq!(failure(&refused)["cause"]["cause"]["message"], format!("Workspace root is not a directory: {}", file.display()));
    assert!(kept().is_empty() && !missing.exists(), "nothing was kept or made");

    // Over HTTP the same refusal is `invalid_command`.
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client.post(format!("http://{}/api/projects/mutate", h.address)).bearer_auth(&h.operator).json(&create("p4", &missing, json!({}))).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(response.json::<Value>().await.unwrap()["reason"], "invalid_command");

    // A folder asked to be made is made, and the project keeps the absolute path of it.
    let made = rpc(&mut socket, 5, "projects.mutate", create("p5", &missing, json!({"createWorkspaceRootIfMissing":true}))).await;
    assert_eq!(success(&made)["workspaceRoot"], json!(missing));
    assert!(missing.is_dir());

    // Moving a project is the same: to a folder that is there, but not to one that is not.
    let there = h.base.join("there");
    std::fs::create_dir(&there).unwrap();
    let moved = rpc(&mut socket, 6, "projects.mutate", json!({"type":"project.update","commandId":"move","projectId":"p5","workspaceRoot":there})).await;
    assert_eq!(success(&moved)["workspaceRoot"], json!(there));
    let refused = rpc(&mut socket, 7, "projects.mutate", json!({"type":"project.update","commandId":"move-away","projectId":"p5","workspaceRoot":h.base.join("nowhere")})).await;
    assert_eq!(failure(&refused)["cause"]["operation"], "normalize-workspace");
    assert_eq!(kept()[0]["workspaceRoot"], json!(there));
    h.finish().await;
}
