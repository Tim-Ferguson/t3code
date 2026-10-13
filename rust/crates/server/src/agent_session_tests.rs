//! The agent session scan and import, held to the cases of `AgentSessionScanner.test.ts`,
//! `AgentSessionImporter.test.ts` and `AgentSessionJson.test.ts` (the last is in `agent_session_json.rs`).
//!
//! The original's tests run against a simulated file system (counting opens, serving a template file for
//! many names, failing a stat). These use real directories and hard links to one template instead, so a case
//! that needs to observe or fail a particular call is checked through its effect, or not ported; each such
//! case is noted where it would be.
use super::*;
use crate::project::ProjectService;
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Cursor,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const DAY: i64 = 24 * 60 * 60 * 1000;

/// A machine: a home, a T3 base directory, a temp root, one Claude home and one Codex home, and a store.
struct World {
    directory: tempfile::TempDir,
    host: Host,
    store: Store,
}
impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        for name in ["home", "base", "tmp", "claude", "codex", "work"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let host = Host { home: root.join("home"), base_dir: root.join("base"), temp_dirs: vec![root.join("tmp")], environment: HashMap::new() };
        Self { directory, host, store: Store::memory().unwrap() }
    }
    fn path(&self, relative: &str) -> PathBuf {
        self.directory.path().join(relative)
    }
    fn claude(&self) -> PathBuf {
        self.path("claude")
    }
    fn codex(&self) -> PathBuf {
        self.path("codex")
    }
    /// A directory a session can have run in.
    fn workspace(&self, name: &str) -> PathBuf {
        let path = self.path("work").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    /// Settings as the original's tests give them: the legacy homes, and the instances if there are any.
    fn settings(&self, instances: Value) -> Value {
        let mut settings = json!({"providers":{"claudeAgent":{"homePath":self.claude()},"codex":{"homePath":self.codex()}}});
        if !instances.is_null() {
            settings["providerInstances"] = instances;
        }
        settings
    }
    fn scan(&self) -> Value {
        self.scan_with(Value::Null)
    }
    fn scan_with(&self, instances: Value) -> Value {
        scan(&self.settings(instances), &self.store, &self.host).unwrap()
    }
    fn project(&self, id: &str, root: &Path) {
        ProjectService::new(self.store.clone()).mutate(json!({"type":"project.create","commandId":format!("project-{id}"),"projectId":id,"title":id,"workspaceRoot":root}), Utc::now()).unwrap();
    }
    fn import(&self, project: &str) -> (u64, u64) {
        self.import_with(Value::Null, project)
    }
    fn import_with(&self, instances: Value, project: &str) -> (u64, u64) {
        import(&self.settings(instances), &self.store, &self.host, &ImportRequest { project_id: project, expected_workspace_root: None }).unwrap()
    }
    fn thread(&self, id: &str) -> Option<Value> {
        self.store.projection("thread", id).unwrap()
    }
    /// What a thread shows: its items' kinds and texts.
    fn items(&self, id: &str) -> Vec<(String, String)> {
        let thread = self.thread(id).unwrap_or_else(|| panic!("{id} was not imported"));
        thread["turnItems"].as_array().unwrap().iter().map(|item| (item["type"].as_str().unwrap().to_owned(), item["text"].as_str().unwrap().to_owned())).collect()
    }
    fn texts(&self, id: &str) -> Vec<String> {
        self.items(id).into_iter().map(|(_, text)| text).collect()
    }
}

fn millis(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso).unwrap().timestamp_millis()
}
fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// `writeTranscript`: the file, with its modification time set so ordering never depends on write timing.
fn write(path: &Path, contents: impl AsRef<[u8]>, mtime_ms: i64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    set_mtime(path, mtime_ms);
}
fn set_mtime(path: &Path, mtime_ms: i64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(mtime_ms as u64)).unwrap();
}
fn link(template: &Path, path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::hard_link(template, path).unwrap();
}

/// `claudeSessionLine`: the first record carries the real `cwd`.
fn claude_session_line(cwd: &Path) -> String {
    format!("{}\n{}\n", json!({"type":"user","cwd":cwd,"sessionId":"s1"}), json!({"type":"assistant"}))
}
/// `codexRolloutLine`: session metadata is nested under `payload`.
fn codex_rollout_line(cwd: &Path) -> String {
    format!("{}\n", json!({"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"r1","cwd":cwd}}))
}
fn lines(records: &[Value]) -> String {
    records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
}
fn rollout(world: &World, home: &Path, day: &str, name: &str) -> PathBuf {
    let _ = world;
    home.join("sessions").join(day).join(name)
}
fn session_meta(id: &str, cwd: &Path) -> Value {
    json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}})
}
fn user_event(message: &str) -> Value {
    json!({"type":"event_msg","payload":{"type":"user_message","message":message}})
}
fn paths(scan: &Value) -> Vec<String> {
    scan["candidates"].as_array().unwrap().iter().map(|candidate| candidate["path"].as_str().unwrap().to_owned()).collect()
}
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn base_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

// --- scan ------------------------------------------------------------------------------------

#[test]
fn reads_claude_project_cwds_from_transcripts_newest_first() {
    let world = World::new();
    let (older, newer) = (world.workspace("older"), world.workspace("newer"));
    // Slugs are intentionally lossy; the scanner must not decode them.
    write(&world.claude().join("projects/-slug-older/a.jsonl"), claude_session_line(&older), millis("2026-01-01T00:00:00.000Z"));
    write(&world.claude().join("projects/-slug-older/b.jsonl"), claude_session_line(&older), millis("2026-01-02T00:00:00.000Z"));
    write(&world.claude().join("projects/-slug-newer/c.jsonl"), claude_session_line(&newer), millis("2026-03-01T00:00:00.000Z"));
    let result = world.scan();
    assert_eq!(
        result["candidates"],
        json!([
            {"path":newer,"title":base_name(&newer),"sources":["claudeAgent"],"threadCount":1,"lastActiveAt":"2026-03-01T00:00:00.000Z","alreadyImported":false,"git":null},
            {"path":older,"title":base_name(&older),"sources":["claudeAgent"],"threadCount":2,"lastActiveAt":"2026-01-02T00:00:00.000Z","alreadyImported":false,"git":null},
        ])
    );
}

#[test]
fn groups_codex_rollouts_by_cwd_across_date_directories() {
    let world = World::new();
    let (workspace, other) = (world.workspace("workspace"), world.workspace("other"));
    write(&rollout(&world, &world.codex(), "2026/01/05", "rollout-2026-01-05T10-00-00-aaa.jsonl"), codex_rollout_line(&workspace), millis("2026-01-05T10:00:00.000Z"));
    write(&rollout(&world, &world.codex(), "2026/02/09", "rollout-2026-02-09T10-00-00-bbb.jsonl"), codex_rollout_line(&workspace), millis("2026-02-09T10:00:00.000Z"));
    write(&rollout(&world, &world.codex(), "2026/02/09", "rollout-2026-02-09T11-00-00-ccc.jsonl"), codex_rollout_line(&other), millis("2026-02-09T11:00:00.000Z"));
    assert_eq!(
        world.scan()["candidates"],
        json!([
            {"path":other,"title":base_name(&other),"sources":["codex"],"threadCount":1,"lastActiveAt":"2026-02-09T11:00:00.000Z","alreadyImported":false,"git":null},
            {"path":workspace,"title":base_name(&workspace),"sources":["codex"],"threadCount":2,"lastActiveAt":"2026-02-09T10:00:00.000Z","alreadyImported":false,"git":null},
        ])
    );
}

/// Only `rollout-*.jsonl` files are rollouts; anything else in a day directory is ignored.
#[test]
fn only_rollout_files_are_codex_transcripts() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    let mtime = millis("2026-01-05T10:00:00.000Z");
    write(&rollout(&world, &world.codex(), "2026/01/05", "rollout-a.jsonl"), codex_rollout_line(&workspace), mtime);
    write(&rollout(&world, &world.codex(), "2026/01/05", "notes.jsonl"), codex_rollout_line(&workspace), mtime);
    write(&rollout(&world, &world.codex(), "2026/01/05", "rollout-b.txt"), codex_rollout_line(&workspace), mtime);
    // A transcript directly under the sessions directory is not in the date layout.
    write(&world.codex().join("sessions/rollout-c.jsonl"), codex_rollout_line(&workspace), mtime);
    assert_eq!(world.scan()["candidates"][0]["threadCount"], 1);
}

