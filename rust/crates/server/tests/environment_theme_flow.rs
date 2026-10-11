//! The environment theme store and `t3 theme`, against what the unchanged original made of the same scenarios
//! (`tools/generate_environment_theme_fixtures.mjs`): `readPublishedThemes` over a directory of every shape, and the command
//! as the real binary runs it (`theme set|clear|show`, with a working directory, an isolated HOME and a base directory), then the
//! service over a real watch and a real socket. Nothing here reads or writes outside the temporary directory.
mod rpc_support;
use rpc_support::{harness_with_config, next, send};
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};
use t3_server::{environment_theme, test_support::HANG_BOUND};

fn scenarios() -> Vec<Value> {
    include_str!("fixtures/environment-themes.jsonl").lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn text_of(file: &Value) -> Vec<u8> {
    if let Some(base64) = file["base64"].as_str() {
        use base64::Engine;
        return base64::engine::general_purpose::STANDARD.decode(base64).unwrap();
    }
    if let Some(parts) = file["parts"].as_array() {
        return parts
            .iter()
            .map(|part| match part.as_str() {
                Some(text) => text.to_owned(),
                None => part["repeat"][0].as_str().unwrap().repeat(part["repeat"][1].as_u64().unwrap() as usize),
            })
            .collect::<String>()
            .into_bytes();
    }
    file["text"].as_str().unwrap_or_default().as_bytes().to_vec()
}

fn write_files(directory: &Path, files: &Value) {
    for file in files.as_array().into_iter().flatten() {
        let path = directory.join(file["name"].as_str().unwrap());
        if let Some(target) = file["symlink"].as_str() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &path).unwrap();
        } else if file["dir"] == true {
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::write(&path, text_of(file)).unwrap();
        }
    }
}

#[test]
fn a_directory_publishes_the_themes_the_original_publishes() {
    let mut count = 0;
    for scenario in scenarios().iter().filter(|scenario| scenario["kind"] == "read") {
        let name = scenario["name"].as_str().unwrap();
        let root = tempfile::Builder::new().prefix("t3code-rust-port-test-themes-").tempdir().unwrap();
        let themes = root.path().join("themes");
        if scenario["symlinkedDirectory"] == true {
            let real = root.path().join("real-themes");
            std::fs::create_dir(&real).unwrap();
            write_files(&real, &scenario["files"]);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&real, &themes).unwrap();
        } else {
            std::fs::create_dir(&themes).unwrap();
            write_files(&themes, &scenario["files"]);
        }
        let published = serde_json::to_value(environment_theme::read_published(&themes)).unwrap();
        assert_eq!(published, scenario["themes"], "{name}");
        count += 1;
    }
    assert!(count >= 15, "{count} scenarios");
}

fn run(root: &Path, args: &[String]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_t3-server"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .current_dir(root)
        .arg("theme")
        .args(args)
        .output()
        .unwrap()
}

fn listing(directory: &Path, root: &str) -> Value {
    match std::fs::read_dir(directory) {
        Err(_) => Value::Null,
        Ok(entries) => {
            let mut names: Vec<_> = entries.map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
            names.sort();
            Value::Object(names.into_iter().map(|name| (name.clone(), json!(String::from_utf8_lossy(&std::fs::read(directory.join(&name)).unwrap()).replace(root, "<ROOT>")))).collect())
        }
    }
}

