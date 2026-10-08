//! Source history window integration, including the preceding sample, clipped
//! counter deltas, identity retention, inclusive final bucket and legacy view.
use crate::resource_model::{self, MergeInput, Process, ProcessState, TelemetryCounters};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use t3_contracts::{
    DesktopHostTelemetrySnapshot, ResourceMonitorSnapshotEvent, ResourceTelemetryHealth,
};

pub fn normalize(window_ms: f64, bucket_ms: f64) -> (f64, f64) {
    let window = window_ms.clamp(1000., 3_600_000.);
    (window, bucket_ms.clamp(1000., window))
}
pub struct HistoryInput<'a> {
    pub read_at_ms: i64,
    pub window_ms: f64,
    pub bucket_ms: f64,
    pub sample_interval_ms: u64,
    pub server_pid: u64,
    pub sidecar_pid: Option<u64>,
    pub desktop_snapshot: Option<&'a DesktopHostTelemetrySnapshot>,
    pub snapshots: &'a [ResourceMonitorSnapshotEvent],
    pub health: &'a ResourceTelemetryHealth,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub started_at: String,
    pub ended_at: String,
    pub avg_cpu_percent: f64,
    pub max_cpu_percent: f64,
    pub max_rss_bytes: f64,
    pub io_read_bytes: f64,
    pub io_write_bytes: f64,
    pub max_process_count: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub identity: resource_model::Identity,
    pub ppid: u64,
    pub depth: usize,
    pub name: String,
    pub command: String,
    pub category: String,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub current_cpu_percent: f64,
    pub avg_cpu_percent: f64,
    pub max_cpu_percent: f64,
    pub cpu_time_ms: f64,
    pub current_rss_bytes: f64,
    pub peak_rss_bytes: f64,
    pub io_read_bytes: f64,
    pub io_write_bytes: f64,
    pub io_semantics: String,
    pub sample_count: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    pub read_at: String,
    pub window_ms: f64,
    pub bucket_ms: f64,
    pub sample_interval_ms: u64,
    pub retained_sample_count: usize,
    pub buckets: Vec<Bucket>,
    pub legacy_backend_buckets: Vec<Bucket>,
    pub top_processes: Vec<Summary>,
    pub health: ResourceTelemetryHealth,
}
impl History {
    pub fn wire(&self) -> Result<t3_contracts::ResourceTelemetryHistory, serde_json::Error> {
        serde_json::from_value(serde_json::to_value(self)?)
    }
}
struct AggregateSample {
    sampled: f64,
    cpu: f64,
    rss: f64,
    count: usize,
    read: f64,
    write: f64,
}
struct ProcessSample {
    sampled: f64,
    process: Process,
    cpu_time: f64,
    read: f64,
    write: f64,
}
fn buckets(samples: &[AggregateSample], now: f64, window: f64, bucket: f64) -> Vec<Bucket> {
    let mut result = Vec::new();
    let mut start = now - window;
    while start < now {
        let end = now.min(start + bucket);
        let selected: Vec<_> = samples
            .iter()
            .filter(|sample| {
                sample.sampled >= start
                    && if end == now {
                        sample.sampled <= end
                    } else {
                        sample.sampled < end
                    }
            })
            .collect();
        result.push(Bucket {
            started_at: resource_model::timestamp(start),
            ended_at: resource_model::timestamp(end),
            avg_cpu_percent: if selected.is_empty() {
                0.
            } else {
                selected.iter().map(|sample| sample.cpu).sum::<f64>() / selected.len() as f64
            },
            max_cpu_percent: selected.iter().map(|sample| sample.cpu).fold(0., f64::max),
            max_rss_bytes: selected.iter().map(|sample| sample.rss).fold(0., f64::max),
            io_read_bytes: selected.iter().map(|sample| sample.read).sum(),
            io_write_bytes: selected.iter().map(|sample| sample.write).sum(),
            max_process_count: selected
                .iter()
                .map(|sample| sample.count)
                .max()
                .unwrap_or(0),
        });
        start += bucket;
    }
    result
}
fn summaries(samples: Vec<ProcessSample>) -> Vec<Summary> {
    let mut groups = IndexMap::<String, Vec<ProcessSample>>::new();
    for sample in samples {
        groups
            .entry(resource_model::identity_key(
                sample.process.identity.pid,
                sample.process.identity.start_time_ms,
            ))
            .or_default()
            .push(sample);
    }
    let mut result: Vec<_> = groups
        .into_values()
        .map(|mut samples| {
            samples.sort_by(|a, b| a.sampled.total_cmp(&b.sampled));
            let first = &samples[0].process;
            let latest = &samples.last().unwrap().process;
            Summary {
                identity: latest.identity.clone(),
                ppid: latest.ppid,
                depth: latest.depth,
                name: latest.name.clone(),
                command: latest.command.clone(),
                category: latest.category.clone(),
                first_seen_at: first.first_seen_at.clone(),
                last_seen_at: latest.last_seen_at.clone(),
                current_cpu_percent: latest.cpu_percent,
                avg_cpu_percent: samples
                    .iter()
                    .map(|sample| sample.process.cpu_percent)
                    .sum::<f64>()
                    / samples.len() as f64,
                max_cpu_percent: samples
                    .iter()
                    .map(|sample| sample.process.cpu_percent)
                    .fold(0., f64::max),
                cpu_time_ms: samples.iter().map(|sample| sample.cpu_time).sum(),
                current_rss_bytes: latest.resident_bytes,
                peak_rss_bytes: samples
                    .iter()
                    .map(|sample| sample.process.resident_bytes)
                    .fold(0., f64::max),
                io_read_bytes: samples.iter().map(|sample| sample.read).sum(),
                io_write_bytes: samples.iter().map(|sample| sample.write).sum(),
                io_semantics: latest.io_semantics.clone(),
                sample_count: samples.len(),
            }
        })
        .collect();
    result.sort_by(|a, b| {
        b.cpu_time_ms
            .total_cmp(&a.cpu_time_ms)
            .then_with(|| b.peak_rss_bytes.total_cmp(&a.peak_rss_bytes))
    });
    result
}
pub fn build(input: HistoryInput<'_>) -> History {
    let now = input.read_at_ms as f64;
    let (window, bucket) = normalize(input.window_ms, input.bucket_ms);
    let start = now - window;
    let mut eligible: Vec<_> = input
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.sampled_at_unix_ms.0 as f64 <= now)
        .collect();
    eligible.sort_by_key(|snapshot| snapshot.sampled_at_unix_ms.0);
    let preceding = eligible
        .iter()
        .rfind(|snapshot| (snapshot.sampled_at_unix_ms.0 as f64) < start)
        .copied();
    let snapshots: Vec<_> = preceding
        .into_iter()
        .chain(
            eligible
                .into_iter()
                .filter(|snapshot| snapshot.sampled_at_unix_ms.0 as f64 >= start),
        )
        .collect();
    let mut samples = Vec::new();
    let mut legacy = Vec::new();
    let mut process_samples = Vec::new();
    let mut previous = IndexMap::<String, ProcessState>::new();
    let mut counters = TelemetryCounters::default();
    let mut previous_time = None;
    for snapshot in snapshots {
        let sampled = snapshot.sampled_at_unix_ms.0 as f64;
        let fraction = match previous_time {
            Some(previous) if previous < start && sampled > previous => {
                ((sampled - start) / (sampled - previous)).clamp(0., 1.)
            }
            _ => 1.,
        };
        previous_time = Some(sampled);
        let mut roots = IndexSet::new();
        let mut starts = HashMap::new();
        if let Some(recorded) = &snapshot.external_processes {
            for process in recorded {
                roots.insert(process.pid.0);
                if let Some(start) = process.start_time_ms {
                    starts.insert(process.pid.0, start.0);
                }
            }
        } else if let Some(desktop) = input.desktop_snapshot {
            roots.insert(desktop.electron_pid.0);
            if let Some(metric) = desktop
                .electron_processes
                .iter()
                .find(|metric| metric.pid.0 == desktop.electron_pid.0)
            {
                starts.insert(desktop.electron_pid.0, metric.creation_time_ms.0);
            }
        }
        let merged = resource_model::merge(MergeInput {
            server_pid: input.server_pid,
            sidecar_pid: input.sidecar_pid,
            fallback_sampled_at_ms: sampled,
            native_snapshot: Some(snapshot),
            desktop_snapshot: None,
            electron_root_pids: &roots,
            electron_root_start_times: &starts,
            previous: &previous,
            counters: &counters,
            update_previous: true,
        });
        // Source retains exited identities so a returning process keeps counters
        // and firstSeenAt; history lifecycle counters themselves are not emitted.
        previous.extend(merged.previous);
        counters = merged.counters;
        if sampled < start {
            continue;
        }
        let deltas: Vec<_> = merged
            .deltas
            .into_iter()
            .map(|mut delta| {
                if fraction != 1. {
                    delta.cpu_time_ms = (delta.cpu_time_ms * fraction + 0.5).floor();
                    delta.io_read_bytes = (delta.io_read_bytes * fraction + 0.5).floor();
                    delta.io_write_bytes = (delta.io_write_bytes * fraction + 0.5).floor();
                }
                delta
            })
            .collect();
        samples.push(AggregateSample {
            sampled,
            cpu: merged.groups.all_t3.current_cpu_percent,
            rss: merged.groups.all_t3.current_rss_bytes,
            count: merged.groups.all_t3.process_count,
            read: deltas.iter().map(|delta| delta.io_read_bytes).sum(),
            write: deltas.iter().map(|delta| delta.io_write_bytes).sum(),
        });
        let backend: Vec<_> = deltas
            .iter()
            .filter(|delta| {
                matches!(
                    delta.category.as_str(),
                    "server" | "server-child" | "provider-root" | "terminal-root"
                )
            })
            .collect();
        legacy.push(AggregateSample {
            sampled,
            cpu: merged.groups.backend.current_cpu_percent,
            rss: merged.groups.backend.current_rss_bytes,
            count: merged.groups.backend.process_count,
            read: backend.iter().map(|delta| delta.io_read_bytes).sum(),
            write: backend.iter().map(|delta| delta.io_write_bytes).sum(),
        });
        let by_identity: HashMap<_, _> = deltas
            .iter()
            .map(|delta| (&delta.identity_key, delta))
            .collect();
        for process in merged.processes {
            let key =
                resource_model::identity_key(process.identity.pid, process.identity.start_time_ms);
            let delta = by_identity.get(&key);
            process_samples.push(ProcessSample {
                sampled,
                process,
                cpu_time: delta.map(|delta| delta.cpu_time_ms).unwrap_or(0.),
                read: delta.map(|delta| delta.io_read_bytes).unwrap_or(0.),
                write: delta.map(|delta| delta.io_write_bytes).unwrap_or(0.),
            });
        }
    }
    History {
        read_at: resource_model::timestamp(now),
        window_ms: window,
        bucket_ms: bucket,
        sample_interval_ms: input.sample_interval_ms,
        retained_sample_count: samples.len() + process_samples.len(),
        buckets: buckets(&samples, now, window, bucket),
        legacy_backend_buckets: buckets(&legacy, now, window, bucket),
        top_processes: summaries(process_samples),
        health: input.health.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    #[test]
    fn history_matches_original_window_clipping_bucket_identity_and_legacy_oracle() {
        for (index, line) in include_str!("../tests/fixtures/resource-history.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let input = &fixture["input"];
            if fixture["kind"] == "normalize" {
                let (window, bucket) = normalize(
                    input["windowMs"].as_f64().unwrap(),
                    input["bucketMs"].as_f64().unwrap(),
                );
                assert_eq!(window, fixture["expected"]["windowMs"].as_f64().unwrap());
                assert_eq!(bucket, fixture["expected"]["bucketMs"].as_f64().unwrap());
                continue;
            }
            let snapshots: Vec<ResourceMonitorSnapshotEvent> =
                serde_json::from_value(input["snapshots"].clone()).unwrap();
            let health = serde_json::from_value(input["health"].clone()).unwrap();
            let desktop: Option<DesktopHostTelemetrySnapshot> = input
                .get("desktopSnapshot")
                .map(|value| serde_json::from_value(value.clone()).unwrap());
            let read_at = chrono::DateTime::parse_from_rfc3339(input["readAt"].as_str().unwrap())
                .unwrap()
                .timestamp_millis();
            let result = build(HistoryInput {
                read_at_ms: read_at,
                window_ms: input["windowMs"].as_f64().unwrap(),
                bucket_ms: input["bucketMs"].as_f64().unwrap(),
                sample_interval_ms: input["sampleIntervalMs"].as_u64().unwrap(),
                server_pid: input["serverPid"].as_u64().unwrap(),
                sidecar_pid: input.get("sidecarPid").and_then(Value::as_u64),
                desktop_snapshot: desktop.as_ref(),
                snapshots: &snapshots,
                health: &health,
            });
            crate::resource_model::tests::same_numbers(
                &serde_json::to_value(&result).unwrap(),
                &fixture["expected"],
                &format!("history{index}"),
            );
            result.wire().unwrap();
        }
    }
}