#[test]
fn does_not_open_a_non_file_transcript() {
    for source in [Source::Claude, Source::Codex] {
        let world = World::new();
        // A directory with a transcript's name.
        let transcript = match source {
            Source::Claude => world.claude().join("projects/-slug/session.jsonl"),
            Source::Codex => world.codex().join("sessions/2026/08/24/rollout-session.jsonl"),
        };
        std::fs::create_dir_all(&transcript).unwrap();
        assert_eq!(world.scan()["candidates"], json!([]));
    }
}

/// `discoverCodexTranscripts` and `discoverClaudeTranscripts` stop at their operation budget and say so.
#[test]
fn stops_directory_reads_at_the_discovery_operation_budget() {
    // The original's 20,001 directories are scaled to a budget of 10 operations: one read of the root, then one each.
    for (shape, build) in [("claude", 0), ("codex", 1)] {
        for (count, truncated) in [(9usize, false), (10, true), (11, true)] {
            let world = World::new();
            let home = world.path(shape);
            for index in 0..count {
                let directory = if build == 0 { home.join("projects").join(format!("project-{index:02}")) } else { home.join("sessions").join(format!("{index:04}")) };
                std::fs::create_dir_all(directory).unwrap();
            }
            let (transcripts, cut) = if build == 0 { discover_claude(&home, "claudeAgent", 10) } else { discover_codex(&home, "codex", 10) };
            assert!(transcripts.is_empty());
            assert_eq!(cut, truncated, "{shape} with {count} directories");
        }
    }
}

#[test]
fn codex_discovery_walks_newest_dates_first_and_stops_with_the_older_ones_unread() {
    let world = World::new();
    let home = world.codex();
    for day in ["2025/12/31", "2026/01/01", "2026/01/02", "2026/02/01"] {
        write(&home.join("sessions").join(day).join("rollout-x.jsonl"), "{}\n", 1_000);
    }
    // The five operations are the reads of sessions, 2026, 2026/02 and 2026/02/01, and the stat of its one rollout.
    let (transcripts, truncated) = discover_codex(&home, "codex", 5);
    assert!(truncated);
    assert_eq!(transcripts.len(), 1);
    assert!(transcripts[0].path.ends_with("sessions/2026/02/01/rollout-x.jsonl"));
    let (all, truncated) = discover_codex(&home, "codex", 100);
    assert!(!truncated);
    let order: Vec<String> = all.iter().map(|transcript| transcript.path.strip_prefix(&home).unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(order, ["sessions/2026/02/01/rollout-x.jsonl", "sessions/2026/01/02/rollout-x.jsonl", "sessions/2026/01/01/rollout-x.jsonl", "sessions/2025/12/31/rollout-x.jsonl"]);
}

#[test]
fn merges_the_same_cwd_seen_by_both_agents_and_flags_imported_projects() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    write(&world.claude().join("projects/-slug/a.jsonl"), claude_session_line(&workspace), millis("2026-01-01T00:00:00.000Z"));
    write(&rollout(&world, &world.codex(), "2026/04/01", "rollout-2026-04-01T09-00-00-aaa.jsonl"), codex_rollout_line(&workspace), millis("2026-04-01T09:00:00.000Z"));
    world.project("project-1", &workspace);
    assert_eq!(
        world.scan()["candidates"],
        json!([{"path":workspace,"title":base_name(&workspace),"projectId":"project-1","sources":["claudeAgent","codex"],"threadCount":2,"lastActiveAt":"2026-04-01T09:00:00.000Z","alreadyImported":true,"git":null}])
    );
}

#[cfg(unix)]
#[test]
fn merges_a_symlink_alias_of_a_codex_directory_with_the_directory() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    let alias = world.path("alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    write(&world.claude().join("projects/-slug/a.jsonl"), claude_session_line(&alias), millis("2026-01-01T00:00:00.000Z"));
    write(&rollout(&world, &world.codex(), "2026/01/02", "rollout-b.jsonl"), codex_rollout_line(&workspace), millis("2026-01-02T00:00:00.000Z"));
    let result = world.scan();
    assert_eq!(result["candidates"].as_array().unwrap().len(), 1);
    assert_eq!((result["candidates"][0]["sources"].clone(), result["candidates"][0]["threadCount"].clone()), (json!(["claudeAgent", "codex"]), json!(2)));
}

#[test]
fn uses_explicit_provider_instance_homes_instead_of_overridden_legacy_homes() {
    let world = World::new();
    let (claude_instance, codex_instance) = (world.path("claude-instance"), world.path("codex-instance"));
    let (legacy, claude_workspace, codex_workspace) = (world.workspace("legacy"), world.workspace("claude"), world.workspace("codex"));
    write(&world.claude().join("projects/-legacy/session.jsonl"), claude_session_line(&legacy), millis("2026-01-01T00:00:00.000Z"));
    write(&claude_instance.join("projects/-actual/session.jsonl"), claude_session_line(&claude_workspace), millis("2026-02-01T00:00:00.000Z"));
    write(&codex_instance.join("sessions/2026/03/01/rollout-instance.jsonl"), codex_rollout_line(&codex_workspace), millis("2026-03-01T00:00:00.000Z"));
    let result = world.scan_with(json!({"claudeAgent":{"driver":"claudeAgent","config":{"homePath":claude_instance}},"codex":{"driver":"codex","config":{"homePath":codex_instance}}}));
    assert_eq!(paths(&result), [path_text(&codex_workspace), path_text(&claude_workspace)]);
}

#[test]
fn scans_each_distinct_home_across_multiple_instances_once() {
    let world = World::new();
    let other_home = world.path("codex-other");
    let (workspace, other) = (world.workspace("workspace"), world.workspace("other"));
    for (home, cwd) in [(world.codex(), &workspace), (other_home.clone(), &other)] {
        write(&home.join("sessions/2026/01/01/rollout-session.jsonl"), codex_rollout_line(cwd), millis("2026-01-01T00:00:00.000Z"));
    }
    let result = world.scan_with(json!({"codex-personal":{"driver":"codex","config":{"homePath":world.codex()}},"codex-work":{"driver":"codex","config":{"homePath":other_home}}}));
    let counts: Vec<Value> = result["candidates"].as_array().unwrap().iter().map(|candidate| candidate["threadCount"].clone()).collect();
    assert_eq!(counts, [json!(1), json!(1)]);
    let mut found = paths(&result);
    found.sort();
    let mut expected = vec![path_text(&workspace), path_text(&other)];
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn honors_provider_instance_home_directory_environment_variables() {
    let world = World::new();
    let (claude_home, codex_home) = (world.path("claude-env"), world.path("codex-env"));
    let (claude_workspace, codex_workspace) = (world.workspace("claude"), world.workspace("codex"));
    write(&claude_home.join("projects/-actual/session.jsonl"), claude_session_line(&claude_workspace), millis("2026-01-01T00:00:00.000Z"));
    write(&codex_home.join("sessions/2026/01/01/rollout-session.jsonl"), codex_rollout_line(&codex_workspace), millis("2026-01-02T00:00:00.000Z"));
    let result = world.scan_with(json!({
        "claudeAgent":{"driver":"claudeAgent","environment":[{"name":"CLAUDE_CONFIG_DIR","value":claude_home,"sensitive":false}],"config":{}},
        "codex":{"driver":"codex","environment":[{"name":"CODEX_HOME","value":codex_home,"sensitive":false}],"config":{}},
    }));
    assert_eq!(paths(&result), [path_text(&codex_workspace), path_text(&claude_workspace)]);
}

/// The process environment's `CODEX_HOME` is used when the instance configures no home, and the instance's own
/// environment (its last entry for the variable) wins over it.
#[test]
fn the_process_codex_home_applies_when_the_instance_names_none() {
    let mut world = World::new();
    let (ambient, own) = (world.path("codex-ambient"), world.path("codex-own"));
    let (ambient_workspace, own_workspace) = (world.workspace("ambient"), world.workspace("own"));
    write(&ambient.join("sessions/2026/01/01/rollout-a.jsonl"), codex_rollout_line(&ambient_workspace), millis("2026-01-01T00:00:00.000Z"));
    write(&own.join("sessions/2026/01/01/rollout-b.jsonl"), codex_rollout_line(&own_workspace), millis("2026-01-01T00:00:00.000Z"));
    world.host.environment.insert("CODEX_HOME".into(), path_text(&ambient));
    let ambient_only = world.scan_with(json!({"codex":{"driver":"codex","config":{}}}));
    assert_eq!(paths(&ambient_only), [path_text(&ambient_workspace)]);
    let overridden = world.scan_with(json!({"codex":{"driver":"codex","environment":[{"name":"CODEX_HOME","value":"/nowhere"},{"name":"CODEX_HOME","value":own}],"config":{}}}));
    assert_eq!(paths(&overridden), [path_text(&own_workspace)]);
    // An explicit home beats the environment.
    let explicit = world.scan_with(json!({"codex":{"driver":"codex","config":{"homePath":own}}}));
    assert_eq!(paths(&explicit), [path_text(&own_workspace)]);
}

#[test]
fn the_default_codex_home_is_under_the_users_home() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    write(&world.host.home.join(".codex/sessions/2026/01/01/rollout-a.jsonl"), codex_rollout_line(&workspace), millis("2026-01-01T00:00:00.000Z"));
    let settings = json!({"providers":{"codex":{"homePath":""}}});
    assert_eq!(paths(&scan(&settings, &world.store, &world.host).unwrap()), [path_text(&workspace)]);
    // The same through an instance with an empty config.
    let instance = json!({"providerInstances":{"codex":{"driver":"codex"}}});
    assert_eq!(paths(&scan(&instance, &world.store, &world.host).unwrap()), [path_text(&workspace)]);
}

