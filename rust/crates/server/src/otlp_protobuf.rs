//! `http/protobuf` bodies of the OTLP exporters: the bytes `OtlpSerialization.layerProtobuf` of effect makes of the same data structures
//! (`observability/internal/otlpProtobuf.ts`, `protobuf.ts`), written field by field in the order the original writes them so the output is
//! byte for byte the original's. The input is the JSON body `otlp.rs` builds for the `http/json` protocol, which is that data structure.
//!
//! A value the original's encoder would throw on (a list that is not there, a number that is not an integer where an integer is
//! encoded) is an error here, and `tests/fixtures/otlp-exports.jsonl` holds the bytes the original made of every shape the exporters build.
//!
//! TODO(original `encodeHistogramDataPoint`): `bucketCounts` and `explicitBounds` are written one tagged field per element, where proto3
//! packs repeated numbers. A decoder takes both, so the form is kept.
use crate::otlp_config::Signal;
use serde_json::Value;

pub const CONTENT_TYPE: &str = "application/x-protobuf";

type Encoded = Result<(), String>;

/// `encodeTracesData`, `encodeMetricsData` or `encodeLogsData`, by the signal the body is for.
pub fn encode(signal: Signal, body: &Value) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    match signal {
        Signal::Traces => repeated(&mut out, 1, body, "resourceSpans", resource_spans)?,
        Signal::Metrics => repeated(&mut out, 1, body, "resourceMetrics", resource_metrics)?,
        Signal::Logs => repeated(&mut out, 1, body, "resourceLogs", resource_logs)?,
    }
    Ok(out)
}

// ---- the wire format (`protobuf.ts`) ------------------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value > 127 {
        out.push((value & 127) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
fn tag(out: &mut Vec<u8>, field: u64, wire: u64) {
    varint(out, field << 3 | wire);
}
fn varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    tag(out, field, 0);
    varint(out, value);
}
fn fixed64_field(out: &mut Vec<u8>, field: u64, value: u64) {
    tag(out, field, 1);
    out.extend_from_slice(&value.to_le_bytes());
}
fn fixed32_field(out: &mut Vec<u8>, field: u64, value: u32) {
    tag(out, field, 5);
    out.extend_from_slice(&value.to_le_bytes());
}
fn double_field(out: &mut Vec<u8>, field: u64, value: f64) {
    tag(out, field, 1);
    out.extend_from_slice(&value.to_le_bytes());
}
fn bytes_field(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    tag(out, field, 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}
fn string_field(out: &mut Vec<u8>, field: u64, value: &str) {
    bytes_field(out, field, value.as_bytes());
}
/// `optionalStringField`: a string that is there and not empty.
fn optional_string(out: &mut Vec<u8>, field: u64, value: Option<&Value>) {
    if let Some(text) = value.and_then(Value::as_str).filter(|text| !text.is_empty()) {
        string_field(out, field, text);
    }
}
/// `bytesFieldFromHex`: `parseInt` of each pair of characters, `NaN` (stored as 0) when a pair does not start with a hex digit, and an
/// odd trailing character dropped.
fn hex_bytes(hex: &str) -> Vec<u8> {
    let characters: Vec<char> = hex.chars().collect();
    (0..characters.len() / 2)
        .map(|index| {
            let pair = &characters[index * 2..index * 2 + 2];
            match (pair[0].to_digit(16), pair[1].to_digit(16)) {
                (Some(high), Some(low)) => (high * 16 + low) as u8,
                (Some(high), None) => high as u8,
                _ => 0,
            }
        })
        .collect()
}
fn hex_field(out: &mut Vec<u8>, field: u64, value: Option<&Value>) -> Encoded {
    let hex = value.and_then(Value::as_str).ok_or("an id is not a hex string")?;
    bytes_field(out, field, &hex_bytes(hex));
    Ok(())
}
/// A message field: the message is written by `write` into a buffer of its own.
fn message(out: &mut Vec<u8>, field: u64, write: impl FnOnce(&mut Vec<u8>) -> Encoded) -> Encoded {
    let mut inner = Vec::new();
    write(&mut inner)?;
    bytes_field(out, field, &inner);
    Ok(())
}
/// `repeatedField` of the list at `key`; a list that is not there is the `TypeError` of `undefined.map`.
fn repeated(out: &mut Vec<u8>, field: u64, parent: &Value, key: &str, write: impl Fn(&mut Vec<u8>, &Value) -> Encoded) -> Encoded {
    let items = parent.get(key).and_then(Value::as_array).ok_or_else(|| format!("{key} is not a list"))?;
    for item in items {
        message(out, field, |inner| write(inner, item))?;
    }
    Ok(())
}
fn present<'a>(parent: &'a Value, key: &str) -> Option<&'a Value> {
    parent.get(key).filter(|value| !value.is_null())
}

