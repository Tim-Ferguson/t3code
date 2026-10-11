//! The ChatGPT sign-in URL checks against the unchanged ones (`tools/generate_codex_auth_handoff_fixtures.mjs`).
use serde_json::{Value, json};
use t3_server::codex_auth_handoff::*;

fn handoff(value: &Value) -> Handoff {
    serde_json::from_value(value.clone()).unwrap()
}

#[test]
fn the_checks_agree_with_the_original_on_every_witness() {
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    for line in include_str!("fixtures/codex-auth-handoff.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let kind = row["kind"].as_str().unwrap().to_owned();
        *kinds.entry(kind.clone()).or_default() += 1;
        let label = format!("{kind}: {}", row["name"].as_str().or(row["value"].as_str()).unwrap_or(""));
        // `ok` is the value the original returned, `error` the failure it threw.
        let expect_ok = |got: Result<Value, ()>| match (got, row.get("ok"), row.get("error")) {
            (Ok(got), Some(ok), _) => assert_eq!(&got, ok, "{label}"),
            (Err(()), _, Some(_)) => {}
            (got, ok, error) => panic!("{label}: got {got:?}, the original answered ok={ok:?} error={error:?}"),
        };
        match kind.as_str() {
            "authorization" => expect_ok(codex_authorization_request(row["value"].as_str().unwrap()).map(|request| json!({"authorizationUrl":request.authorization_url,"redirectUri":request.redirect_uri,"state":request.state})).map_err(|_| ())),
            "callback" => expect_ok(codex_callback_url(row["value"].as_str().unwrap(), row["redirectUri"].as_str().unwrap(), row["state"].as_str().unwrap()).map(|url| json!(url.to_string())).map_err(|_| ())),
            "return" => assert_eq!(provider_auth_return_url(row["value"].as_str()).map_or(Value::Null, Value::from), row["answer"], "{label}"),
            "handoff" => {
                let development = row["development"].as_bool().unwrap();
                let url = codex_auth_handoff_url(&handoff(&row["input"]), development).unwrap();
                assert_eq!(json!(url), row["url"], "{label}");
                assert_eq!(read_codex_auth_handoff(&url, development).map(|read| json!(read)).unwrap_or(Value::Null), row["read"], "{label}");
                assert_eq!(read_codex_auth_handoff(&url, !development).map(|read| json!(read)).unwrap_or(Value::Null), row["readOther"], "{label}");
            }
            "handoff-read" => match row["value"].as_str() {
                Some(value) => assert_eq!(read_codex_auth_handoff(value, false).map(|read| json!(read)).unwrap_or(Value::Null), row["read"], "{label}"),
                None => assert!(row["encodeError"].is_string(), "{label}"),
            },
            "delivery" => {
                let got = codex_auth_delivery_url(&handoff(&row["input"]), row["callbackUrl"].as_str().unwrap());
                expect_ok(got.clone().map(Value::from).map_err(|_| ()));
                if let Ok(url) = got {
                    let read = read_codex_auth_delivery(&url).map(|read| json!({"environmentId":read.environment_id,"instanceId":read.instance_id,"flowId":read.flow_id,"callbackUrl":read.callback_url,"returnHash":read.return_hash,"returnUrl":read.return_url})).unwrap_or(Value::Null);
                    assert_eq!(read, row["read"], "{label}");
                }
            }
            "delivery-read" => assert_eq!(read_codex_auth_delivery(row["value"].as_str().unwrap()).map_or(Value::Null, |read| json!(read.flow_id)), if row["read"].is_null() { Value::Null } else { row["read"]["flowId"].clone() }, "{label}"),
            other => panic!("{other}"),
        }
    }
    assert_eq!(kinds.keys().map(String::as_str).collect::<Vec<_>>(), ["authorization", "callback", "delivery", "delivery-read", "handoff", "handoff-read", "return"]);
}
