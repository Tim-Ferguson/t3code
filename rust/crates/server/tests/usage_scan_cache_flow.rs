#![cfg(unix)]
//! The persisted usage scan cache over real transcript files: a transcript that has not changed is not read again, one that grew is
//! read from where the last parse stopped, usage survives a deleted transcript and a restart, a source keeps its volume id when its
//! directory is recreated, and entries last written more than 90 days ago go. The home is a temporary directory, the rate table is
//! served from loopback, and no provider is run. One test, because the environment is the process's.
use serde_json::{Value, json};
use std::path::Path;
use t3_server::usage_service::UsageService;

fn claude_line(id: u32, tokens: u32) -> String {
    json!({"type":"assistant","timestamp":format!("2026-10-05T10:{:02}:00.000Z", id % 60),"requestId":format!("req_{id}"),"sessionId":"session-1","message":{"id":format!("msg_{id}"),"model":"claude-sonnet-4-6","usage":{"input_tokens":tokens,"output_tokens":1}}}).to_string()
}

fn set_modified(path: &Path, time: std::time::SystemTime) {
    std::fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
}

fn settings(claude_home: &Path, codex_home: &Path) -> Value {
    json!({
        "providerInstances": {
            "claudeAgent": {"driver":"claudeAgent","config":{"homePath":claude_home}},
            "codex": {"driver":"codex","config":{"homePath":codex_home}},
        },
        "usagePriceOverrides": {}, "usageModelAliases": {},
    })
}

async fn summary(service: &std::sync::Arc<UsageService>, settings: &Value) -> Value {
    let input = json!({"sinceDay":"2026-09-01","untilDay":"2026-10-31","timeZone":"UTC"});
    match service.read_summary(&input, settings).await {
        Ok(summary) => summary,
        Err(error) => panic!("{}", error.wire()),
    }
}

/// Uncached input tokens of Claude, and the Claude source.
fn claude(summary: &Value) -> (u64, Value) {
    let tokens = summary["buckets"].as_array().unwrap().iter().filter(|bucket| bucket["provider"] == "claude").map(|bucket| bucket["totals"]["uncachedInputTokens"].as_u64().unwrap_or(0)).sum();
    let source = summary["sources"].as_array().unwrap().iter().find(|source| source["fingerprint"]["provider"] == "claude").cloned().unwrap();
    (tokens, source)
}

