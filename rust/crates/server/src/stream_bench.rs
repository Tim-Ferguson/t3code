//! What a streamed assistant delta costs on threads of different lengths, through the write path a provider adapter takes
//! (`codex_runtime::commit`: read, plan, withhold by `responseStreamingMode`, reduce, store, publish) and then what each subscribed
//! client is sent (`transport::stream_item`, for one `orchestration.subscribeThread` stream and one `orchestration.subscribeShell`
//! stream, the way the stream loop calls it). Run on demand, in release, not by the suite:
//!   cargo test --release -p t3-server --lib stream_bench -- --ignored --nocapture --test-threads 1
//! The other half of this file is the guard that holds the *work* a delta does to a bound that does not grow with the thread
//! (`a_delta_does_work_in_proportion_to_its_message_alone`); it counts bytes and rows, never time.
use crate::{
    assistant_streaming::{Mode, install_source},
    auth::AuthService,
    codex_runtime::commit,
    persistence::{Event, Store, StoredEvent, legacy, work, write_projection},
    shell_enrichment::Dedupe,
    transport::{ApiState, live_items},
};
use serde_json::{Value, json};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// Counts allocations so the bench can report them; costs two relaxed increments.
struct Counting;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}
#[global_allocator]
static COUNTING: Counting = Counting;

fn thread_cpu_nanos() -> u64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

const AT: &str = "2026-01-01T00:00:00.000Z";

