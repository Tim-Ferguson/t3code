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