/// The settings the server hands the scan are its typed settings, serialized: a fresh install (nothing configured)
/// scans `~/.codex`, and a configured instance its own home.
#[test]
fn the_servers_own_settings_serialized_scan_the_default_and_configured_codex_homes() {
    let world = World::new();
    let (default_workspace, configured_workspace) = (world.workspace("default"), world.workspace("configured"));
    write(&world.host.home.join(".codex/sessions/2026/01/01/rollout-a.jsonl"), codex_rollout_line(&default_workspace), millis("2026-01-01T00:00:00.000Z"));
    let fresh: t3_contracts::ServerSettings = serde_json::from_value(json!({})).unwrap();
    assert_eq!(paths(&scan(&serde_json::to_value(&fresh).unwrap(), &world.store, &world.host).unwrap()), [path_text(&default_workspace)]);

    let configured_home = world.path("codex-configured");
    write(&configured_home.join("sessions/2026/02/01/rollout-b.jsonl"), codex_rollout_line(&configured_workspace), millis("2026-02-01T00:00:00.000Z"));
    let configured: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":true,"config":{"homePath":configured_home},"environment":[{"name":"FIXTURE","value":"1"}]}}})).unwrap();
    assert_eq!(paths(&scan(&serde_json::to_value(&configured).unwrap(), &world.store, &world.host).unwrap()), [path_text(&configured_workspace)]);

    let disabled: t3_contracts::ServerSettings = serde_json::from_value(json!({"providers":{"codex":{"enabled":false}}})).unwrap();
    assert_eq!(scan(&serde_json::to_value(&disabled).unwrap(), &world.store, &world.host).unwrap()["candidates"], json!([]));
}

/// A shadow home holds one account's credentials and links the rest to the shared home, which is where the
/// sessions are.
#[test]
fn a_shadow_home_scans_the_shared_home() {
    let world = World::new();
    let (shared, shadow) = (world.path("codex-shared"), world.path("codex-shadow"));
    let (workspace, hidden) = (world.workspace("workspace"), world.workspace("hidden"));
    write(&shared.join("sessions/2026/01/01/rollout-a.jsonl"), codex_rollout_line(&workspace), millis("2026-01-01T00:00:00.000Z"));
    write(&shadow.join("sessions/2026/01/01/rollout-b.jsonl"), codex_rollout_line(&hidden), millis("2026-01-01T00:00:00.000Z"));
    let result = world.scan_with(json!({"codex-work":{"driver":"codex","config":{"homePath":shared,"shadowHomePath":shadow}}}));
    assert_eq!(paths(&result), [path_text(&workspace)]);
}

#[test]
fn ignores_invalid_provider_instances_while_scanning_the_remaining_providers() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    write(&world.claude().join("projects/-actual/session.jsonl"), claude_session_line(&workspace), millis("2026-01-01T00:00:00.000Z"));
    // The codex instance does not decode, and it is the built-in one, so the legacy home is not used either.
    write(&rollout(&world, &world.codex(), "2026/01/01", "rollout-a.jsonl"), codex_rollout_line(&world.workspace("codex")), millis("2026-01-01T00:00:00.000Z"));
    let result = world.scan_with(json!({"codex":{"driver":"codex","config":{"homePath":123}}}));
    assert_eq!(paths(&result), [path_text(&workspace)]);
    // So does a shadow home that is not text.
    let result = world.scan_with(json!({"codex":{"driver":"codex","config":{"shadowHomePath":["x"]}}}));
    assert_eq!(paths(&result), [path_text(&workspace)]);
}

#[test]
fn does_not_scan_provider_instances_disabled_by_the_envelope_or_config() {
    let world = World::new();
    let (envelope_home, config_home) = (world.path("codex-envelope"), world.path("codex-config"));
    for (home, name) in [(&envelope_home, "envelope"), (&config_home, "config")] {
        write(&home.join(format!("sessions/2026/08/24/rollout-{name}-disabled.jsonl")), codex_rollout_line(&world.workspace(name)), millis("2026-08-24T12:00:00.000Z"));
    }
    let result = world.scan_with(json!({
        "codex-envelope-disabled":{"driver":"codex","enabled":false,"config":{"homePath":envelope_home}},
        "codex-config-disabled":{"driver":"codex","config":{"enabled":false,"homePath":config_home}},
    }));
    assert_eq!(result["candidates"], json!([]));
}

/// Settings that predate provider instances configure the built-in instance under `providers`, and disable it there.
#[test]
fn a_legacy_codex_provider_can_be_disabled() {
    let world = World::new();
    write(&rollout(&world, &world.codex(), "2026/01/01", "rollout-a.jsonl"), codex_rollout_line(&world.workspace("legacy")), millis("2026-01-01T00:00:00.000Z"));
    let mut settings = world.settings(Value::Null);
    assert_eq!(paths(&scan(&settings, &world.store, &world.host).unwrap()).len(), 1);
    settings["providers"]["codex"]["enabled"] = json!(false);
    assert_eq!(scan(&settings, &world.store, &world.host).unwrap()["candidates"], json!([]));
}

#[test]
fn drops_codex_candidates_whose_directory_no_longer_exists_or_is_relative() {
    let world = World::new();
    write(&rollout(&world, &world.codex(), "2026/01/01", "rollout-a.jsonl"), codex_rollout_line(&world.path("does-not-exist")), millis("2026-01-01T00:00:00.000Z"));
    write(&rollout(&world, &world.codex(), "2026/01/02", "rollout-b.jsonl"), format!("{}\n", json!({"type":"session_meta","payload":{"id":"r","cwd":"relative/dir"}})), millis("2026-01-02T00:00:00.000Z"));
    assert_eq!(world.scan()["candidates"], json!([]));
}

#[test]
fn excludes_the_home_directory_temporary_root_and_t3_data_directory_for_codex() {
    let world = World::new();
    let keep = world.workspace("keep");
    let excluded = [world.host.home.clone(), world.host.temp_dirs[0].clone(), world.host.base_dir.clone(), world.host.base_dir.join("worktrees/t3code/wt-1")];
    std::fs::create_dir_all(&excluded[3]).unwrap();
    for (index, cwd) in excluded.iter().chain([&keep]).enumerate() {
        write(&rollout(&world, &world.codex(), "2026/01/01", &format!("rollout-{index}.jsonl")), codex_rollout_line(cwd), millis("2026-01-01T00:00:00.000Z"));
    }
    assert_eq!(paths(&world.scan()), [path_text(&keep)]);
}

/// Codex creates one scratch directory per conversation under `~/Documents/Codex/<date>/<slug>`; neither those nor
/// anything unpacked into Downloads is a project.
#[test]
fn excludes_codex_scratch_directories_and_downloads() {
    let world = World::new();
    let scratch = world.host.home.join("Documents/Codex/2026-09-01/some-conversation");
    let downloads = world.host.home.join("Downloads/unpacked");
    let keep = world.workspace("keep");
    for directory in [&scratch, &downloads] {
        std::fs::create_dir_all(directory).unwrap();
    }
    for (index, cwd) in [&scratch, &downloads, &keep].into_iter().enumerate() {
        write(&rollout(&world, &world.codex(), "2026/09/01", &format!("rollout-{index}.jsonl")), codex_rollout_line(cwd), millis("2026-09-01T00:00:00.000Z"));
    }
    assert_eq!(paths(&world.scan()), [path_text(&keep)]);
}

