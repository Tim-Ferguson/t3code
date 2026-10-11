//! The label and the machine kind of the environment descriptor against what the unchanged original made of the same scripted
//! hosts (`tools/generate_environment_host_fixtures.mjs`): the decisions over every chassis type, vendor and product string, and
//! over hosts whose files and commands answer, fail or are missing.
use serde_json::{Value, json};
use std::collections::HashMap;
use t3_server::environment_host::{self, HostProbe};

struct Scripted {
    platform: String,
    hostname: String,
    files: HashMap<String, (Option<String>, String)>,
    commands: HashMap<String, (i32, String)>,
}

impl HostProbe for Scripted {
    fn platform(&self) -> &str {
        &self.platform
    }
    fn hostname(&self) -> Option<String> {
        Some(self.hostname.clone())
    }
    fn read_file(&self, path: &str) -> Option<String> {
        match self.files.get(path) {
            Some((error, text)) if error.as_deref() != Some("read") => Some(text.clone()),
            _ => None,
        }
    }
    fn file_exists(&self, path: &str) -> Option<bool> {
        match self.files.get(path) {
            Some((Some(error), _)) if error == "exists" => None,
            other => Some(other.is_some()),
        }
    }
    fn run(&self, command: &str, args: &[&str]) -> Option<(i32, String)> {
        let mut key = vec![command];
        key.extend(args);
        self.commands.get(&serde_json::to_string(&key).unwrap()).cloned()
    }
}

fn scripted(host: &Value) -> Scripted {
    Scripted {
        platform: host["platform"].as_str().unwrap().into(),
        hostname: host["hostname"].as_str().unwrap_or("host").into(),
        files: host["files"].as_object().into_iter().flatten().map(|(path, file)| (path.clone(), (file["error"].as_str().map(str::to_owned), file["text"].as_str().unwrap_or_default().to_owned()))).collect(),
        commands: host["commands"].as_object().into_iter().flatten().filter(|(_, answer)| answer["fails"] != true).map(|(key, answer)| (key.clone(), (answer["code"].as_i64().unwrap() as i32, answer["stdout"].as_str().unwrap().to_owned()))).collect(),
    }
}

fn kind(value: Option<t3_contracts::EnvironmentMachineKind>) -> Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn the_label_and_the_kind_are_what_the_original_decides() {
    let (mut pure, mut hosts) = (0, 0);
    for line in include_str!("fixtures/environment-host.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        match row["kind"].as_str().unwrap() {
            "apple" => {
                assert_eq!(kind(environment_host::machine_kind_from_apple_product_name(row["input"].as_str().unwrap())), row["output"], "{row}");
                pure += 1;
            }
            "dmi" => {
                let input = &row["input"];
                assert_eq!(kind(environment_host::machine_kind_from_dmi(input["chassisType"].as_str(), input["sysVendor"].as_str(), input["productName"].as_str())), row["output"], "{row}");
                pure += 1;
            }
            _ => {
                let host = &row["host"];
                let probe = scripted(host);
                assert_eq!(json!(environment_host::resolve_label(&probe, host["cwdBaseName"].as_str().unwrap())), row["label"], "{} label", host["name"]);
                assert_eq!(kind(environment_host::detect_machine_kind(&probe)), row["machine"], "{} machine", host["name"]);
                hosts += 1;
            }
        }
    }
    assert!(pure > 4000 && hosts > 40, "{pure} pure rows, {hosts} hosts");
}
