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