#[test]
fn skips_linked_git_worktrees_and_reports_the_origin_of_codex_checkouts() {
    let world = World::new();
    let (repo, worktree, plain) = (world.workspace("repo"), world.workspace("worktree"), world.workspace("plain"));
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/config"), "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:pingdotgg/t3code.git\n").unwrap();
    std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display())).unwrap();
    for (index, cwd) in [&repo, &worktree, &plain].into_iter().enumerate() {
        write(&rollout(&world, &world.codex(), "2026/01/01", &format!("rollout-{index}.jsonl")), codex_rollout_line(cwd), millis(&format!("2026-01-0{}T00:00:00.000Z", index + 1)));
    }
    let result = world.scan();
    let found: Vec<(String, Value)> = result["candidates"].as_array().unwrap().iter().map(|candidate| (candidate["path"].as_str().unwrap().to_owned(), candidate["git"].clone())).collect();
    assert_eq!(found, [(path_text(&plain), Value::Null), (path_text(&repo), json!({"remoteKey":"github.com/pingdotgg/t3code","repository":"pingdotgg/t3code"}))]);
}

/// A Codex session's first record carries its instructions, which can be long: the cwd is found within it.
#[test]
fn finds_the_cwd_of_a_codex_session_with_a_long_instruction_header() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    let header = json!({"type":"session_meta","payload":{"id":"long","instructions":"x".repeat(600 * 1024),"cwd":workspace}});
    write(&rollout(&world, &world.codex(), "2026/01/01", "rollout-long.jsonl"), format!("{header}\n"), millis("2026-01-01T00:00:00.000Z"));
    assert_eq!(paths(&world.scan()), [path_text(&workspace)]);
}

#[test]
fn does_not_read_past_a_megabyte_for_a_codex_cwd() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    let header = json!({"type":"session_meta","payload":{"id":"huge","instructions":"x".repeat(1100 * 1024),"cwd":workspace}});
    write(&rollout(&world, &world.codex(), "2026/01/01", "rollout-huge.jsonl"), format!("{header}\n"), millis("2026-01-01T00:00:00.000Z"));
    let result = world.scan();
    assert_eq!(result["candidates"], json!([]));
    assert_eq!(result["truncated"], true);
}

#[test]
fn finds_the_codex_cwd_on_a_later_line_and_a_record_at_the_exact_chunk_boundary() {
    let world = World::new();
    let (later, boundary) = (world.workspace("later"), world.workspace("boundary"));
    write(&rollout(&world, &world.codex(), "2026/01/02", "rollout-later.jsonl"), format!("{}\n{}\n{}", json!({"type":"noise"}), json!({"type":"noise"}), codex_rollout_line(&later)), millis("2026-01-02T00:00:00.000Z"));
    let record = codex_rollout_line(&boundary).trim().to_owned();
    let (prefix, suffix) = ("{\"padding\":\"", format!("\",{}", &record[1..]));
    let contents = format!("{prefix}{}{suffix}", "x".repeat(32 * 1024 - prefix.len() - suffix.len()));
    assert_eq!(contents.len(), 32 * 1024);
    write(&rollout(&world, &world.codex(), "2026/01/01", "rollout-boundary.jsonl"), contents, millis("2026-01-01T00:00:00.000Z"));
    assert_eq!(paths(&world.scan()), [path_text(&later), path_text(&boundary)]);
}

/// 64 MiB of metadata reads are shared by the homes of a source, and the scan says when files were left unread.
#[test]
fn shares_metadata_bytes_across_codex_homes() {
    for count in [64usize, 65] {
        let world = World::new();
        let second_home = world.path("codex-second");
        let (first, second) = (world.workspace("first"), world.workspace("second"));
        let template_of = |home: &Path, workspace: &Path, name: &str| {
            let record = json!({"cwd":workspace}).to_string();
            let template = home.join(name);
            write(&template, format!("{}{record}", " ".repeat(1024 * 1024 - record.len())), millis("2026-01-01T00:00:00.000Z"));
            template
        };
        std::fs::create_dir_all(world.codex()).unwrap();
        let first_template = template_of(&world.codex(), &first, "template.jsonl");
        std::fs::create_dir_all(&second_home).unwrap();
        let second_template = template_of(&second_home, &second, "template.jsonl");
        for index in 0..32 {
            link(&first_template, &rollout(&world, &world.codex(), "2026/01/01", &format!("rollout-{index:03}.jsonl")));
        }
        for index in 0..count - 32 {
            link(&second_template, &rollout(&world, &second_home, "2026/01/01", &format!("rollout-{index:03}.jsonl")));
        }
        let result = world.scan_with(json!({"codex-work":{"driver":"codex","config":{"homePath":second_home}}}));
        let threads: Vec<Value> = result["candidates"].as_array().unwrap().iter().map(|candidate| candidate["threadCount"].clone()).collect();
        let mut found = paths(&result);
        found.sort();
        let mut expected = vec![path_text(&first), path_text(&second)];
        expected.sort();
        assert_eq!(found, expected, "{count} files");
        assert_eq!(threads, [json!(32), json!(32)], "{count} files");
        assert_eq!(result.get("truncated").cloned(), (count == 65).then_some(json!(true)), "{count} files");
    }
}

/// A transcript of unparseable lines uses up its own record budget, not another account's.
#[test]
fn bounds_malformed_metadata_records_without_excluding_another_codex_account() {
    let world = World::new();
    let second_home = world.path("codex-second");
    let workspace = world.workspace("workspace");
    let template = world.codex().join("template.jsonl");
    write(&template, "x\n".repeat(1_001), millis("2026-01-02T00:00:00.000Z"));
    for index in 0..102 {
        link(&template, &rollout(&world, &world.codex(), "2026/01/02", &format!("rollout-{index:03}.jsonl")));
    }
    write(&rollout(&world, &second_home, "2026/01/01", "rollout-session.jsonl"), json!({"cwd":workspace}).to_string(), millis("2026-01-01T00:00:00.000Z"));
    let result = world.scan_with(json!({"codex-work":{"driver":"codex","config":{"homePath":second_home}}}));
    assert_eq!(paths(&result), [path_text(&workspace)]);
    assert_eq!(result["truncated"], true);
}

#[test]
fn skips_malformed_codex_transcripts_without_failing_the_scan() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    write(&rollout(&world, &world.codex(), "2026/05/01", "rollout-broken.jsonl"), "not json at all\n", millis("2026-05-01T00:00:00.000Z"));
    // Valid JSON, but no cwd anywhere in the record.
    write(&rollout(&world, &world.codex(), "2026/05/02", "rollout-no-cwd.jsonl"), "{\"type\":\"summary\"}\n", millis("2026-05-02T00:00:00.000Z"));
    write(&rollout(&world, &world.codex(), "2026/05/03", "rollout-empty.jsonl"), "", millis("2026-05-03T00:00:00.000Z"));
    write(&rollout(&world, &world.codex(), "2026/05/04", "rollout-good.jsonl"), codex_rollout_line(&workspace), millis("2026-05-03T00:00:00.000Z"));
    assert_eq!(world.scan()["candidates"], json!([{"path":workspace,"title":base_name(&workspace),"sources":["codex"],"threadCount":1,"lastActiveAt":"2026-05-03T00:00:00.000Z","alreadyImported":false,"git":null}]));
}

#[test]
fn returns_an_empty_result_when_neither_home_directory_exists() {
    let world = World::new();
    let settings = json!({"providers":{"claudeAgent":{"homePath":world.path("no-claude")},"codex":{"homePath":world.path("no-codex")}}});
    let result = scan(&settings, &world.store, &world.host).unwrap();
    assert_eq!(result["candidates"], json!([]));
    assert!(result["scannedAt"].as_str().unwrap().starts_with("20"));
    assert!(result.get("truncated").is_none());
}

/// 5,000 transcripts are inspected per source, and an account whose files are older still gets its turn.
#[test]
fn keeps_a_second_codex_account_when_the_first_has_5000_newer_files() {
    let world = World::new();
    let second_home = world.path("codex-recent");
    let (old, recent) = (world.workspace("old"), world.workspace("recent"));
    let template = world.codex().join("template.jsonl");
    let now = now_ms();
    write(&template, codex_rollout_line(&old), now);
    for index in 0..5_000 {
        link(&template, &rollout(&world, &world.codex(), "2026/08/24", &format!("rollout-old-{index:04}.jsonl")));
    }
    write(&rollout(&world, &second_home, "2026/08/24", "rollout-recent.jsonl"), codex_rollout_line(&recent), now - 1_000);
    let instances = json!({"codex-work":{"driver":"codex","config":{"homePath":second_home}}});
    let result = world.scan_with(instances);
    assert_eq!(result["truncated"], true);
    let found = paths(&result);
    assert!(found.contains(&path_text(&recent)), "{found:?}");
}