fn thread_record(id: &str) -> Value {
    json!({"createdBy":"user","creationSource":"web","id":id,"projectId":"project","title":"Streaming","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"gpt-5.4"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"activeProviderThreadId":"pt","lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":id},"forkedFrom":null,"createdAt":AT,"updatedAt":AT,"archivedAt":null,"settledOverride":null,"settledAt":null,"snoozedUntil":null,"snoozedAt":null,"lastVisitedAt":null,"deletedAt":null})
}

fn turn_item(thread: &str, index: usize, kind: &str, text: &str) -> Value {
    let mut item = json!({"id":format!("turn-item:{thread}:{index}"),"threadId":thread,"runId":"run:1","nodeId":format!("node:{thread}:{index}"),"providerThreadId":"pt","providerTurnId":"turn:1","parentItemId":null,"ordinal":index,"type":kind,"status":"completed","title":null,"updatedAt":AT,"startedAt":AT,"completedAt":AT,"nativeItemRef":{"driver":"codex","nativeId":format!("native-{index}"),"strength":"strong"}});
    match kind {
        "command_execution" => {
            item["input"] = json!("cargo test -p t3-server");
            item["output"] = json!(text);
            item["exitCode"] = json!(0);
        }
        _ => {
            item["messageId"] = json!(format!("message:{thread}:{index}"));
            item["text"] = json!(text);
            item["streaming"] = json!(false);
        }
    }
    item
}

fn node(thread: &str, index: usize) -> Value {
    json!({"id":format!("node:{thread}:{index}"),"threadId":thread,"runId":"run:1","parentNodeId":"node:root","rootNodeId":"node:root","kind":"assistant_message","status":"completed","countsForRun":false,"providerThreadId":"pt","providerTurnId":"turn:1","nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":AT,"completedAt":AT})
}

/// Stores a thread of `items` conversation items (as many nodes, and a message for each text item) in one write: building it by
/// events would cost as many writes as the thread is long.
fn seed(store: &Store, id: &str, items: usize) -> usize {
    let created = Event { event_id: format!("created:{id}"), aggregate_kind: "thread".into(), aggregate_id: id.into(), occurred_at: AT.into(), command_id: None, causation_event_id: None, correlation_id: None, event_type: "thread.created".into(), payload: thread_record(id), metadata: json!({}) };
    let mut projection = crate::thread::projection_after(None, &created).unwrap();
    projection["runs"] = json!([{"id":"run:1","threadId":id,"ordinal":1,"status":"running","requestedAt":AT,"startedAt":AT,"completedAt":null,"activeAttemptId":"attempt:1"}]);
    let (mut turn_items, mut nodes, mut messages) = (vec![], vec![], vec![]);
    for index in 0..items {
        let (kind, text) = match index % 3 {
            0 => ("user_message", "Please look at the failing test in the server crate and tell me why it hangs when the socket closes."),
            1 => ("assistant_message", "The test waits on a channel that the connection task only drops after the heartbeat, so the close is never observed."),
            _ => ("command_execution", "running 3 tests\ntest connection::closes ... ok\ntest connection::heartbeat ... ok\ntest result: ok. 3 passed; 0 failed"),
        };
        let item = turn_item(id, index, kind, text);
        if kind != "command_execution" {
            messages.push(json!({"createdBy":"agent","creationSource":"provider","id":item["messageId"],"threadId":id,"runId":"run:1","nodeId":item["nodeId"],"role":if kind == "user_message" { "user" } else { "assistant" },"text":text,"attachments":[],"streaming":false,"createdAt":AT,"updatedAt":AT}));
        }
        nodes.push(node(id, index));
        turn_items.push(item);
    }
    projection["turnItems"] = Value::Array(turn_items);
    projection["nodes"] = Value::Array(nodes);
    projection["messages"] = Value::Array(messages);
    projection["visibleTurnItems"] = Value::Array(crate::thread_fork::local_visible_rows(&projection));
    projection[crate::shell_state::KEY] = crate::shell_state::scan(&projection);
    let bytes = serde_json::to_string(&projection).unwrap().len();
    store.transaction(|transaction| write_projection(transaction, "thread", id, &projection)).unwrap();
    bytes
}

fn state(store: &Store) -> ApiState {
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_test".into(), "loopback-browser".into()).unwrap();
    ApiState { store: store.clone(), auth, environment: json!({"environmentId":"bench"}), config: None, settings: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None, discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None }
}

/// What the adapter plans for streaming assistant text (`codex_projection::Projector::assistant`): the node, and the item with its
/// text so far. It looks the item's ordinal up in the projection it is handed, as the adapters do.
fn assistant_delta(view: &Value, thread: &str, text: &str) -> Vec<(&'static str, Value)> {
    let item_id = format!("turn-item:{thread}:stream");
    let _ordinal = view["turnItems"].as_array().into_iter().flatten().find(|item| item["id"] == item_id.as_str()).and_then(|item| item["ordinal"].as_u64()).unwrap_or_else(|| view["turnItems"].as_array().map_or(0, |items| items.len() as u64));
    let mut item = turn_item(thread, 0, "assistant_message", text);
    item["id"] = json!(item_id);
    item["nodeId"] = json!(format!("node:{thread}:stream"));
    item["messageId"] = json!(format!("message:{thread}:stream"));
    item["ordinal"] = json!(7);
    item["status"] = json!("running");
    item["streaming"] = json!(true);
    item["completedAt"] = Value::Null;
    let mut node = node(thread, 0);
    node["id"] = json!(format!("node:{thread}:stream"));
    node["status"] = json!("running");
    vec![("node.updated", node), ("turn-item.updated", item)]
}

/// A running tool call whose output grows (`Projector::flush`: finds the row, rewrites its output).
fn tool_delta(view: &Value, thread: &str, output: &str) -> Vec<(&'static str, Value)> {
    let id = format!("turn-item:{thread}:tool");
    let mut row = view["turnItems"].as_array().into_iter().flatten().find(|item| item["id"] == id.as_str()).cloned().unwrap_or_else(|| {
        let mut row = turn_item(thread, 0, "command_execution", "");
        row["id"] = json!(id);
        row["nodeId"] = json!(format!("node:{thread}:tool"));
        row["ordinal"] = json!(7);
        row["status"] = json!("running");
        row["completedAt"] = Value::Null;
        row
    });
    row["output"] = json!(output);
    vec![("turn-item.updated", row)]
}

#[derive(Default, Debug, Clone, Copy)]
struct Cost {
    wall_us: f64,
    cpu_us: f64,
    allocations: f64,
    allocated_kb: f64,
    stored_bytes: f64,
    parsed_bytes: f64,
    reads: f64,
    writes: f64,
    rows_cloned: f64,
    thread_bytes: f64,
    shell_bytes: f64,
}

struct Rig {
    store: Store,
    state: ApiState,
    thread_events: tokio::sync::broadcast::Receiver<Vec<StoredEvent>>,
    shell_events: tokio::sync::broadcast::Receiver<Vec<StoredEvent>>,
    dedupe: Dedupe,
    _directory: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("bench.sqlite")).unwrap();
        let state = state(&store);
        let (thread_events, shell_events) = (store.subscribe(), store.subscribe());
        Rig { store, state, thread_events, shell_events, dedupe: Dedupe::default(), _directory: directory }
    }

    /// Delivers what a commit published to the two subscribed clients, as the stream loop does; returns the bytes each is sent.
    fn deliver(&mut self, thread: &str) -> (usize, usize) {
        let (mut thread_bytes, mut shell_bytes) = (0, 0);
        while let Ok(batch) = self.thread_events.try_recv() {
            for item in live_items(&self.state, batch, true, false, Some(thread), 0).unwrap() {
                thread_bytes += serde_json::to_vec(&item).unwrap().len();
            }
        }
        let legacy = legacy::on();
        // The shell stream takes every batch that is waiting together, then drops the shells it has already sent. Before, it
        // read the thread for each event and sent each shell.
        let mut waiting = Vec::new();
        while let Ok(batch) = self.shell_events.try_recv() {
            waiting.extend(batch);
        }
        let groups: Vec<Vec<StoredEvent>> = if legacy { waiting.into_iter().map(|event| vec![event]).collect() } else { vec![waiting] };
        for group in groups {
            for item in live_items(&self.state, group, false, false, None, 0).unwrap() {
                if legacy || self.dedupe.keep(&item) {
                    shell_bytes += serde_json::to_vec(&item).unwrap().len();
                }
            }
        }
        (thread_bytes, shell_bytes)
    }

    /// One write by an adapter, then its delivery; the cost of both.
    fn step(&mut self, thread: &str, plan: impl FnOnce(&Value) -> Vec<(&'static str, Value)>) -> Cost {
        let _ = work::take();
        let (wall, cpu) = (std::time::Instant::now(), thread_cpu_nanos());
        let (allocations, allocated) = (ALLOCATIONS.load(Ordering::Relaxed), ALLOCATED_BYTES.load(Ordering::Relaxed));
        commit(&self.store, thread, |view| Ok(plan(view))).unwrap();
        let (thread_bytes, shell_bytes) = self.deliver(thread);
        let (cpu, wall) = (thread_cpu_nanos() - cpu, wall.elapsed());
        let (allocations, allocated) = (ALLOCATIONS.load(Ordering::Relaxed) - allocations, ALLOCATED_BYTES.load(Ordering::Relaxed) - allocated);
        let work = work::take();
        Cost {
            wall_us: wall.as_secs_f64() * 1e6,
            cpu_us: cpu as f64 / 1e3,
            allocations: allocations as f64,
            allocated_kb: allocated as f64 / 1024.0,
            stored_bytes: (work.event_bytes + work.projection_write_bytes) as f64,
            parsed_bytes: work.projection_read_bytes as f64,
            reads: work.projection_reads as f64,
            writes: work.projection_writes as f64,
            rows_cloned: work.rows_cloned as f64,
            thread_bytes: thread_bytes as f64,
            shell_bytes: shell_bytes as f64,
        }
    }
}

fn mean(costs: &[Cost]) -> Cost {
    let n = costs.len() as f64;
    let sum = |field: fn(&Cost) -> f64| costs.iter().map(field).sum::<f64>() / n;
    Cost { wall_us: sum(|c| c.wall_us), cpu_us: sum(|c| c.cpu_us), allocations: sum(|c| c.allocations), allocated_kb: sum(|c| c.allocated_kb), stored_bytes: sum(|c| c.stored_bytes), parsed_bytes: sum(|c| c.parsed_bytes), reads: sum(|c| c.reads), writes: sum(|c| c.writes), rows_cloned: sum(|c| c.rows_cloned), thread_bytes: sum(|c| c.thread_bytes), shell_bytes: sum(|c| c.shell_bytes) }
}

fn text_of(size: usize) -> String {
    "The connection task drops the channel only after the heartbeat. ".repeat(size / 64 + 1)[..size].to_owned()
}

const DELTA: usize = 160;

/// `deltas` streamed writes of one assistant message that starts `message` bytes long on a thread of `items` items.
fn streaming(items: usize, message: usize, mode: Option<Mode>, deltas: usize, before: bool) -> (usize, Cost) {
    legacy::set(before);
    let mut rig = Rig::new();
    let blob = seed(&rig.store, "thread:big", items);
    if let Some(mode) = mode {
        install_source(&rig.store, Arc::new(move |_| Some(mode)));
    }
    let mut text = text_of(message);
    rig.step("thread:big", |view| assistant_delta(view, "thread:big", &text));
    let costs: Vec<Cost> = (0..deltas)
        .map(|_| {
            text.push_str(&text_of(DELTA));
            rig.step("thread:big", |view| assistant_delta(view, "thread:big", &text))
        })
        .collect();
    (blob, mean(&costs))
}

fn header() {
    eprintln!("{:<34} {:>9} {:>9} {:>9} {:>10} {:>10} {:>9} {:>8} {:>8} {:>8} {:>8}", "scenario", "wall us", "cpu us", "allocs", "alloc KB", "stored B", "parsed B", "reads", "rows cp", "thr B", "shell B");
}
fn row(label: &str, cost: Cost) {
    eprintln!("{label:<34} {:>9.0} {:>9.0} {:>9.0} {:>10.0} {:>10.0} {:>9.0} {:>8.1} {:>8.0} {:>8.0} {:>8.0}", cost.wall_us, cost.cpu_us, cost.allocations, cost.allocated_kb, cost.stored_bytes, cost.parsed_bytes, cost.reads, cost.rows_cloned, cost.thread_bytes, cost.shell_bytes);
}

#[test]
#[ignore]
fn streaming_costs_by_thread_length_and_message_size() {
    header();
    for before in [true, false] {
        let tag = if before { "BEFORE" } else { "AFTER " };
        for items in [10, 300, 600, 3000, 10000] {
            for message in [1_000, 40_000, 200_000] {
                let (blob, cost) = streaming(items, message, None, 30, before);
                row(&format!("{tag} assistant {items}i {}K msg, {}K", message / 1000, blob / 1000), cost);
            }
        }
        // Deltas inside the 400 ms window are withheld; the rows the filter holds are laid over what the adapter plans from.
        for items in [300, 3000, 10000] {
            let (blob, cost) = streaming(items, 40_000, Some(Mode::Paragraph), 30, before);
            row(&format!("{tag} withheld {items}i 40K msg, {}K", blob / 1000), cost);
        }
        // A running tool call whose output grows by 160 bytes from 2 KB.
        for items in [10, 300, 3000, 10000] {
            legacy::set(before);
            let mut rig = Rig::new();
            seed(&rig.store, "thread:big", items);
            let mut output = text_of(2_000);
            rig.step("thread:big", |view| tool_delta(view, "thread:big", &output));
            let costs: Vec<Cost> = (0..30)
                .map(|_| {
                    output.push_str(&text_of(DELTA));
                    rig.step("thread:big", |view| tool_delta(view, "thread:big", &output))
                })
                .collect();
            row(&format!("{tag} tool call {items}i"), mean(&costs));
        }
        // An unrelated 10-item thread's delta while a big thread has just streamed.
        for items in [10, 3000, 10000] {
            legacy::set(before);
            let mut rig = Rig::new();
            seed(&rig.store, "thread:big", items);
            seed(&rig.store, "thread:small", 10);
            let mut text = text_of(1_000);
            rig.step("thread:big", |view| assistant_delta(view, "thread:big", &text));
            let costs: Vec<Cost> = (0..30)
                .map(|_| {
                    text.push_str(&text_of(DELTA));
                    rig.step("thread:small", |view| assistant_delta(view, "thread:small", &text))
                })
                .collect();
            row(&format!("{tag} small thread beside {items}i"), mean(&costs));
        }
    }
    legacy::set(false);
}

/// The work of a delta, in units that do not depend on the machine's load. The bound is the size of the message being streamed
/// (the original sends and stores a whole item per update too) plus a constant: it must not grow with the length of the thread.
/// What does grow with it, by the design of one stored document per thread, is the document itself: serialised and handed to
/// SQLite on every write (`stored_bytes`), which the guard reports but cannot bound.
#[test]
fn a_delta_does_work_in_proportion_to_its_message_alone() {
    let measure = |items: usize, mode: Option<Mode>, before: bool| {
        legacy::set(before);
        let mut rig = Rig::new();
        seed(&rig.store, "thread:big", items);
        if let Some(mode) = mode {
            install_source(&rig.store, Arc::new(move |_| Some(mode)));
        }
        let mut text = text_of(2_000);
        rig.step("thread:big", |view| assistant_delta(view, "thread:big", &text));
        let costs: Vec<Cost> = (0..4)
            .map(|_| {
                text.push_str(&text_of(DELTA));
                rig.step("thread:big", |view| assistant_delta(view, "thread:big", &text))
            })
            .collect();
        legacy::set(false);
        costs
    };
    for mode in [None, Some(Mode::Paragraph), Some(Mode::Turn)] {
        let (short, long) = (measure(20, mode, false), measure(1_000, mode, false));
        for (short, long) in short.iter().zip(&long) {
            // No thread is parsed, copied or rebuilt for a delta, and the shell stream sends nothing for text that streams.
            for cost in [short, long] {
                assert_eq!((cost.reads, cost.parsed_bytes, cost.rows_cloned, cost.shell_bytes), (0.0, 0.0, 0.0, 0.0), "{mode:?}: {cost:?}");
                // The thread is written once for a delta, however many events (a node and its item) the delta is.
                assert!(cost.writes <= 1.0, "{mode:?}: {cost:?}");
            }
            assert_eq!(short.thread_bytes, long.thread_bytes, "{mode:?}: bytes sent to a thread subscriber grow with the thread");
        }
        // The same measurement before the changes does grow with the thread, so the guard would have caught them.
        let (short, long) = (measure(20, mode, true), measure(1_000, mode, true));
        let (short, long) = (short.last().unwrap(), long.last().unwrap());
        assert!(long.parsed_bytes > short.parsed_bytes && long.rows_cloned > short.rows_cloned, "{mode:?}: {short:?} {long:?}");
    }
}