fn cache_file(state: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(state.join("usage-scan-cache-v5.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn transcripts_are_read_once_resumed_when_they_grow_and_remembered_when_they_go() {
    let root = tempfile::Builder::new().prefix("t3code-rust-port-test-usage-cache-").tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let (home, state) = (root.join("home"), root.join("state"));
    let (claude_home, codex_home) = (root.join("claude"), root.join("codex"));
    let projects = claude_home.join("projects");
    std::fs::create_dir_all(projects.join("p")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let rates = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rates_url = format!("http://{}", rates.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rates, axum::Router::new().fallback(|| async { "{}" })).await.unwrap() });
    // SAFETY: this test binary runs this one test, so nothing else reads the environment while it changes.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("T3CODE_USAGE_RATES_URL", &rates_url);
        std::env::remove_var("OPENCODE_DATA_DIR");
        std::env::remove_var("ANTIGRAVITY_DATA_DIR");
        std::env::remove_var("XDG_DATA_HOME");
    }
    let settings = settings(&claude_home, &codex_home);
    let service = UsageService::new(&state);

    // A transcript with two messages, the second without its newline yet: both count, only the first is committed to the position.
    let transcript = projects.join("p").join("one.jsonl");
    std::fs::write(&transcript, format!("{}\n{}", claude_line(1, 10), claude_line(2, 20))).unwrap();
    let (tokens, source) = claude(&summary(&service, &settings).await);
    assert_eq!(tokens, 30);
    assert_eq!((source["status"].as_str(), source["scannedFiles"].as_u64()), (Some("ok"), Some(1)));
    let volume_id = source["fingerprint"]["volumeId"].as_str().unwrap().to_owned();
    assert!(!volume_id.is_empty());
    let cache = cache_file(&state);
    let entry = &cache["files"][transcript.to_str().unwrap()];
    let first_line = claude_line(1, 10).len() + 1;
    assert_eq!((entry["r"].as_array().unwrap().len(), entry["t"].as_array().unwrap().len(), entry["o"].as_u64()), (1, 1, Some(first_line as u64)));
    assert_eq!(cache["sources"][format!("claude\0{}", projects.display())]["volumeId"], json!(volume_id));

    // The writer finishes the line and adds a third: the tail is read once more, never counted twice, and the parse starts at the position.
    let mut grown = std::fs::read_to_string(&transcript).unwrap();
    grown.push_str(&format!("\n{}\n", claude_line(3, 40)));
    std::fs::write(&transcript, &grown).unwrap();
    let (tokens, _) = claude(&summary(&service, &settings).await);
    assert_eq!(tokens, 70);
    let entry = &cache_file(&state)["files"][transcript.to_str().unwrap()];
    assert_eq!((entry["r"].as_array().unwrap().len(), entry["t"].as_array().unwrap().len(), entry["o"].as_u64()), (3, 0, Some(grown.len() as u64)));

    // A file whose size and modification time are as they were is not read again: other bytes of the same length change nothing.
    let modified = std::fs::metadata(&transcript).unwrap().modified().unwrap();
    std::fs::write(&transcript, grown.replace("\"input_tokens\":40", "\"input_tokens\":99")).unwrap();
    set_modified(&transcript, modified);
    let (tokens, _) = claude(&summary(&service, &settings).await);
    assert_eq!(tokens, 70, "the cached parse answers");

    // A second transcript, then the first is deleted: its usage stays, and a restarted server (a service that loads the file) has it too.
    let second = projects.join("p").join("two.jsonl");
    std::fs::write(&second, format!("{}\n", claude_line(4, 1000))).unwrap();
    assert_eq!(claude(&summary(&service, &settings).await).0, 1070);
    std::fs::remove_file(&transcript).unwrap();
    let (tokens, source) = claude(&summary(&service, &settings).await);
    assert_eq!((tokens, source["scannedFiles"].as_u64()), (1070, Some(2)), "the saved usage of the deleted transcript still counts");
    let restarted = UsageService::new(&state);
    assert_eq!(claude(&summary(&restarted, &settings).await).0, 1070);

    // The directory is deleted and made again: the source keeps the fingerprint it had, since saved history still reports under it.
    std::fs::remove_dir_all(&projects).unwrap();
    std::fs::create_dir_all(&projects).unwrap();
    let (tokens, source) = claude(&summary(&restarted, &settings).await);
    assert_eq!((tokens, source["fingerprint"]["volumeId"].as_str()), (1070, Some(volume_id.as_str())));

    // Entries last written more than 90 days ago are not kept: read while the file is there, gone with it.
    let ancient = projects.join("p").join("three.jsonl");
    std::fs::create_dir_all(ancient.parent().unwrap()).unwrap();
    std::fs::write(&ancient, format!("{}\n", claude_line(5, 7))).unwrap();
    set_modified(&ancient, std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 24 * 60 * 60));
    let long = json!({"sinceDay":"2020-01-01","untilDay":"2026-10-31","timeZone":"UTC"});
    let read = |service: &std::sync::Arc<UsageService>| {
        let (service, long, settings) = (service.clone(), long.clone(), settings.clone());
        async move { service.read_summary(&long, &settings).await.ok().unwrap() }
    };
    assert_eq!(claude(&read(&restarted).await).0, 1070 + 7);
    assert!(cache_file(&state)["files"].get(ancient.to_str().unwrap()).is_none(), "{}", cache_file(&state));
    std::fs::remove_file(&ancient).unwrap();
    assert_eq!(claude(&read(&restarted).await).0, 1070);
}