// --- import ----------------------------------------------------------------------------------

fn codex_transcript(session: &str, cwd: &Path, records: &[Value]) -> String {
    let mut all = vec![session_meta(session, cwd)];
    all.extend(records.iter().cloned());
    lines(&all)
}
fn assistant_response(text: &str) -> Value {
    json!({"type":"response_item","timestamp":"2026-08-24T10:01:00.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}})
}

fn record_limit_transcript(cwd: &Path, overflow: bool) -> String {
    let records = format!("{}\n{}\n{}", json!({"type":"session_meta","payload":{"id":"record-limit-session","cwd":cwd}}), user_event("First prompt"), "{}\n".repeat(99_998));
    if overflow { format!("{records}\n{}\n", user_event("Overflow prompt")) } else { records }
}

#[test]
fn counts_terminal_newlines_correctly_with_record_overflow() {
    for overflow in [false, true] {
        let world = World::new();
        let workspace = world.workspace("workspace");
        world.project("project", &workspace);
        let now = now_ms();
        write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-records.jsonl"), record_limit_transcript(&workspace, overflow), now);
        write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-older.jsonl"), codex_transcript("older-session", &workspace, &[user_event("Older prompt")]), now - 1_000);
        assert_eq!(world.import("project"), (1, 1));
        let (kept, left_out) = if overflow { ("older-session", "record-limit-session") } else { ("record-limit-session", "older-session") };
        assert_eq!(world.texts(&format!("import:codex:{kept}")), [if overflow { "Older prompt" } else { "First prompt" }]);
        assert!(world.thread(&format!("import:codex:{left_out}")).is_none());
    }
}

#[test]
fn imports_recent_claude_and_codex_sessions_for_the_selected_project_only() {
    let world = World::new();
    let (workspace, other) = (world.workspace("workspace"), world.workspace("other"));
    world.project("selected", &workspace);
    let now = now_ms();
    let claude = |cwd: &Path, session: &str| {
        lines(&[
            json!({"type":"user","cwd":cwd,"sessionId":session,"timestamp":"2026-08-23T12:00:00.000Z","message":{"role":"user","content":"Fix the project"}}),
            json!({"type":"assistant","sessionId":session,"timestamp":"2026-08-23T12:01:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Done"}]}}),
        ])
    };
    let (recent, old, elsewhere) = ("11111111-2222-4333-8444-555555555555", "22222222-2222-4333-8444-555555555555", "33333333-2222-4333-8444-555555555555");
    write(&world.claude().join("projects/-selected/recent.jsonl"), claude(&workspace, recent), now - DAY);
    write(&world.claude().join("projects/-selected/old.jsonl"), claude(&workspace, old), now - 31 * DAY);
    write(&world.claude().join("projects/-other/other.jsonl"), claude(&other, elsewhere), now - DAY);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-codex-recent.jsonl"), codex_transcript("codex-recent", &workspace, &[json!({"type":"event_msg","timestamp":"2026-08-24T10:00:00.000Z","payload":{"type":"user_message","message":"Review this code"}}), json!({"type":"response_item","timestamp":"2026-08-24T10:01:00.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Looks good"}]}})]), now - 3_600_000);
    assert_eq!(world.import("selected"), (2, 0));
    assert_eq!(world.texts("import:codex:codex-recent"), ["Review this code", "Looks good"]);
    assert_eq!(world.texts(&format!("import:claudeAgent:{recent}")), ["Fix the project", "Done"]);
    assert!(world.thread(&format!("import:claudeAgent:{old}")).is_none());
    assert!(world.thread(&format!("import:claudeAgent:{elsewhere}")).is_none());
}

/// What the original's importer writes for a Codex session: a settled thread whose history is the transcript's,
/// created and last updated when the transcript was, and a provider thread that resumes the session.
#[test]
fn an_imported_codex_thread_resumes_the_session_the_way_the_original_writes_it() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let mtime = now_ms() - 3_600_000;
    let session = "0199c0de-1111-7222-8333-444455556666";
    write(
        &rollout(&world, &world.codex(), "2026/08/24", &format!("rollout-2026-08-24T10-00-00-{session}.jsonl")),
        codex_transcript(session, &workspace, &[json!({"type":"turn_context","payload":{"model":"gpt-5.6-sol"}}), json!({"type":"event_msg","timestamp":"2026-08-24T10:00:00.000Z","payload":{"type":"user_message","message":"Review this code"}}), assistant_response("Looks good")]),
        mtime,
    );
    assert_eq!(world.import("selected"), (1, 0));
    let id = format!("import:codex:{session}");
    let thread = world.thread(&id).unwrap();
    let provider_thread = format!("provider-thread:provider:codex:native-thread:{session}");
    let summary = &thread["thread"];
    assert_eq!(summary["historyOrigin"], "v1_import");
    assert_eq!((summary["createdBy"].clone(), summary["creationSource"].clone()), (json!("system"), json!("server")));
    assert_eq!((summary["projectId"].clone(), summary["title"].clone()), (json!("selected"), json!("Review this code")));
    assert_eq!(summary["modelSelection"], json!({"instanceId":"codex","model":"gpt-5.6-sol"}));
    assert_eq!((summary["runtimeMode"].clone(), summary["interactionMode"].clone()), (json!("full-access"), json!("default")));
    assert_eq!(summary["settledOverride"], "settled");
    assert_eq!((summary["createdAt"].clone(), summary["updatedAt"].clone(), summary["settledAt"].clone()), (json!("2026-08-24T10:00:00.000Z"), json!(iso(mtime)), json!(iso(mtime))));
    assert_eq!(summary["activeProviderThreadId"], provider_thread);
    assert_eq!(thread["providerThreads"].as_array().unwrap().len(), 1);
    let provider = &thread["providerThreads"][0];
    assert_eq!((provider["id"].clone(), provider["driver"].clone(), provider["providerInstanceId"].clone(), provider["status"].clone(), provider["providerSessionId"].clone()), (json!(provider_thread), json!("codex"), json!("codex"), json!("not_loaded"), Value::Null));
    assert_eq!(provider["nativeThreadRef"], json!({"driver":"codex","nativeId":session,"strength":"strong"}));
    assert_eq!(world.items(&id), [("user_message".to_owned(), "Review this code".to_owned()), ("assistant_message".to_owned(), "Looks good".to_owned())]);
    assert_eq!(thread["messages"][0]["createdBy"], "user");
    assert_eq!(thread["messages"][1]["createdBy"], "agent");
    // Importing again finds the thread and leaves it as it was.
    assert_eq!(world.import("selected"), (1, 0));
    assert_eq!(world.items(&id).len(), 2);
}

#[test]
fn a_codex_session_without_a_model_selects_the_default_codex_model() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-a.jsonl"), codex_transcript("no-model", &workspace, &[user_event("Hello")]), now_ms() - 1_000);
    assert_eq!(world.import("selected"), (1, 0));
    assert_eq!(world.thread("import:codex:no-model").unwrap()["thread"]["modelSelection"], json!({"instanceId":"codex","model":"gpt-6-astra"}));
}

/// A Codex session id is whatever the transcript says; only Claude's must be a UUID to be resumed.
#[test]
fn a_codex_session_id_need_not_be_a_uuid() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-a.jsonl"), codex_transcript("not-a-uuid", &workspace, &[user_event("Hello")]), now_ms() - 1_000);
    write(&world.claude().join("projects/-x/b.jsonl"), lines(&[json!({"type":"user","cwd":workspace,"sessionId":"not-a-uuid","message":{"content":"Hi"}})]), now_ms() - 2_000);
    assert_eq!(world.import("selected"), (1, 1));
    assert!(world.thread("import:codex:not-a-uuid").is_some());
    assert!(world.thread("import:claudeAgent:not-a-uuid").is_none());
}

#[test]
fn keeps_the_provider_instance_that_owns_a_custom_codex_session_home() {
    let world = World::new();
    let custom = world.path("codex-custom");
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&custom.join("sessions/2026/08/24/rollout-custom.jsonl"), codex_transcript("custom-session", &workspace, &[user_event("Use my work account")]), now_ms() - 1_000);
    assert_eq!(world.import_with(json!({"codex-work":{"driver":"codex","config":{"homePath":custom}}}), "selected"), (1, 0));
    let thread = world.thread("import:codex-work:custom-session").unwrap();
    assert_eq!(thread["thread"]["providerInstanceId"], "codex-work");
    assert_eq!(thread["thread"]["modelSelection"]["instanceId"], "codex-work");
    assert_eq!(thread["providerThreads"][0]["providerInstanceId"], "codex-work");
}

