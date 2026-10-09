//! Tracked clones over a real socket: `projectClone.start`, `.cancel`, `.retry` and the
//! `subscribeProjectClones` stream, with real git against local bare repositories. A clone that
//! must stall is pointed at a loopback `git://` listener that accepts and never answers. Nothing
//! leaves the machine, and every wait is on a stream event or an accepted connection.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use t3_server::source_control_repository::parse_clone_progress_line;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// A bare repository with one commit holding `README.md`.
fn bare_repository(parent: &Path, name: &str) -> std::path::PathBuf {
    let work = parent.join(format!("{name}-work"));
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = parent.join(format!("{name}.git"));
    git(parent, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    bare
}

async fn subscribe(socket: &mut Socket, id: u64) {
    send(socket, json!({"_tag":"Request","id":id,"tag":"subscribeProjectClones","payload":{},"headers":[]})).await;
}

/// The next list the stream sends, acknowledged so the following one flows.
async fn next_list(socket: &mut Socket, id: u64) -> Vec<Value> {
    loop {
        let message = next(socket).await;
        if message["_tag"] == "Chunk" && message["requestId"] == id {
            send(socket, json!({"_tag":"Ack","requestId":id})).await;
            return message["values"][0].as_array().unwrap().clone();
        }
        assert_ne!(message["_tag"], "Exit", "the stream ended: {message}");
    }
}

async fn until(socket: &mut Socket, id: u64, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    loop {
        let list = next_list(socket, id).await;
        if done(&list) {
            return list;
        }
    }
}

fn phase_of(list: &[Value], project: &str) -> Option<String> {
    list.iter().find(|entry| entry["projectId"] == project).map(|entry| entry["phase"].as_str().unwrap().to_owned())
}

fn start_input(project: &str, remote: &str, destination: &Path) -> Value {
    json!({"projectId":project,"title":"Cloned","createdAt":"2026-10-09T00:00:00.000Z","remoteUrl":remote,"destinationPath":destination})
}

#[test]
fn progress_lines_parse_as_the_original_does() {
    let rows: Vec<Value> = serde_json::from_str(include_str!("fixtures/project-clone-progress.json")).unwrap();
    assert!(rows.len() >= 20);
    for row in rows {
        let parsed = parse_clone_progress_line(row["line"].as_str().unwrap()).map(|progress| {
            json!({"stage":serde_json::to_value(progress.stage).unwrap(),"percent":progress.percent,"detail":progress.detail})
        });
        assert_eq!(parsed.unwrap_or(Value::Null), row["parsed"], "{}", row["line"]);
    }
}

#[test]
fn a_tracked_clone_reports_cancels_retries_and_completes() {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        let sources = h.base.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        let mut operator = h.connect(&h.operator).await;
        let mut observer = h.connect(&h.reader).await;
        subscribe(&mut observer, 100).await;
        assert!(next_list(&mut observer, 100).await.is_empty());

        // A reader may watch the clones but not start, cancel or retry one.
        let mut reader = h.connect(&h.reader).await;
        for tag in ["projectClone.start", "projectClone.cancel", "projectClone.retry"] {
            let payload = if tag == "projectClone.start" { start_input("p-denied", "/nowhere", &h.base.join("denied")) } else { json!({"projectId":"p-denied"}) };
            let exit = rpc(&mut reader, 1, tag, payload).await;
            assert_eq!(failure(&exit)["_tag"], "EnvironmentAuthorizationError", "{tag}");
            assert_eq!(failure(&exit)["requiredScope"], "orchestration:operate", "{tag}");
        }
        assert!(!h.base.join("denied").exists());

        // Decoding and validation fail before anything is created.
        let bad = rpc(&mut operator, 2, "projectClone.start", json!({"projectId":"p-bad"})).await;
        assert_eq!(defect(&bad).as_str().is_some(), true);
        let empty = rpc(&mut operator, 3, "projectClone.start", json!({"projectId":"p-empty","title":"T","createdAt":"x","destinationPath":h.base.join("empty")})).await;
        assert_eq!(failure(&empty)["_tag"], "SourceControlRepositoryError");
        assert_eq!(failure(&empty)["detail"], "Enter a repository path or clone URL before cloning.");
        assert!(h.store.projection("project", "p-empty").unwrap().is_none());

        // A clone from a repository that does not exist yet fails and is kept for a retry.
        let late = sources.join("late.git");
        let destination = h.base.join("late-dest");
        let started = success(&rpc(&mut operator, 4, "projectClone.start", start_input("p-late", late.to_str().unwrap(), &destination)).await).clone();
        assert_eq!(started["projectId"], "p-late");
        assert_eq!(started["cwd"], destination.to_str().unwrap());
        assert_eq!(started["remoteUrl"], late.to_str().unwrap());
        // The project exists, rooted at the destination, before the clone ends.
        assert_eq!(h.store.projection("project", "p-late").unwrap().unwrap()["workspaceRoot"], destination.to_str().unwrap());
        let list = until(&mut observer, 100, |list| phase_of(list, "p-late").as_deref() == Some("failed")).await;
        let failed = list.iter().find(|entry| entry["projectId"] == "p-late").unwrap();
        assert!(failed["error"].as_str().unwrap().len() > 0 && failed["error"].as_str().unwrap().len() <= 1000);
        assert_eq!(failed["destinationPath"], destination.to_str().unwrap());
        assert!(failed["endedAt"].is_string());
        // Cancel only applies to a running clone.
        assert_eq!(success(&rpc(&mut operator, 5, "projectClone.cancel", json!({"projectId":"p-late"})).await), &json!({"applied":false}));
        assert_eq!(success(&rpc(&mut operator, 6, "projectClone.retry", json!({"projectId":"nobody"})).await), &json!({"applied":false}));
        // The repository appears; the retry clones into the same destination.
        bare_repository(&sources, "late");
        assert_eq!(success(&rpc(&mut operator, 7, "projectClone.retry", json!({"projectId":"p-late"})).await), &json!({"applied":true}));
        let list = until(&mut observer, 100, |list| phase_of(list, "p-late").as_deref() == Some("done")).await;
        let done = list.iter().find(|entry| entry["projectId"] == "p-late").unwrap();
        assert_eq!((done["percent"].as_u64(), done["error"].is_null()), (Some(100), true));
        assert_eq!(std::fs::read_to_string(destination.join("README.md")).unwrap(), "hello\n");
        assert!(destination.join(".git").is_dir());
        // A finished clone cannot be retried or cancelled.
        assert_eq!(success(&rpc(&mut operator, 8, "projectClone.retry", json!({"projectId":"p-late"})).await), &json!({"applied":false}));
        assert_eq!(success(&rpc(&mut operator, 9, "projectClone.cancel", json!({"projectId":"p-late"})).await), &json!({"applied":false}));

        // A clone whose server never answers: a launch into the project is refused, a second clone
        // into the same destination is refused, and cancelling ends git and empties the folder.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = format!("git://127.0.0.1:{}/repo", silent.local_addr().unwrap().port());
        let slow = h.base.join("slow-dest");
        success(&rpc(&mut operator, 10, "projectClone.start", start_input("p-slow", &remote, &slow)).await);
        let (_held, _) = silent.accept().await.unwrap();
        // git has initialised the destination before it connects.
        assert!(slow.join(".git").exists());
        assert_eq!(phase_of(&until(&mut observer, 100, |list| phase_of(list, "p-slow").is_some()).await, "p-slow").as_deref(), Some("running"));
        // The destination git is filling is not empty, so another project cannot take it...
        let occupied = rpc(&mut operator, 11, "projectClone.start", start_input("p-other", &remote, &slow)).await;
        assert_eq!(failure(&occupied)["detail"], "Destination path already exists and is not empty.");
        // ...and the same project cannot start a second clone elsewhere.
        let duplicate = rpc(&mut operator, 17, "projectClone.start", start_input("p-slow", &remote, &h.base.join("elsewhere"))).await;
        assert_eq!(failure(&duplicate)["detail"], "A clone into this destination is already in progress.");
        assert!(h.store.projection("project", "p-other").unwrap().is_none());
        let launch = rpc(&mut operator, 12, "orchestration.launchThread", json!({"commandId":"launch-1","projectId":"p-slow","title":"T","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}})).await;
        assert_eq!(failure(&launch)["_tag"], "OrchestrationV2ThreadLaunchError");
        assert_eq!(failure(&launch)["message"], "The repository is still being cloned.");
        assert_eq!(success(&rpc(&mut operator, 13, "projectClone.cancel", json!({"projectId":"p-slow"})).await), &json!({"applied":true}));
        let list = until(&mut observer, 100, |list| phase_of(list, "p-slow").as_deref() == Some("cancelled")).await;
        assert!(list.iter().find(|entry| entry["projectId"] == "p-slow").unwrap()["error"].is_null());
        // No partial checkout is left, and the workspace root itself stays.
        assert!(slow.is_dir());
        assert_eq!(std::fs::read_dir(&slow).unwrap().count(), 0);
        let launch = rpc(&mut operator, 14, "orchestration.launchThread", json!({"commandId":"launch-2","projectId":"p-slow","title":"T","modelSelection":{"instanceId":"codex","model":"m"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"root"}})).await;
        assert_eq!(failure(&launch)["message"], "The repository was not cloned. Retry the clone first.");

        // Retrying the cancelled clone runs it again; deleting the project ends it for good.
        assert_eq!(success(&rpc(&mut operator, 15, "projectClone.retry", json!({"projectId":"p-slow"})).await), &json!({"applied":true}));
        let (_held_again, _) = silent.accept().await.unwrap();
        let deleted = rpc(&mut operator, 16, "projects.mutate", json!({"type":"project.delete","commandId":"delete-slow","projectId":"p-slow"})).await;
        assert_eq!(deleted["_tag"], "Success", "{deleted}");
        let list = until(&mut observer, 100, |list| phase_of(list, "p-slow").is_none()).await;
        assert!(list.iter().all(|entry| entry["projectId"] != "p-slow"));
        assert_eq!(std::fs::read_dir(&slow).map(|entries| entries.count()).unwrap_or(0), 0);
        h.finish().await;
    }));
}
