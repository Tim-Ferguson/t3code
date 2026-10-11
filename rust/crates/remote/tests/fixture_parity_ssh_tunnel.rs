//! Replays the oracle fixture's SSH tunnel, askpass, release, service-protocol and `pair` rows
//! (`rust/tools/generate_remote_fixtures.mjs`) against their Rust ports. The remote shell scripts are
//! compared byte for byte.
mod support;

use std::cmp::Ordering;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::fixture_rows;
use support::ssh_fakes::*;
use t3_remote::cli_release::*;
use t3_remote::service_preflight::*;
use t3_remote::service_protocol::*;
use t3_remote::ssh::SshTarget;
use t3_remote::ssh_auth::*;
use t3_remote::ssh_scripts::*;
use t3_remote::ssh_tunnel::*;
use t3_remote::tailscale_pair::*;

fn s(row: &Value, key: &str) -> String {
    row[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {row}"))
        .to_owned()
}

fn runner_options(row: &Value) -> Option<RemoteT3RunnerOptions> {
    if row["undefinedOptions"] == true {
        return None;
    }
    let options = &row["options"];
    let get = |key: &str| options[key].as_str().map(str::to_owned);
    Some(RemoteT3RunnerOptions {
        node_script_path: get("nodeScriptPath"),
        node_engine_range: get("nodeEngineRange"),
        archive_version: get("archiveVersion"),
        release_base_url: get("releaseBaseUrl"),
    })
}

fn runner_from(value: &Value) -> RemoteT3RunnerOptions {
    let get = |key: &str| value[key].as_str().map(str::to_owned);
    RemoteT3RunnerOptions {
        node_script_path: get("nodeScriptPath"),
        node_engine_range: get("nodeEngineRange"),
        archive_version: get("archiveVersion"),
        release_base_url: get("releaseBaseUrl"),
    }
}

fn error_tag(error: &RunnerScriptError) -> &'static str {
    match error {
        RunnerScriptError::MissingRunner => "SshMissingRunnerError",
        RunnerScriptError::InvalidArchiveVersion { .. } => "SshInvalidArchiveVersionError",
    }
}

fn check_script(row: &Value, name: &str, actual: Result<String, RunnerScriptError>) {
    let expected = &row[name];
    match actual {
        Ok(script) => {
            assert_eq!(expected["ok"], true, "{name} for {}", row["options"]);
            assert_eq!(
                script,
                expected["value"].as_str().unwrap(),
                "{name} for {}",
                row["options"]
            );
        }
        Err(error) => {
            assert_eq!(expected["ok"], false, "{name} for {}: {error}", row["options"]);
            assert_eq!(error_tag(&error), expected["tag"], "{}", row["options"]);
            assert_eq!(error.to_string(), expected["message"], "{}", row["options"]);
        }
    }
}

#[test]
fn templates_are_the_originals_bytes() {
    let rows = fixture_rows("ssh_template");
    assert_eq!(rows.len(), 11);
    for row in rows {
        let actual = match s(&row, "name").as_str() {
            "node_env.tmpl" => REMOTE_NODE_ENV_SCRIPT,
            "runner.tmpl" => REMOTE_RUNNER_SCRIPT,
            "launch.tmpl" => REMOTE_LAUNCH_SCRIPT,
            "pairing.tmpl" => REMOTE_PAIRING_SCRIPT,
            "stop.tmpl" => REMOTE_STOP_SCRIPT,
            "log_tail.tmpl" => REMOTE_LOG_TAIL_SCRIPT,
            "pick_port.tmpl" => REMOTE_PICK_PORT_SCRIPT,
            "wait_ready.tmpl" => REMOTE_WAIT_READY_SCRIPT,
            "askpass_posix.tmpl" => ASKPASS_POSIX_SCRIPT,
            "askpass_windows_launcher.tmpl" => ASKPASS_WINDOWS_LAUNCHER_SCRIPT,
            "askpass_windows.tmpl" => ASKPASS_WINDOWS_SCRIPT,
            other => panic!("unknown template {other}"),
        };
        assert_eq!(actual, s(&row, "text"), "{}", row["name"]);
    }
}