// ---- numbers: `BigInt(value)`, as the original's fixed64 and varint fields take them --------------------

/// `BigInt(value)` of a JSON number or numeric string, wrapped to 64 bits as `DataView.setBigUint64` and `asUintN` do.
fn wide(value: Option<&Value>) -> Result<u64, String> {
    let integer = match value {
        Some(Value::Number(number)) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(signed), _, _) => i128::from(signed),
            (_, Some(unsigned), _) => i128::from(unsigned),
            (_, _, Some(float)) if float.fract() == 0.0 && float.abs() < 1e38 => float as i128,
            _ => return Err(format!("{number} is not an integer")),
        },
        Some(Value::String(text)) => text.trim().parse::<i128>().map_err(|_| format!("{text:?} is not an integer"))?,
        Some(Value::Bool(flag)) => i128::from(*flag),
        other => return Err(format!("{other:?} is not an integer")),
    };
    Ok(integer as u64)
}
fn float(value: Option<&Value>) -> Result<f64, String> {
    value.and_then(Value::as_f64).ok_or_else(|| format!("{value:?} is not a number"))
}
/// `count > 0 ? varintField : nothing`, for the dropped counts.
fn dropped(out: &mut Vec<u8>, field: u64, value: Option<&Value>) {
    if let Some(count) = value.and_then(Value::as_u64).filter(|count| *count > 0) {
        varint_field(out, field, count);
    }
}
fn flag32(out: &mut Vec<u8>, field: u64, value: Option<&Value>) -> Encoded {
    if let Some(flags) = value {
        fixed32_field(out, field, u32::try_from(wide(Some(flags))?).map_err(|_| "flags do not fit 32 bits".to_owned())?);
    }
    Ok(())
}

// ---- common (`opentelemetry.proto.common.v1`) ----------------------------------------------------------

fn any_value(out: &mut Vec<u8>, value: &Value) -> Encoded {
    if let Some(text) = present(value, "stringValue") {
        string_field(out, 1, text.as_str().ok_or("stringValue is not a string")?);
    } else if let Some(flag) = present(value, "boolValue") {
        varint_field(out, 2, u64::from(flag.as_bool().ok_or("boolValue is not a boolean")?));
    } else if let Some(integer) = present(value, "intValue") {
        varint_field(out, 3, wide(Some(integer))?);
    } else if let Some(number) = present(value, "doubleValue") {
        double_field(out, 4, float(Some(number))?);
    } else if let Some(list) = value.get("arrayValue") {
        message(out, 5, |inner| repeated(inner, 1, list, "values", any_value))?;
    } else if let Some(list) = value.get("kvlistValue") {
        message(out, 6, |inner| repeated(inner, 1, list, "values", key_value))?;
    }
    Ok(())
}
fn key_value(out: &mut Vec<u8>, entry: &Value) -> Encoded {
    string_field(out, 1, entry.get("key").and_then(Value::as_str).ok_or("an attribute has no key")?);
    let value = entry.get("value").ok_or("an attribute has no value")?;
    message(out, 2, |inner| any_value(inner, value))
}
fn attributes(out: &mut Vec<u8>, field: u64, parent: &Value) -> Encoded {
    repeated(out, field, parent, "attributes", key_value)
}
fn scope(out: &mut Vec<u8>, scope: &Value) -> Encoded {
    string_field(out, 1, scope.get("name").and_then(Value::as_str).ok_or("a scope has no name")?);
    optional_string(out, 2, scope.get("version"));
    if scope.get("attributes").is_some() {
        attributes(out, 3, scope)?;
    }
    dropped(out, 4, scope.get("droppedAttributesCount"));
    Ok(())
}
fn resource(out: &mut Vec<u8>, resource: &Value) -> Encoded {
    attributes(out, 1, resource)?;
    dropped(out, 2, resource.get("droppedAttributesCount"));
    Ok(())
}