#[test]
fn suppresses_duplicate_session_copies_without_reporting_a_skipped_import() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let now = now_ms();
    let contents = codex_transcript("copied-session", &workspace, &[user_event("Import this session once")]);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-copy-a.jsonl"), &contents, now - 1_000);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-copy-b.jsonl"), &contents, now - 2_000);
    assert_eq!(world.import("selected"), (1, 0));
}

/// Transcripts of 16 MiB of padding are streamed, not held: the visible history is all that is kept.
#[test]
fn streams_large_transcripts_across_providers_without_hiding_projects() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let now = now_ms();
    for index in 0..4 {
        let session = format!("0000000{index}-2222-4333-8444-555555555555");
        let (path, contents) = if index % 2 == 0 {
            (rollout(&world, &world.codex(), "2026/08/24", &format!("rollout-budget-{index}.jsonl")), codex_transcript(&session, &workspace, &[user_event("Imported prompt")]))
        } else {
            (world.claude().join(format!("projects/selected/{session}.jsonl")), json!({"type":"user","cwd":workspace,"sessionId":session,"message":{"content":"Imported prompt"}}).to_string())
        };
        write(&path, format!("{contents}\n").into_bytes().into_iter().chain(std::iter::repeat_n(b' ', 16 * 1024 * 1024)).collect::<Vec<u8>>(), now - index * 1_000);
    }
    assert_eq!(world.scan()["candidates"][0]["threadCount"], 4);
    assert_eq!(world.import("selected"), (4, 0));
}

#[test]
fn skips_excessive_records_without_blocking_an_older_valid_codex_transcript() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let now = now_ms();
    for (session, padding, mtime) in [("excessive", "\n".repeat(100_001), now), ("older", String::new(), now - 1_000)] {
        write(&rollout(&world, &world.codex(), "2026/08/24", &format!("rollout-{session}.jsonl")), codex_transcript(session, &workspace, &[user_event("Imported prompt")]) + &padding, mtime);
    }
    assert_eq!(world.import("selected"), (1, 1));
    assert!(world.thread("import:codex:older").is_some());
    assert!(world.thread("import:codex:excessive").is_none());
}

#[test]
fn imports_visible_history_from_a_codex_transcript_with_an_oversized_tool_record() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let record = json!({"type":"tool_result","data":""}).to_string();
    let tool = format!("{record}{}", " ".repeat(16 * 1024 * 1024 + 1 - record.len()));
    let transcript = format!("{}\n{tool}", codex_transcript("large-session", &workspace, &[user_event("Import this large session")]));
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-large.jsonl"), transcript, now_ms() - 1_000);
    assert_eq!(world.import("selected"), (1, 0));
    assert_eq!(world.texts("import:codex:large-session"), ["Import this large session"]);
}

/// The original streams a multi-GiB record through its field selector; this one is a megabyte of screenshot in a
/// content block beside the text, which stays out of the history.
#[test]
fn a_screenshot_in_a_codex_response_item_is_not_held() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let screenshot = "A".repeat(2 * 1024 * 1024);
    let transcript = codex_transcript(
        "screenshot-session",
        &workspace,
        &[
            user_event("Look at this"),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Seen"},{"type":"input_image","image_url":format!("data:image/png;base64,{screenshot}")}]}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":screenshot}}),
        ],
    );
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-screenshot.jsonl"), transcript, now_ms() - 1_000);
    assert_eq!(world.import("selected"), (1, 0));
    assert_eq!(world.texts("import:codex:screenshot-session"), ["Look at this", "Seen"]);
}

/// A transcript whose selected history alone is over the 32 MiB budget is refused whole.
#[test]
fn a_codex_transcript_whose_visible_history_exceeds_the_memory_budget_is_skipped() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let big = "x".repeat(9 * 1024 * 1024);
    let records: Vec<Value> = (0..2).map(|_| user_event(&big)).collect();
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-big.jsonl"), codex_transcript("big-session", &workspace, &records), now_ms() - 1_000);
    assert_eq!(world.import("selected"), (0, 1));
    // The same history in smaller pieces is within the budget (each character costs two).
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-ok.jsonl"), codex_transcript("ok-session", &workspace, &[user_event(&"x".repeat(1024 * 1024))]), now_ms() - 1_000);
    assert_eq!(world.import("selected"), (1, 0));
}

#[test]
fn a_codex_transcript_without_a_visible_user_message_is_skipped() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-parse.jsonl"), lines(&[session_meta("parse-session", &workspace)]), now_ms() - 1_000);
    assert_eq!(world.import("selected"), (0, 1));
}

#[test]
fn does_not_import_a_codex_transcript_dated_after_the_current_time() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-future.jsonl"), codex_transcript("future-session", &workspace, &[user_event("Future work")]), now_ms() + 60_000);
    assert_eq!(world.import("selected"), (0, 0));
    assert!(world.thread("import:codex:future-session").is_none());
}

#[test]
fn does_not_import_sessions_from_a_t3_managed_worktree() {
    let world = World::new();
    let workspace = world.host.base_dir.join("worktrees/t3code/managed-worktree");
    std::fs::create_dir_all(&workspace).unwrap();
    world.project("managed", &workspace);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-managed.jsonl"), codex_transcript("managed-session", &workspace, &[user_event("Do not import this session")]), now_ms() - 1_000);
    assert_eq!(world.import("managed"), (0, 0));
    assert!(world.thread("import:codex:managed-session").is_none());
}

#[test]
fn uses_one_deterministic_provider_instance_for_a_shared_session_home() {
    let world = World::new();
    let shared = world.path("codex-shared");
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&shared.join("sessions/2026/08/24/rollout-shared.jsonl"), codex_transcript("shared-session", &workspace, &[user_event("Use the shared session")]), now_ms() - 1_000);
    let instances = json!({
        "codex":{"driver":"codex","config":{"homePath":shared}},
        "codex-personal":{"driver":"codex","config":{"homePath":shared}},
        "codex-work":{"driver":"codex","config":{"homePath":shared}},
    });
    assert_eq!(world.import_with(instances, "selected"), (1, 0));
    assert!(world.thread("import:codex:shared-session").is_some());
    assert!(world.thread("import:codex-work:shared-session").is_none());
}

#[test]
fn uses_configured_order_when_custom_instances_share_a_session_home() {
    let world = World::new();
    let shared = world.path("codex-shared");
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    write(&shared.join("sessions/2026/08/24/rollout-shared.jsonl"), codex_transcript("shared-session", &workspace, &[user_event("Use the first account")]), now_ms() - 1_000);
    let instances = json!({"codex-work":{"driver":"codex","config":{"homePath":shared}},"codex-personal":{"driver":"codex","config":{"homePath":shared}}});
    assert_eq!(world.import_with(instances, "selected"), (1, 0));
    assert!(world.thread("import:codex-work:shared-session").is_some());
    assert!(world.thread("import:codex-personal:shared-session").is_none());
}

#[test]
fn keeps_a_second_account_when_the_first_has_5000_newer_codex_files() {
    let world = World::new();
    let second_home = world.path("codex-recent");
    let (old, recent) = (world.workspace("old"), world.workspace("recent"));
    world.project("recent", &recent);
    let now = now_ms();
    let template = world.codex().join("template.jsonl");
    write(&template, codex_transcript("old-session", &old, &[user_event("Old work")]), now);
    for index in 0..5_000 {
        link(&template, &rollout(&world, &world.codex(), "2026/08/24", &format!("rollout-old-{index:04}.jsonl")));
    }
    write(&rollout(&world, &second_home, "2026/08/24", "rollout-recent.jsonl"), codex_transcript("recent-session", &recent, &[user_event("Recent work")]), now - 1_000);
    let instances = json!({"codex-work":{"driver":"codex","config":{"homePath":second_home}}});
    assert_eq!(world.import_with(instances, "recent"), (1, 0));
    assert!(world.thread("import:codex-work:recent-session").is_some());
}

/// A file that changes between the scan and the read is not trusted: its identity is checked on both sides.
#[test]
fn skips_a_transcript_that_grew_or_shrank_since_its_size_was_checked() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    let path = rollout(&world, &world.codex(), "2026/08/24", "rollout-growing.jsonl");
    let contents = codex_transcript("growing-session", &workspace, &[user_event("Do not import a changing file")]);
    write(&path, &contents, 1_700_000_000_000);
    let identity = identity_of(&std::fs::metadata(&path).unwrap());
    assert!(read_transcript(&path, &identity, MAX_IMPORT_RECORDS, Source::Codex).is_some());
    write(&path, format!("{contents}\nchanged"), 1_700_000_000_000);
    assert!(read_transcript(&path, &identity, MAX_IMPORT_RECORDS, Source::Codex).is_none(), "grew");
    write(&path, &contents[..contents.len() - 10], 1_700_000_000_000);
    assert!(read_transcript(&path, &identity, MAX_IMPORT_RECORDS, Source::Codex).is_none(), "shrank");
    assert!(read_transcript(&world.path("missing.jsonl"), &identity, MAX_IMPORT_RECORDS, Source::Codex).is_none());
}

