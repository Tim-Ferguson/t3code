#![cfg(unix)]
//! `shell.openInEditor` and editor discovery. Parity: the original builds the command line for
//! every editor, target spelling, platform and environment over a PATH of stub executables
//! (`generate_external_launcher_fixtures.mjs`). Behaviour: over a real socket, with a fake `code`
//! at an absolute temp path that reports its arguments through a FIFO, so nothing real is
//! launched and nothing is polled.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};
use std::{collections::HashMap, os::unix::fs::PermissionsExt, path::{Path, PathBuf}};
use t3_server::{
    external_launcher::{EDITORS, Probe, available_editors, file_manager_reveal_kind, launch, resolve_editor_command, resolve_launch},
    provider_maintenance::Platform,
};

fn executable(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn stub_bin(base: &Path, stubbed: &[&str]) -> PathBuf {
    let bin = base.join("bin");
    for name in stubbed {
        executable(&bin.join(name), if *name == "xdg-mime" { "echo org.gnome.Nautilus.desktop" } else { "" });
    }
    bin
}

fn probe(platform: Platform, bin: &Path, home: &Path, extra: &[(&str, &str)]) -> Probe {
    let mut env: HashMap<String, String> = HashMap::from([("PATH".into(), bin.to_string_lossy().into_owned()), ("HOME".into(), home.to_string_lossy().into_owned())]);
    env.extend(extra.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())));
    // Nothing outside the temp directory is searched, so an editor installed on this machine is invisible.
    Probe { platform, env, applications: vec![], linux_dirs: vec![] }
}

fn environment(name: &str) -> Vec<(&'static str, &'static str)> {
    match name {
        "display" => vec![("DISPLAY", ":0")],
        "wayland" => vec![("WAYLAND_DISPLAY", "wayland-0")],
        "wsl-graphical" => vec![("DISPLAY", ":0"), ("WSL_DISTRO_NAME", "Ubuntu")],
        "wsl-ssh" => vec![("DISPLAY", ":0"), ("WSL_DISTRO_NAME", "Ubuntu"), ("SSH_TTY", "/dev/pts/1")],
        _ => vec![],
    }
}
fn platform_of(name: &str) -> Platform {
    if name == "darwin" { Platform::Darwin } else { Platform::Linux }
}

#[test]
fn command_lines_and_discovery_match_the_original() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/external-launcher.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let stubbed: Vec<&str> = fixture["stubbed"].as_array().unwrap().iter().map(|name| name.as_str().unwrap()).collect();
    let bin = stub_bin(&base, &stubbed);
    let ids: Vec<&str> = fixture["editors"].as_array().unwrap().iter().map(|id| id.as_str().unwrap()).collect();
    assert_eq!(ids, EDITORS.iter().map(|editor| editor.id).collect::<Vec<_>>(), "the editor table drifted from the original's");
    let mut ok = 0;
    for row in fixture["rows"].as_array().unwrap() {
        let probe = probe(platform_of(row["platform"].as_str().unwrap()), &bin, &home, &environment(row["environment"].as_str().unwrap()));
        let input = &row["input"];
        let outcome = resolve_launch(input["editor"].as_str().unwrap(), input["cwd"].as_str().unwrap(), input["reveal"] == true, &probe);
        let label = format!("{} / {} / {input}", row["platform"], row["environment"]);
        match (&row["result"]["spawned"], outcome) {
            (Value::Array(spawned), Ok(launch)) => {
                assert_eq!(spawned.len(), 1, "{label}");
                assert_eq!(launch.command, spawned[0]["command"].as_str().unwrap().replace("{bin}", bin.to_str().unwrap()), "{label}");
                let expected: Vec<String> = spawned[0]["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap().to_owned()).collect();
                assert_eq!(launch.args, expected, "{label}");
                ok += 1;
            }
            (Value::Null, Err(error)) => assert_eq!(error.wire()["_tag"], row["result"]["error"], "{label}"),
            (expected, got) => panic!("{label}: expected {expected}, got {got:?}"),
        }
    }
    assert!(ok > 1500, "fixture lost coverage: {ok}");
    for row in fixture["discovery"].as_array().unwrap() {
        // An empty PATH leaves only what this machine has installed, which differs from machine to machine.
        if row["env"]["PATH"] == "" {
            continue;
        }
        let extra: Vec<(&str, &str)> = ["DISPLAY", "WSL_DISTRO_NAME"].iter().filter_map(|name| row["env"][*name].as_str().map(|value| (*name, value))).collect();
        let probe = probe(platform_of(row["platform"].as_str().unwrap()), &bin, &home, &extra);
        let found: Vec<&str> = available_editors(&probe);
        let expected: Vec<&str> = row["available"].as_array().unwrap().iter().map(|id| id.as_str().unwrap()).collect();
        // Every editor is stubbed, so the answer does not depend on what the host has installed.
        assert_eq!(found, expected, "{row}");
        assert_eq!(file_manager_reveal_kind(&probe), row["revealKind"].as_str(), "{row}");
    }
}