// ---- traces ------------------------------------------------------------------------------------------

fn status(out: &mut Vec<u8>, status: &Value) -> Encoded {
    optional_string(out, 2, status.get("message"));
    varint_field(out, 3, wide(status.get("code"))?);
    Ok(())
}
fn event(out: &mut Vec<u8>, event: &Value) -> Encoded {
    fixed64_field(out, 1, wide(event.get("timeUnixNano"))?);
    string_field(out, 2, event.get("name").and_then(Value::as_str).ok_or("an event has no name")?);
    attributes(out, 3, event)?;
    dropped(out, 4, event.get("droppedAttributesCount"));
    Ok(())
}
fn link(out: &mut Vec<u8>, link: &Value) -> Encoded {
    hex_field(out, 1, link.get("traceId"))?;
    hex_field(out, 2, link.get("spanId"))?;
    optional_string(out, 3, link.get("traceState"));
    attributes(out, 4, link)?;
    dropped(out, 5, link.get("droppedAttributesCount"));
    flag32(out, 6, present(link, "flags"))
}
fn span(out: &mut Vec<u8>, span: &Value) -> Encoded {
    hex_field(out, 1, span.get("traceId"))?;
    hex_field(out, 2, span.get("spanId"))?;
    optional_string(out, 3, span.get("traceState"));
    if let Some(parent) = present(span, "parentSpanId") {
        hex_field(out, 4, Some(parent))?;
    }
    string_field(out, 5, span.get("name").and_then(Value::as_str).ok_or("a span has no name")?);
    varint_field(out, 6, wide(span.get("kind"))?);
    fixed64_field(out, 7, wide(span.get("startTimeUnixNano"))?);
    fixed64_field(out, 8, wide(span.get("endTimeUnixNano"))?);
    attributes(out, 9, span)?;
    dropped(out, 10, span.get("droppedAttributesCount"));
    repeated(out, 11, span, "events", event)?;
    dropped(out, 12, span.get("droppedEventsCount"));
    repeated(out, 13, span, "links", link)?;
    dropped(out, 14, span.get("droppedLinksCount"));
    message(out, 15, |inner| status(inner, span.get("status").ok_or("a span has no status")?))?;
    flag32(out, 16, present(span, "flags"))
}
fn scope_spans(out: &mut Vec<u8>, scope_spans: &Value) -> Encoded {
    message(out, 1, |inner| scope(inner, scope_spans.get("scope").ok_or("scopeSpans has no scope")?))?;
    repeated(out, 2, scope_spans, "spans", span)?;
    optional_string(out, 3, scope_spans.get("schemaUrl"));
    Ok(())
}
fn resource_spans(out: &mut Vec<u8>, resource_spans: &Value) -> Encoded {
    message(out, 1, |inner| resource(inner, resource_spans.get("resource").ok_or("resourceSpans has no resource")?))?;
    repeated(out, 2, resource_spans, "scopeSpans", scope_spans)?;
    optional_string(out, 3, resource_spans.get("schemaUrl"));
    Ok(())
}

// ---- metrics -----------------------------------------------------------------------------------------

