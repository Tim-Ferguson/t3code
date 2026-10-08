//! Source ACP Registry readiness and discovery projection; discovery is not sign-in.
use serde_json::{Value, json};
use std::sync::LazyLock;

pub(crate) fn readiness(inspection: &Value) -> Value {
    let version = inspection.get("version").cloned().unwrap_or(Value::Null);
    match inspection["status"].as_str().unwrap_or("failed") {
        "ready" => json!({"installed":true,"version":version,"status":"ready"}),
        "unconfigured" => {
            json!({"installed":false,"version":null,"status":"warning","message":"Select an ACP Registry agent or configure a local ACP executable before starting a thread."})
        }
        "not_found" => {
            json!({"installed":false,"version":null,"status":"error","message":format!("ACP Registry does not contain agent '{}'.",inspection["agentId"].as_str().unwrap_or(""))})
        }
        "unsupported" => {
            json!({"installed":false,"version":version,"status":"error","message":format!("ACP Registry agent '{}' has no compatible distribution for this environment.",inspection["agentId"].as_str().unwrap_or(""))})
        }
        "missing_runner" => {
            json!({"installed":false,"version":version,"status":"error","message":if inspection["distribution"]=="local" {"Local ACP executable is not available on this environment's PATH.".into()}else{format!("ACP executable '{}' is not available on this environment's PATH.",inspection["runner"].as_str().unwrap_or(""))}})
        }
        "unprepared" => {
            json!({"installed":false,"version":version,"status":"warning","message":format!("ACP Registry agent '{}' has not been prepared on this environment.",inspection["agentId"].as_str().unwrap_or(""))})
        }
        _ => {
            json!({"installed":false,"version":null,"status":"error","message":inspection["message"]})
        }
    }
}
fn official_icon(agent: &str, icon: Option<&str>) -> Option<String> {
    if let Some(icon) = icon.and_then(|icon| url::Url::parse(icon).ok()) {
        if icon.scheme() == "https"
            && icon.host_str() == Some("cdn.agentclientprotocol.com")
            && icon.port().is_none()
            && icon.username().is_empty()
            && icon.password().is_none()
        {
            return Some(icon.to_string());
        }
    }
    if agent
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && agent.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        Some(format!(
            "https://cdn.agentclientprotocol.com/registry/v1/latest/{agent}.svg"
        ))
    } else {
        None
    }
}
/// Direct counterpart of buildCheckedAcpRegistrySnapshot. Its inputs are the
/// typed settings, disk inspection, and source-normalized probe fields.
pub(crate) fn checked(input: &Value) -> Value {
    let settings = &input["settings"];
    let inspection = &input["inspection"];
    let ready = readiness(inspection);
    let probe = input.get("probe");
    let failure = input.get("probeError");
    let authentication_failed =
        failure.is_some_and(|failure| failure["reason"] == "authentication_failed");
    let methods = failure.and_then(|failure| failure["authMethods"].as_array());
    let advertised = if authentication_failed {
        methods.and_then(|methods| {
            methods
                .iter()
                .find(|method| method["id"] == settings["authMethodId"])
                .or_else(|| methods.first())
        })
    } else {
        None
    };
    let mut auth = if let Some(probe) = probe {
        json!({"status":"unknown","canLogout":probe["probe"]["sessionManagement"]["canLogout"]})
    } else if authentication_failed {
        json!({"status":"unauthenticated","canLogout":false})
    } else {
        json!({"status":"unknown","canLogout":false})
    };
    if let Some(method) = advertised {
        auth["type"] = method["type"].clone();
        auth["label"] = method["name"].clone();
    }
    if authentication_failed {
        if let Some(action) = failure.and_then(|failure| failure.get("authAction")) {
            auth["action"] = action.clone();
        }
    }
    let configuration = probe
        .map(|probe| {
            serde_json::from_value::<crate::acp_coordinator::LiveConfiguration>(
                probe["probe"].clone(),
            )
            .expect("normalized probe configuration")
        })
        .unwrap_or_default();
    let custom = settings["customModels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut output = json!({"instanceId":input["instanceId"],"driver":"acpRegistry","continuation":{"groupKey":input["continuationKey"]},"supportsTextGeneration":false,"enabled":settings["enabled"],"installed":ready["installed"],"version":ready["version"],"status":if settings["enabled"]==true{if failure.is_some(){json!("warning")}else{ready["status"].clone()}}else{json!("disabled")},"auth":auth,"checkedAt":input["checkedAt"],"setup":{"canInstall":false,"canAuthenticate":ready["installed"]==true && if let Some(probe)=probe {probe["probe"]["authMethods"].as_array().is_some_and(|methods|!methods.is_empty())}else if settings["source"]=="local"{methods.is_some_and(|methods|!methods.is_empty())}else{!settings["agentId"].as_str().unwrap_or("").is_empty()}},"models":crate::acp_model::models_from_live_configuration(&configuration,&custom),"slashCommands":probe.map(|probe|probe["slashCommands"].clone()).unwrap_or(json!([])),"skills":probe.map(|probe|probe["skills"].clone()).unwrap_or(json!([]))});
    for field in ["displayName", "accentColor"] {
        if input[field].as_str().is_some_and(|text| !text.is_empty()) {
            output[field] = input[field].clone();
        }
    }
    if settings["source"] != "local" {
        if let Some(icon) = official_icon(
            settings["agentId"].as_str().unwrap_or(""),
            probe.and_then(|probe| probe["probe"]["icon"].as_str()),
        ) {
            output["iconUrl"] = json!(icon);
        }
    }
    if inspection["status"] == "ready"
        && inspection["documentationUrl"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    {
        output["setup"]["documentationUrl"] = inspection["documentationUrl"].clone();
    }
    let message=advertised.and_then(|method|{
        let name=method["name"].as_str().unwrap_or("");
        Some(match method["type"].as_str().unwrap_or("agent") {
            "terminal" if method["command"].as_str().is_some_and(|text|!text.is_empty())=>format!("Sign in in provider settings using \"{name}\". The login terminal runs on this environment."),
            "env_var" if method["envVarNames"].as_array().is_some_and(|names|!names.is_empty())=>format!("Set {} under this instance's environment variables in provider settings. T3 Code will detect it on the next provider refresh.",method["envVarNames"].as_array().unwrap().iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
            _=>format!("Sign in in provider settings using \"{name}\"."),
        })
    }).or_else(||failure.and_then(|failure|failure["message"].as_str()).map(str::to_owned)).or_else(||ready["message"].as_str().map(str::to_owned));
    if let Some(message) = message.filter(|text| !text.is_empty()) {
        output["message"] = json!(message);
    }
    if let Some(probe) = probe {
        let management = &probe["probe"]["sessionManagement"];
        output["nativeSessions"] = json!({"canList":management["canList"],"canLoad":management["canLoad"],"canResume":management["canResume"],"canDelete":management["canDelete"]});
        output["configurableProviders"] = management["canConfigureProviders"].clone();
    }
    output
}
fn web_url(value: &str) -> Option<String> {
    if value.encode_utf16().count() > 2048 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}
fn opaque(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| {
        !value.is_empty()
            && *value == t3_contracts::trim_wire_string(value)
            && value.encode_utf16().count() <= 128
    })
}
fn bounded(value: &Value, max: usize) -> String {
    String::from_utf16_lossy(
        &t3_contracts::trim_wire_string(value.as_str().unwrap_or(""))
            .encode_utf16()
            .take(max)
            .collect::<Vec<_>>(),
    )
}
fn shell_token(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
pub(crate) fn auth_methods(initialize: &Value, command: &str, args: &[String]) -> Vec<Value> {
    initialize["authMethods"].as_array().into_iter().flatten().filter_map(|method|{
        let id=opaque(&method["id"])?;let kind=method["type"].as_str().unwrap_or("agent");let name=bounded(&method["name"],160);let description=bounded(&method["description"],1024);
        let mut normalized=json!({"id":id,"name":if name.is_empty(){id}else{&name},"description":if description.is_empty(){Value::Null}else{json!(description)},"type":kind});
        if kind=="terminal" {
            let mut parts=method["env"].as_object().into_iter().flatten().filter_map(|(name,value)|value.as_str().map(|value|format!("{name}={}",shell_token(value)))).collect::<Vec<_>>();
            parts.push(shell_token(command));parts.extend(args.iter().map(|value|shell_token(value)));parts.extend(method["args"].as_array().into_iter().flatten().filter_map(Value::as_str).map(shell_token));let display=parts.join(" ");if display.encode_utf16().count()<=2048{normalized["command"]=json!(display);}
        }
        if kind=="env_var" {let names=method["vars"].as_array().into_iter().flatten().filter_map(|var|opaque(&var["name"])).take(16).collect::<Vec<_>>();if !names.is_empty(){normalized["envVarNames"]=json!(names);}if let Some(link)=method["link"].as_str().and_then(web_url){normalized["link"]=json!(link);}}
        Some(normalized)
    }).take(32).collect()
}
pub(crate) fn management(initialize: &Value) -> Value {
    let caps = &initialize["agentCapabilities"];
    json!({"canList":!caps["sessionCapabilities"]["list"].is_null(),"canLoad":caps["loadSession"]==true,"canResume":!caps["sessionCapabilities"]["resume"].is_null(),"canLogout":!caps["auth"]["logout"].is_null(),"canDelete":!caps["sessionCapabilities"]["delete"].is_null(),"canConfigureProviders":!caps["providers"].is_null()})
}
pub(crate) fn probe_failure(
    error: &t3_acp::AcpError,
    methods: Vec<Value>,
    action: Option<Value>,
) -> Value {
    // ECMAScript /iu word boundaries include ASCII plus the long-s/Kelvin folds;
    // its whitespace class includes BOM and excludes NEL.
    static AUTH: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?iu)(?:^|[^A-Za-z0-9_ſK])(?:authenticat(?:e|ed|es|ing|ion)|credentials?|log(?:[\x09-\x0d\x20\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000\ufeff-]+)?in)(?:$|[^A-Za-z0-9_ſK])").unwrap()
    });
    let required = matches!(error,t3_acp::AcpError::Failure(failure) if matches!(failure.as_ref(),t3_acp::errors::Failure::Request(request) if request.code == -32000));
    let mut failure = json!({"reason":if required||AUTH.is_match(&error.to_string()){"authentication_failed"}else{"probe_failed"},"message":if required||AUTH.is_match(&error.to_string()){"The ACP agent could not complete authentication."}else{"The ACP agent could not create a test session."}});
    if !methods.is_empty() {
        failure["authMethods"] = json!(methods);
    }
    if let Some(action) = action {
        failure["authAction"] = action;
    }
    failure
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_snapshot_auth_method_and_failure_witnesses_match() {
        for (index, line) in include_str!("../tests/fixtures/acp-health.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let input = &row["input"];
            let actual = match row["operation"].as_str().unwrap() {
                "checked" => checked(input),
                "methods" => json!(auth_methods(
                    &input["initialize"],
                    input["command"].as_str().unwrap(),
                    &serde_json::from_value::<Vec<String>>(input["args"].clone()).unwrap()
                )),
                "failure" => {
                    let error = t3_acp::errors::RequestError::new(
                        input["code"].as_i64().unwrap(),
                        input["detail"].as_str().unwrap(),
                        None,
                    )
                    .into();
                    probe_failure(&error, vec![], None)
                }
                operation => panic!("unknown operation {operation}"),
            };
            assert_eq!(
                actual, row["output"],
                "source witness {index}: {}",
                row["operation"]
            );
        }
    }
}
