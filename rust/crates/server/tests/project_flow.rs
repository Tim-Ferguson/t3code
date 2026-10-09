//! `projects.ensureScratch` and `projects.createNew` over a real socket and real git, in temporary
//! directories. Git's identity and signing configuration are pinned for the whole process before
//! the runtime starts, so the developer's own settings never reach a test.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use std::{path::Path, process::Command};

fn pin_git_environment(directory: &Path) {
    let global = directory.join("pinned.gitconfig");
    std::fs::write(&global, "[commit]\n\tgpgsign = false\n").unwrap();
    // Safety: runs before the runtime and any other thread of this test binary exists.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        for key in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
            std::env::set_var(key, "Test");
        }
        for key in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
            std::env::set_var(key, "test@test.com");
        }
    }
}

fn git(dir: &str, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn project(h: &Harness, id: &str) -> Value {
    h.store.projection("project", id).unwrap().unwrap_or_else(|| panic!("no project {id}"))
}

#[test]
fn scratch_and_new_projects_over_the_socket() {
    let pin = tempfile::tempdir().unwrap();
    pin_git_environment(pin.path());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness().await;
        let mut operator = h.connect(&h.operator).await;

        // Scratch: created once, found again, folder and icon in place.
        let first = success(&rpc(&mut operator, 1, "projects.ensureScratch", json!({})).await).clone();
        let scratch_id = first["projectId"].as_str().unwrap().to_owned();
        let again = success(&rpc(&mut operator, 2, "projects.ensureScratch", json!({"ignored":true})).await).clone();
        assert_eq!(again, first);
        let scratch = project(&h, &scratch_id);
        assert_eq!(scratch["title"], "No project");
        assert_eq!(scratch["workspaceRoot"], h.base.join("scratch").to_string_lossy().as_ref());
        assert_eq!(scratch["projectIcon"]["name"], "message-square-dashed");
        assert!(h.base.join("scratch").is_dir());

        // The new project is a committed repository, named from the title, and visible to shell readers.
        let created = success(&rpc(&mut operator, 3, "projects.createNew", json!({"name":"  Pinball Stats  "})).await).clone();
        let root = h.base.join("projects").join("pinball-stats");
        assert_eq!(created["workspaceRoot"], root.to_string_lossy().as_ref());
        assert!(created.get("commitError").is_none(), "{created}");
        let id = created["projectId"].as_str().unwrap();
        assert_eq!(project(&h, id)["title"], "Pinball Stats");
        assert_eq!(git(root.to_str().unwrap(), &["log", "--format=%s"]), "Initial commit");
        let snapshot = success(&rpc(&mut operator, 4, "orchestration.getArchivedShellSnapshot", json!({})).await).clone();
        assert!(snapshot.is_object());
        let second = success(&rpc(&mut operator, 5, "projects.createNew", json!({"name":"pinball stats"})).await).clone();
        assert_eq!(second["workspaceRoot"], h.base.join("projects/pinball-stats-2").to_string_lossy().as_ref());
        assert_ne!(second["projectId"], created["projectId"]);

        // Payloads that do not decode die the way the original's schema failures do.
        for (id, tag, payload) in [
            (10, "projects.createNew", json!({"name":"   "})),
            (11, "projects.createNew", json!({"name":""})),
            (12, "projects.createNew", json!({"name":"x".repeat(201)})),
            (13, "projects.createNew", json!({})),
            (14, "projects.createNew", json!({"name":5})),
            (15, "projects.createNew", Value::Null),
            (16, "projects.ensureScratch", Value::Null),
            (17, "projects.ensureScratch", json!("x")),
        ] {
            let exit = rpc(&mut operator, id, tag, payload.clone()).await;
            defect(&exit);
        }
        // A name at the limit is fine, and nothing was made for the refused ones.
        let longest = success(&rpc(&mut operator, 18, "projects.createNew", json!({"name":"y".repeat(200)})).await).clone();
        assert!(longest["workspaceRoot"].as_str().unwrap().ends_with(&"y".repeat(64)));
        let names = std::fs::read_dir(h.base.join("projects")).unwrap().count();
        assert_eq!(names, 3);

        // projects.list/add/remove are named in WS_METHODS but have no RPC in the original.
        for (id, tag) in [(20, "projects.list"), (21, "projects.add"), (22, "projects.remove")] {
            let exit = rpc(&mut operator, id, tag, json!({})).await;
            assert_eq!(failure(&exit)["_tag"], "NativeMethodUnsupportedError", "{exit}");
        }

        // A connection without orchestration:operate is refused before anything is made.
        let mut reader = h.connect(&h.reader).await;
        for (id, tag, payload) in [(30, "projects.ensureScratch", json!({})), (31, "projects.createNew", json!({"name":"Refused"}))] {
            let exit = rpc(&mut reader, id, tag, payload).await;
            let error = failure(&exit);
            assert_eq!(error["_tag"], "EnvironmentAuthorizationError", "{exit}");
            assert_eq!(error["operation"], tag);
            assert_eq!(error["requiredPermission"], "orchestration:operate");
        }
        assert!(!h.base.join("projects/refused").exists());
        h.finish().await;

        // Inside a Git work tree Scratch is withheld with the original's wire error.
        let h = harness_in_checkout().await;
        let mut operator = h.connect(&h.operator).await;
        let exit = rpc(&mut operator, 1, "projects.ensureScratch", json!({})).await;
        assert_eq!(
            failure(&exit),
            &json!({"_tag":"OrchestrationDispatchCommandError","message":"Threads without a project are not available on this environment."})
        );
        assert!(!h.base.join("scratch").exists());
        h.finish().await;
    }));
}