fn number_data_point(out: &mut Vec<u8>, point: &Value) -> Encoded {
    fixed64_field(out, 2, wide(point.get("startTimeUnixNano"))?);
    fixed64_field(out, 3, wide(point.get("timeUnixNano"))?);
    if let Some(value) = present(point, "asDouble") {
        double_field(out, 4, float(Some(value))?);
    }
    if let Some(value) = present(point, "asInt") {
        fixed64_field(out, 6, wide(Some(value))?);
    }
    attributes(out, 7, point)?;
    if let Some(flags) = present(point, "flags") {
        varint_field(out, 8, wide(Some(flags))?);
    }
    Ok(())
}
fn histogram_data_point(out: &mut Vec<u8>, point: &Value) -> Encoded {
    fixed64_field(out, 2, wide(point.get("startTimeUnixNano"))?);
    fixed64_field(out, 3, wide(point.get("timeUnixNano"))?);
    fixed64_field(out, 4, wide(point.get("count"))?);
    if let Some(sum) = present(point, "sum") {
        double_field(out, 5, float(Some(sum))?);
    }
    for count in point.get("bucketCounts").and_then(Value::as_array).ok_or("bucketCounts is not a list")? {
        fixed64_field(out, 6, wide(Some(count))?);
    }
    for bound in point.get("explicitBounds").and_then(Value::as_array).ok_or("explicitBounds is not a list")? {
        double_field(out, 7, float(Some(bound))?);
    }
    attributes(out, 9, point)?;
    if let Some(flags) = present(point, "flags") {
        varint_field(out, 10, wide(Some(flags))?);
    }
    for (field, key) in [(11, "min"), (12, "max")] {
        if let Some(value) = present(point, key) {
            double_field(out, field, float(Some(value))?);
        }
    }
    Ok(())
}
fn sum(out: &mut Vec<u8>, sum: &Value) -> Encoded {
    repeated(out, 1, sum, "dataPoints", number_data_point)?;
    varint_field(out, 2, wide(sum.get("aggregationTemporality"))?);
    varint_field(out, 3, u64::from(sum.get("isMonotonic").and_then(Value::as_bool).ok_or("isMonotonic is not a boolean")?));
    Ok(())
}
fn histogram(out: &mut Vec<u8>, histogram: &Value) -> Encoded {
    repeated(out, 1, histogram, "dataPoints", histogram_data_point)?;
    varint_field(out, 2, wide(histogram.get("aggregationTemporality"))?);
    Ok(())
}
fn metric(out: &mut Vec<u8>, metric: &Value) -> Encoded {
    string_field(out, 1, metric.get("name").and_then(Value::as_str).ok_or("a metric has no name")?);
    optional_string(out, 2, metric.get("description"));
    optional_string(out, 3, metric.get("unit"));
    if let Some(gauge) = present(metric, "gauge") {
        message(out, 5, |inner| repeated(inner, 1, gauge, "dataPoints", number_data_point))?;
    }
    if let Some(value) = present(metric, "sum") {
        message(out, 7, |inner| sum(inner, value))?;
    }
    if let Some(value) = present(metric, "histogram") {
        message(out, 9, |inner| histogram(inner, value))?;
    }
    Ok(())
}
fn scope_metrics(out: &mut Vec<u8>, scope_metrics: &Value) -> Encoded {
    message(out, 1, |inner| scope(inner, scope_metrics.get("scope").ok_or("scopeMetrics has no scope")?))?;
    repeated(out, 2, scope_metrics, "metrics", metric)?;
    optional_string(out, 3, scope_metrics.get("schemaUrl"));
    Ok(())
}
fn resource_metrics(out: &mut Vec<u8>, resource_metrics: &Value) -> Encoded {
    message(out, 1, |inner| resource(inner, resource_metrics.get("resource").ok_or("resourceMetrics has no resource")?))?;
    repeated(out, 2, resource_metrics, "scopeMetrics", scope_metrics)?;
    optional_string(out, 3, resource_metrics.get("schemaUrl"));
    Ok(())
}

// ---- logs --------------------------------------------------------------------------------------------