#[test]
fn an_existing_import_is_left_alone_unless_it_belongs_elsewhere_or_has_activity_of_its_own() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let thread = |session: &str| Imported {
        source: Source::Codex,
        session_id: session.into(),
        title: "Imported".into(),
        model: None,
        created_at: "2026-08-24T10:00:00.000Z".into(),
        updated_at: "2026-08-24T10:05:00.000Z".into(),
        messages: vec![Message { role: "user", text: "Hello".into(), created_at: "2026-08-24T10:00:00.000Z".into() }],
    };
    write_thread(&world.store, "selected", "codex", &thread("first")).unwrap();
    // Again for the same project: untouched. For another project: refused.
    write_thread(&world.store, "selected", "codex", &thread("first")).unwrap();
    assert_eq!(world.items("import:codex:first").len(), 1);
    let elsewhere = write_thread(&world.store, "other", "codex", &thread("first")).unwrap_err();
    assert!(elsewhere.contains("belongs to project 'selected', not 'other'"), "{elsewhere}");
    // A thread of the same id that a person made is not an import.
    let command = |kind: &str, created_by: &str, source: &str| {
        json!({"type":"thread.create","commandId":format!("{kind}-{created_by}"),"threadId":format!("import:codex:{kind}"),"projectId":"selected","title":"Mine","modelSelection":{"instanceId":"codex","model":"gpt-6-astra"},
            "runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"createdBy":created_by,"creationSource":source})
    };
    crate::thread::ThreadService::new(world.store.clone()).dispatch(&command("mine", "user", "web"), Utc::now()).unwrap();
    let modified = write_thread(&world.store, "selected", "codex", &thread("mine")).unwrap_err();
    assert!(modified.contains("already contains non-imported activity"), "{modified}");
    // One an earlier build of this server imported, made by the server and never run, still counts as the import.
    crate::thread::ThreadService::new(world.store.clone()).dispatch(&command("earlier", "system", "server"), Utc::now()).unwrap();
    write_thread(&world.store, "selected", "codex", &thread("earlier")).unwrap();
}

#[test]
fn a_repeated_import_of_a_project_with_both_agents_counts_each_session_once() {
    let world = World::new();
    let workspace = world.workspace("workspace");
    world.project("selected", &workspace);
    let now = now_ms();
    let session = "44444444-2222-4333-8444-555555555555";
    write(&world.claude().join("projects/-x/a.jsonl"), lines(&[json!({"type":"user","cwd":workspace,"sessionId":session,"message":{"content":"Hello"}})]), now - 1_000);
    write(&rollout(&world, &world.codex(), "2026/08/24", "rollout-a.jsonl"), codex_transcript("codex-one", &workspace, &[user_event("Hi")]), now - 2_000);
    assert_eq!(world.import("selected"), (2, 0));
    assert_eq!(world.import("selected"), (2, 0));
    assert_eq!(world.items(&format!("import:claudeAgent:{session}")).len(), 1);
}

// --- parseAgentSessionTranscript -------------------------------------------------------------

/// `parseAgentSessionTranscript`.
fn parse(source: Source, contents: &str, fallback: &str, last_active_ms: i64) -> Option<Imported> {
    let (records, _) = read_records(Cursor::new(contents.as_bytes().to_vec()), source, MAX_IMPORT_RECORDS).unwrap()?;
    parse_records(source, &records, fallback, last_active_ms)
}
fn parse_codex(records: &[Value]) -> Option<Imported> {
    parse(Source::Codex, &lines(records), "fallback", millis("2026-08-24T12:00:00.000Z"))
}
fn texts(thread: &Option<Imported>) -> Vec<&str> {
    thread.as_ref().unwrap().messages.iter().map(|message| message.text.as_str()).collect()
}
fn response_user(turn: Option<&str>, text: &str) -> Value {
    let mut payload = json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]});
    if let Some(turn) = turn {
        payload["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
    }
    json!({"type":"response_item","payload":payload})
}
fn response_assistant(text: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}})
}
fn meta(id: &str) -> Value {
    json!({"type":"session_meta","payload":{"id":id}})
}

#[test]
fn handles_the_exact_record_limit_and_an_interior_blank() {
    for overflow in [false, true] {
        let thread = parse(Source::Codex, &record_limit_transcript(Path::new("/project"), overflow), "unused", millis("2026-08-24T12:00:00.000Z"));
        if overflow {
            assert!(thread.is_none());
        } else {
            assert_eq!(texts(&thread), ["First prompt"]);
        }
    }
}

#[test]
fn keeps_claude_text_and_titles_while_dropping_malformed_and_tool_records() {
    let contents = [
        "not valid json".to_owned(),
        json!({"type":"ai-title","aiTitle":"Fix authentication"}).to_string(),
        json!({"type":"user","sessionId":"claude-session","isMeta":true,"message":{"role":"user","content":"Injected skill instructions"}}).to_string(),
        json!({"type":"user","sessionId":"claude-session","isCompactSummary":true,"message":{"role":"user","content":"Injected compaction summary"}}).to_string(),
        json!({"type":"user","sessionId":"claude-session","timestamp":"2026-08-24T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Fix authentication"}]}}).to_string(),
        json!({"type":"user","sessionId":"claude-session","message":{"role":"user","content":[{"type":"tool_result","text":"hidden"}]}}).to_string(),
        json!({"type":"assistant","sessionId":"claude-session","message":{"role":"assistant","model":"claude-sonnet-5","content":[{"type":"text","text":"Updated the login flow"}]}}).to_string(),
        json!({"type":"assistant","sessionId":"claude-session","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"The provider request failed"}]}}).to_string(),
    ]
    .join("\n");
    let thread = parse(Source::Claude, &contents, "fallback", millis("2026-08-24T12:00:00.000Z"));
    let thread = thread.unwrap();
    assert_eq!((thread.session_id.as_str(), thread.title.as_str(), thread.model.as_deref()), ("claude-session", "Fix authentication", Some("claude-sonnet-5")));
    assert_eq!(thread.messages.iter().map(|message| (message.role, message.text.as_str())).collect::<Vec<_>>(), [("user", "Fix authentication"), ("assistant", "Updated the login flow"), ("assistant", "The provider request failed")]);
    assert_eq!(thread.messages[0].created_at, "2026-08-24T10:00:00.000Z");
}

#[test]
fn drops_injected_codex_instructions_while_keeping_the_visible_user_event() {
    let thread = parse_codex(&[
        json!({"type":"session_meta","payload":{"id":"codex-session"}}),
        response_user(Some("turn-1"), "<user_instructions>\nInternal setup instructions\n</user_instructions>"),
        user_event("Fix the actual bug"),
        response_user(Some("turn-1"), "Fix the actual bug"),
        response_assistant("Fixed"),
    ]);
    assert_eq!(texts(&thread), ["Fix the actual bug", "Fixed"]);
}