#[test]
fn remote_scripts_are_byte_identical() {
    let rows = fixture_rows("ssh_scripts");
    assert!(rows.len() > 25);
    for row in rows {
        let options = runner_options(&row);
        check_script(&row, "runner", build_remote_t3_runner_script(options.as_ref()));
        check_script(&row, "launch", build_remote_launch_script(options.as_ref()));
        check_script(
            &row,
            "pairing",
            build_remote_pairing_script("711bc738002d72fd", options.as_ref()),
        );
        assert_eq!(
            build_remote_node_env_script(options.as_ref()),
            s(&row, "nodeEnv"),
            "{}",
            row["options"]
        );
    }
    for row in fixture_rows("ssh_stop_script") {
        assert_eq!(build_remote_stop_script(&s(&row, "stateKey")), s(&row, "script"));
    }
}

/// The original's `$$` quirk is in the bytes: the embedded runner writes the literal `$` as the lock
/// owner, while the standalone runner keeps `"$$"`.
#[test]
fn the_dollar_dollar_collapse_of_the_original_is_kept() {
    let archive = RemoteT3RunnerOptions {
        archive_version: Some("1.2.3".into()),
        ..Default::default()
    };
    let standalone = build_remote_t3_runner_script(Some(&archive)).unwrap();
    assert!(standalone.contains("printf '%s\\n' \"$$\" > \"$T3_LOCK/pid.tmp\""));
    for embedded in [
        build_remote_launch_script(Some(&archive)).unwrap(),
        build_remote_pairing_script("k", Some(&archive)).unwrap(),
    ] {
        assert!(embedded.contains("printf '%s\\n' \"$\" > \"$T3_LOCK/pid.tmp\""));
        assert!(!embedded.contains("\"$$\""));
    }
}

