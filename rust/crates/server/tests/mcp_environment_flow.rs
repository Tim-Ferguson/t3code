#![cfg(unix)]
//! The attachment, environment and project tools over the real MCP HTTP transport with a
//! thread-scoped credential and a real store, settings service and execution service. Tests wait
//! on persisted events, never on sleeps.
mod mcp_support;
use mcp_support::*;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use t3_server::{attachment_service::AttachmentService, attachment_upload::StoreOutcome};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git").args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// Uploads `bytes` through the signed URL the tool returned, as the HTTP route does.
async fn upload(h: &Harness, issued: &Value, bytes: &[u8]) {
    let service = AttachmentService::for_store(&h.store).unwrap();
    let token = issued["relativeUrl"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    let claims = service.validate_upload_token(&token, chrono::Utc::now().timestamp_millis()).expect("a fresh signed URL validates");
    let stream = futures_util::stream::iter([Ok::<_, std::io::Error>(bytes.to_vec())]);
    assert_eq!(service.store_upload(&claims, stream).await, StoreOutcome::Stored);
}

fn stored_files(h: &Harness) -> Vec<String> {
    let dir = AttachmentService::for_store(&h.store).unwrap().attachments_dir().to_path_buf();
    let mut names: Vec<String> = std::fs::read_dir(dir).map(|entries| entries.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
async fn uploaded_files_are_sent_to_a_thread_discarded_or_refused() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    // Files go to a thread of their own: a message sent to the caller's own running turn steers it, and the
    // fake provider then finishes that turn, which would end the caller's live run while this test still needs it.
    let target = h.running_thread("target", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let file = |size: usize| json!({"type":"file","name":"notes.txt","mimeType":"text/plain","sizeBytes":size});

    // Prepare returns a signed URL for exactly the declared bytes; the upload is pending until sent.
    let issued = h.ok(&caller, "t3_attachment_prepare_upload", json!({"upload":file(5)})).await;
    let pending = issued["attachmentId"].as_str().unwrap().to_owned();
    assert!(issued["relativeUrl"].as_str().unwrap().starts_with("/api/attachments/upload/"), "{issued}");
    upload(&h, &issued, b"hello").await;
    assert!(stored_files(&h).iter().any(|name| name.starts_with(&pending)), "{:?}", stored_files(&h));

    // Send claims the pending file into the thread and delivers it as a new agent message.
    let attachment = json!({"type":"file","id":pending,"name":"notes.txt","mimeType":"text/plain","sizeBytes":5});
    let sent = h.ok(&caller, "t3_thread_send_attachments", json!({"threadId":target,"message":"see the notes","attachments":[attachment]})).await;
    assert_eq!(sent["threadId"], "target");
    let message = h.view("target")["messages"].as_array().unwrap().iter().find(|message| message["id"] == sent["messageId"]).unwrap().clone();
    assert_eq!((message["text"].as_str(), message["createdBy"].as_str(), message["creationSource"].as_str(), message["senderThreadId"].as_str()), (Some("see the notes"), Some("agent"), Some("mcp"), Some("parent")));
    let held = message["attachments"][0].clone();
    assert_ne!(held["id"], pending.as_str(), "the thread holds its own copy");
    assert!(stored_files(&h).iter().any(|name| name.starts_with(held["id"].as_str().unwrap())), "the thread's copy is stored: {:?}", stored_files(&h));
    // Discarding the pending upload leaves that copy alone.
    h.ok(&caller, "t3_attachment_discard", json!({"attachmentId":pending})).await;
    assert!(!stored_files(&h).iter().any(|name| name.starts_with(&pending)));

    // A file the thread already holds can be sent again; one it never held cannot.
    h.ok(&caller, "t3_thread_send_attachments", json!({"threadId":target,"attachments":[held]})).await;
    let foreign = json!({"type":"file","id":"parent-00000000-0000-4000-8000-000000000000-txt","name":"x.txt","mimeType":"text/plain","sizeBytes":1});
    assert_eq!(h.refused(&caller, "t3_thread_send_attachments", json!({"threadId":target,"attachments":[foreign]})).await, json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"Attachments must be pending uploads or already belong to the target thread."}));

    // Discard removes only a pending upload.
    let second = h.ok(&caller, "t3_attachment_prepare_upload", json!({"upload":file(3)})).await;
    upload(&h, &second, b"abc").await;
    let second_id = second["attachmentId"].as_str().unwrap().to_owned();
    assert_eq!(h.ok(&caller, "t3_attachment_discard", json!({"attachmentId":second_id})).await, json!({}));
    assert!(!stored_files(&h).iter().any(|name| name.starts_with(&second_id)));
    let delivered = held["id"].as_str().unwrap().to_owned();
    h.ok(&caller, "t3_attachment_discard", json!({"attachmentId":delivered})).await;
    assert!(stored_files(&h).iter().any(|name| name.starts_with(&delivered)), "a delivered attachment is never deleted by discard");

    // A caller below the target's modes sends nothing to it; an archived target takes nothing.
    let weak = h.running_thread("weak", "approval-required", "plan").await;
    let weak_caller = h.caller(&weak).await;
    let before = h.view("parent")["messages"].as_array().unwrap().len();
    assert_eq!(h.refused(&weak_caller, "t3_thread_send_attachments", json!({"threadId":parent,"attachments":[attachment]})).await["code"], "runtime_mode_escalation_denied");
    assert_eq!(h.view("parent")["messages"].as_array().unwrap().len(), before);
    let sibling = h.ok(&caller, "t3_thread_launch", json!({"title":"Sibling"})).await["threadId"].as_str().unwrap().to_owned();
    h.ok(&caller, "t3_thread_organize", json!({"threadId":sibling,"action":"archive"})).await;
    assert_eq!(h.refused(&caller, "t3_thread_send_attachments", json!({"threadId":sibling,"attachments":[attachment]})).await, json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"Unarchive the target thread before sending attachments."}));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_environment_tools_read_and_change_preferences_for_a_full_access_caller_only() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let read = h.ok(&caller, "t3_environment_read", json!({})).await;
    assert_eq!((read["environmentId"].as_str(), read["label"].as_str(), read["serverVersion"].as_str(), read["platform"]["os"].as_str()), (Some("environment-1"), Some("Test server"), Some("0.0.0-test"), Some("linux")));
    let settings = h.settings.snapshot().await.unwrap();
    assert_eq!(read["preferences"]["newWorktreesStartFromOrigin"], json!(settings.new_worktrees_start_from_origin));

    // An update goes through settings persistence and answers with the preferences it left.
    let long = "y".repeat(4100);
    let updated = h.ok(&caller, "t3_environment_preferences_update", json!({"newWorktreesStartFromOrigin":!settings.new_worktrees_start_from_origin,"sourceControlWritingStyle":{"customInstructions":long}})).await;
    assert_eq!(updated["newWorktreesStartFromOrigin"], json!(!settings.new_worktrees_start_from_origin));
    assert_eq!((updated["sourceControlWritingStyle"]["customInstructions"].as_str().unwrap().chars().count(), updated["sourceControlWritingStyle"]["truncated"].as_bool()), (4000, Some(true)));
    let stored = h.settings.snapshot().await.unwrap();
    assert_eq!(stored.new_worktrees_start_from_origin, !settings.new_worktrees_start_from_origin);
    assert_eq!(stored.source_control_writing_style.custom_instructions.as_str().len(), 4100);
    assert_eq!(h.ok(&caller, "t3_environment_read", json!({})).await["preferences"], updated);

    // Other callers change nothing: below full access, or full access but planning only.
    let denied = json!({"_tag":"OrchestratorMcpFailure","code":"capability_denied","message":"Changing projects or environment settings needs a live full-access/default calling thread or a full-access client."});
    for (id, runtime, interaction) in [("weak", "approval-required", "default"), ("planner", "full-access", "plan")] {
        let id = h.running_thread(id, runtime, interaction).await;
        let other = h.caller(&id).await;
        assert_eq!(h.refused(&other, "t3_environment_preferences_update", json!({"enableProviderUpdateChecks":false})).await, denied, "{runtime}/{interaction}");
        assert_eq!(h.ok(&other, "t3_environment_read", json!({})).await["environmentId"], "environment-1", "reading needs no write access");
    }
    assert_eq!(h.settings.snapshot().await.unwrap().enable_provider_update_checks, stored.enable_provider_update_checks);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_project_tools_create_update_clone_and_delete_projects_for_a_full_access_caller_only() {
    let h = Harness::new().await;
    let parent = h.running_thread("parent", "full-access", "default").await;
    let caller = h.caller(&parent).await;
    let base = std::fs::canonicalize(h._directory.path()).unwrap();
    let root = base.join("registered");
    std::fs::create_dir(&root).unwrap();

    // Create registers a workspace through the project service; the same workspace is refused twice.
    let created = h.ok(&caller, "t3_project_create", json!({"title":"Registered","workspaceRoot":root})).await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!((created["title"].as_str(), created["workspaceRoot"].as_str(), created["deletedAt"].clone()), (Some("Registered"), root.to_str(), Value::Null));
    assert_eq!(h.ok(&caller, "t3_project_read", json!({"projectId":id})).await["id"], id.as_str());
    assert_eq!(h.refused(&caller, "t3_project_create", json!({"title":"Again","workspaceRoot":root})).await, json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"The workspace is already registered to a project."}));
    assert_eq!(h.refused(&caller, "t3_project_create", json!({"title":"Missing","workspaceRoot":base.join("missing")})).await["message"], "The operation could not be completed.");
    let made = h.ok(&caller, "t3_project_create", json!({"title":"Made","workspaceRoot":base.join("made/deep"),"createWorkspaceRootIfMissing":true})).await;
    assert!(base.join("made/deep").is_dir() && made["workspaceRoot"] == base.join("made/deep").to_str().unwrap());
    // From a title alone the app makes the folder itself, and takes nothing else.
    let scaffolded = h.ok(&caller, "t3_project_create", json!({"title":"Fresh Start"})).await;
    assert!(Path::new(scaffolded["workspaceRoot"].as_str().unwrap()).join(".git").is_dir(), "{scaffolded}");
    assert_eq!(h.refused(&caller, "t3_project_create", json!({"title":"Fresh","scripts":[]})).await["code"], "invalid_request");

    // Update changes the fields given and keeps the rest, and moves the workspace when asked.
    let moved = base.join("moved");
    std::fs::create_dir(&moved).unwrap();
    let updated = h.ok(&caller, "t3_project_update", json!({"projectId":id,"title":"Renamed","workspaceRoot":moved,"autoPull":true})).await;
    assert_eq!((updated["title"].as_str(), updated["workspaceRoot"].as_str(), updated["autoPull"].as_bool()), (Some("Renamed"), moved.to_str(), Some(true)));
    assert_eq!(h.refused(&caller, "t3_project_update", json!({"projectId":"missing","title":"x"})).await, json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"The project was not found."}));
    assert_eq!(h.refused(&caller, "t3_project_update", json!({"projectId":id,"workspaceRoot":root.join("..").join("made/deep")})).await["code"], "invalid_request");

    // Clone only clones; the destination then registers as a project.
    let work = base.join("origin-work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = base.join("origin.git");
    git(&base, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    let cloned = h.ok(&caller, "t3_project_clone", json!({"remoteUrl":bare,"destinationPath":base.join("clone")})).await;
    assert_eq!(cloned["cwd"], base.join("clone").to_str().unwrap());
    assert!(base.join("clone/README.md").is_file());
    let occupied = h.refused(&caller, "t3_project_clone", json!({"remoteUrl":bare,"destinationPath":base.join("clone")})).await;
    assert_eq!(occupied["code"], "orchestration_error");
    h.ok(&caller, "t3_project_create", json!({"title":"Clone","workspaceRoot":cloned["cwd"]})).await;

    // Delete: a project with threads needs force, and force deletes them with it.
    let thread = h.ok(&caller, "t3_thread_launch", json!({"projectId":id,"title":"In the project","workspaceStrategy":{"type":"root"}})).await["threadId"].as_str().unwrap().to_owned();
    let not_empty = h.refused(&caller, "t3_project_delete", json!({"projectId":id})).await;
    assert_eq!(not_empty, json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"The project is not empty; force=true is required to delete it."}));
    assert!(h.store.projection("project", &id).unwrap().unwrap()["deletedAt"].is_null());
    let deleted = h.ok(&caller, "t3_project_delete", json!({"projectId":id,"force":true})).await;
    assert!(!deleted["deletedAt"].is_null());
    assert!(!h.view(&thread)["thread"]["deletedAt"].is_null(), "the project's threads are deleted with it");
    assert_eq!(h.refused(&caller, "t3_project_delete", json!({"projectId":id})).await["code"], "invalid_request");
    assert!(h.ok(&caller, "t3_project_list", json!({})).await["projects"].as_array().unwrap().iter().all(|project| project["id"] != id.as_str()));

    // Every project-changing tool refuses a caller below full access, and changes nothing.
    let weak = h.running_thread("weak", "approval-required", "default").await;
    let weak_caller = h.caller(&weak).await;
    let before = h.store.projections("project").unwrap().len();
    for (tool, arguments) in [
        ("t3_project_create", json!({"title":"Nope","workspaceRoot":base.join("nope")})),
        ("t3_project_update", json!({"projectId":"project","title":"Nope"})),
        ("t3_project_delete", json!({"projectId":"project","force":true})),
        ("t3_project_clone", json!({"remoteUrl":bare,"destinationPath":base.join("nope-clone")})),
    ] {
        let refusal = h.refused(&weak_caller, tool, arguments).await;
        assert_eq!(refusal["code"], "capability_denied", "{tool}");
    }
    assert_eq!(h.store.projections("project").unwrap().len(), before);
    assert_eq!(h.store.projection("project", "project").unwrap().unwrap()["title"], "Project");
    assert!(!base.join("nope-clone").exists() && !base.join("nope").exists());
    h.finish().await;
}
