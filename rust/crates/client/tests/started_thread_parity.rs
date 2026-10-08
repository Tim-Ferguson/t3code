use serde_json::{Value, json};
use t3_contracts::*;

#[test]
fn original_started_session_eligibility_including_runtime_instance_and_duplicate_providers() {
    for (index, line) in include_str!("fixtures/started-thread.jsonl")
        .lines()
        .enumerate()
    {
        let fixture: Value = serde_json::from_str(line).unwrap();
        let input = &fixture["input"];
        let providers:Vec<ServerProvider>=input["providers"].as_array().unwrap().iter().map(|provider| {
            let mut row=json!({"instanceId":provider["instanceId"],"driver":"codex","enabled":true,"installed":true,"version":null,"status":"ready","auth":{"status":"authenticated"},"checkedAt":"2026-10-08T12:00:00Z","models":[],"slashCommands":[],"skills":[]});
            if let Some(flag)=provider.get("requiresNewThreadForModelChange"){row["requiresNewThreadForModelChange"]=flag.clone();}
            serde_json::from_value(row).unwrap()
        }).collect();
        let current = serde_json::from_value(input["currentModelSelection"].clone()).unwrap();
        let next = serde_json::from_value(input["nextModelSelection"].clone()).unwrap();
        let active = input["currentProviderInstanceId"]
            .as_str()
            .map(|id| ProviderInstanceId::new(id).unwrap());
        let actual = t3_client::started_thread::model_change_block(
            &providers,
            input["hasStartedSession"].as_bool().unwrap(),
            input["supportsProviderSwitchingViaHandoff"] == true,
            &current,
            active.as_ref(),
            &next,
        )
        .map(|block| json!({"title":block.title,"description":block.description}))
        .unwrap_or(Value::Null);
        assert_eq!(
            actual,
            fixture["expected"],
            "source eligibility fixture {}",
            index + 1
        );
    }
}

#[test]
fn original_provider_handoff_attached_stopped_missing_session_and_import_rules() {
    for (index, line) in include_str!("fixtures/thread-handoff.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            t3_client::started_thread::supports_provider_handoff(&row["input"]),
            row["expected"].as_bool().unwrap(),
            "source handoff case {}",
            index + 1
        );
    }
}

#[test]
fn original_reported_model_requires_active_provider_thread_and_matching_instance() {
    for (index, line) in include_str!("fixtures/reported-model.jsonl")
        .lines()
        .enumerate()
    {
        let row: Value = serde_json::from_str(line).unwrap();
        let actual = t3_client::started_thread::reported_model_selection(&row["input"])
            .map(|selection| serde_json::to_value(selection).unwrap())
            .unwrap_or(Value::Null);
        assert_eq!(
            actual,
            row["expected"],
            "source reported model case {}",
            index + 1
        );
    }
}
