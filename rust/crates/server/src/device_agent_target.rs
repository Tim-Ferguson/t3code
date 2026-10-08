//! Stable host endpoint files and thread/host/device session identities.
use crate::device_agent_daemon::AgentDeviceEndpoint;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

fn key(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub fn config_path(state_dir: &Path, host: &str) -> PathBuf {
    state_dir
        .join("device/hosts")
        .join(format!("{}.json", key(host)))
}
pub fn session(thread: &str, host: &str, device: &str) -> String {
    format!(
        "t3-{}",
        key(&serde_json::to_string(&[thread, host, device]).unwrap())
    )
}
pub fn write_config(file: &Path, endpoint: &AgentDeviceEndpoint) -> io::Result<()> {
    // Struct serialization preserves the original source's field order.
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Config<'a> {
        daemon_base_url: &'a str,
        daemon_auth_token: &'a str,
    }
    let content = serde_json::to_string(&Config {
        daemon_base_url: &endpoint.base_url,
        daemon_auth_token: &endpoint.token,
    })?;
    let parent = file
        .parent()
        .ok_or_else(|| io::Error::other("endpoint has no parent"))?;
    std::fs::create_dir_all(parent)?;
    if std::fs::read_to_string(file).ok().as_deref() == Some(&content) {
        return Ok(());
    }
    let temporary = parent.join(format!(".endpoint-{}", uuid::Uuid::new_v4()));
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove = Remove(temporary.clone());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temporary)?;
    output.write_all(content.as_bytes())?;
    drop(output);
    std::fs::rename(temporary, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_host_paths_and_sessions_match_utf8_hashes() {
        for row in include_str!("../tests/fixtures/device-target.jsonl").lines() {
            let row: serde_json::Value = serde_json::from_str(row).unwrap();
            let host = row["host"].as_str().unwrap();
            assert_eq!(
                config_path(Path::new("/fixture/state"), host),
                PathBuf::from(row["config"].as_str().unwrap())
            );
            assert_eq!(
                session("thread 👋", host, "same-id"),
                row["session"].as_str().unwrap()
            );
        }
        assert_ne!(session("a", "local", "d"), session("b", "local", "d"));
        assert_ne!(session("a", "local", "d"), session("a", "local", "e"));
    }
    #[test]
    fn endpoint_update_is_private_atomic_and_keeps_other_hosts_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let first = config_path(root.path(), "mini");
        let second = config_path(root.path(), "android");
        let mut endpoint = AgentDeviceEndpoint {
            base_url: "http://127.0.0.1:1000".into(),
            token: "isolated token".into(),
            entry_path: "fixture".into(),
            pid: None,
            version: None,
        };
        write_config(&first, &endpoint).unwrap();
        endpoint.base_url = "http://127.0.0.1:1001".into();
        write_config(&second, &endpoint).unwrap();
        let second_bytes = std::fs::read(&second).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let before = std::fs::metadata(&second).unwrap();
            write_config(&second, &endpoint).unwrap();
            assert_eq!(std::fs::metadata(&second).unwrap().ino(), before.ino());
            assert_eq!(before.mode() & 0o777, 0o600);
        }
        endpoint.base_url = "http://127.0.0.1:2000".into();
        endpoint.token = "new isolated token".into();
        write_config(&first, &endpoint).unwrap();
        assert_eq!(std::fs::read(&second).unwrap(), second_bytes);
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "{\"daemonBaseUrl\":\"http://127.0.0.1:2000\",\"daemonAuthToken\":\"new isolated token\"}"
        );
        assert!(
            !std::fs::read_dir(first.parent().unwrap())
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".endpoint-"))
        );
    }
}
