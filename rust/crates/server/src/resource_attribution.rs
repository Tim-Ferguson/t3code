//! Source logical-I/O accounting. These counters measure application work,
//! separately from native process storage I/O; they are never added together.
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
#[derive(Clone, Debug, Default)]
pub struct AttributionRecord {
    pub component: String,
    pub operation: String,
    pub logical_read_bytes: Option<f64>,
    pub logical_write_bytes: Option<f64>,
    pub count: Option<f64>,
    pub duration_ms: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub component: String,
    pub operation: String,
    pub logical_read_bytes: f64,
    pub logical_write_bytes: f64,
    pub count: f64,
    pub duration_ms: f64,
}
#[derive(Clone, Default)]
pub struct ResourceAttribution(Arc<Mutex<IndexMap<String, Entry>>>);
fn non_negative_integer(value: Option<f64>, fallback: f64) -> f64 {
    match value {
        None => fallback,
        Some(value) if value.is_finite() => {
            (value.floor() + if value - value.floor() >= 0.5 { 1. } else { 0. }).max(0.)
        }
        _ => 0.,
    }
}
impl ResourceAttribution {
    pub fn record(&self, input: AttributionRecord) {
        let key = format!("{}\0{}", input.component, input.operation);
        let mut entries = self.0.lock().unwrap();
        let current = entries.entry(key).or_insert_with(|| Entry {
            component: input.component.clone(),
            operation: input.operation.clone(),
            logical_read_bytes: 0.,
            logical_write_bytes: 0.,
            count: 0.,
            duration_ms: 0.,
        });
        // Source replaces labels on a colliding component\0operation key.
        current.component = input.component;
        current.operation = input.operation;
        current.logical_read_bytes += non_negative_integer(input.logical_read_bytes, 0.);
        current.logical_write_bytes += non_negative_integer(input.logical_write_bytes, 0.);
        current.count += non_negative_integer(input.count, 1.);
        current.duration_ms += non_negative_integer(input.duration_ms, 0.);
    }
    pub fn entries(&self) -> Vec<Entry> {
        let mut entries: Vec<_> = self.0.lock().unwrap().values().cloned().collect();
        entries.sort_by(|a, b| b.logical_write_bytes.total_cmp(&a.logical_write_bytes));
        entries
    }
    pub fn snapshot(&self, read_at_ms: i64) -> Value {
        json!({"readAt":crate::resource_model::timestamp(read_at_ms as f64),"entries":self.entries()})
    }
    pub fn wire(
        &self,
        read_at_ms: i64,
    ) -> Result<t3_contracts::ResourceAttributionSnapshot, serde_json::Error> {
        serde_json::from_value(self.snapshot(read_at_ms))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn number(value: Option<&Value>) -> Option<f64> {
        value.map(|v| match v.as_str() {
            Some("NaN") => f64::NAN,
            Some("Infinity") => f64::INFINITY,
            Some("-Infinity") => f64::NEG_INFINITY,
            _ => v.as_f64().unwrap(),
        })
    }
    #[test]
    fn logical_io_rounding_order_key_collisions_and_wire_bounds_match_original_service() {
        for (index, line) in include_str!("../tests/fixtures/resource-attribution.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let attribution = ResourceAttribution::default();
            for input in fixture["records"].as_array().unwrap() {
                attribution.record(AttributionRecord {
                    component: input["component"].as_str().unwrap().into(),
                    operation: input["operation"].as_str().unwrap().into(),
                    logical_read_bytes: number(input.get("logicalReadBytes")),
                    logical_write_bytes: number(input.get("logicalWriteBytes")),
                    count: number(input.get("count")),
                    duration_ms: number(input.get("durationMs")),
                });
            }
            crate::resource_model::tests::same_numbers(
                &serde_json::to_value(attribution.entries()).unwrap(),
                &fixture["entries"],
                &format!("source witness {index}"),
            );
            assert_eq!(
                attribution.wire(1000).is_ok(),
                fixture["accepted"].as_bool().unwrap(),
                "source wire witness {index}"
            );
        }
    }
}