#[test]
fn theme_set_clear_and_show_do_what_the_original_does() {
    // The original also logs what it skips (a warning record per file) on standard output; a command here stays quiet.
    let warning = regex::Regex::new(r"\[\d\d:\d\d:\d\d\.\d+\] WARN \(#\d+\): [^\n]*\{\n(?:  [^\n]*\n)*\}\n").unwrap();
    let stamp = regex::Regex::new(r#""defaultThemeSetAt": "[^"]+""#).unwrap();
    let (mut commands, mut failures) = (0, 0);
    for scenario in scenarios().iter().filter(|scenario| scenario["kind"] == "cli") {
        let name = scenario["name"].as_str().unwrap();
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-themes-").tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let root_text = root.to_str().unwrap();
        let userdata = root.join("home/userdata");
        std::fs::create_dir_all(userdata.join("themes")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let setup = &scenario["setup"];
        if setup["noThemesDir"] == true {
            std::fs::remove_dir_all(userdata.join("themes")).unwrap();
        }
        if let Some(settings) = setup["settings"].as_str() {
            std::fs::write(userdata.join("settings.json"), settings).unwrap();
        }
        if setup["settingsIsDirectory"] == true {
            std::fs::create_dir(userdata.join("settings.json")).unwrap();
        }
        write_files(&userdata.join("themes"), &setup["themes"]);
        write_files(&root.join("src"), &setup["sources"]);
        for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
            let args: Vec<String> = step["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap().replace("<BASE>", &format!("{root_text}/home"))).collect();
            let output = run(&root, &args);
            let label = format!("{name} step {index} {:?}", step["args"]);
            let stdout = String::from_utf8_lossy(&output.stdout).replace(root_text, "<ROOT>");
            // The original writes a failure as a log record on standard output; the message is what it carries.
            let expected_stdout = if step["error"].is_null() { warning.replace_all(step["stdout"].as_str().unwrap(), "").into_owned() } else { String::new() };
            assert_eq!(stdout, expected_stdout, "{label} stdout");
            assert_eq!(output.status.code(), step["exit"].as_i64().map(|code| code as i32), "{label} exit");
            let stderr = String::from_utf8_lossy(&output.stderr).replace(root_text, "<ROOT>");
            match step["error"].as_object() {
                // The original logs a failure as a record with a timestamp and a source position; the message is the same.
                Some(error) => {
                    assert_eq!(stderr.trim_end(), error["message"].as_str().unwrap(), "{label} error {}", error["tag"]);
                    failures += 1;
                }
                None => assert_eq!(stderr, "", "{label} stderr"),
            }
            let settings = std::fs::read(userdata.join("settings.json")).ok().map(|bytes| stamp.replace_all(&String::from_utf8_lossy(&bytes).replace(root_text, "<ROOT>"), r#""defaultThemeSetAt": "<NOW>""#).into_owned());
            assert_eq!(settings.as_deref(), step["settings"].as_str(), "{label} settings");
            assert_eq!(listing(&userdata.join("themes"), root_text), step["themes"], "{label} themes");
            commands += 1;
        }
    }
    assert!(commands >= 60 && failures >= 20, "{commands} commands, {failures} failures");
}

#[tokio::test]
async fn the_service_streams_the_published_set_then_each_change() {
    let root = tempfile::Builder::new().prefix("t3code-rust-port-test-themes-").tempdir().unwrap();
    let themes = root.path().join("themes");
    let nightfall = |name: &str| json!({"name": name, "appearance": "dark", "canvas": "#101820", "accent": "#ffb000"}).to_string();
    std::fs::create_dir(&themes).unwrap();
    std::fs::write(themes.join("nightfall.json"), nightfall("Nightfall")).unwrap();
    // No debounce: the wait below is for the event, which is bounded by the hang bound, not by a sleep.
    let service = environment_theme::EnvironmentThemes::start(themes.clone(), Duration::ZERO).await;
    let mut stream = service.subscribe().await;
    let first = tokio::time::timeout(HANG_BOUND, stream.next()).await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(&*first).unwrap(), json!([{"id": "nightfall", "name": "Nightfall", "appearance": "dark", "canvas": "#101820", "accent": "#ffb000"}]));

    // A change is delivered once the directory watch sees it.
    std::fs::write(themes.join("sunrise.json"), nightfall("Sunrise")).unwrap();
    loop {
        let set = tokio::time::timeout(HANG_BOUND, stream.next()).await.unwrap().unwrap();
        if set.iter().any(|theme| theme.id.as_str() == "sunrise") {
            break;
        }
    }
    // A rewrite with identical contents publishes nothing (the sequence stands); one that changes the set does.
    let before = service.refresh().await.seq;
    std::fs::write(themes.join("nightfall.json"), nightfall("Nightfall")).unwrap();
    assert_eq!(service.refresh().await.seq, before);
    std::fs::write(themes.join("nightfall.json"), nightfall("Renamed")).unwrap();
    loop {
        let set = tokio::time::timeout(HANG_BOUND, stream.next()).await.unwrap().unwrap();
        if set.iter().any(|theme| theme.name.0.as_str() == "Renamed") {
            break;
        }
    }
    // A subscriber that connects later starts from the current set.
    let mut late = service.subscribe().await;
    let current = tokio::time::timeout(HANG_BOUND, late.next()).await.unwrap().unwrap();
    assert!(current.iter().any(|theme| theme.name.0.as_str() == "Renamed"));
    assert_eq!(service.current().await.len(), 2);
}

/// Every `Chunk` value of a stream until one satisfies `wanted`, each chunk acknowledged.
async fn event_where(socket: &mut rpc_support::Socket, id: u64, wanted: impl Fn(&Value) -> bool) -> Value {
    loop {
        let message = tokio::time::timeout(HANG_BOUND, next(socket)).await.unwrap();
        assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
        if message["_tag"] != "Chunk" {
            continue;
        }
        send(socket, json!({"_tag":"Ack","requestId":id})).await;
        for value in message["values"].as_array().unwrap() {
            if wanted(value) {
                return value.clone();
            }
        }
    }
}

#[tokio::test]
async fn the_config_stream_carries_the_published_themes_to_the_clients_that_ask() {
    let h = harness_with_config().await;
    let themes = h.base.join("userdata/themes");
    std::fs::create_dir_all(&themes).unwrap();
    std::fs::write(themes.join("nightfall.json"), json!({"name": "Nightfall", "appearance": "dark", "canvas": "#101820", "accent": "#ffb000"}).to_string()).unwrap();
    environment_theme::install(&h.store, environment_theme::EnvironmentThemes::start(themes.clone(), Duration::ZERO).await);

    // A client that asked: the snapshot, then the set as it is, then each change.
    let mut socket = h.connect(&h.operator).await;
    send(&mut socket, json!({"_tag":"Request","id":1,"tag":"subscribeServerConfig","payload":{"environmentThemes":true},"headers":[]})).await;
    let snapshot = event_where(&mut socket, 1, |value| value["type"] == "snapshot").await;
    assert!(snapshot["config"].get("environmentThemes").is_none());
    let published = event_where(&mut socket, 1, |value| value["type"] == "environmentThemesUpdated").await;
    assert_eq!(published["version"], 1);
    assert_eq!(published["payload"]["themes"], json!([{"id": "nightfall", "name": "Nightfall", "appearance": "dark", "canvas": "#101820", "accent": "#ffb000"}]));
    std::fs::write(themes.join("dusk.json"), json!({"name": "Dusk", "appearance": "dark", "colors": {"canvas": "#000"}}).to_string()).unwrap();
    let changed = event_where(&mut socket, 1, |value| value["type"] == "environmentThemesUpdated" && value["payload"]["themes"].as_array().is_some_and(|themes| themes.len() == 2)).await;
    assert_eq!(changed["payload"]["themes"][0]["id"], "dusk");

    // A client that did not ask is sent none: its decoder would not know the event.
    let mut quiet = h.connect(&h.operator).await;
    send(&mut quiet, json!({"_tag":"Request","id":2,"tag":"subscribeServerConfig","payload":{},"headers":[]})).await;
    event_where(&mut quiet, 2, |value| value["type"] == "snapshot").await;
    std::fs::write(themes.join("dawn.json"), json!({"name": "Dawn", "appearance": "light", "colors": {"canvas": "#fff"}}).to_string()).unwrap();
    // The asking client sees the third theme; the quiet one is sent none (what it would get is decoded by a client that does not
    // know the event, so the server must not send it).
    event_where(&mut socket, 1, |value| value["type"] == "environmentThemesUpdated" && value["payload"]["themes"].as_array().is_some_and(|themes| themes.len() == 3)).await;
    quiet.close(None).await.ok();
    h.finish().await;
}
