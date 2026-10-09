//! The provider management words and rules, replayed against what the original's helpers answered
//! (`rust/tools/generate_client_provider_manage_fixtures.mjs`).
use serde_json::{Value, json};
use t3_client::provider_manage as manage;

fn rows(kind: &str) -> Vec<Value> {
    let all: Vec<Value> = serde_json::from_str(include_str!("fixtures/provider_manage.json")).unwrap();
    all.into_iter().filter(|row| row["type"] == kind).collect()
}

#[test]
fn a_version_reads_as_the_original_shows_it() {
    let rows = rows("label");
    assert!(rows.len() >= 10);
    for row in rows {
        assert_eq!(manage::version_label(row["version"].as_str()).as_deref(), row["label"].as_str(), "{row}");
    }
}

#[test]
fn the_version_popover_says_and_offers_what_the_original_does() {
    let rows = rows("advisory");
    assert!(rows.len() >= 100);
    for row in rows {
        let provider = &row["provider"];
        let ours = manage::advisory(provider);
        match (&row["advisory"], ours) {
            (Value::Null, None) => {}
            (expected, Some(ours)) => {
                assert_eq!(ours.title, expected["title"].as_str().unwrap(), "{row}");
                assert_eq!(ours.detail, expected["detail"].as_str().unwrap(), "{row}");
                assert_eq!(ours.update_command.as_deref(), expected["updateCommand"].as_str(), "{row}");
                assert_eq!(ours.strong, expected["emphasis"] == "strong", "{row}");
                assert_eq!(ours.target_version.as_deref(), expected["targetVersion"].as_str(), "{row}");
            }
            (expected, ours) => panic!("{row}: expected {expected}, got {ours:?}"),
        }
        assert_eq!(manage::is_update_candidate(provider), row["candidate"] == true, "{row}");
    }
}

#[test]
fn an_update_in_progress_is_the_one_the_server_has_queued_or_is_running() {
    for row in rows("active") {
        assert_eq!(manage::update_active(&row["provider"]), row["active"] == true, "{row}");
    }
}

#[test]
fn a_new_instances_id_is_derived_numbered_and_checked_as_the_original_does() {
    let taken = |id: &str| ["codex_work", "codex_work_2", "claudeAgent_a"].contains(&id);
    let derived = rows("derive");
    assert!(derived.len() >= 8);
    for row in derived {
        let (driver, label) = (row["driver"].as_str().unwrap(), row["label"].as_str().unwrap());
        assert_eq!(manage::derive_instance_id(driver, label), row["id"].as_str().unwrap(), "{row}");
        assert_eq!(manage::available_instance_id(driver, label, &taken), row["available"].as_str().unwrap(), "{row}");
    }
    let checked = rows("validate");
    assert!(checked.len() >= 10);
    for row in checked {
        assert_eq!(manage::validate_instance_id(row["id"].as_str().unwrap(), &taken).as_deref(), row["error"].as_str(), "{row}");
    }
}

