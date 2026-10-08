//! Pure process identity, tree, category and counter model from Model.ts.
//! Arithmetic stays in JavaScript's Number domain. In particular Electron's
//! inferred cumulative CPU time may be fractional; never truncate it to fit the
//! stricter public integer schema. Validate the wire DTO before publication.
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use t3_contracts::{
    DesktopElectronProcessMetric, DesktopHostTelemetrySnapshot, ResourceMonitorSnapshotEvent,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupCounters {
    pub cpu_time_ms: f64,
    pub io_read_bytes: f64,
    pub io_write_bytes: f64,
    pub process_starts: f64,
    pub process_exits: f64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryCounters {
    pub backend: GroupCounters,
    pub electron: GroupCounters,
    pub monitor: GroupCounters,
    pub all_t3: GroupCounters,
}
impl TelemetryCounters {
    fn group(&mut self, category: &str) -> &mut GroupCounters {
        match category_group(category) {
            "electron" => &mut self.electron,
            "monitor" => &mut self.monitor,
            _ => &mut self.backend,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub pid: u64,
    pub start_time_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    pub identity: Identity,
    pub ppid: u64,
    pub child_pids: Vec<u64>,
    pub depth: usize,
    pub name: String,
    pub command: String,
    pub status: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub electron_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub electron_service_name: Option<String>,
    pub cpu_percent: f64,
    pub cpu_time_ms: f64,
    pub resident_bytes: f64,
    pub peak_resident_bytes: f64,
    pub virtual_bytes: f64,
    pub io_read_bytes: f64,
    pub io_write_bytes: f64,
    pub io_read_bytes_per_second: f64,
    pub io_write_bytes_per_second: f64,
    pub io_semantics: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub idle_wakeups_per_second: Option<f64>,
    pub run_time_ms: f64,
    pub first_seen_at: String,
    pub last_seen_at: String,
}
impl Process {
    pub fn wire(&self) -> Result<t3_contracts::ResourceTelemetryProcess, serde_json::Error> {
        serde_json::from_value(serde_json::to_value(self)?)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessState {
    pub process: Process,
    pub sampled_at_ms: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessDelta {
    pub identity_key: String,
    pub category: String,
    pub cpu_time_ms: f64,
    pub io_read_bytes: f64,
    pub io_write_bytes: f64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aggregate {
    pub process_count: usize,
    pub current_cpu_percent: f64,
    pub cpu_time_ms: f64,
    pub current_rss_bytes: f64,
    pub peak_rss_bytes: f64,
    pub io_read_bytes: f64,
    pub io_write_bytes: f64,
    pub io_read_bytes_per_second: f64,
    pub io_write_bytes_per_second: f64,
    pub process_starts: f64,
    pub process_exits: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Groups {
    pub backend: Aggregate,
    pub electron: Aggregate,
    pub monitor: Aggregate,
    pub all_t3: Aggregate,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeResult {
    pub sampled_at_ms: f64,
    pub processes: Vec<Process>,
    pub previous: IndexMap<String, ProcessState>,
    pub counters: TelemetryCounters,
    pub groups: Groups,
    pub deltas: Vec<ProcessDelta>,
}
pub struct MergeInput<'a> {
    pub server_pid: u64,
    pub sidecar_pid: Option<u64>,
    pub fallback_sampled_at_ms: f64,
    pub native_snapshot: Option<&'a ResourceMonitorSnapshotEvent>,
    pub desktop_snapshot: Option<&'a DesktopHostTelemetrySnapshot>,
    pub electron_root_pids: &'a IndexSet<u64>,
    pub electron_root_start_times: &'a HashMap<u64, u64>,
    pub previous: &'a IndexMap<String, ProcessState>,
    pub counters: &'a TelemetryCounters,
    pub update_previous: bool,
}
#[derive(Clone)]
struct Sample {
    pid: u64,
    ppid: u64,
    start: u64,
    run: f64,
    name: String,
    command: String,
    status: String,
    cpu_percent: f64,
    cpu_time: f64,
    rss: f64,
    virtual_bytes: f64,
    read: f64,
    write: f64,
    io_semantics: String,
}
pub fn identity_key(pid: u64, start: u64) -> String {
    format!("{pid}:{start}")
}
pub fn category_group(category: &str) -> &'static str {
    if category == "resource-monitor" {
        "monitor"
    } else if category.starts_with("electron-") {
        "electron"
    } else {
        "backend"
    }
}
fn finite(value: f64) -> f64 {
    if value.is_finite() { value.max(0.) } else { 0. }
}
fn delta(current: f64, previous: f64, elapsed: f64) -> f64 {
    if elapsed <= 0. || elapsed > 30_000. || current < previous {
        0.
    } else {
        current - previous
    }
}
pub(crate) fn timestamp(value: f64) -> String {
    // Source DateTime.makeUnsafe also rejects times outside its representable range.
    chrono::DateTime::from_timestamp_millis(value as i64)
        .expect("representable source timestamp")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn metric_type(metric: &DesktopElectronProcessMetric) -> String {
    serde_json::to_value(&metric.r#type)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}
fn category(metric: &DesktopElectronProcessMetric) -> &'static str {
    match metric_type(metric).as_str() {
        "Browser" => "electron-main",
        "Tab" => "electron-renderer",
        "GPU" => "electron-gpu",
        _ => "electron-utility",
    }
}
fn ancestor(pid: u64, processes: &IndexMap<u64, Sample>, electron: &IndexSet<u64>) -> bool {
    let mut visited = HashSet::new();
    let mut current = pid;
    while current > 0 && visited.insert(current) {
        if electron.contains(&current) {
            return true;
        }
        let Some(process) = processes.get(&current) else {
            return false;
        };
        if process.ppid == current {
            return false;
        }
        current = process.ppid;
    }
    false
}
fn aggregate<'a>(
    processes: impl Iterator<Item = &'a Process>,
    counters: &GroupCounters,
) -> Aggregate {
    let mut result = Aggregate {
        cpu_time_ms: counters.cpu_time_ms,
        io_read_bytes: counters.io_read_bytes,
        io_write_bytes: counters.io_write_bytes,
        process_starts: counters.process_starts,
        process_exits: counters.process_exits,
        ..Default::default()
    };
    for process in processes {
        result.process_count += 1;
        result.current_cpu_percent += process.cpu_percent;
        result.current_rss_bytes += process.resident_bytes;
        result.peak_rss_bytes += process.peak_resident_bytes;
        result.io_read_bytes_per_second += process.io_read_bytes_per_second;
        result.io_write_bytes_per_second += process.io_write_bytes_per_second;
    }
    result
}
fn visit(
    pid: u64,
    processes: &IndexMap<u64, Process>,
    children: &HashMap<u64, Vec<u64>>,
    visited: &mut HashSet<u64>,
    ordered: &mut Vec<Process>,
) {
    if !visited.insert(pid) {
        return;
    }
    let Some(process) = processes.get(&pid) else {
        return;
    };
    ordered.push(process.clone());
    if let Some(child_pids) = children.get(&pid) {
        for child in child_pids {
            visit(*child, processes, children, visited, ordered);
        }
    }
}
pub fn merge(input: MergeInput<'_>) -> MergeResult {
    let native_time = input
        .native_snapshot
        .map(|snapshot| snapshot.sampled_at_unix_ms.0 as f64);
    let desktop_time = input
        .desktop_snapshot
        .map(|snapshot| snapshot.sampled_at_unix_ms.0 as f64);
    let sampled_at_ms = match (native_time, desktop_time) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), _) | (_, Some(a)) => a,
        _ => input.fallback_sampled_at_ms,
    };
    let mut processes = IndexMap::<u64, Sample>::new();
    let mut native_pids = HashSet::new();
    if let Some(snapshot) = input.native_snapshot {
        for process in &snapshot.processes {
            native_pids.insert(process.pid.0);
            processes.insert(
                process.pid.0,
                Sample {
                    pid: process.pid.0,
                    ppid: process.ppid.0,
                    start: process.start_time_ms.0,
                    run: process.run_time_ms.0 as f64,
                    name: process.name.clone(),
                    command: process.command.clone(),
                    status: process.status.clone(),
                    cpu_percent: process.cpu_percent.as_f64().unwrap(),
                    cpu_time: process.cpu_time_ms.0 as f64,
                    rss: process.resident_bytes.0 as f64,
                    virtual_bytes: process.virtual_bytes.0 as f64,
                    read: process.io_read_bytes.0 as f64,
                    write: process.io_write_bytes.0 as f64,
                    io_semantics: serde_json::to_value(&process.io_semantics)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .into(),
                },
            );
        }
    }
    let mut metrics = IndexMap::new();
    if let Some(desktop) = input.desktop_snapshot {
        for metric in &desktop.electron_processes {
            let pid = metric.pid.0;
            if let Some(native) = processes.get(&pid) {
                if (native.start as f64 - metric.creation_time_ms.0 as f64).abs() <= 2000. {
                    metrics.insert(pid, metric);
                }
                continue;
            }
            let previous = input
                .previous
                .get(&identity_key(pid, metric.creation_time_ms.0));
            let sampled = desktop_time.unwrap_or(sampled_at_ms);
            let cpu_time = if let Some(cumulative) = metric.cumulative_cpu_seconds.as_ref() {
                (cumulative.as_f64().unwrap() * 1000. + 0.5).floor().max(0.)
            } else if let Some(previous) = previous {
                previous.process.cpu_time_ms
                    + (((sampled - previous.sampled_at_ms) * metric.cpu_percent.as_f64().unwrap())
                        / 100.)
                        .max(0.)
            } else {
                0.
            };
            let name = metric
                .name
                .as_ref()
                .or(metric.service_name.as_ref())
                .cloned()
                .unwrap_or_else(|| metric_type(metric));
            processes.insert(
                pid,
                Sample {
                    pid,
                    ppid: 0,
                    start: metric.creation_time_ms.0,
                    run: (sampled - metric.creation_time_ms.0 as f64).max(0.),
                    name: name.clone(),
                    command: name,
                    status: "Running".into(),
                    cpu_percent: metric.cpu_percent.as_f64().unwrap(),
                    cpu_time,
                    rss: metric.working_set_bytes.0 as f64,
                    virtual_bytes: 0.,
                    read: 0.,
                    write: 0.,
                    io_semantics: "storage".into(),
                },
            );
            metrics.insert(pid, metric);
        }
    }
    let explicit: IndexSet<u64> = input
        .electron_root_pids
        .iter()
        .copied()
        .filter(|pid| {
            let Some(process) = processes.get(pid) else {
                return false;
            };
            if let Some(start) = input.electron_root_start_times.get(pid) {
                return (process.start as f64 - *start as f64).abs() <= 2000.;
            }
            metrics.contains_key(pid)
                || input.previous.values().any(|previous| {
                    previous.process.category == "electron-main"
                        && previous.process.identity.pid == *pid
                        && previous.process.identity.start_time_ms == process.start
                })
        })
        .collect();
    let mut electron: IndexSet<u64> = metrics.keys().copied().collect();
    electron.extend(explicit.iter().copied());
    let mut electron_roots: Vec<u64> = electron
        .iter()
        .copied()
        .filter(|pid| {
            !explicit.contains(pid)
                && processes
                    .get(pid)
                    .is_none_or(|process| !ancestor(process.ppid, &processes, &electron))
        })
        .collect();
    electron_roots.sort_unstable();
    let mut roots: IndexSet<u64> = std::iter::once(input.server_pid)
        .chain(explicit.iter().copied())
        .chain(electron_roots)
        .collect();
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    for process in processes.values() {
        children.entry(process.ppid).or_default().push(process.pid);
    }
    let mut queue: VecDeque<(u64, usize)> = roots.iter().map(|pid| (*pid, 0)).collect();
    let mut depths = HashMap::new();
    while let Some((pid, depth)) = queue.pop_front() {
        if depths.contains_key(&pid) {
            continue;
        }
        depths.insert(pid, depth);
        if let Some(children) = children.get(&pid) {
            queue.extend(children.iter().map(|pid| (*pid, depth + 1)));
        }
    }
    let mut next = IndexMap::new();
    let mut deltas = Vec::new();
    let mut normalized = IndexMap::new();
    for process in processes.values() {
        let key = identity_key(process.pid, process.start);
        let previous = input.previous.get(&key);
        let counter_time = if native_pids.contains(&process.pid) {
            native_time.unwrap_or(sampled_at_ms)
        } else {
            desktop_time.unwrap_or(sampled_at_ms)
        };
        let elapsed = previous
            .map(|previous| counter_time - previous.sampled_at_ms)
            .unwrap_or(0.);
        let cpu = previous
            .map(|previous| delta(process.cpu_time, previous.process.cpu_time_ms, elapsed))
            .unwrap_or(0.);
        let read = previous
            .map(|previous| delta(process.read, previous.process.io_read_bytes, elapsed))
            .unwrap_or(0.);
        let write = previous
            .map(|previous| delta(process.write, previous.process.io_write_bytes, elapsed))
            .unwrap_or(0.);
        let metric = metrics.get(&process.pid).copied();
        let category = if process.pid == input.server_pid {
            "server"
        } else if input.sidecar_pid == Some(process.pid) {
            "resource-monitor"
        } else if explicit.contains(&process.pid) {
            "electron-main"
        } else if let Some(metric) = metric {
            category(metric)
        } else if ancestor(process.pid, &processes, &electron) {
            let command = process.command.to_lowercase();
            if command.contains("--type=renderer") {
                "electron-renderer"
            } else if command.contains("--type=gpu-process") {
                "electron-gpu"
            } else {
                "electron-utility"
            }
        } else {
            "server-child"
        };
        let preserve = !input.update_previous && previous.is_some();
        let cpu_percent = if preserve {
            previous.unwrap().process.cpu_percent
        } else if previous.is_some() && elapsed > 0. && elapsed <= 30_000. {
            cpu / elapsed * 100.
        } else {
            finite(process.cpu_percent)
        };
        let mut child_pids = children.get(&process.pid).cloned().unwrap_or_default();
        child_pids.sort_unstable();
        let normalized_process = Process {
            identity: Identity {
                pid: process.pid,
                start_time_ms: process.start,
            },
            ppid: process.ppid,
            child_pids,
            depth: *depths.get(&process.pid).unwrap_or(&0),
            name: process.name.clone(),
            command: process.command.clone(),
            status: process.status.clone(),
            category: category.into(),
            electron_type: metric.map(metric_type),
            electron_service_name: metric
                .and_then(|metric| metric.service_name.clone().filter(|name| !name.is_empty())),
            cpu_percent: finite(cpu_percent),
            cpu_time_ms: process.cpu_time,
            resident_bytes: process.rss,
            peak_resident_bytes: process
                .rss
                .max(
                    metric
                        .map(|metric| metric.peak_working_set_bytes.0 as f64)
                        .unwrap_or(0.),
                )
                .max(
                    previous
                        .map(|previous| previous.process.peak_resident_bytes)
                        .unwrap_or(0.),
                ),
            virtual_bytes: process.virtual_bytes,
            io_read_bytes: process.read,
            io_write_bytes: process.write,
            io_read_bytes_per_second: if preserve {
                previous.unwrap().process.io_read_bytes_per_second
            } else if elapsed > 0. {
                finite(read * 1000. / elapsed)
            } else {
                0.
            },
            io_write_bytes_per_second: if preserve {
                previous.unwrap().process.io_write_bytes_per_second
            } else if elapsed > 0. {
                finite(write * 1000. / elapsed)
            } else {
                0.
            },
            io_semantics: process.io_semantics.clone(),
            idle_wakeups_per_second: metric
                .map(|metric| metric.idle_wakeups_per_second.as_f64().unwrap()),
            run_time_ms: process.run,
            first_seen_at: previous
                .map(|previous| previous.process.first_seen_at.clone())
                .unwrap_or_else(|| timestamp(sampled_at_ms)),
            last_seen_at: timestamp(sampled_at_ms),
        };
        next.insert(
            key.clone(),
            ProcessState {
                process: normalized_process.clone(),
                sampled_at_ms: counter_time,
            },
        );
        deltas.push(ProcessDelta {
            identity_key: key,
            category: category.into(),
            cpu_time_ms: cpu,
            io_read_bytes: read,
            io_write_bytes: write,
        });
        normalized.insert(process.pid, normalized_process);
    }
    for values in children.values_mut() {
        values.sort_unstable();
    }
    let mut ordered = Vec::new();
    let mut visited = HashSet::new();
    for pid in roots.drain(..) {
        visit(pid, &normalized, &children, &mut visited, &mut ordered);
    }
    let mut leftovers: Vec<_> = normalized.values().collect();
    leftovers.sort_by_key(|process| (process.depth, process.identity.pid));
    for process in leftovers {
        visit(
            process.identity.pid,
            &normalized,
            &children,
            &mut visited,
            &mut ordered,
        );
    }
    let mut counters = input.counters.clone();
    if input.update_previous {
        for delta in &deltas {
            let group = counters.group(&delta.category);
            group.cpu_time_ms += delta.cpu_time_ms;
            group.io_read_bytes += delta.io_read_bytes;
            group.io_write_bytes += delta.io_write_bytes;
            counters.all_t3.cpu_time_ms += delta.cpu_time_ms;
            counters.all_t3.io_read_bytes += delta.io_read_bytes;
            counters.all_t3.io_write_bytes += delta.io_write_bytes;
        }
        for (key, state) in &next {
            if !input.previous.contains_key(key) {
                counters.group(&state.process.category).process_starts += 1.;
                counters.all_t3.process_starts += 1.;
            }
        }
        for (key, state) in input.previous {
            if !next.contains_key(key) {
                counters.group(&state.process.category).process_exits += 1.;
                counters.all_t3.process_exits += 1.;
            }
        }
    }
    let groups = Groups {
        backend: aggregate(
            ordered
                .iter()
                .filter(|process| category_group(&process.category) == "backend"),
            &counters.backend,
        ),
        electron: aggregate(
            ordered
                .iter()
                .filter(|process| category_group(&process.category) == "electron"),
            &counters.electron,
        ),
        monitor: aggregate(
            ordered
                .iter()
                .filter(|process| category_group(&process.category) == "monitor"),
            &counters.monitor,
        ),
        all_t3: aggregate(ordered.iter(), &counters.all_t3),
    };
    MergeResult {
        sampled_at_ms,
        processes: ordered,
        previous: if input.update_previous {
            next
        } else {
            input.previous.clone()
        },
        counters,
        groups,
        deltas,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;
    pub(crate) fn same_numbers(actual: &Value, expected: &Value, path: &str) {
        match (actual, expected) {
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.as_f64(), b.as_f64(), "{path}"),
            (Value::Array(a), Value::Array(b)) => {
                assert_eq!(a.len(), b.len(), "{path}");
                for (index, (a, b)) in a.iter().zip(b).enumerate() {
                    same_numbers(a, b, &format!("{path}[{index}]"));
                }
            }
            (Value::Object(a), Value::Object(b)) => {
                assert_eq!(a.len(), b.len(), "{path}: object keys");
                for (key, b) in b {
                    same_numbers(
                        a.get(key)
                            .unwrap_or_else(|| panic!("{path}: missing {key}")),
                        b,
                        &format!("{path}.{key}"),
                    );
                }
            }
            _ => assert_eq!(actual, expected, "{path}"),
        }
    }
    #[test]
    fn model_matches_original_process_identity_counter_and_electron_wire_oracle() {
        for (index, line) in include_str!("../tests/fixtures/resource-model.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let input = &fixture["input"];
            let native: Option<ResourceMonitorSnapshotEvent> = input
                .get("nativeSnapshot")
                .map(|value| serde_json::from_value(value.clone()).unwrap());
            let desktop: Option<DesktopHostTelemetrySnapshot> = input
                .get("desktopSnapshot")
                .map(|value| serde_json::from_value(value.clone()).unwrap());
            let previous = serde_json::from_value(input["previous"].clone()).unwrap();
            let counters = serde_json::from_value(input["counters"].clone()).unwrap();
            let roots = serde_json::from_value(input["electronRootPids"].clone()).unwrap();
            let starts = serde_json::from_value(input["electronRootStartTimes"].clone()).unwrap();
            let actual = merge(MergeInput {
                server_pid: input["serverPid"].as_u64().unwrap(),
                sidecar_pid: input.get("sidecarPid").and_then(Value::as_u64),
                fallback_sampled_at_ms: input["fallbackSampledAtMs"].as_f64().unwrap(),
                native_snapshot: native.as_ref(),
                desktop_snapshot: desktop.as_ref(),
                previous: &previous,
                counters: &counters,
                electron_root_pids: &roots,
                electron_root_start_times: &starts,
                update_previous: input["updatePrevious"].as_bool().unwrap(),
            });
            same_numbers(
                &serde_json::to_value(&actual).unwrap(),
                &fixture["expected"],
                &format!("fixture{index}"),
            );
            let acceptance: Vec<bool> = actual
                .processes
                .iter()
                .map(|process| process.wire().is_ok())
                .collect();
            assert_eq!(
                serde_json::to_value(acceptance).unwrap(),
                fixture["wireAcceptance"],
                "fixture{index}: source wire validation"
            );
        }
    }
}