#[test]
fn editors_are_found_in_their_install_folders_without_one_stealing_another() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let (applications, home) = (base.join("Applications"), base.join("home"));
    std::fs::create_dir_all(&home).unwrap();
    let mac = Probe { platform: Platform::Darwin, env: HashMap::from([("PATH".into(), String::new()), ("HOME".into(), home.to_string_lossy().into_owned())]), applications: vec![applications.clone()], linux_dirs: vec![] };
    let editor = |id: &str| EDITORS.iter().find(|editor| editor.id == id).unwrap();
    // Only the Insiders bundle is installed: stable VS Code is absent, Insiders is found.
    executable(&applications.join("Visual Studio Code - Insiders.app/Contents/Resources/app/bin/code-insiders"), "");
    assert_eq!(resolve_editor_command(editor("vscode"), &mac), None);
    let (command, args) = resolve_editor_command(editor("vscode-insiders"), &mac).unwrap();
    assert!(command.ends_with("Visual Studio Code - Insiders.app/Contents/Resources/app/bin/code-insiders") && args.is_empty());
    // Toolbox names JetBrains bundles after their version.
    executable(&applications.join("IntelliJ IDEA 2026.1.4.app/Contents/MacOS/idea"), "");
    assert!(resolve_editor_command(editor("idea"), &mac).unwrap().0.ends_with("IntelliJ IDEA 2026.1.4.app/Contents/MacOS/idea"));
    // A name that merely starts with the editor's is not its bundle.
    executable(&applications.join("PyCharm Helper.app/Contents/MacOS/pycharm"), "");
    assert_eq!(resolve_editor_command(editor("pycharm"), &mac), None);
    // Zed uses its cli, and Kiro drops its subcommand when installed as an app.
    executable(&applications.join("Zed.app/Contents/MacOS/cli"), "");
    assert!(resolve_editor_command(editor("zed"), &mac).unwrap().0.ends_with("Zed.app/Contents/MacOS/cli"));
    executable(&applications.join("Kiro.app/Contents/Resources/app/bin/kiro"), "");
    assert_eq!(resolve_editor_command(editor("kiro"), &mac).unwrap().1, Vec::<String>::new());
    // Linux looks in the user's and the system's bin folders.
    let system = base.join("usr/bin");
    executable(&system.join("zeditor"), "");
    let linux = Probe { platform: Platform::Linux, env: HashMap::from([("PATH".into(), String::new()), ("HOME".into(), home.to_string_lossy().into_owned())]), applications: vec![], linux_dirs: vec![system.clone()] };
    assert_eq!(resolve_editor_command(editor("zed"), &linux).unwrap().0, system.join("zeditor").to_string_lossy());
    // A command that is not there is reported, not guessed.
    let missing = resolve_launch("goland", "/work", false, &linux).unwrap();
    assert!(matches!(launch(&missing, &linux), Err(t3_server::external_launcher::LaunchError::CommandNotFound { .. })));
    assert_eq!(launch(&missing, &linux).unwrap_err().wire(), json!({"_tag":"ExternalLauncherCommandNotFoundError","editor":"goland","command":"goland"}));
}

#[test]
fn opening_in_an_editor_launches_the_resolved_command_and_refuses_the_rest() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let bin = base.join("bin");
    let fifo = base.join("report");
    let fifo_c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    // The fake editor reports its arguments (one per line) through the FIFO, then exits.
    executable(&bin.join("code"), &format!("for argument in \"$@\"; do echo \"$argument\"; done > '{}'", fifo.display()));
    executable(&bin.join("open"), "");
    // Safety: runs before the runtime and any other thread of this test binary exists.
    unsafe { std::env::set_var("PATH", &bin) };
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let h = harness_with_config().await;
        let mut operator = h.connect(&h.operator).await;
        let mut reader = h.connect(&h.reader).await;

        let denied = rpc(&mut reader, 1, "shell.openInEditor", json!({"cwd":"/work","editor":"vscode"})).await;
        assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
        assert_eq!(failure(&denied)["requiredScope"], "orchestration:operate");
        for (id, payload) in [
            (2, json!({"cwd":"/work","editor":"emacs"})),
            (3, json!({"cwd":"  ","editor":"vscode"})),
            (4, json!({"editor":"vscode"})),
            (5, json!({"cwd":"/work","editor":"vscode","reveal":"yes"})),
        ] {
            let exit = rpc(&mut operator, id, "shell.openInEditor", payload.clone()).await;
            assert!(!defect(&exit).is_null(), "{payload}");
        }

        // The target is one argument, with its position, and the editor sees nothing else.
        let reported = tokio::task::spawn_blocking({
            let fifo = fifo.clone();
            move || std::fs::read_to_string(fifo).unwrap()
        });
        let exit = rpc(&mut operator, 6, "shell.openInEditor", json!({"cwd":"/work/my project/src/lib.rs:12:4","editor":"vscode"})).await;
        assert_eq!(success(&exit), &Value::Null);
        assert_eq!(reported.await.unwrap(), "--goto\n/work/my project/src/lib.rs:12:4\n");

        // An editor that is not installed is reported with the command that was looked for: the
        // search folders are the host's, so the editor is one nobody installs.
        let unsupported = rpc(&mut operator, 7, "shell.openInEditor", json!({"cwd":"/work","editor":"file-manager","reveal":true})).await;
        // `open` is a stub that does nothing here; on a Linux host without a graphical session it is unsupported.
        assert!(unsupported["_tag"] == "Success" || failure(&unsupported)["_tag"] == "ExternalLauncherUnsupportedEditorError", "{unsupported}");

        // Discovery reaches the config snapshot.
        let config = success(&rpc(&mut operator, 8, "server.getConfig", json!({})).await).clone();
        assert!(config["availableEditors"].as_array().unwrap().iter().any(|id| id == "vscode"), "{}", config["availableEditors"]);
        h.finish().await;
    }));
}
