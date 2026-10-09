//! `secrets.answerRequest` over a real socket: saving keeps the value under a one-use ref and marks
//! the card saved, declining marks it declined, and every other state of the request refuses.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use t3_server::secret_requests::SecretRequests;

fn card(id: &str, run: &str, status: &str) -> Value {
    json!({"id":id,"threadId":"thread","runId":run,"nodeId":"node","providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":1,"status":"waiting","title":"API key","startedAt":"2026-01-01T00:00:00.000Z","completedAt":null,"updatedAt":"2026-01-01T00:00:00.000Z","type":"secret_request","label":"API key","reason":"To call the API","secretStatus":status})
}

fn seed(h: &Harness) {
    t3_server::launch::ThreadLaunchService::new(h.store.clone())
        .launch(json!({"commandId":"launch","threadId":"thread","projectId":"project","title":"T","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}}), chrono::Utc::now())
        .unwrap();
}

#[test]
fn answering_a_secret_request_keeps_the_value_once_and_records_the_card() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        let mut operator = h.connect(&h.operator).await;
        let mut reader = h.connect(&h.reader).await;
        std::fs::create_dir_all(h.base.join("work")).unwrap();
        success(&rpc(&mut operator, 1, "projects.mutate", json!({"type":"project.create","commandId":"p","projectId":"project","title":"P","workspaceRoot":h.base.join("work")})).await);
        seed(&h);
        // A running run that owns the cards, and a finished one that owns a stale card.
        let mut thread = h.store.projection("thread", "thread").unwrap().unwrap();
        let run = |id: &str, status: &str| json!({"id":id,"threadId":"thread","rootNodeId":"node","status":status});
        thread["runs"] = json!([run("run-live", "running"), run("run-done", "completed")]);
        thread["turnItems"] = json!([card("ask-save", "run-live", "pending"), card("ask-decline", "run-live", "pending"), card("ask-stale", "run-done", "pending"), card("ask-answered", "run-live", "declined")]);
        h.store.transaction(|transaction| t3_server::persistence::write_projection(transaction, "thread", "thread", &thread)).unwrap();
        let secrets = SecretRequests::for_store(&h.store).unwrap();

        // Authorized, decoded, and refused before anything is kept.
        let denied = rpc(&mut reader, 2, "secrets.answerRequest", json!({"threadId":"thread","turnItemId":"ask-save","answer":{"type":"decline"}})).await;
        assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
        for payload in [
            json!({"threadId":"thread","turnItemId":"ask-save","answer":{"type":"save","secret":"   "}}),
            json!({"threadId":"thread","turnItemId":"ask-save","answer":{"type":"maybe"}}),
            json!({"threadId":"thread","answer":{"type":"decline"}}),
        ] {
            assert!(!defect(&rpc(&mut operator, 3, "secrets.answerRequest", payload.clone()).await).is_null(), "{payload}");
        }
        let error = |reason: &str| json!({"_tag":"SecretRequestError","reason":reason});
        let missing = rpc(&mut operator, 4, "secrets.answerRequest", json!({"threadId":"thread","turnItemId":"nobody","answer":{"type":"decline"}})).await;
        assert_eq!(failure(&missing), &error("not_found"));
        let stale = rpc(&mut operator, 5, "secrets.answerRequest", json!({"threadId":"thread","turnItemId":"ask-stale","answer":{"type":"save","secret":"late"}})).await;
        assert_eq!(failure(&stale), &error("agent_stopped"));
        assert_eq!(secrets.saved_ref("thread", "ask-stale"), None, "nothing is kept for a run that has ended");
        let answered = rpc(&mut operator, 6, "secrets.answerRequest", json!({"threadId":"thread","turnItemId":"ask-answered","answer":{"type":"save","secret":"again"}})).await;
        assert_eq!(failure(&answered), &error("already_answered"));
        let no_thread = rpc(&mut operator, 7, "secrets.answerRequest", json!({"threadId":"nowhere","turnItemId":"ask-save","answer":{"type":"decline"}})).await;
        assert_eq!(failure(&no_thread)["reason"], "load_failed");

        // Declining records the card and keeps nothing.
        assert_eq!(success(&rpc(&mut operator, 8, "secrets.answerRequest", json!({"threadId":"thread","turnItemId":"ask-decline","answer":{"type":"decline"}})).await), &Value::Null);
        let view = h.store.projection("thread", "thread").unwrap().unwrap();
        let item = |id: &str| view["turnItems"].as_array().unwrap().iter().find(|item| item["id"] == id).unwrap().clone();
        assert_eq!((item("ask-decline")["secretStatus"].clone(), item("ask-decline")["status"].clone()), (json!("declined"), json!("cancelled")));
        assert_eq!(secrets.saved_ref("thread", "ask-decline"), None);
        // Answered once: a second answer is refused.
        let twice = rpc(&mut operator, 9, "secrets.answerRequest", json!({"threadId":"thread","turnItemId":"ask-decline","answer":{"type":"save","secret":"x"}})).await;
        assert_eq!(failure(&twice), &error("already_answered"));

        // Saving keeps the (trimmed) value under the request's ref, and the ref works exactly once, for its project.
        assert_eq!(success(&rpc(&mut operator, 10, "secrets.answerRequest", json!({"threadId":"thread","turnItemId":"ask-save","answer":{"type":"save","secret":"  hunter2  "}})).await), &Value::Null);
        let view = h.store.projection("thread", "thread").unwrap().unwrap();
        let saved = view["turnItems"].as_array().unwrap().iter().find(|item| item["id"] == "ask-save").unwrap().clone();
        assert_eq!((saved["secretStatus"].clone(), saved["status"].clone()), (json!("saved"), json!("completed")));
        assert!(!view.to_string().contains("hunter2"), "the value never reaches a projection");
        let reference = secrets.saved_ref("thread", "ask-save").unwrap();
        assert!(reference.starts_with("secret-ref:") && reference.len() == "secret-ref:".len() + 32);
        assert_eq!(secrets.consume(&reference, "elsewhere").unwrap_err().reason, "ref_unavailable");
        assert_eq!(secrets.consume("secret-ref:zz", "project").unwrap_err().reason, "invalid_ref");
        assert_eq!(secrets.consume(&reference, "project").unwrap(), "hunter2");
        assert_eq!(secrets.consume(&reference, "project").unwrap_err().reason, "ref_unavailable");
        assert_eq!(secrets.saved_ref("thread", "ask-save"), None);
        h.finish().await;
    }));
}
