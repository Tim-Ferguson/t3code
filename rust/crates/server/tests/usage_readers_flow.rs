//! The OpenCode and Antigravity usage readers against what the original's own readers make of the same stores
//! (`usage-readers.jsonl`, written by `rust/tools/generate_usage_reader_fixtures.mjs` from the unchanged
//! `opencodeUsageReader.ts` and `antigravityUsageReader.ts`): the stores are rebuilt from the fixture in a temporary directory and read.
use base64::Engine;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use t3_server::usage_readers::{read_antigravity_usage, read_opencode_usage};

fn record(record: &t3_server::usage_readers::Record) -> Value {
    json!({
        "provider": record.provider,
        "timestampMs": record.timestamp_ms,
        "model": record.model,
        "sessionId": record.session_id,
        "totals": {
            "uncachedInputTokens": record.totals.uncached_input,
            "cachedInputTokens": record.totals.cached_input,
            "cacheCreationTokens": record.totals.cache_creation,
            "outputTokens": record.totals.output,
            "reasoningTokens": record.totals.reasoning,
        },
        "reportedCostUsd": record.reported_cost_usd,
        "speed": "standard",
        "dedupeKey": record.dedupe_key,
    })
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/")
}

#[test]
fn both_readers_make_what_the_originals_make_of_the_same_stores() {
    let mut checked = 0;
    for line in include_str!("fixtures/usage-readers.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let name = row["name"].as_str().unwrap();
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-usage-readers-").tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        for (path, content) in row["files"].as_object().unwrap() {
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let bytes = match content["text"].as_str() {
                Some(text) => text.as_bytes().to_vec(),
                None => base64::engine::general_purpose::STANDARD.decode(content["base64"].as_str().unwrap()).unwrap(),
            };
            std::fs::write(&target, bytes).unwrap();
        }
        for (path, ms) in row["mtimes"].as_object().unwrap() {
            let file = std::fs::File::options().write(true).open(root.join(path)).unwrap();
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms.as_u64().unwrap())).unwrap();
        }
        for call in row["calls"].as_array().unwrap() {
            let roots: Vec<PathBuf> = call["roots"].as_array().unwrap().iter().map(|part| root.join(part.as_str().unwrap()).components().collect()).collect();
            let since = call["sinceMs"].as_i64().unwrap();
            let expected = &call["result"];
            let actual = if row["kind"] == "opencode" {
                let read = read_opencode_usage(&roots[0], since);
                let mut files: Vec<Value> = read.files.iter().map(|(path, records)| json!({"path": relative(&root, path), "records": records.iter().map(record).collect::<Vec<_>>()})).collect();
                files.sort_by_key(|file| file["path"].as_str().unwrap().to_owned());
                json!({"files": files, "missing": read.missing, "error": read.error})
            } else {
                let read = read_antigravity_usage(&roots, since);
                json!({
                    "files": read.files.iter().map(|file| json!({"root": relative(&root, &file.root), "path": relative(&root, &file.path), "records": file.records.iter().map(record).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                    "errors": read.errors.iter().map(|path| relative(&root, path)).collect::<Vec<_>>(),
                })
            };
            // The original names the root "" for the directory itself; a path relative to it is "." there and "" here.
            let normalize = |value: &Value| value.to_string().replace("\"root\":\".\"", "\"root\":\"\"");
            assert_eq!(normalize(&actual), normalize(expected), "{name} (since {since})");
            checked += 1;
        }
    }
    assert_eq!(checked, 15);
}
