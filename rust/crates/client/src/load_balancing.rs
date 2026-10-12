//! Which environment a new thread starts on when the person lets the app balance the load: the choice among the machines that host
//! the project, and the preference each machine is given. Port of `packages/client-runtime/src/load-balancing.ts` and the rules of
//! `LoadBalancingSettings.tsx`.
use t3_contracts::HostResourcesSnapshot;

/// The four preferences a machine can be given, with the weight each stores.
pub const PREFERENCES: [(u8, &str); 4] = [(100, "Prefer"), (50, "Normal"), (25, "Less often"), (0, "Manual only")];
/// The weight of a machine nobody has chosen a preference for.
pub const DEFAULT_WEIGHT: u8 = 50;

/// `loadPreferenceForWeight`: snaps a saved weight (older builds stored a slider value) onto the four preferences.
pub fn preference_for_weight(weight: Option<u8>) -> u8 {
    match weight {
        None | Some(50) => 50,
        Some(0) => 0,
        Some(weight) if weight < 50 => 25,
        Some(_) => 100,
    }
}
pub fn preference_label(preference: u8) -> &'static str {
    PREFERENCES.iter().find(|(value, _)| *value == preference).map_or("Normal", |(_, label)| label)
}

/// `summarizeLoadPreferences`: the machines not at Normal, so a folded section still tells what is set. None when every machine is.
pub fn summarize<'a>(machines: impl IntoIterator<Item = (&'a str, &'a str)>, weight_of: impl Fn(&str) -> Option<u8>) -> Option<String> {
    let parts: Vec<String> = machines
        .into_iter()
        .filter_map(|(environment, label)| {
            let preference = preference_for_weight(weight_of(environment));
            (preference != 50).then(|| format!("{label} {}", preference_label(preference).to_lowercase()))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

/// A machine that could take the thread: connected, hosting the project, and the provider the thread uses.
pub struct Candidate<'a> {
    pub environment: &'a str,
    pub resources: Option<&'a HostResourcesSnapshot>,
    /// When this client received the sample, so that clocks on different machines are never compared.
    pub received_at: Option<u64>,
    pub weight: f64,
}

/// `chooseLoadBalancedEnvironment`: the machine with the best weighted free CPU and memory, or none when no machine has a fresh
/// sample, is busy (95% CPU or more, 5% memory or less), or is set to "Manual only".
pub fn choose<'a>(candidates: &[Candidate<'a>], now: u64) -> Option<&'a str> {
    let (mut selected, mut best) = (None, 0.0_f64);
    for candidate in candidates {
        let Some(resources) = candidate.resources else { continue };
        let sampled_at = candidate.received_at.unwrap_or(resources.sampled_at.0);
        let Some(utilization) = resources.cpu_utilization.as_ref().and_then(|value| value.0.as_f64()) else { continue };
        if !candidate.weight.is_finite()
            || candidate.weight <= 0.0
            || now.saturating_sub(sampled_at) > 15_000
            || sampled_at > now + 5_000
            || utilization >= 0.95
            || resources.total_memory_bytes.0 == 0
            || resources.cpu_count.0 == 0
        {
            continue;
        }
        let memory_available = resources.available_memory_bytes.0 as f64 / resources.total_memory_bytes.0 as f64;
        if memory_available <= 0.05 {
            continue;
        }
        let score = candidate.weight * resources.cpu_count.0 as f64 * (1.0 - utilization) * memory_available;
        if score > best {
            selected = Some(candidate.environment);
            best = score;
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn host(cpu: Option<f64>, count: u64, available: u64, total: u64, sampled_at: u64) -> HostResourcesSnapshot {
        serde_json::from_value(json!({"sampledAt":sampled_at,"cpuUtilization":cpu,"cpuCount":count,"availableMemoryBytes":available,"totalMemoryBytes":total})).unwrap()
    }
    fn candidate<'a>(environment: &'a str, resources: &'a HostResourcesSnapshot, weight: f64) -> Candidate<'a> {
        Candidate { environment, resources: Some(resources), received_at: None, weight }
    }

    #[test]
    fn the_machine_with_the_most_weighted_free_cpu_and_memory_takes_the_thread() {
        let (idle, busy, small) = (host(Some(0.1), 8, 8, 16, 1_000), host(Some(0.6), 8, 8, 16, 1_000), host(Some(0.1), 2, 8, 16, 1_000));
        let now = 2_000;
        assert_eq!(choose(&[candidate("busy", &busy, 50.0), candidate("idle", &idle, 50.0), candidate("small", &small, 50.0)], now), Some("idle"));
        // A machine given a better preference can outrank a freer one; "Manual only" never takes a thread.
        assert_eq!(choose(&[candidate("busy", &busy, 100.0), candidate("idle", &idle, 25.0)], now), Some("busy"));
        assert_eq!(choose(&[candidate("idle", &idle, 0.0), candidate("busy", &busy, 50.0)], now), Some("busy"));
        assert_eq!(choose(&[candidate("idle", &idle, 0.0)], now), None);
    }

    #[test]
    fn a_machine_without_a_fresh_trustworthy_sample_is_not_chosen() {
        let now = 100_000;
        let fresh = host(Some(0.2), 4, 8, 16, now);
        let stale = host(Some(0.0), 64, 16, 16, now - 15_001);
        let future = host(Some(0.0), 64, 16, 16, now + 5_001);
        let unknown_cpu = host(None, 64, 16, 16, now);
        let saturated = host(Some(0.95), 64, 16, 16, now);
        let starved = host(Some(0.0), 64, 1, 100, now);
        let no_cpus = host(Some(0.0), 0, 16, 16, now);
        let no_memory = host(Some(0.0), 8, 0, 0, now);
        for (name, resources) in [("stale", &stale), ("future", &future), ("unknown", &unknown_cpu), ("saturated", &saturated), ("starved", &starved), ("no cpus", &no_cpus), ("no memory", &no_memory)] {
            assert_eq!(choose(&[candidate(name, resources, 50.0), candidate("fresh", &fresh, 50.0)], now), Some("fresh"), "{name} is passed over");
        }
        assert_eq!(choose(&[Candidate { environment: "none", resources: None, received_at: None, weight: 50.0 }], now), None);
        // The time this client received a sample counts, not the time the other machine wrote on it.
        let skewed = host(Some(0.0), 8, 16, 16, 0);
        assert_eq!(choose(&[Candidate { environment: "skewed", resources: Some(&skewed), received_at: Some(now - 1_000), weight: 50.0 }], now), Some("skewed"));
        assert_eq!(choose(&[candidate("skewed", &skewed, 50.0)], now), None);
    }

    #[test]
    fn saved_weights_snap_to_the_four_preferences_and_the_summary_names_the_machines_that_are_not_normal() {
        for (weight, preference) in [(None, 50), (Some(50), 50), (Some(0), 0), (Some(10), 25), (Some(49), 25), (Some(51), 100), (Some(100), 100)] {
            assert_eq!(preference_for_weight(weight), preference, "{weight:?}");
        }
        let weights = |environment: &str| match environment {
            "a" => Some(100),
            "b" => Some(0),
            "c" => Some(50),
            _ => None,
        };
        assert_eq!(summarize([("a", "Laptop"), ("b", "Desktop"), ("c", "Server"), ("d", "Mini")], weights).as_deref(), Some("Laptop prefer \u{b7} Desktop manual only"));
        assert_eq!(summarize([("c", "Server"), ("d", "Mini")], weights), None);
    }
}