#[test]
fn keeps_the_canonical_first_prompt_after_long_codex_transcripts_are_capped() {
    let canonical_prompt = "\n  Keep the canonical prompt  \n";
    let canonical_timestamp = "2026-08-24T10:01:00.000Z";
    let mut records = vec![
        meta("codex-session"),
        json!({"type":"response_item","timestamp":"2026-08-24T10:00:00.000Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Keep the canonical prompt"}]}}),
        json!({"type":"event_msg","timestamp":canonical_timestamp,"payload":{"type":"user_message","message":canonical_prompt}}),
    ];
    records.extend((0..200).map(|index| json!({"type":"response_item","timestamp":format!("2026-08-24T11:{:02}:00.000Z", index % 60),"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("Assistant message {index}")}]}})));
    let thread = parse_codex(&records).unwrap();
    assert_eq!(thread.messages.len(), 200);
    assert_eq!((thread.messages[0].role, thread.messages[0].text.as_str(), thread.messages[0].created_at.as_str()), ("user", canonical_prompt, canonical_timestamp));
}

#[test]
fn restores_the_canonical_first_prompt_when_a_later_user_message_remains() {
    let canonical_prompt = "\n  Keep the canonical prompt  \n";
    let canonical_timestamp = "2026-08-24T10:01:00.000Z";
    let mut records = vec![
        meta("codex-session"),
        json!({"type":"response_item","timestamp":"2026-08-24T10:00:00.000Z","payload":{"type":"message","role":"user","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"},"content":[{"type":"input_text","text":"Keep the canonical prompt"}]}}),
        json!({"type":"event_msg","timestamp":canonical_timestamp,"payload":{"type":"user_message","message":canonical_prompt}}),
    ];
    records.extend((0..198).map(|index| json!({"type":"response_item","timestamp":format!("2026-08-24T11:{:02}:00.000Z", index % 60),"payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("Assistant message {index}")}]}})));
    records.push(json!({"type":"event_msg","timestamp":"2026-08-24T11:58:30.000Z","payload":{"type":"user_message","message":"Keep this later prompt"}}));
    records.push(json!({"type":"response_item","timestamp":"2026-08-24T11:59:00.000Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Keep this latest response"}]}}));
    let thread = parse_codex(&records).unwrap();
    assert_eq!(thread.messages.len(), 200);
    assert_eq!((thread.messages[0].role, thread.messages[0].text.as_str(), thread.messages[0].created_at.as_str()), ("user", canonical_prompt, canonical_timestamp));
    assert_eq!(thread.messages.iter().filter(|message| message.text.trim() == canonical_prompt.trim()).count(), 1);
    assert!(thread.messages.iter().any(|message| message.text == "Keep this later prompt"));
    assert_eq!(thread.messages.last().unwrap().text, "Keep this latest response");
}

#[test]
fn keeps_mixed_format_response_users_when_turn_ids_repeat_after_an_assistant() {
    let thread = parse_codex(&[
        meta("codex-session"),
        response_user(Some("turn-older"), "Keep this older prompt"),
        user_event("Keep this newer prompt"),
        response_user(Some("turn-newer"), "Keep this newer prompt"),
        response_assistant("Ask again when needed"),
        response_user(Some("turn-newer"), "Keep this newer prompt"),
    ]);
    assert_eq!(texts(&thread), ["Keep this older prompt", "Keep this newer prompt", "Ask again when needed", "Keep this newer prompt"]);
}

#[test]
fn preserves_response_user_text_when_codex_turn_metadata_is_ambiguous() {
    let thread = parse_codex(&[
        meta("codex-session"),
        json!({"type":"response_item","payload":{"type":"message","role":"user","internal_chat_message_metadata_passthrough":["unexpected"],"content":[{"type":"input_text","text":"Keep this legacy prompt"}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","internal_chat_message_metadata_passthrough":{"turn_id":"   "},"content":[{"type":"input_text","text":"Keep this prompt with a blank turn ID"}]}}),
    ]);
    assert_eq!(texts(&thread), ["Keep this legacy prompt", "Keep this prompt with a blank turn ID"]);
}

#[test]
fn uses_the_first_valid_codex_session_id_when_a_fork_copies_ancestor_metadata() {
    let thread = parse_codex(&[json!({"type":"session_meta","payload":{"id":"fork-session","forked_from_id":"parent-session"}}), meta("parent-session"), user_event("Continue in the fork")]);
    assert_eq!(thread.unwrap().session_id, "fork-session");
    // `session_id` stands in for a missing `id`.
    let thread = parse_codex(&[json!({"type":"session_meta","payload":{"session_id":"older-style"}}), user_event("Hi")]);
    assert_eq!(thread.unwrap().session_id, "older-style");
}

#[test]
fn skips_codex_transcripts_without_a_resumable_session_id() {
    let thread = parse(Source::Codex, &user_event("This transcript has no session metadata").to_string(), "rollout-2026-08-24T12-00-00-not-a-session-id", millis("2026-08-24T12:00:00.000Z"));
    assert!(thread.is_none());
}

#[test]
fn uses_the_canonical_codex_event_when_its_turn_has_generated_response_context() {
    let thread = parse(
        Source::Codex,
        &lines(&[
            meta("codex-session"),
            response_user(Some("turn-1"), "<environment_context>\n<cwd>/tmp/project</cwd>\n<shell>zsh</shell>\n</environment_context>"),
            response_user(Some("turn-1"), "# AGENTS.md instructions for /tmp/project\n\n<INSTRUCTIONS>\nPrivate project rules\n</INSTRUCTIONS>"),
            user_event("Do something here so it looks like a real project."),
            response_user(Some("turn-1"), "Do something here so it looks like a real project."),
            response_assistant("Created the project."),
        ]),
        "fallback",
        millis("2026-08-25T08:00:00.000Z"),
    );
    assert_eq!(thread.as_ref().unwrap().title, "Do something here so it looks like a real project.");
    assert_eq!(texts(&thread), ["Do something here so it looks like a real project.", "Created the project."]);
}

#[test]
fn preserves_context_markup_in_response_only_codex_messages() {
    let context = "<environment_context>\n<cwd>/tmp/project</cwd>\n</environment_context>";
    let thread = parse_codex(&[meta("codex-session"), response_user(None, context), response_user(None, "Initialize Git and add a README.")]);
    assert_eq!(thread.as_ref().unwrap().title, "<environment_context>");
    assert_eq!(texts(&thread), [context, "Initialize Git and add a README."]);
}

#[test]
fn preserves_a_canonical_codex_event_that_starts_with_context_markup() {
    let prompt = "<environment_context>\n<cwd>/tmp/project</cwd>\n</environment_context>\n\nCreate a useful project.";
    let thread = parse_codex(&[meta("codex-session"), user_event(prompt)]);
    assert_eq!(thread.as_ref().unwrap().title, "<environment_context>");
    assert_eq!(texts(&thread), [prompt]);
}

#[test]
fn preserves_a_codex_request_heading_in_a_canonical_event() {
    let prompt = "\n  ## My request for Codex:\n\nFix the visible bug";
    let thread = parse_codex(&[meta("codex-session"), user_event(prompt)]);
    assert_eq!(thread.as_ref().unwrap().title, "## My request for Codex:");
    assert_eq!(texts(&thread), [prompt]);
}

#[test]
fn keeps_context_markup_quoted_inside_visible_codex_user_text() {
    let quoted = "Do not remove this example:\n<environment_context>\n<cwd>/tmp/example</cwd>\n</environment_context>";
    let thread = parse_codex(&[meta("codex-session"), user_event(quoted)]);
    assert_eq!(texts(&thread), [quoted]);
}

#[test]
fn skips_sessions_without_a_visible_user_message() {
    let thread = parse(Source::Claude, &json!({"type":"assistant","message":{"role":"assistant","content":"Done"}}).to_string(), "claude-session", millis("2026-08-24T12:00:00.000Z"));
    assert!(thread.is_none());
}

#[test]
fn keeps_the_first_prompt_when_later_assistant_output_exceeds_the_message_limit() {
    let mut records = vec![json!({"type":"user","sessionId":"claude-session","message":{"role":"user","content":"Keep this prompt"}})];
    records.extend((0..250).map(|index| json!({"type":"assistant","message":{"role":"assistant","content":format!("Assistant update {index}")}})));
    let thread = parse(Source::Claude, &lines(&records), "fallback", millis("2026-08-24T12:00:00.000Z")).unwrap();
    assert_eq!(thread.messages.len(), 200);
    assert_eq!(thread.messages[0].text, "Keep this prompt");
    assert_eq!(thread.messages.last().unwrap().text, "Assistant update 249");
}

/// A record that does not match the schema as a whole is dropped, as the original's decode drops it.
#[test]
fn a_record_with_a_mistyped_field_is_dropped_whole() {
    let thread = parse_codex(&[
        meta("codex-session"),
        // `message` must be text; the record is not a user message at all.
        json!({"type":"event_msg","payload":{"type":"user_message","message":7}}),
        // Content must be a list of blocks.
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":"plain"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":null}}),
        user_event("The one that decodes"),
    ]);
    assert_eq!(texts(&thread), ["The one that decodes"]);
}

#[test]
fn the_title_is_the_first_line_of_the_first_prompt_cut_to_100_units() {
    let long = format!("{}\nsecond line", "é".repeat(150));
    let thread = parse_codex(&[meta("codex-session"), user_event(&long)]).unwrap();
    assert_eq!(thread.title, "é".repeat(100));
    let thread = parse_codex(&[meta("codex-session"), user_event("   \n  ")]);
    assert!(thread.is_none());
    let thread = parse_codex(&[meta("codex-session"), user_event(&format!("{}x", "😀".repeat(60)))]).unwrap();
    assert_eq!(thread.title, "😀".repeat(50));
}