fn target_from(value: &Value) -> SshTarget {
    SshTarget {
        alias: s(value, "alias"),
        hostname: s(value, "hostname"),
        username: value["username"].as_str().map(str::to_owned),
        port: value["port"].as_i64(),
    }
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn check_calls(spawner: &FakeSpawner, expected: &Value) {
    let recorded = spawner.recorded();
    let expected = expected.as_array().unwrap();
    assert_eq!(recorded.len(), expected.len());
    for (call, want) in recorded.iter().zip(expected) {
        // The original records the command name `ssh`, followed by the arguments.
        let args: Vec<String> = want["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(call.args, args);
        let stdin = call.stdin.clone().unwrap_or_default();
        assert_eq!(stdin.chars().count(), want["stdinLength"].as_u64().unwrap() as usize);
        assert_eq!(sha256_hex(&stdin), s(want, "stdinSha256"), "{:?}", call.args);
    }
}

fn error_message(error: &SshEnvironmentError) -> (String, String, Option<String>) {
    let stdout = match error {
        SshEnvironmentError::Launch { stdout, .. } | SshEnvironmentError::Pairing { stdout, .. } => {
            Some(stdout.clone())
        }
        _ => None,
    };
    (error.tag().to_owned(), error.to_string(), stdout)
}

#[tokio::test]
async fn launch_output_is_read_alike() {
    let rows = fixture_rows("ssh_launch_output");
    assert!(rows.len() > 20);
    let dir = support::temp_dir("parity-launch");
    for row in rows {
        let stdout = s(&row, "stdout");
        let spawner = {
            let stdout = stdout.clone();
            FakeSpawner::new(move |_| FakeProcess::finished(&stdout, "", 0))
        };
        let rt = runtime(spawner.clone(), FakeProbe::ok(), dir.path());
        let target = SshTarget {
            alias: "devbox".into(),
            hostname: "devbox.example.com".into(),
            username: Some("julius".into()),
            port: Some(2222),
        };
        let runner = RemoteT3RunnerOptions {
            archive_version: Some("1.2.3".into()),
            ..Default::default()
        };
        let result = support::within(launch_or_reuse_remote_server(
            &rt,
            &target,
            &Default::default(),
            Some(&runner),
        ))
        .await;
        match result {
            Ok(launch) => {
                assert_eq!(row["ok"], true, "{stdout:?}");
                assert_eq!(
                    launch.remote_port,
                    row["value"]["remotePort"].as_i64().unwrap(),
                    "{stdout:?}"
                );
                let kind = launch
                    .remote_server_kind
                    .map(|k| serde_json::to_value(k).unwrap());
                assert_eq!(
                    kind.unwrap_or(Value::Null),
                    row["value"]["remoteServerKind"].clone(),
                    "{stdout:?}"
                );
            }
            Err(error) => {
                assert_eq!(row["ok"], false, "{stdout:?}: {error}");
                let (tag, message, out) = error_message(&error);
                assert_eq!(tag, row["error"]["tag"].as_str().unwrap(), "{stdout:?}");
                assert_eq!(message, row["error"]["message"].as_str().unwrap(), "{stdout:?}");
                assert_eq!(
                    out.as_deref(),
                    row["error"]["stdout"].as_str(),
                    "{stdout:?}"
                );
            }
        }
        check_calls(&spawner, &row["calls"]);
    }
}

#[tokio::test]
async fn pairing_output_is_read_alike() {
    let rows = fixture_rows("ssh_pairing_output");
    assert!(rows.len() > 15);
    let dir = support::temp_dir("parity-pairing");
    for row in rows {
        let stdout = s(&row, "stdout");
        let spawner = {
            let stdout = stdout.clone();
            FakeSpawner::new(move |_| FakeProcess::finished(&stdout, "", 0))
        };
        let rt = runtime(spawner.clone(), FakeProbe::ok(), dir.path());
        let target = SshTarget {
            alias: "devbox".into(),
            hostname: "devbox.example.com".into(),
            username: None,
            port: None,
        };
        let runner = runner_from(&row["runner"]);
        let result = support::within(issue_remote_pairing_token(
            &rt,
            &target,
            &Default::default(),
            Some(&runner),
        ))
        .await;
        match result {
            Ok(credential) => {
                assert_eq!(row["ok"], true, "{stdout:?}");
                assert_eq!(credential, row["value"]["credential"].as_str().unwrap());
            }
            Err(error) => {
                assert_eq!(row["ok"], false, "{stdout:?}: {error}");
                let (tag, message, out) = error_message(&error);
                assert_eq!(tag, row["error"]["tag"].as_str().unwrap(), "{stdout:?}");
                assert_eq!(message, row["error"]["message"].as_str().unwrap(), "{stdout:?}");
                assert_eq!(out.as_deref(), row["error"]["stdout"].as_str(), "{stdout:?}");
            }
        }
        check_calls(&spawner, &row["calls"]);
    }
}

#[tokio::test]
async fn connect_pair_disconnect_spawn_the_same_commands() {
    let rows = fixture_rows("ssh_manager_flow");
    assert_eq!(rows.len(), 2);
    let dir = support::temp_dir("parity-flow");
    for row in rows {
        let spawner = FakeSpawner::new(|call| {
            if call.has("-G") {
                FakeProcess::finished("hostname devbox.example.com\nuser julius\nport 2222\n", "", 0)
            } else if call.has("-N") {
                FakeProcess::running()
            } else if call.has("--") {
                FakeProcess::finished("{\"remotePort\":3773,\"serverKind\":\"managed\"}\n", "", 0)
            } else if call.stdin.as_deref().is_some_and(|s| s.contains("auth pairing create")) {
                FakeProcess::finished("{\"credential\":\"TOKEN\"}\n", "", 0)
            } else {
                FakeProcess::finished("{\"stopped\":true}\n", "", 0)
            }
        });
        let rt = runtime(spawner.clone(), FakeProbe::ok(), dir.path());
        let runner = runner_from(&row["runner"]);
        let manager = SshEnvironmentManager::new(
            rt,
            SshEnvironmentManagerOptions {
                resolve_cli_runner: Some(Box::new(move || runner.clone())),
            },
        );
        let requested = target_from(&row["requested"]);
        let bootstrap = support::within(manager.ensure_environment(&requested, true))
            .await
            .unwrap();
        support::within(manager.disconnect_environment(&requested))
            .await
            .unwrap();
        manager.shutdown().await;
        assert_eq!(
            serde_json::to_value(&bootstrap).unwrap(),
            row["bootstrap"],
            "{}",
            row["runner"]
        );
        check_calls(&spawner, &row["calls"]);
    }
}

#[test]
fn loopback_urls_are_judged_alike() {
    for row in fixture_rows("ssh_loopback") {
        let raw = s(&row, "raw");
        let result = resolve_loopback_ssh_http_base_url(&raw);
        assert_eq!(result.is_ok(), row["ok"] == true, "{raw:?}: {result:?}");
        match result {
            Ok(url) => assert_eq!(url, s(&row, "value"), "{raw:?}"),
            Err(message) => assert_eq!(message, s(&row, "message"), "{raw:?}"),
        }
    }
}

#[test]
fn json_objects_are_extracted_alike() {
    for row in fixture_rows("shared_extract_json") {
        assert_eq!(extract_json_object(&s(&row, "raw")), s(&row, "value"), "{}", row["raw"]);
    }
}

#[test]
fn auth_failures_are_recognised_alike() {
    for row in fixture_rows("ssh_auth_failure") {
        assert_eq!(
            is_ssh_auth_failure(&s(&row, "message")),
            row["failure"] == true,
            "{}",
            row["message"]
        );
    }
}

#[test]
fn askpass_helpers_are_described_alike() {
    let rows = fixture_rows("ssh_askpass_descriptor");
    assert_eq!(rows.len(), 7);
    for row in rows {
        let directory = s(&row, "directory");
        let descriptor = build_ssh_askpass_helper_descriptor(&directory, row["platform"] == "win32");
        let expected = &row["descriptor"];
        assert_eq!(descriptor.launcher_path, s(expected, "launcherPath"), "{directory:?}");
        let files = expected["files"].as_array().unwrap();
        assert_eq!(descriptor.files.len(), files.len());
        for (file, want) in descriptor.files.iter().zip(files) {
            assert_eq!(file.path, s(want, "path"), "{directory:?}");
            assert_eq!(file.contents, s(want, "contents"));
            assert_eq!(file.mode.map(u64::from), want["mode"].as_u64());
        }
    }
}

#[test]
fn semver_ranges_are_judged_alike() {
    let rows = fixture_rows("semver_range");
    assert!(rows.len() > 300);
    for row in rows {
        let (version, range) = (s(&row, "version"), s(&row, "range"));
        assert_eq!(
            satisfies_semver_range(&version, &range),
            row["ok"] == true,
            "{version:?} against {range:?}"
        );
    }
}

#[test]
fn release_naming_matches() {
    for row in fixture_rows("cli_platform_key") {
        let (platform, arch) = (s(&row, "platform"), s(&row, "arch"));
        let key = cli_archive_platform_key(&platform, &arch);
        assert_eq!(key, row["key"].as_str(), "{platform}-{arch}");
        if let Some(key) = key {
            assert_eq!(
                cli_archive_file_name("1.2.3-preview.20260911.4", key),
                s(&row, "file")
            );
        }
    }
    for row in fixture_rows("cli_tar_command") {
        let env = row["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(
            cli_archive_tar_command(&s(&row, "platform"), &env),
            s(&row, "command")
        );
    }
    for row in fixture_rows("cli_base_url") {
        assert_eq!(
            cli_release_download_base_url(&s(&row, "version"), row["base"].as_str()),
            s(&row, "url"),
            "{}",
            row["base"]
        );
    }
    for row in fixture_rows("cli_checksums") {
        let parsed = parse_checksums(&s(&row, "text"));
        let expected = row["entries"].as_array().unwrap();
        assert_eq!(parsed.len(), expected.len(), "{}", row["text"]);
        for entry in expected {
            assert_eq!(
                parsed.get(entry[0].as_str().unwrap()).map(String::as_str),
                entry[1].as_str(),
                "{}",
                row["text"]
            );
        }
    }
    for row in fixture_rows("cli_channel") {
        assert_eq!(
            cli_release_channel_of(&s(&row, "version")).as_str(),
            s(&row, "channel"),
            "{}",
            row["version"]
        );
    }
    for row in fixture_rows("cli_newest") {
        let releases: Vec<ReleaseTag> = row["releases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| ReleaseTag {
                tag_name: s(r, "tag_name"),
                draft: r["draft"] == true,
            })
            .collect();
        let channel = match s(&row, "channel").as_str() {
            "stable" => CliReleaseChannel::Stable,
            "nightly" => CliReleaseChannel::Nightly,
            _ => CliReleaseChannel::Preview,
        };
        assert_eq!(
            newest_cli_release_version(&releases, channel).as_deref(),
            row["version"].as_str()
        );
    }
    for row in fixture_rows("cli_index_url") {
        assert_eq!(cli_release_index_page_url(3), s(&row, "url"));
    }
}

fn json_of_update(update: &ServiceUpdateRecord) -> Value {
    update.to_json()
}

fn state_json(state: &Option<ServiceState>) -> Value {
    match state {
        None => Value::Null,
        Some(state) => {
            let mut object = json!({"protocol": 3, "activeVersion": state.active_version});
            if let Some(update) = &state.update {
                object["update"] = json_of_update(update);
            }
            object
        }
    }
}

#[test]
fn service_versions_and_their_order_match() {
    for row in fixture_rows("service_version") {
        assert_eq!(
            is_exact_service_version(&s(&row, "version")),
            row["exact"] == true,
            "{}",
            row["version"]
        );
    }
    let rows = fixture_rows("service_compare");
    assert!(rows.len() > 200);
    for row in rows {
        let order = match compare_exact_service_versions(&s(&row, "left"), &s(&row, "right")) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        };
        assert_eq!(order, row["order"].as_i64().unwrap(), "{} vs {}", row["left"], row["right"]);
    }
}