#[test]
fn a_download_reads_in_megabytes_and_a_managed_runtime_in_the_words_of_the_original() {
    let provider = json!({"installed":false,"setup":{"canAuthenticate":false,"canInstall":true}});
    let downloading = json!({"phase":"downloading","operationId":"op-1","downloadedBytes":2_500_000,"totalBytes":10_000_000,"version":"1.0.0","installedVersion":null,"canRemove":false,"message":null});
    let view = manage::install_view(Some(&downloading), &provider, "Codex", false);
    assert_eq!(view.status, "Downloading 2.5 of 10.0 MB.");
    assert_eq!(view.progress, Some((2_500_000, 10_000_000)));
    assert_eq!((view.action, view.active), (manage::InstallAction::Cancel, true));
    let unknown_size = json!({"phase":"downloading","operationId":"op-1","downloadedBytes":1_000_000,"totalBytes":null,"version":null,"installedVersion":null,"canRemove":false,"message":null});
    let view = manage::install_view(Some(&unknown_size), &provider, "Codex", false);
    assert_eq!((view.status.as_str(), view.progress), ("Downloading 1.0 MB.", None));
    let installed = json!({"phase":"succeeded","operationId":null,"downloadedBytes":1,"totalBytes":1,"version":"1.0.0","installedVersion":"1.0.0","source":"managed","canRemove":true,"message":null});
    let view = manage::install_view(Some(&installed), &json!({"installed":true,"setup":{"canInstall":true}}), "Codex", false);
    assert_eq!(view.status, "Managed by T3 Code \u{b7} v1.0.0.");
    assert_eq!((view.action_label.as_str(), view.can_remove, view.action), ("Reinstall Codex", true, manage::InstallAction::Install));
    let failed = json!({"phase":"failed","operationId":null,"downloadedBytes":0,"totalBytes":null,"version":null,"installedVersion":null,"canRemove":false,"message":"The download failed."});
    let view = manage::install_view(Some(&failed), &provider, "Codex", false);
    // The failure is the status itself, so it is not said twice.
    assert_eq!((view.action_label.as_str(), view.status.as_str(), view.message), ("Retry installation", "The download failed.", None));
    assert_eq!(manage::install_view(None, &provider, "Codex", false).status, "T3 Code downloads and manages Codex for you.");
}

#[test]
fn errors_from_management_requests_say_what_the_server_refused() {
    let update = json!({"_tag":"ServerProviderUpdateError","provider":"codex","reason":"No package manager."});
    assert_eq!(manage::failure_message(&update).as_deref(), Some("Provider update failed for codex: No package manager."));
    assert_eq!(manage::failure_message(&json!([{"_tag":"Fail","error":update}])).as_deref(), Some("Provider update failed for codex: No package manager."));
    assert_eq!(manage::failure_message(&json!({"_tag":"ProviderSetupError","instanceId":"codex","operation":"remove","detail":"A provider instance uses a custom path."})).as_deref(), Some("A provider instance uses a custom path."));
    assert_eq!(manage::failure_message(&json!({"_tag":"AcpRegistryOperationError","reason":"session_import_failed","message":"Could not load it."})).as_deref(), Some("Could not load it."));
    assert_eq!(manage::failure_message(&json!({"_tag":"AcpRegistryOperationError","reason":"session_import_failed","message":""})).as_deref(), Some("The session could not be imported."));
    assert_eq!(manage::failure_message(&json!({"_tag":"SomethingElse"})), None);
}

#[test]
fn provider_headers_are_a_json_object_of_strings_or_the_form_is_refused() {
    let draft = |headers: &str| manage::ProviderDraft { api_type: "openai".into(), base_url: "https://api.example.com".into(), headers: headers.into() };
    let ok = manage::set_provider_input("agent", "project", "gateway", &draft("")).unwrap();
    assert!(ok.get("headers").is_none());
    let ok = manage::set_provider_input("agent", "project", "gateway", &draft(r#"{"Authorization":"Bearer x"}"#)).unwrap();
    assert_eq!(ok["headers"]["Authorization"], "Bearer x");
    for bad in ["[]", r#"{"a":1}"#, "null", "{"] {
        let (title, _) = manage::set_provider_input("agent", "project", "gateway", &draft(bad)).unwrap_err();
        assert_eq!(title, "Invalid provider headers", "{bad}");
    }
    assert_eq!(manage::set_provider_input("agent", "project", "gateway", &draft(r#"{"a":1}"#)).unwrap_err().1, "Headers must be a JSON object with string values.");
}

#[test]
fn command_arguments_keep_their_order_and_every_literal_character() {
    let config = json!({"source":"local","commandPath":"dsh","commandArgs":["--profile","acp"," literal $(value) ; ",""]});
    let args = manage::command_arguments(&config);
    assert_eq!(args, vec!["--profile", "acp", " literal $(value) ; ", ""]);
    assert_eq!(manage::with_command_arguments(&config, &args), config);
    assert_eq!(manage::with_command_arguments(&json!({"source":"local"}), &["x".to_owned()]), json!({"source":"local","commandArgs":["x"]}));
}
