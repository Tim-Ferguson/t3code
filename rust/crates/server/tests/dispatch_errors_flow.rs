//! A refused command reaches the client as the original's typed error, over a real socket:
//! `OrchestrationV2DispatchCommandError` carrying the reason, and
//! `OrchestrationV2ThreadLaunchError` for a launch. Never a `NativeServiceError` with the
//! store's own `Invalid command: ` prefix.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};

fn create(thread: &str, project: &str) -> Value {
    json!({"type":"thread.create","createdBy":"user","creationSource":"web","commandId":format!("create:{thread}"),"threadId":thread,"projectId":project,"title":"Thread","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null})
}

#[test]
fn refused_commands_arrive_as_the_originals_errors() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        let mut operator = h.connect(&h.operator).await;
        let directory = h.base.join("workspace");
        std::fs::create_dir(&directory).unwrap();
        let project = success(&rpc(&mut operator, 1, "projects.mutate", json!({"type":"project.create","commandId":"project","projectId":"project","title":"P","workspaceRoot":directory})).await).clone();
        assert!(project.is_object());
        success(&rpc(&mut operator, 2, "orchestration.dispatchCommand", create("thread", "project")).await);

        // A thread command the planner refuses: the receipt is rejected, the client sees the reason.
        let archive = json!({"type":"thread.unarchive","commandId":"unarchive","threadId":"thread"});
        let error = failure(&rpc(&mut operator, 3, "orchestration.dispatchCommand", archive.clone()).await).clone();
        assert_eq!(error["_tag"], "OrchestrationV2DispatchCommandError");
        assert_eq!(error["commandId"], "unarchive");
        assert_eq!(error["commandType"], "thread.unarchive");
        assert_eq!(error["message"], "Thread thread is not archived.");
        assert_eq!(error["detail"], "Thread thread is not archived.");
        assert_eq!(error["cause"]["name"], "OrchestratorDispatchError");
        assert_eq!(error["cause"]["cause"], "Thread thread is not archived.");

        // The same command id again is a replay of a rejected command.
        let replay = failure(&rpc(&mut operator, 4, "orchestration.dispatchCommand", archive).await).clone();
        assert_eq!(replay["_tag"], "OrchestrationV2DispatchCommandError");
        assert!(replay["message"].as_str().unwrap().starts_with("Command unarchive was previously rejected: "), "{replay}");

        // A command the store refuses before planning.
        let missing = json!({"type":"thread.archive","commandId":"missing","threadId":"nowhere"});
        let error = failure(&rpc(&mut operator, 5, "orchestration.dispatchCommand", missing).await).clone();
        assert_eq!(error["_tag"], "OrchestrationV2DispatchCommandError");
        assert!(!error["message"].as_str().unwrap().starts_with("Invalid command"), "{error}");

        // A launch into a project that does not exist.
        let launch = json!({"commandId":"launch","threadId":"launched","projectId":"nowhere","title":"T","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}});
        let error = failure(&rpc(&mut operator, 6, "orchestration.launchThread", launch).await).clone();
        assert_eq!(error["_tag"], "OrchestrationV2ThreadLaunchError");
        assert_eq!(error["commandId"], "launch");
        assert_eq!(error["projectId"], "nowhere");
        assert_eq!(error["message"], "Project not found.");
        // The workflow script query refuses a path that is not an absolute .js file before it
        // touches any file (the real scripts root is never read here).
        let error = failure(&rpc(&mut operator, 7, "orchestration.getWorkflowScript", json!({"threadId":"thread","scriptPath":"relative/flow.js"})).await).clone();
        assert_eq!(error["_tag"], "OrchestrationGetWorkflowScriptError");
        assert_eq!((error["reason"].clone(), error["scriptPath"].clone()), (json!("invalid-path"), json!("relative/flow.js")));
        h.finish().await;
    }));
}
