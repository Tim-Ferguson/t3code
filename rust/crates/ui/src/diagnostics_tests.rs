//! The Diagnostics page: what a connection that may not read diagnostics sees (through the mounted UI and the real native server, which
//! is asked for nothing), and what the process and trace sections draw from the server's answers. The fixture server runs no resource
//! telemetry, so the live process list is drawn here from an answer of the shape the real server sends (it was read from the real server
//! in the browser); the server's own answers are not faked.
use super::diff_tests::{Fixture, rendered_text, seed, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::control_attribute;
use serde_json::json;

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_read_diagnostics_is_told_so_and_asks_for_nothing() {
    let fixture = Fixture::start(false).await;
    let scopes = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];
    let (mut dom, props) = fixture.client(Gate::default(), &scopes);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    state.view().set(View::Diagnostics);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "This connection cannot read diagnostics.").await;
    assert_eq!(control_attribute(&dom, "Refresh diagnostics", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert!(!rendered_text(&dom).contains("does not have the permission"), "no request was sent to be refused");
    fixture.finish(dom).await;
}

#[derive(Clone)]
struct Answers {
    processes: serde_json::Value,
    traces: serde_json::Value,
    can_signal: bool,
}
fn sections(answers: Answers) -> Element {
    rsx! {
        crate::diagnostics_page::Processes { result: answers.processes, can_signal: answers.can_signal, signalling: None, onsignal: move |_| {} }
        crate::diagnostics_page::Traces { result: answers.traces }
    }
}
fn drawn(answers: Answers) -> String {
    let mut dom = VirtualDom::new_with_props(sections, answers);
    dom.rebuild_in_place();
    crate::runtime::transport_tests::rendered_text(&dom)
}

#[test]
fn the_process_list_shows_each_process_with_its_cpu_memory_and_signals_and_only_a_maintainer_may_send_them() {
    let processes = json!({
        "serverPid": 4242, "readAt": "2026-10-11T10:00:00.000Z", "processCount": 2, "totalRssBytes": 3 * 1024 * 1024, "totalCpuPercent": 12.5,
        "error": {"_tag": "None"},
        "processes": [
            {"pid": 4300, "startTimeMs": 1, "ppid": 4242, "pgid": {"_tag": "None"}, "status": "S", "cpuPercent": 12.5, "rssBytes": 2 * 1024 * 1024, "elapsed": "00:05", "command": "codex app-server", "depth": 0, "childPids": [4301]},
            {"pid": 4301, "startTimeMs": 2, "ppid": 4300, "pgid": {"_tag": "None"}, "status": "S", "cpuPercent": 0.0, "rssBytes": 1024 * 1024, "elapsed": "00:04", "command": "node worker.js", "depth": 1, "childPids": []},
        ],
    });
    let text = drawn(Answers { processes: processes.clone(), traces: json!({"error": {"_tag": "None"}, "recordCount": 0}), can_signal: true });
    for expected in ["Child processes", "12.5%", "3.0 MB", "4242", "codex app-server", "node worker.js", "00:05", "2.0 MB", "INT", "KILL"] {
        assert!(text.contains(expected), "{expected} is drawn: {text}");
    }
    assert!(!text.contains("needs the environment maintenance grant"));
    let viewer = drawn(Answers { processes, traces: json!({"error": {"_tag": "None"}}), can_signal: false });
    assert!(viewer.contains("needs the environment maintenance grant"), "{viewer}");
}

#[test]
fn the_trace_section_names_failures_slow_spans_and_the_busiest_names_or_says_the_file_is_missing() {
    let traces = json!({
        "recordCount": 3, "failureCount": 1, "interruptionCount": 0, "slowSpanThresholdMs": 1000, "slowSpanCount": 1, "parseErrorCount": 0,
        "error": {"_tag": "None"},
        "latestFailures": [{"name": "checkpoint.capture", "cause": "disk full", "durationMs": 9000, "endedAt": "2026-10-11T10:00:00.000Z", "traceId": "t", "spanId": "s2"}],
        "slowestSpans": [{"name": "checkpoint.capture", "durationMs": 9000.0, "endedAt": "2026-10-11T10:00:00.000Z", "traceId": "t", "spanId": "s2"}],
        "topSpansByCount": [{"name": "checkpoint.capture", "count": 2, "failureCount": 1, "totalDurationMs": 9040.0, "averageDurationMs": 4520.0, "maxDurationMs": 9000.0}],
    });
    let text = drawn(Answers { processes: json!({"processes": [], "error": {"_tag": "None"}}), traces, can_signal: false });
    for expected in ["Latest failures", "disk full", "Slowest spans", "9.00 s", "Top span names", "2 calls, 1 failed, 4.52 s on average", "No live descendant processes found."] {
        assert!(text.contains(expected), "{expected} is drawn: {text}");
    }
    let missing = drawn(Answers { processes: json!({"processes": [], "error": {"_tag": "None"}}), traces: json!({"error": {"_tag": "Some", "value": {"kind": "trace-file-not-found", "message": "No local trace files were found."}}}), can_signal: false });
    assert!(missing.contains("No local trace files were found."), "{missing}");
}

// A server with no monitor answers the stream and the history with its refusal, which the server's own outbound check rejects as
// something the original's schema cannot encode (the original's server always has a monitor), so the page's state for it is drawn
// from the refusal's text here and was read from a real server without telemetry in the browser.
#[test]
fn a_server_without_a_resource_monitor_says_so_instead_of_drawing_empty_charts() {
    let mut dom = VirtualDom::new_with_props(|message: String| rsx! { crate::resource_section::Unavailable { message } }, "Resource telemetry is not configured.".to_owned());
    dom.rebuild_in_place();
    let text = crate::runtime::transport_tests::rendered_text(&dom);
    assert!(text.contains("Resource telemetry is not configured.") && text.contains("This server runs no resource monitor"), "{text}");
    assert!(!text.contains("Restart monitor"), "there is no monitor to restart");
}

#[test]
fn the_usage_groups_the_monitors_health_and_a_restart_for_a_monitor_that_is_not_sampling_are_drawn() {
    let snapshot = json!({
        "readAt": "2026-10-11T10:00:00.000Z", "sampleIntervalMs": 2500,
        "groups": {
            "backend": {"processCount": 3, "currentCpuPercent": 12.5, "currentRssBytes": 150 * 1024 * 1024, "ioReadBytesPerSecond": 2048, "ioWriteBytesPerSecond": 0},
            "electron": {"processCount": 0, "currentCpuPercent": 0, "currentRssBytes": 0, "ioReadBytesPerSecond": 0, "ioWriteBytesPerSecond": 0},
            "monitor": {"processCount": 1, "currentCpuPercent": 0.4, "currentRssBytes": 5 * 1024 * 1024, "ioReadBytesPerSecond": 0, "ioWriteBytesPerSecond": 0},
            "allT3": {"processCount": 4, "currentCpuPercent": 12.9, "currentRssBytes": 155 * 1024 * 1024, "ioReadBytesPerSecond": 2048, "ioWriteBytesPerSecond": 0},
        },
        "health": {"native": {"status": "degraded", "lastError": {"_tag": "Some", "value": "sampler stalled"}}, "desktop": {"status": "unavailable", "lastError": {"_tag": "None"}}},
    });
    let drawn = |retry: bool, can_retry: bool| {
        let mut dom = VirtualDom::new_with_props(move |props: (Value, bool, bool)| rsx! { crate::resource_section::Snapshot { value: props.0, retry: props.1, can_retry: props.2, restarting: false, onretry: |_| {} } }, (snapshot.clone(), retry, can_retry));
        dom.rebuild_in_place();
        crate::runtime::transport_tests::rendered_text(&dom)
    };
    let text = drawn(true, true);
    for expected in ["Sampled every 2.5 seconds.", "Backend", "12.5%", "150 MB", "2.00 KB/s", "All T3 Code", "degraded", "sampler stalled", "unavailable", "Restart monitor"] {
        assert!(text.contains(expected), "{expected} is drawn: {text}");
    }
    assert!(!drawn(false, true).contains("Restart monitor"), "a monitor that is sampling is not restarted");

    let buckets = vec![
        json!({"avgCpuPercent": 5.0, "maxCpuPercent": 20.0, "ioReadBytes": 1024, "ioWriteBytes": 0}),
        json!({"avgCpuPercent": 50.0, "maxCpuPercent": 80.0, "ioReadBytes": 0, "ioWriteBytes": 4096}),
    ];
    let mut dom = VirtualDom::new_with_props(|buckets: Vec<Value>| rsx! { crate::resource_section::History { buckets } }, buckets);
    dom.rebuild_in_place();
    // One box per sample, labelled for what it shows; the heights are `t3_client::resource_telemetry::bar_height`'s.
    assert!(crate::runtime::transport_tests::control(&dom, "CPU and I/O history, 2 samples").is_some());
}

#[test]
fn the_process_history_draws_its_totals_a_bar_for_each_bucket_and_the_processes_that_used_the_processor() {
    let result = json!({
        "readAt": "2026-10-11T10:00:00.000Z", "windowMs": 900_000, "bucketMs": 60_000, "sampleIntervalMs": 2500, "retainedSampleCount": 1234, "totalCpuSecondsApprox": 95.0,
        "error": {"_tag": "None"},
        "buckets": [
            {"startedAt": "2026-10-11T09:45:00.000Z", "endedAt": "2026-10-11T09:46:00.000Z", "avgCpuPercent": 5.0, "maxCpuPercent": 20.0, "maxRssBytes": 1, "maxProcessCount": 2},
            {"startedAt": "2026-10-11T09:46:00.000Z", "endedAt": "2026-10-11T09:47:00.000Z", "avgCpuPercent": 50.0, "maxCpuPercent": 80.0, "maxRssBytes": 1, "maxProcessCount": 2},
        ],
        "topProcesses": [
            {"processKey": "4242:1", "pid": 4242, "ppid": 1, "command": "/usr/bin/node /srv/t3/server.js", "depth": 0, "isServerRoot": true, "firstSeenAt": "2026-10-11T09:45:00.000Z", "lastSeenAt": "2026-10-11T10:00:00.000Z",
                "currentCpuPercent": 3.0, "avgCpuPercent": 4.25, "maxCpuPercent": 60.0, "cpuSecondsApprox": 95.0, "currentRssBytes": 100, "maxRssBytes": 150 * 1024 * 1024, "sampleCount": 300},
            {"processKey": "4300:2", "pid": 4300, "ppid": 4242, "command": "codex app-server", "depth": 1, "isServerRoot": false, "firstSeenAt": "2026-10-11T09:45:00.000Z", "lastSeenAt": "2026-10-11T10:00:00.000Z",
                "currentCpuPercent": 0.0, "avgCpuPercent": 1.0, "maxCpuPercent": 9.5, "cpuSecondsApprox": 4.5, "currentRssBytes": 100, "maxRssBytes": 2 * 1024 * 1024, "sampleCount": 300},
        ],
    });
    let mut dom = VirtualDom::new_with_props(|result: Value| rsx! { crate::process_history::History { result } }, result);
    dom.rebuild_in_place();
    let text = crate::runtime::transport_tests::rendered_text(&dom);
    for expected in ["CPU Time", "1.58m", "1,234", "2.50 s", "node", "60.0%", "150.0 MB", "codex", "4.50s", "9.5%", "4300"] {
        assert!(text.contains(expected), "{expected} is drawn: {text}");
    }
    let control = |label: &str| crate::runtime::transport_tests::control(&dom, label).is_some();
    assert!(control("Average CPU 5.0%, peak CPU 20.0%") && control("Average CPU 50.0%, peak CPU 80.0%"), "a bar for each bucket, labelled for its numbers");
    assert!(control("Root process node") && control("Child process codex"));
    // Nothing sampled yet is said, not drawn as an empty table.
    let failed = json!({"buckets": [], "topProcesses": [], "error": {"_tag": "Some", "value": {"failureTag": "ProcessDiagnosticsQueryFailedError", "message": "ps failed"}}});
    let mut empty = VirtualDom::new_with_props(|result: Value| rsx! { crate::process_history::History { result } }, failed);
    empty.rebuild_in_place();
    let text = crate::runtime::transport_tests::rendered_text(&empty);
    assert!(text.contains("No process resource samples found for this window.") && text.contains("ps failed"), "{text}");
}

thread_local! {
    /// The signals the process tree under test asked for.
    static ASKED: std::cell::RefCell<Vec<(u64, u64, &'static str)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn tree_process(pid: u64, ppid: u64, depth: u64, category: &str, name: &str, children: &[u64]) -> Value {
    json!({"identity":{"pid":pid,"startTimeMs":pid * 10},"ppid":ppid,"childPids":children,"depth":depth,"name":name,"command":format!("/bin/{name} --run"),"status":"S","category":category,
        "cpuPercent":12.5,"cpuTimeMs":90_000,"residentBytes":3 * 1024 * 1024,"peakResidentBytes":0,"virtualBytes":0,"ioReadBytes":2048,"ioWriteBytes":4096,"ioReadBytesPerSecond":1024,"ioWriteBytesPerSecond":0,"ioSemantics":"logical","runTimeMs":0})
}

#[test]
fn the_live_process_tree_folds_a_parents_children_and_signals_only_what_the_server_started() {
    use crate::runtime::transport_tests::{click_control, control, control_attribute};
    ASKED.with(|asked| asked.borrow_mut().clear());
    let processes = vec![
        tree_process(1, 0, 0, "server", "t3-server", &[2, 5]),
        tree_process(2, 1, 1, "provider-root", "codex", &[3]),
        tree_process(3, 2, 2, "server-child", "worker", &[]),
        tree_process(5, 1, 1, "resource-monitor", "monitor", &[]),
    ];
    let component = |props: (Vec<Value>, bool)| rsx! { crate::resource_section::ProcessTree { processes: props.0, can_signal: props.1, signalling: None, onsignal: |signal: crate::diagnostics_page::Signalling| ASKED.with(|asked| asked.borrow_mut().push(signal.parts())) } };
    let mut dom = VirtualDom::new_with_props(component, (processes.clone(), true));
    dom.rebuild_in_place();
    let text = crate::runtime::transport_tests::rendered_text(&dom);
    // Every process, with its category, CPU, CPU time, memory and rates; the signals only for what this server started.
    for expected in ["Live process tree", "t3-server", "codex", "worker", "monitor", "Server", "Provider", "Backend child", "Monitor", "12.5%", "1.50m", "3.00 MB", "1.00 KB/s", "2.00 KB", "4.00 KB"] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(control(&dom, "Send SIGINT to process 2").is_some() && control(&dom, "Send SIGINT to process 3").is_some());
    assert!(control(&dom, "Send SIGINT to process 1").is_none() && control(&dom, "Send SIGKILL to process 5").is_none(), "the server and the monitor are not for the page to signal");

    // A parent folds the rows under it, by parentage; folding the root takes the whole tree below it.
    click_control(&mut dom, "Collapse codex");
    let text = crate::runtime::transport_tests::rendered_text(&dom);
    assert!(!text.contains("worker") && text.contains("monitor") && control(&dom, "Expand codex").is_some());
    click_control(&mut dom, "Collapse t3-server");
    let text = crate::runtime::transport_tests::rendered_text(&dom);
    assert!(!text.contains("codex") && !text.contains("monitor") && text.contains("t3-server"));
    click_control(&mut dom, "Expand t3-server");
    assert!(!crate::runtime::transport_tests::rendered_text(&dom).contains("worker"), "the fold of codex is kept");
    click_control(&mut dom, "Expand codex");
    assert!(crate::runtime::transport_tests::rendered_text(&dom).contains("worker"));

    // The signals go to the page with the process and the instant it started.
    click_control(&mut dom, "Send SIGINT to process 3");
    click_control(&mut dom, "Send SIGKILL to process 2");
    assert_eq!(ASKED.with(|asked| asked.borrow().clone()), [(3, 30, "SIGINT"), (2, 20, "SIGKILL")]);

    // Without the maintenance grant they cannot be sent.
    let mut viewer = VirtualDom::new_with_props(component, (processes, false));
    viewer.rebuild_in_place();
    assert_eq!(control_attribute(&viewer, "Send SIGKILL to process 3", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
}

#[test]
fn the_live_process_tree_waits_for_the_monitor_when_it_has_no_processes() {
    let mut dom = VirtualDom::new_with_props(|processes: Vec<Value>| rsx! { crate::resource_section::ProcessTree { processes, can_signal: true, signalling: None, onsignal: |_| {} } }, vec![]);
    dom.rebuild_in_place();
    assert!(crate::runtime::transport_tests::rendered_text(&dom).contains("Waiting for the native process monitor."));
}