#[test]
fn service_state_and_context_decode_alike() {
    for row in fixture_rows("service_state") {
        let text = s(&row, "text");
        assert_eq!(
            state_json(&parse_service_state(&text)),
            row["decoded"].clone(),
            "{text}"
        );
        assert_eq!(
            service_state_has_pending_update(&text),
            row["pending"] == true,
            "{text}"
        );
        assert_eq!(
            service_state_active_version(&text).as_deref(),
            row["activeVersion"].as_str(),
            "{text}"
        );
    }
    for row in fixture_rows("service_context") {
        let text = s(&row, "text");
        let decoded = decode_service_launcher_context(&text);
        let actual = match decoded {
            None => Value::Null,
            Some(context) => {
                let mut object = json!({"protocol": 3, "childVersion": context.child_version});
                if let Some(update) = &context.update {
                    object["update"] = json_of_update(update);
                }
                object
            }
        };
        assert_eq!(actual, row["decoded"], "{text}");
    }
}

#[test]
fn launcher_messages_decode_alike() {
    for row in fixture_rows("service_message") {
        let value = &row["value"];
        let child = decode_service_launcher_child_message(value)
            .map(|m| serde_json::to_value(m).unwrap())
            .unwrap_or(Value::Null);
        let parent = decode_service_launcher_parent_message(value)
            .map(|m| serde_json::to_value(m).unwrap())
            .unwrap_or(Value::Null);
        assert_eq!(child, row["child"], "{value}");
        assert_eq!(parent, row["parent"], "{value}");
    }
}

