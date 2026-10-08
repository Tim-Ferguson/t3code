//! Runtime resolution/normalization of the original background profile settings.
use serde_json::{Value, json};
use t3_contracts::ServerSettings;
const PROFILES: [&str; 3] = ["balanced", "performance", "battery-saver"];
const INTERVALS: [&str; 5] = [
    "automaticGitFetchInterval",
    "providerHealthRefreshInterval",
    "hostPowerMonitorActiveInterval",
    "hostPowerMonitorIdleInterval",
    "idleClientTtl",
];
const FLAGS: [&str; 4] = [
    "pauseWhenHostLocked",
    "pauseWhenHostLowPower",
    "pauseWhenClientLowPower",
    "pauseWhenOnBattery",
];
pub fn preset(profile: &str) -> Value {
    let mut preset = match profile {
        "performance" => {
            json!({"automaticGitFetchInterval":15000,"providerHealthRefreshInterval":60000,"hostPowerMonitorActiveInterval":30000,"hostPowerMonitorIdleInterval":120000,"idleClientTtl":45000,"pauseWhenHostLocked":true,"pauseWhenHostLowPower":false,"pauseWhenClientLowPower":false,"pauseWhenOnBattery":false})
        }
        "battery-saver" => {
            json!({"automaticGitFetchInterval":0,"providerHealthRefreshInterval":900000,"hostPowerMonitorActiveInterval":60000,"hostPowerMonitorIdleInterval":600000,"idleClientTtl":45000,"pauseWhenHostLocked":true,"pauseWhenHostLowPower":true,"pauseWhenClientLowPower":true,"pauseWhenOnBattery":true})
        }
        "balanced" => {
            json!({"automaticGitFetchInterval":30000,"providerHealthRefreshInterval":300000,"hostPowerMonitorActiveInterval":30000,"hostPowerMonitorIdleInterval":300000,"idleClientTtl":45000,"pauseWhenHostLocked":true,"pauseWhenHostLowPower":true,"pauseWhenClientLowPower":true,"pauseWhenOnBattery":false})
        }
        _ => panic!("profile must be contract validated"),
    };
    preset["profile"] = json!(profile);
    preset
}
pub fn base_profile(background: &Value) -> &str {
    match background["profile"].as_str().unwrap() {
        "custom" => background["baseProfile"].as_str().unwrap_or("balanced"),
        profile => profile,
    }
}
pub fn resolve(background: &Value) -> Value {
    let mut result = preset(base_profile(background));
    if background["profile"] == "custom" {
        for field in INTERVALS.into_iter().chain(FLAGS) {
            if let Some(value) = background["overrides"]
                .get(field)
                .filter(|value| !value.is_null())
            {
                result[field] = value.clone();
            }
        }
    }
    result
}
fn settings_equal(left: &Value, right: &Value) -> bool {
    INTERVALS
        .iter()
        .all(|field| left[*field].as_f64() == right[*field].as_f64())
        && FLAGS.iter().all(|field| left[*field] == right[*field])
}
pub fn normalize(background: &Value) -> Value {
    if background["profile"] != "custom" {
        return json!({"schemaVersion":1,"profile":background["profile"],"overrides":{}});
    }
    let resolved = resolve(background);
    let base = base_profile(background);
    for profile in [base, PROFILES[0], PROFILES[1], PROFILES[2]] {
        if settings_equal(&resolved, &preset(profile)) {
            return json!({"schemaVersion":1,"profile":profile,"overrides":{}});
        }
    }
    let preset = preset(base);
    let mut overrides = serde_json::Map::new();
    for field in INTERVALS {
        if resolved[field].as_f64() != preset[field].as_f64() {
            overrides.insert(field.into(), resolved[field].clone());
        }
    }
    for field in FLAGS {
        if resolved[field] != preset[field] {
            overrides.insert(field.into(), resolved[field].clone());
        }
    }
    json!({"schemaVersion":1,"profile":"custom","baseProfile":base,"overrides":overrides})
}
pub fn resolve_server(settings: &Value) -> Value {
    let background = &settings["backgroundActivity"];
    let default = background["profile"] == "balanced"
        && background.get("baseProfile").is_none()
        && background["overrides"].as_object().unwrap().is_empty();
    let defaults = serde_json::to_value(ServerSettings::default()).unwrap();
    let legacy = settings["backgroundActivityProfile"].as_str().unwrap();
    let changed = legacy != "balanced"
        || settings["automaticGitFetchInterval"].as_f64()
            != defaults["automaticGitFetchInterval"].as_f64()
        || settings["providerHealthRefreshInterval"].as_f64()
            != defaults["providerHealthRefreshInterval"].as_f64();
    if default && changed {
        let preset = preset(legacy);
        let mut overrides = serde_json::Map::new();
        for field in [&INTERVALS[0], &INTERVALS[1]] {
            if settings[*field].as_f64() != preset[*field].as_f64() {
                overrides.insert((*field).into(), settings[*field].clone());
            }
        }
        let profile = if overrides.is_empty() {
            legacy
        } else {
            "custom"
        };
        return resolve(
            &json!({"schemaVersion":1,"profile":profile,"baseProfile":legacy,"overrides":overrides}),
        );
    }
    resolve(background)
}
pub fn normalize_server(settings: &Value) -> Value {
    let mut resolved = resolve_server(settings);
    let profile = resolved.as_object_mut().unwrap().remove("profile").unwrap();
    normalize(
        &json!({"schemaVersion":1,"profile":"custom","baseProfile":profile,"overrides":resolved}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_profiles_match_actual_shared_helpers() {
        let mut count = 0;
        for row in include_str!("../tests/fixtures/background-settings.jsonl").lines() {
            let row: Value = serde_json::from_str(row).unwrap();
            match row["op"].as_str().unwrap() {
                "preset" => assert_eq!(preset(row["profile"].as_str().unwrap()), row["output"]),
                "background" => {
                    let input: t3_contracts::BackgroundActivitySettings =
                        serde_json::from_value(row["input"].clone()).unwrap();
                    let input = serde_json::to_value(input).unwrap();
                    assert_eq!(resolve(&input), row["resolved"], "{row}");
                    assert_eq!(normalize(&input), row["normalized"], "{row}");
                }
                "server" => {
                    let input: ServerSettings =
                        serde_json::from_value(row["input"].clone()).unwrap();
                    let input = serde_json::to_value(input).unwrap();
                    assert_eq!(resolve_server(&input), row["resolved"], "{row}");
                    assert_eq!(normalize_server(&input), row["normalized"], "{row}");
                }
                _ => unreachable!(),
            }
            count += 1;
        }
        assert_eq!(count, 672);
    }
}
