#[cfg(unix)]
#[test]
fn native_launcher_guards_target_clears_credentials_and_quotes_fixed_executables() {
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state ' quoted $(touch unsafe)");
    std::fs::create_dir_all(&root).unwrap();
    let entry = root.join("entry ' tool.sh");
    std::fs::write(&entry, "printf '%s\\n' \"${AGENT_DEVICE_DAEMON_BASE_URL-missing}\" \"${AGENT_DEVICE_DAEMON_AUTH_TOKEN-missing}\" \"${AGENT_DEVICE_CONFIG-missing}\" \"$@\"\nexit 7\n").unwrap();
    let wrapper = t3_server::agent_device_launcher::ensure(
        &root,
        std::path::Path::new(env!("CARGO_BIN_EXE_t3-server")),
        std::path::Path::new("/bin/sh"),
        &entry,
    )
    .unwrap();
    let rejected = Command::new(&wrapper)
        .arg("open")
        .env("T3CODE_BOOTSTRAP_FD", "987654")
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(rejected.stderr).unwrap().trim(),
        t3_server::agent_device_launcher::MISSING_TARGET
    );
    for args in [
        vec!["help"],
        vec![
            "open",
            "--config",
            "config with ' quotes",
            "--session",
            "thread-session",
            "--udid",
            "$(touch unsafe)",
        ],
    ] {
        let result = Command::new(&wrapper)
            .args(&args)
            .env("AGENT_DEVICE_DAEMON_BASE_URL", "untrusted-url")
            .env("AGENT_DEVICE_DAEMON_AUTH_TOKEN", "untrusted-token")
            .env("AGENT_DEVICE_CONFIG", "untrusted-config")
            .env("T3CODE_BOOTSTRAP_FD", "987654")
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(7));
        let output = String::from_utf8(result.stdout).unwrap();
        assert_eq!(
            output.split_terminator('\n').collect::<Vec<_>>(),
            [vec!["missing", "missing", "missing"], args].concat()
        );
        assert!(!temp.path().join("unsafe").exists());
    }
    assert!(!root.join("rust-state.sqlite").exists());
}

#[cfg(unix)]
#[test]
fn concurrent_publication_exposes_only_complete_executable_wrappers() {
    use std::{
        path::Path,
        process::Command,
        sync::{Arc, Barrier},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(temp.path().join("concurrent ' launcher"));
    std::fs::create_dir_all(root.as_ref()).unwrap();
    let entries = Arc::new([root.join("first tool.sh"), root.join("second ' tool.sh")]);
    for (index, entry) in entries.iter().enumerate() {
        std::fs::write(entry, format!("printf '%s' 'complete-{index}'\n")).unwrap();
    }
    let barrier = Arc::new(Barrier::new(8));
    std::thread::scope(|scope| {
        for publisher in 0..8 {
            let root = root.clone();
            let entries = entries.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                for turn in 0..10 {
                    let wrapper = t3_server::agent_device_launcher::ensure(
                        root.as_ref(),
                        Path::new(env!("CARGO_BIN_EXE_t3-server")),
                        Path::new("/bin/sh"),
                        &entries[(publisher + turn) % 2],
                    )
                    .unwrap();
                    let result = Command::new(wrapper).arg("help").output().unwrap();
                    assert!(
                        result.status.success(),
                        "{}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    assert!(result.stdout == b"complete-0" || result.stdout == b"complete-1");
                }
            });
        }
    });
    assert!(
        std::fs::read_dir(root.join("device/bin"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".launcher-"))
    );
}