#[test]
fn service_preflight_matches() {
    for row in fixture_rows("service_preflight_run") {
        let result = run_service_preflight(
            "/db",
            row["launcherProtocol"].as_u64().unwrap() as u32,
            &s(&row, "version"),
        );
        assert_eq!(result.to_json(), row["result"], "{row}");
    }
    for row in fixture_rows("service_preflight_decode") {
        let decoded = decode_service_preflight_result(&row["value"])
            .map(|r| r.to_json())
            .unwrap_or(Value::Null);
        assert_eq!(decoded, row["decoded"], "{}", row["value"]);
    }
}

#[test]
fn tailscale_local_targets_match() {
    let rows = fixture_rows("pair_local_target");
    assert_eq!(rows.len(), 20);
    for row in rows {
        let state = PairServerState {
            host: row["state"]["host"].as_str().map(str::to_owned),
            port: row["state"]["port"].as_u64().unwrap() as u16,
            dev_url: row["state"]["devUrl"].as_str().map(str::to_owned),
        };
        match resolve_tailscale_local_target(&state) {
            Ok(target) => {
                assert_eq!(row["error"], Value::Null, "{state:?}");
                assert_eq!(
                    json!({"localPort": target.local_port}).as_object().unwrap().clone(),
                    {
                        let mut expected = row["target"].as_object().unwrap().clone();
                        let host = expected.remove("localHost");
                        assert_eq!(target.local_host.as_deref(), host.as_ref().and_then(|h| h.as_str()), "{state:?}");
                        expected
                    },
                    "{state:?}"
                );
            }
            Err(error) => assert_eq!(error.to_string(), s(&row, "error"), "{state:?}"),
        }
    }
    for row in fixture_rows("pair_error") {
        let tag = s(&row, "tag");
        let error = match tag.as_str() {
            "NoRunningServerError" => PairError::NoRunningServer {
                checked_state_paths: if row["message"].as_str().unwrap().contains("checked") {
                    vec![
                        "/a/userdata/server-runtime.json".into(),
                        "/b/dev/server-runtime.json".into(),
                    ]
                } else {
                    vec![]
                },
            },
            "TailscaleUnavailableError" => PairError::TailscaleUnavailable { cause: "x".into() },
            "MagicDnsNameMissingError" => PairError::MagicDnsNameMissing,
            "ServesOtherEnvironmentError" => PairError::ServesOtherEnvironment { serve_port: 8443 },
            "TailscaleServeFailedError" => PairError::TailscaleServeFailed {
                serve_port: 443,
                cause: "x".into(),
            },
            "ServePortOccupiedError" => PairError::ServePortOccupied { serve_port: 8443 },
            "DevServerNotProxiableError" => PairError::DevServerNotProxiable {
                dev_url: "https://localhost:5733/".into(),
            },
            other => panic!("{other}"),
        };
        assert_eq!(error.to_string(), s(&row, "message"), "{tag}");
    }
}
