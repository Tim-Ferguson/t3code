use serde_json::Value;
use t3_client::provider_settings as policy;
#[test]
fn original_provider_settings_policy() {
    let raw = include_str!("fixtures/provider-settings.jsonl");
    for (index, line) in raw.lines().enumerate() {
        let row: Value = serde_json::from_str(line).unwrap();
        let actual = match row["kind"].as_str().unwrap() {
            "fields" => serde_json::to_value(policy::fields(
                row["driver"].as_str().unwrap(),
                &row["config"],
            ))
            .unwrap(),
            "next" => policy::next_field_value(
                &row["config"],
                &serde_json::from_value(row["field"].clone()).unwrap(),
                &row["value"],
            )
            .unwrap_or(Value::Null),
            "operate" => {
                let input = &row["input"];
                let session = if input["session"].is_null() {
                    None
                } else {
                    Some(serde_json::from_value(input["session"].clone()).unwrap())
                };
                serde_json::to_value(policy::operate_access(
                    session.as_ref(),
                    input["isPending"].as_bool().unwrap(),
                    input["hasError"].as_bool().unwrap(),
                ))
                .unwrap()
            }
            "instances" => {
                let live: Vec<Value> = serde_json::from_value(row["providers"].clone()).unwrap();
                let rows = policy::instance_rows(&row["settings"], &live, row["target"].as_str());
                let selected = policy::selected_instance(
                    &rows,
                    row["selected"].as_str(),
                    row["target"].as_str(),
                );
                serde_json::json!({"rows":rows,"selectedId":selected.map(|row|row.instance_id.clone()),"targetInstanceMissing":row["target"].as_str().is_some()&&row["selected"]==row["target"]&&!rows.iter().any(|r|Some(r.instance_id.as_str())==row["target"].as_str())})
            }
            "selected" => {
                let ids: Vec<String> = serde_json::from_value(row["ids"].clone()).unwrap();
                serde_json::to_value(policy::selected_environment(
                    &ids,
                    row["selected"].as_str(),
                    row["primary"].as_str(),
                ))
                .unwrap()
            }
            "access" => {
                let input = &row["input"];
                policy::environment_access(
                    input["connectionPhase"].as_str().unwrap(),
                    input["hasServerConfig"].as_bool().unwrap(),
                    serde_json::from_value(input["operateAccess"].clone()).unwrap(),
                )
            }
            _ => panic!("unknown fixture"),
        };
        assert_eq!(actual, row["expected"], "original row {index}");
    }
}
