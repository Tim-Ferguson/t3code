#![cfg(unix)]
//! OpenCode and Antigravity history in the usage summary over the real socket: the stores the originals' own tests build (rebuilt
//! from `usage-readers.jsonl`) are found through `OPENCODE_DATA_DIR` and `ANTIGRAVITY_DATA_DIR`, counted once however many names a
//! directory has, and reported as sources, a damaged store as a partial one. The home is a temporary directory, and the rate table
//! is served from loopback, so nothing outside the test is read or contacted. One test, because the environment is the process's.
mod rpc_support;

use base64::Engine;
use rpc_support::{harness_with_config, rpc, success};
use serde_json::{Value, json};
use std::path::Path;

fn rebuild(name: &str, destination: &Path) {
    for line in include_str!("fixtures/usage-readers.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["name"] != name {
            continue;
        }
        for (path, content) in row["files"].as_object().unwrap() {
            let target = destination.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let bytes = match content["text"].as_str() {
                Some(text) => text.as_bytes().to_vec(),
                None => base64::engine::general_purpose::STANDARD.decode(content["base64"].as_str().unwrap()).unwrap(),
            };
            std::fs::write(target, bytes).unwrap();
        }
        // A database with no time of its own is dated by its file, as it was when the original read it.
        for (path, ms) in row["mtimes"].as_object().unwrap() {
            let file = std::fs::File::options().write(true).open(destination.join(path)).unwrap();
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms.as_u64().unwrap())).unwrap();
        }
        return;
    }
    panic!("no scenario {name}");
}

#[tokio::test]
async fn opencode_and_antigravity_history_is_counted_once_and_reported_as_sources() {
    let home = tempfile::Builder::new().prefix("t3code-rust-port-test-usage-sources-").tempdir().unwrap();
    let home = std::fs::canonicalize(home.path()).unwrap();
    let (opencode, alias, antigravity) = (home.join("opencode-store"), home.join("opencode-alias"), home.join("antigravity"));
    rebuild("a migrated store counts each message once", &opencode);
    std::os::unix::fs::symlink(&opencode, &alias).unwrap();
    rebuild("step-only stores are read, a malformed database is reported, a large unused varint is skipped", &antigravity);
    // The rate table is a loopback server with nothing in it: every model reports unpriced, which is what is wanted here.
    let rates = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rates_url = format!("http://{}", rates.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rates, axum::Router::new().fallback(|| async { "{}" })).await.unwrap() });
    // SAFETY: this test binary runs this one test, so nothing else reads the environment while it changes.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("OPENCODE_DATA_DIR", format!("{},{}", opencode.display(), alias.display()));
        std::env::set_var("ANTIGRAVITY_DATA_DIR", antigravity.display().to_string());
        std::env::set_var("T3CODE_USAGE_RATES_URL", &rates_url);
        std::env::remove_var("XDG_DATA_HOME");
    }
    let harness = harness_with_config().await;
    let mut socket = harness.connect(&harness.operator).await;
    let summary = rpc(&mut socket, 1, "server.getUsageSummary", json!({"sinceDay":"2026-05-27","untilDay":"2026-05-30","timeZone":"UTC"})).await;
    let summary = success(&summary).clone();

    let sources: Vec<(String, String, String)> = summary["sources"].as_array().unwrap().iter().map(|source| (source["fingerprint"]["provider"].as_str().unwrap().to_owned(), source["status"].as_str().unwrap().to_owned(), source["message"].as_str().unwrap_or("").to_owned())).collect();
    // The two names of one OpenCode directory are one source; the Antigravity store has a database it cannot read.
    let opencode_sources: Vec<_> = sources.iter().filter(|source| source.0 == "opencode").collect();
    assert_eq!(opencode_sources.len(), 1, "{sources:?}");
    assert_eq!(opencode_sources[0].1, "ok");
    let antigravity_sources: Vec<_> = sources.iter().filter(|source| source.0 == "antigravity").collect();
    assert_eq!(antigravity_sources.len(), 1, "{sources:?}");
    assert_eq!((antigravity_sources[0].1.as_str(), antigravity_sources[0].2.as_str()), ("partial", "Some Antigravity history could not be read."));

    // msg-1 and msg-2 of the store (one of them also as a file the database has migrated), msg-9 only as a file, and the two
    // Antigravity records that have usage: 4 + the others.
    let tokens = |provider: &str| -> u64 {
        summary["buckets"].as_array().unwrap().iter().filter(|bucket| bucket["provider"] == provider).map(|bucket| bucket["totals"]["uncachedInputTokens"].as_u64().unwrap_or(0)).sum()
    };
    assert_eq!(tokens("opencode"), 300, "three distinct messages of 100 uncached tokens each, however the store is named");
    assert_eq!(tokens("antigravity"), 20);
    let opencode_models: Vec<&str> = summary["buckets"].as_array().unwrap().iter().filter(|bucket| bucket["provider"] == "opencode").map(|bucket| bucket["model"].as_str().unwrap()).collect();
    assert!(opencode_models.iter().all(|model| *model == "claude-sonnet-4-5"), "{opencode_models:?}");
    drop(socket);
    harness.finish().await;
}