fn log_record(out: &mut Vec<u8>, record: &Value) -> Encoded {
    fixed64_field(out, 1, wide(record.get("timeUnixNano"))?);
    if let Some(number) = present(record, "severityNumber") {
        varint_field(out, 2, wide(Some(number))?);
    }
    optional_string(out, 3, record.get("severityText"));
    if let Some(body) = present(record, "body") {
        message(out, 5, |inner| any_value(inner, body))?;
    }
    attributes(out, 6, record)?;
    dropped(out, 7, record.get("droppedAttributesCount"));
    flag32(out, 8, present(record, "flags"))?;
    for (field, key) in [(9, "traceId"), (10, "spanId")] {
        if present(record, key).and_then(Value::as_str).is_some_and(|id| !id.is_empty()) {
            hex_field(out, field, record.get(key))?;
        }
    }
    if let Some(observed) = present(record, "observedTimeUnixNano") {
        fixed64_field(out, 11, wide(Some(observed))?);
    }
    Ok(())
}
fn scope_logs(out: &mut Vec<u8>, scope_logs: &Value) -> Encoded {
    message(out, 1, |inner| scope(inner, scope_logs.get("scope").ok_or("scopeLogs has no scope")?))?;
    repeated(out, 2, scope_logs, "logRecords", log_record)?;
    optional_string(out, 3, scope_logs.get("schemaUrl"));
    Ok(())
}
fn resource_logs(out: &mut Vec<u8>, resource_logs: &Value) -> Encoded {
    message(out, 1, |inner| resource(inner, resource_logs.get("resource").ok_or("resourceLogs has no resource")?))?;
    repeated(out, 2, resource_logs, "scopeLogs", scope_logs)?;
    optional_string(out, 3, resource_logs.get("schemaUrl"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    /// The requests the original made with its protobuf serialization for `case`: `(the data it encoded, the bytes it made)`.
    fn original(case: &str) -> Vec<(Value, Vec<u8>)> {
        include_str!("../tests/fixtures/otlp-exports.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|entry| entry["name"] == case)
            .unwrap_or_else(|| panic!("no case {case}"))["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|request| {
                assert_eq!(request["contentType"], CONTENT_TYPE);
                (request["body"].clone(), base64::engine::general_purpose::STANDARD.decode(request["protobuf"].as_str().unwrap()).unwrap())
            })
            .collect()
    }
    fn assert_original(case: &str, signal: Signal) {
        let requests = original(case);
        assert!(!requests.is_empty());
        for (data, bytes) in requests {
            assert_eq!(encode(signal, &data).unwrap(), bytes, "{case}");
        }
    }

    #[test]
    fn traces_are_the_bytes_the_original_made() {
        assert_original("spans-protobuf", Signal::Traces);
    }

    #[test]
    fn spans_with_every_attribute_type_a_link_an_event_and_a_parent_are_the_bytes_the_original_made() {
        assert_original("rich-spans-protobuf", Signal::Traces);
    }

    #[test]
    fn metrics_are_the_bytes_the_original_made() {
        assert_original("metrics-protobuf", Signal::Metrics);
    }

    #[test]
    fn logs_are_the_bytes_the_original_made() {
        assert_original("logs-protobuf", Signal::Logs);
        assert_original("rich-logs-protobuf", Signal::Logs);
    }

    #[test]
    fn the_wire_primitives_are_the_originals() {
        let mut out = Vec::new();
        varint(&mut out, 300);
        assert_eq!(out, [0xac, 0x02]);
        out.clear();
        // A negative integer is ten bytes, as `asUintN(64)` makes it.
        varint_field(&mut out, 3, wide(Some(&json!(-1))).unwrap());
        assert_eq!(out, [0x18, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
        assert_eq!(wide(Some(&json!("1700000000000000000"))).unwrap(), 1_700_000_000_000_000_000);
        assert_eq!(hex_bytes("0aFf"), [0x0a, 0xff]);
        // `parseInt("1g")` is 1, `parseInt("zz")` is NaN (0), and a trailing character is dropped.
        assert_eq!(hex_bytes("1gzzf"), [1, 0]);
    }

    #[test]
    fn a_list_that_is_missing_or_a_number_that_is_not_an_integer_fails_as_the_original_throws() {
        assert!(encode(Signal::Traces, &json!({})).is_err());
        assert!(encode(Signal::Logs, &json!({"resourceLogs":[{"resource":{"attributes":[{"key":"k","value":{"intValue":1.5}}]},"scopeLogs":[]}]})).is_err());
        assert_eq!(encode(Signal::Logs, &json!({"resourceLogs":[]})).unwrap(), Vec::<u8>::new());
    }
}
