//! OTLP/HTTP export of the server's traces, metrics and logs (`observability/Observability.ts`, `serverLogger.ts`, and the exporters of
//! effect they install: `OtlpExporter`, `OtlpTracer`, `OtlpMetrics`, `OtlpLogger`), with JSON bodies or, for a signal whose protocol is
//! `http/protobuf`, the bytes `otlp_protobuf.rs` makes of the same body. `tests/fixtures/otlp-exports.jsonl` holds what those exporters
//! send, made by `tools/generate_otlp_fixtures.mjs`; the tests below compare this module's bodies to it.
//!
//! Every signal has one batch exporter: records are buffered and posted on the signal's interval (10 s unless configured), a batch of 1000
//! is posted at once (traces and logs), transient failures (a transport error, 408, 429, 5xx) are retried three times a second apart (a 429's
//! `Retry-After` when it has one), and a failure that is not recovered switches the exporter off for 60 seconds and drops what it held. What
//! is left is posted when the server stops, within three seconds.
//!
//! Divergences from the original: the spans are the ones this server runs (`ws.rpc.*`, `rpc_spans.rs`), a log record has no `fiberId` or
//! `logSpan.*` attribute (there are no fibers) and carries the message as a single string body, and the metrics of `Metrics.ts` are
//! recorded where this server has the event they count (RPC requests, effect claims, terminal sessions and restarts, webhook deliveries
//! and runs, secret requests and refs) except the provider session and turn counters and the git command counter and timer, which
//! nothing here records yet.
use crate::otlp_config::{Endpoint, ObservabilityConfig, Protocol, Signal};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}},
    time::Duration,
};
use tokio::{task::JoinHandle, time::Instant};

const SCOPE_AND_SERVICE: &str = "t3code-server";
const DISABLE_FOR: Duration = Duration::from_secs(60);
const RETRIES: usize = 3;

// ---- the resource and attribute values (`OtlpResource`) --------------------------------------------

/// `unknownToAttributeValue`
pub fn attribute_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => json!({"arrayValue":{"values":items.iter().map(attribute_value).collect::<Vec<_>>()}}),
        Value::String(text) => json!({"stringValue":text}),
        Value::Bool(flag) => json!({"boolValue":flag}),
        Value::Number(number) if number.is_i64() || number.is_u64() => json!({"intValue":number}),
        Value::Number(number) => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && float.abs() < 9e15 => json!({"intValue":float as i64}),
            _ => json!({"doubleValue":number}),
        },
        other => json!({"stringValue":other.to_string()}),
    }
}
pub fn entries_to_attributes<'a>(entries: impl IntoIterator<Item = (&'a str, &'a Value)>) -> Vec<Value> {
    entries.into_iter().map(|(key, value)| json!({"key":key,"value":attribute_value(value)})).collect()
}

/// `otlpResource` and `OtlpResource.make`: the environment's resource attributes, then the server's, then the service name.
pub fn resource(mode: &str, environment: &BTreeMap<String, String>) -> Value {
    let mut attributes: Vec<(String, Value)> = environment.iter().filter(|(key, _)| !matches!(key.as_str(), "service.name" | "service.version")).map(|(key, value)| (key.clone(), json!(value))).collect();
    for (key, value) in [("service.namespace", "t3code"), ("service.runtime", "t3-server"), ("service.mode", mode)] {
        attributes.retain(|(existing, _)| existing != key);
        attributes.push((key.to_owned(), json!(value)));
    }
    let mut list = entries_to_attributes(attributes.iter().map(|(key, value)| (key.as_str(), value)));
    list.push(json!({"key":"service.name","value":{"stringValue":SCOPE_AND_SERVICE}}));
    json!({"attributes":list,"droppedAttributesCount":0})
}

// ---- the batch exporter (`OtlpExporter.make`) ------------------------------------------------------

/// `OtlpSerialization`: the body of one export and its content type, for the protocol the signal's endpoint asks for.
fn encode_body(protocol: Protocol, signal: Signal, payload: &Value) -> Result<(Vec<u8>, &'static str), String> {
    match protocol {
        Protocol::Json => Ok((serde_json::to_vec(payload).map_err(|error| error.to_string())?, "application/json")),
        Protocol::Protobuf => Ok((crate::otlp_protobuf::encode(signal, payload)?, crate::otlp_protobuf::CONTENT_TYPE)),
    }
}

pub struct ExporterOptions {
    /// `OtlpTracer`, `OtlpLogger` or `OtlpMetrics`: part of the `user-agent`.
    pub label: &'static str,
    /// Which message a protobuf body is made of.
    pub signal: Signal,
    pub endpoint: Endpoint,
    /// Posts a batch when it reaches this many records; `None` for an exporter that only posts on its interval.
    pub max_batch: Option<usize>,
    /// Posts on the interval even with nothing buffered (metrics, whose body is a snapshot).
    pub export_empty: bool,
    pub shutdown_timeout: Duration,
    /// The JSON payload for a batch.
    pub body: Box<dyn Fn(Vec<Value>) -> Value + Send + Sync>,
}
struct Shared {
    options: ExporterOptions,
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
    buffer: Mutex<Vec<Value>>,
    disabled_until: Mutex<Option<Instant>>,
    in_flight: Mutex<Vec<JoinHandle<()>>>,
    interval: Mutex<Option<JoinHandle<()>>>,
}
#[derive(Clone)]
pub struct Exporter(Arc<Shared>);

fn transient(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}
/// `retryAfterDelay`: the seconds a `Retry-After` names, or until the date it names, else five seconds.
fn retry_after(value: Option<&str>) -> Duration {
    let Some(value) = value else { return Duration::from_secs(5) };
    if let Ok(seconds) = value.trim().parse::<f64>() {
        return Duration::from_secs_f64(seconds.max(0.0));
    }
    chrono::DateTime::parse_from_rfc2822(value.trim()).map_or(Duration::from_secs(5), |date| (date.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().unwrap_or(Duration::from_millis(1)).max(Duration::from_millis(1)))
}

impl Exporter {
    /// Starts the interval loop. `max_batch` pushes post on a task of the runtime this is called in.
    pub fn start(options: ExporterOptions) -> Self {
        let interval = options.endpoint.interval;
        let shared = Arc::new(Shared {
            client: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("an OTLP HTTP client"),
            handle: tokio::runtime::Handle::current(),
            options,
            buffer: Mutex::new(Vec::new()),
            disabled_until: Mutex::new(None),
            in_flight: Mutex::new(Vec::new()),
            interval: Mutex::new(None),
        });
        let weak = Arc::downgrade(&shared);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(shared) = weak.upgrade() else { return };
                Exporter(shared).export().await;
            }
        });
        *shared.interval.lock().unwrap() = Some(task);
        Self(shared)
    }
    pub fn push(&self, record: Value) {
        if self.0.disabled_until.lock().unwrap().is_some() {
            return;
        }
        let full = {
            let mut buffer = self.0.buffer.lock().unwrap();
            buffer.push(record);
            self.0.options.max_batch.is_some_and(|max| buffer.len() >= max)
        };
        if full {
            let exporter = self.clone();
            let task = self.0.handle.spawn(async move { exporter.export().await });
            let mut in_flight = self.0.in_flight.lock().unwrap();
            in_flight.retain(|task| !task.is_finished());
            in_flight.push(task);
        }
    }
    /// `runExport`
    pub async fn export(&self) {
        {
            let mut disabled = self.0.disabled_until.lock().unwrap();
            match *disabled {
                Some(until) if Instant::now() < until => return,
                Some(_) => *disabled = None,
                None => {}
            }
        }
        let items = {
            let mut buffer = self.0.buffer.lock().unwrap();
            if buffer.is_empty() && !(self.0.options.max_batch.is_none() && self.0.options.export_empty) {
                return;
            }
            std::mem::take(&mut *buffer)
        };
        let payload = (self.0.options.body)(items);
        if self.post(&payload).await.is_err() {
            let mut disabled = self.0.disabled_until.lock().unwrap();
            if disabled.is_none() {
                *disabled = Some(Instant::now() + DISABLE_FOR);
                self.0.buffer.lock().unwrap().clear();
                tracing::debug!(target: "t3_server::otlp", label = self.0.options.label, "Disabling exporter for 60 seconds");
            }
        }
    }
    async fn post(&self, payload: &Value) -> Result<(), ()> {
        let options = &self.0.options;
        // A body that cannot be encoded is an exception of the export, which switches the exporter off like a failed post.
        let (body, content_type) = encode_body(options.endpoint.protocol, options.signal, payload).map_err(|_| ())?;
        for attempt in 0..=RETRIES {
            // The body's content type and the exporter's own user agent replace what the configured headers name.
            let mut headers = reqwest::header::HeaderMap::new();
            for (name, value) in &options.endpoint.headers {
                if let (Ok(name), Ok(value)) = (reqwest::header::HeaderName::from_bytes(name.as_bytes()), reqwest::header::HeaderValue::from_str(value)) {
                    headers.insert(name, value);
                }
            }
            headers.insert(reqwest::header::CONTENT_TYPE, reqwest::header::HeaderValue::from_static(content_type));
            if let Ok(agent) = reqwest::header::HeaderValue::from_str(&format!("effect-opentelemetry-{}/0.0.0", options.label)) {
                headers.insert(reqwest::header::USER_AGENT, agent);
            }
            let request = self.0.client.post(&options.endpoint.url).headers(headers).body(body.clone());
            let delay = match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    let status = response.status().as_u16();
                    if !transient(status) {
                        return Err(());
                    }
                    if status == 429 { retry_after(response.headers().get("retry-after").and_then(|value| value.to_str().ok())) } else { Duration::from_secs(1) }
                }
                Err(_) => Duration::from_secs(1),
            };
            if attempt < RETRIES {
                tokio::time::sleep(delay).await;
            }
        }
        Err(())
    }
    /// Posts what is left, within the shutdown timeout, and ends the interval loop.
    pub async fn shutdown(&self) {
        if let Some(task) = self.0.interval.lock().unwrap().take() {
            task.abort();
        }
        let tasks: Vec<_> = self.0.in_flight.lock().unwrap().drain(..).collect();
        let last = async {
            if self.0.disabled_until.lock().unwrap().is_none() {
                self.export().await;
            }
            for task in tasks {
                let _ = task.await;
            }
        };
        let _ = tokio::time::timeout(self.0.options.shutdown_timeout, last).await;
    }
}

// ---- spans (`OtlpTracer.makeOtlpSpan`) -------------------------------------------------------------

fn span_kind(kind: &str) -> u8 {
    match kind {
        "internal" => 1,
        "server" => 2,
        "client" => 3,
        "producer" => 4,
        "consumer" => 5,
        _ => 0,
    }
}

/// One `effect-span` trace record (`spanToTraceRecord`) as the OTLP span the original's tracer exports for the same span.
pub fn span_from_record(record: &Value) -> Value {
    let empty = Map::new();
    let mut attributes = entries_to_attributes(record["attributes"].as_object().unwrap_or(&empty).iter().map(|(key, value)| (key.as_str(), value)));
    let mut events: Vec<Value> = record["events"]
        .as_array()
        .map(|events| events.iter().map(|event| json!({"name":event["name"],"timeUnixNano":event["timeUnixNano"],"attributes":entries_to_attributes(event["attributes"].as_object().unwrap_or(&empty).iter().map(|(key, value)| (key.as_str(), value))),"droppedAttributesCount":0})).collect())
        .unwrap_or_default();
    let end = record["endTimeUnixNano"].clone();
    let status = match record["exit"]["_tag"].as_str() {
        Some("Success") => json!({"code":1}),
        Some("Interrupted") => {
            attributes.push(json!({"key":"span.label","value":{"stringValue":"⚠︎ Interrupted"}}));
            attributes.push(json!({"key":"status.interrupted","value":{"boolValue":true}}));
            json!({"code":1,"message":"Interrupted"})
        }
        _ => {
            // The cause is the error as the client was told it; its tag names the exception.
            let cause = record["exit"]["cause"].as_str().unwrap_or_default();
            let told: Option<Value> = serde_json::from_str(cause).ok();
            let name = told.as_ref().and_then(|told| told["_tag"].as_str()).unwrap_or("Error");
            let message = told.as_ref().and_then(|told| told["message"].as_str()).unwrap_or(cause);
            events.push(json!({"name":"exception","timeUnixNano":end,"droppedAttributesCount":0,"attributes":[
                {"key":"exception.type","value":{"stringValue":name}},
                {"key":"exception.message","value":{"stringValue":message}},
                {"key":"exception.stacktrace","value":{"stringValue":"No stack trace available"}},
            ]}));
            json!({"code":2,"message":message})
        }
    };
    let links: Vec<Value> = record["links"].as_array().map(|links| links.iter().map(|link| json!({"traceId":link["traceId"],"spanId":link["spanId"],"attributes":entries_to_attributes(link["attributes"].as_object().unwrap_or(&empty).iter().map(|(key, value)| (key.as_str(), value))),"droppedAttributesCount":0})).collect()).unwrap_or_default();
    let mut span = Map::new();
    span.insert("traceId".into(), record["traceId"].clone());
    span.insert("spanId".into(), record["spanId"].clone());
    if let Some(parent) = record.get("parentSpanId").filter(|parent| parent.is_string()) {
        span.insert("parentSpanId".into(), parent.clone());
    }
    span.insert("name".into(), record["name"].clone());
    span.insert("kind".into(), json!(span_kind(record["kind"].as_str().unwrap_or_default())));
    span.insert("startTimeUnixNano".into(), record["startTimeUnixNano"].clone());
    span.insert("endTimeUnixNano".into(), end);
    span.insert("attributes".into(), json!(attributes));
    span.insert("droppedAttributesCount".into(), json!(0));
    span.insert("events".into(), json!(events));
    span.insert("droppedEventsCount".into(), json!(0));
    span.insert("status".into(), status);
    span.insert("links".into(), json!(links));
    span.insert("droppedLinksCount".into(), json!(0));
    Value::Object(span)
}

fn traces_body(resource: Value) -> impl Fn(Vec<Value>) -> Value + Send + Sync {
    move |spans| json!({"resourceSpans":[{"resource":resource,"scopeSpans":[{"scope":{"name":SCOPE_AND_SERVICE},"spans":spans}]}]})
}

// ---- logs (`OtlpLogger.makeLogRecord`) -------------------------------------------------------------

fn logs_body(resource: Value) -> impl Fn(Vec<Value>) -> Value + Send + Sync {
    move |records| json!({"resourceLogs":[{"resource":resource,"scopeLogs":[{"scope":{"name":SCOPE_AND_SERVICE},"logRecords":records}]}]})
}

static LOGS: OnceLock<Exporter> = OnceLock::new();

fn nanos_now() -> String {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos()).to_string()
}
/// The log record of an event; `severity` is the level as the original names it.
pub fn log_record(level: &tracing::Level, message: &str, fields: &Map<String, Value>) -> Value {
    let (number, text) = match *level {
        tracing::Level::TRACE => (1, "Trace"),
        tracing::Level::DEBUG => (5, "Debug"),
        tracing::Level::INFO => (9, "Info"),
        tracing::Level::WARN => (13, "Warn"),
        tracing::Level::ERROR => (17, "Error"),
    };
    let now = nanos_now();
    json!({
        "severityNumber":number,"severityText":text,"timeUnixNano":now,"observedTimeUnixNano":now,
        "attributes":entries_to_attributes(fields.iter().map(|(key, value)| (key.as_str(), value))),
        "body":{"stringValue":message},"droppedAttributesCount":0,
    })
}

#[derive(Default)]
struct Fields {
    message: String,
    fields: Map<String, Value>,
}
impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" { self.message = value.to_owned() } else { self.fields.insert(field.name().into(), json!(value)); }
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(field.name().into(), json!(value));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(field.name().into(), json!(value));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields.insert(field.name().into(), json!(value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields.insert(field.name().into(), json!(value));
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" { self.message = format!("{value:?}") } else { self.fields.insert(field.name().into(), json!(format!("{value:?}"))); }
    }
}
/// Sends the server's log events to the logs exporter once one is installed; a layer of the subscriber that writes them to the console.
pub struct LogLayer;
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let Some(exporter) = LOGS.get() else { return };
        let target = event.metadata().target();
        // What the exporter itself and its HTTP client log would be exported in turn.
        if ["t3_server::otlp", "hyper", "h2", "reqwest", "rustls", "tower", "tokio"].iter().any(|prefix| target.starts_with(prefix)) {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        exporter.push(log_record(event.metadata().level(), &fields.message, &fields.fields));
    }
}

// ---- metrics (`OtlpMetrics`, `Metrics.ts`) ---------------------------------------------------------

/// `Metric.timer`'s boundaries: 0.5 ms doubling 34 times.
fn timer_boundaries() -> Vec<f64> {
    (0..34).map(|power| 0.5 * 2f64.powi(power)).collect()
}
enum State {
    Counter { count: f64 },
    Timer { count: u64, sum: f64, min: f64, max: f64, buckets: Vec<u64> },
}
struct Series {
    name: &'static str,
    description: &'static str,
    attributes: Vec<(String, String)>,
    state: State,
}
static RECORDING: AtomicBool = AtomicBool::new(false);
static SERIES: Mutex<Vec<Series>> = Mutex::new(Vec::new());

/// Whether a metrics exporter is installed; nothing is recorded before one is.
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}
fn with_series(name: &'static str, description: &'static str, attributes: Vec<(String, String)>, new: impl FnOnce() -> State, update: impl FnOnce(&mut State)) {
    let mut series = SERIES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = series.iter_mut().find(|series| series.name == name && series.attributes == attributes) {
        update(&mut existing.state);
        return;
    }
    let mut state = new();
    update(&mut state);
    series.push(Series { name, description, attributes, state });
}
/// `Metrics.increment`: a counter, by the attributes it is recorded with.
pub fn count(name: &'static str, description: &'static str, attributes: &[(&str, &str)], amount: f64) {
    if !recording() {
        return;
    }
    let attributes = attributes.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect();
    with_series(name, description, attributes, || State::Counter { count: 0.0 }, |state| {
        if let State::Counter { count } = state {
            *count += amount;
        }
    });
}
/// A timer observation, in milliseconds.
pub fn time(name: &'static str, description: &'static str, attributes: &[(&str, &str)], millis: f64) {
    if !recording() {
        return;
    }
    let attributes = std::iter::once(("time_unit".to_owned(), "milliseconds".to_owned())).chain(attributes.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))).collect();
    with_series(name, description, attributes, || State::Timer { count: 0, sum: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY, buckets: vec![0; 35] }, |state| {
        if let State::Timer { count, sum, min, max, buckets } = state {
            let index = timer_boundaries().iter().position(|bound| millis <= *bound).unwrap_or(34);
            buckets[index] += 1;
            *count += 1;
            *sum += millis;
            *min = min.min(millis);
            *max = max.max(millis);
        }
    });
}
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 { json!(value as i64) } else { json!(value) }
}
/// The cumulative snapshot `OtlpMetrics` exports: counters as sums, timers as histograms, grouped by name in the order they first appeared.
fn snapshot(start: &str) -> Vec<Value> {
    let now = nanos_now();
    let series = SERIES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut metrics: Vec<(&'static str, Value)> = Vec::new();
    for entry in series.iter() {
        let attributes: Vec<Value> = entry.attributes.iter().map(|(key, value)| json!({"key":key,"value":{"stringValue":value}})).collect();
        let (kind, unit, point) = match &entry.state {
            State::Counter { count } => ("sum", "1", json!({"attributes":attributes,"startTimeUnixNano":start,"timeUnixNano":now,"asDouble":number(*count)})),
            State::Timer { count, sum, min, max, buckets } => ("histogram", "milliseconds", json!({
                "attributes":attributes,"startTimeUnixNano":start,"timeUnixNano":now,"count":count,"min":number(*min),"max":number(*max),"sum":number(*sum),
                "bucketCounts":buckets,"explicitBounds":timer_boundaries().into_iter().map(number).collect::<Vec<_>>(),
            })),
        };
        match metrics.iter_mut().find(|(name, _)| *name == entry.name) {
            Some((_, metric)) => metric[kind]["dataPoints"].as_array_mut().unwrap().push(point),
            None => {
                let body = if kind == "sum" { json!({"aggregationTemporality":2,"isMonotonic":false,"dataPoints":[point]}) } else { json!({"aggregationTemporality":2,"dataPoints":[point]}) };
                metrics.push((entry.name, json!({"name":entry.name,"description":entry.description,"unit":unit,kind:body})));
            }
        }
    }
    metrics.into_iter().map(|(_, metric)| metric).collect()
}
fn metrics_body(resource: Value) -> impl Fn(Vec<Value>) -> Value + Send + Sync {
    let start = nanos_now();
    move |_| json!({"resourceMetrics":[{"resource":resource,"scopeMetrics":[{"scope":{"name":SCOPE_AND_SERVICE},"metrics":snapshot(&start)}]}]})
}

// ---- the server's exporters ---------------------------------------------------------------------------

/// The exporters a server runs.
#[derive(Clone, Default)]
pub struct Otlp {
    pub traces: Option<Exporter>,
    pub metrics: Option<Exporter>,
    pub logs: Option<Exporter>,
    pub traces_endpoint: Option<Endpoint>,
    pub metrics_endpoint: Option<Endpoint>,
    pub logs_endpoint: Option<Endpoint>,
}
impl Otlp {
    /// Starts an exporter for each signal the configuration names, with the protocol its endpoint asks for. Installs the log and metric
    /// sources.
    pub fn start(config: &ObservabilityConfig, mode: &str) -> Self {
        let resource = resource(mode, &config.resource_attributes);
        let (traces, metrics, logs) = (config.traces.clone(), config.metrics.clone(), config.logs.clone());
        let shutdown_timeout = Duration::from_secs(3);
        let traces_exporter = traces.clone().map(|endpoint| Exporter::start(ExporterOptions { label: "OtlpTracer", signal: Signal::Traces, endpoint, max_batch: Some(1000), export_empty: false, shutdown_timeout, body: Box::new(traces_body(resource.clone())) }));
        let metrics_exporter = metrics.clone().map(|endpoint| {
            RECORDING.store(true, Ordering::Relaxed);
            Exporter::start(ExporterOptions { label: "OtlpMetrics", signal: Signal::Metrics, endpoint, max_batch: None, export_empty: true, shutdown_timeout, body: Box::new(metrics_body(resource.clone())) })
        });
        let logs_exporter = logs.clone().map(|endpoint| Exporter::start(ExporterOptions { label: "OtlpLogger", signal: Signal::Logs, endpoint, max_batch: Some(1000), export_empty: false, shutdown_timeout, body: Box::new(logs_body(resource.clone())) }));
        if let Some(exporter) = &logs_exporter {
            let _ = LOGS.set(exporter.clone());
        }
        Self { traces: traces_exporter, metrics: metrics_exporter, logs: logs_exporter, traces_endpoint: traces, metrics_endpoint: metrics, logs_endpoint: logs }
    }
    pub async fn shutdown(&self) {
        for exporter in [&self.traces, &self.metrics, &self.logs].into_iter().flatten() {
            exporter.shutdown().await;
        }
    }
}

/// The browser's own export, posted once to the traces endpoint as it came (`serialization.traces(bodyJson)`: encoded as the endpoint's
/// protocol says), with the configured headers and no retry. An error says what failed, without the URL's query (an intake may take its
/// key there). A browser body the protobuf encoder cannot read answers like a failed post, where the original's handler throws (a 500).
pub async fn forward(endpoint: &Endpoint, payload: &Value) -> Result<(), String> {
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().map_err(|error| error.to_string())?;
    let (body, content_type) = encode_body(endpoint.protocol, Signal::Traces, payload)?;
    let mut request = client.post(&endpoint.url).header("content-type", content_type).body(body);
    for (name, value) in &endpoint.headers {
        request = request.header(name, value);
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => Ok(()),
        Ok(response) => Err(format!("The collector answered {}.", response.status())),
        Err(error) => Err(error.without_url().to_string()),
    }
}

#[derive(Default)]
struct Slot(std::sync::OnceLock<Otlp>);
/// Installs the exporters the server runs, which `server.getConfig` reports and the browser trace route forwards to.
pub fn install(store: &crate::persistence::Store, otlp: Otlp) {
    let _ = store.extension(Slot::default).0.set(otlp);
}
pub fn current(store: &crate::persistence::Store) -> Option<Otlp> {
    store.extension(Slot::default).0.get().cloned()
}
/// `observability` of the server config: a signal is enabled when an exporter for it runs, and its URL is the one it posts to.
pub fn overlay_config(store: &crate::persistence::Store, config: &mut Value) {
    let Some(otlp) = current(store) else { return };
    for (signal, endpoint) in [("Traces", &otlp.traces_endpoint), ("Metrics", &otlp.metrics_endpoint), ("Logs", &otlp.logs_endpoint)] {
        if let Some(endpoint) = endpoint {
            config["observability"][format!("otlp{signal}Url")] = json!(endpoint.url);
            config["observability"][format!("otlp{signal}Enabled")] = json!(true);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::HANG_BOUND;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The metric registry is the process's, so the tests that record into it (or end an RPC span, which does) take turns.
    static REGISTRY: Mutex<()> = Mutex::new(());

    /// What the original's exporters sent for the same inputs: `(case name, requests)`.
    fn original(case: &str) -> Vec<Value> {
        include_str!("../tests/fixtures/otlp-exports.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|entry| entry["name"] == case)
            .map(|entry| entry["requests"].as_array().unwrap().clone())
            .unwrap()
    }
    fn environment() -> BTreeMap<String, String> {
        [("deployment.environment", "test lab"), ("team", "t3")].into_iter().map(|(key, value)| (key.to_owned(), value.to_owned())).collect()
    }

    /// Replaces what the original generates at random or reads from its clock with a marker, keeping the shape.
    fn normalized(mut value: Value) -> Value {
        match &mut value {
            Value::Object(map) => {
                for (key, entry) in map.iter_mut() {
                    match key.as_str() {
                        "traceId" | "spanId" | "parentSpanId" => *entry = json!("<id>"),
                        "startTimeUnixNano" | "endTimeUnixNano" | "timeUnixNano" | "observedTimeUnixNano" => *entry = json!("<time>"),
                        _ => *entry = normalized(entry.take()),
                    }
                }
            }
            Value::Array(items) => *items = items.drain(..).map(normalized).collect(),
            _ => {}
        }
        value
    }
    fn attribute<'a>(attributes: &'a Value, key: &str) -> Option<&'a Value> {
        attributes.as_array()?.iter().find(|attribute| attribute["key"] == key).map(|attribute| &attribute["value"])
    }

    #[test]
    fn the_resource_is_the_one_the_original_exports() {
        let spans = &original("spans")[0]["body"]["resourceSpans"][0]["resource"];
        assert_eq!(&resource("web", &environment()), spans);
    }

    #[test]
    fn spans_are_the_originals_for_success_interruption_and_failure() {
        let sent = &original("spans")[0]["body"]["resourceSpans"][0]["scopeSpans"][0];
        let attributes = json!({"rpc.transport":"websocket","rpc.system":"effect-rpc","rpc.method":"server.getConfig","rpc.aggregate":"server"});
        let record = |exit: Value, attributes: Value| json!({"type":"effect-span","name":"ws.rpc.server.getConfig","traceId":"t","spanId":"s","sampled":true,"kind":"internal","startTimeUnixNano":"1","endTimeUnixNano":"2","durationMs":0.0,"attributes":attributes,"events":[],"links":[],"exit":exit});
        let success = span_from_record(&record(json!({"_tag":"Success"}), attributes.clone()));
        assert_eq!(normalized(success), normalized(sent["spans"][0].clone()));
        let mut interrupted = span_from_record(&record(json!({"_tag":"Interrupted","cause":"All fibers interrupted without error"}), json!({})));
        interrupted["name"] = json!("ws.rpc.subscribeThread");
        assert_eq!(normalized(interrupted), normalized(sent["spans"][2].clone()));
        // A failure carries the error as an exception event; the stack is the original's own, which this server has none of.
        let failed = span_from_record(&record(json!({"_tag":"Failure","cause":"{\"_tag\":\"Rejected\",\"message\":\"no such thing\"}"}), json!({"rpc.method":"thread.get","count":3,"ratio":0.5,"flag":true})));
        let expected = &sent["spans"][1];
        assert_eq!(failed["status"], expected["status"]);
        assert_eq!(failed["attributes"], expected["attributes"]);
        assert_eq!(failed["events"][0]["name"], "exception");
        assert_eq!(attribute(&failed["events"][0]["attributes"], "exception.message"), attribute(&expected["events"][0]["attributes"], "exception.message"));
        assert_eq!(failed["events"][0]["attributes"].as_array().unwrap().len(), 3);
        assert_eq!(failed["parentSpanId"], Value::Null);
    }

    #[test]
    fn a_log_record_has_the_originals_fields_without_the_fiber() {
        let sent = &original("logs")[0]["body"]["resourceLogs"][0]["scopeLogs"][0]["logRecords"][1];
        let mut ours = log_record(&tracing::Level::WARN, "careful", &Map::new());
        // The original names the fiber the message was logged on; there are no fibers here.
        let mut theirs = sent.clone();
        theirs["attributes"] = json!([]);
        assert_eq!(normalized(ours.take()), normalized(theirs));
        let record = log_record(&tracing::Level::INFO, "hello", &[("thread".to_owned(), json!("t-1")), ("count".to_owned(), json!(2))].into_iter().collect());
        assert_eq!((record["severityNumber"].clone(), record["severityText"].clone()), (json!(9), json!("Info")));
        assert_eq!(record["attributes"], json!([{"key":"thread","value":{"stringValue":"t-1"}},{"key":"count","value":{"intValue":2}}]));
    }

    #[test]
    fn metrics_are_the_originals_counters_and_timer_histograms() {
        let _turn = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        RECORDING.store(true, Ordering::Relaxed);
        SERIES.lock().unwrap().clear();
        for _ in 0..2 {
            count("t3_rpc_requests_total", "Total RPC requests handled by the websocket RPC server.", &[("rpc.method", "server.getConfig"), ("outcome", "success")], 1.0);
        }
        count("t3_rpc_requests_total", "Total RPC requests handled by the websocket RPC server.", &[("rpc.method", "thread.get"), ("outcome", "failure")], 1.0);
        time("t3_rpc_request_duration", "RPC request handling duration.", &[("rpc.method", "server.getConfig")], 12.0);
        time("t3_rpc_request_duration", "RPC request handling duration.", &[("rpc.method", "server.getConfig")], 300.0);
        let body = metrics_body(resource("web", &environment()))(Vec::new());
        RECORDING.store(false, Ordering::Relaxed);
        SERIES.lock().unwrap().clear();
        assert_eq!(normalized(body), normalized(original("metrics")[0]["body"].clone()));
    }

    #[test]
    fn the_other_counters_and_timers_of_the_original_are_recorded_where_their_events_happen() {
        let _turn = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        RECORDING.store(true, Ordering::Relaxed);
        SERIES.lock().unwrap().clear();
        let directory = tempfile::tempdir().unwrap();
        let store = crate::persistence::Store::open(directory.path().join("state.sqlite")).unwrap();
        // An empty outbox is an attempt that claimed nothing.
        assert!(store.claim_effect("worker", chrono::Utc::now(), chrono::Duration::minutes(1)).unwrap().is_none());
        let secrets = crate::secret_requests::SecretRequests::for_store(&store).expect("secret storage in a memory store");
        assert!(secrets.consume("not a ref", "project").is_err());
        let metrics = snapshot("0");
        RECORDING.store(false, Ordering::Relaxed);
        SERIES.lock().unwrap().clear();
        let point = |name: &str| metrics.iter().find(|metric| metric["name"] == name).unwrap_or_else(|| panic!("{name} is recorded: {metrics:?}"))["sum"]["dataPoints"][0].clone();
        assert_eq!(point("t3_orchestration_effect_claims_total")["attributes"], json!([{"key":"result","value":{"stringValue":"empty"}}]));
        assert_eq!(point("t3_secret_refs_consumed_total")["attributes"], json!([{"key":"result","value":{"stringValue":"rejected"}}]));
    }

    // ---- the exporter against a loopback collector ----

    /// A collector that answers each request with the next status of `statuses` (200 when they run out) and reports what it received.
    type Request = (Vec<u8>, std::collections::HashMap<String, String>);
    async fn collector(statuses: Vec<(u16, Option<&'static str>)>) -> (String, tokio::sync::mpsc::UnboundedReceiver<Request>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let statuses = Arc::new(Mutex::new(statuses.into_iter()));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let (sender, statuses) = (sender.clone(), statuses.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(count) => request.extend_from_slice(&chunk[..count]),
                        }
                        let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8_lossy(&request[..end]).into_owned();
                        let length = head.lines().find_map(|line| line.to_lowercase().strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break (head, request[end + 4..end + 4 + length].to_vec());
                        }
                    };
                    let headers = head.lines().skip(1).filter_map(|line| line.split_once(':')).map(|(name, value)| (name.to_lowercase(), value.trim().to_owned())).collect();
                    // Anything but an export (another test's client may find this port after its own server closed) is none of the collector's.
                    if !head.starts_with("POST /v1/traces") {
                        let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    }
                    let (status, retry) = statuses.lock().unwrap().next().unwrap_or((200, None));
                    let _ = sender.send((body, headers));
                    let retry = retry.map_or(String::new(), |value| format!("Retry-After: {value}\r\n"));
                    let _ = socket.write_all(format!("HTTP/1.1 {status} X\r\n{retry}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await;
                });
            }
        });
        (url, receiver)
    }
    fn endpoint(url: String, interval: Duration) -> Endpoint {
        Endpoint { url, protocol: Protocol::Json, headers: [("authorization".to_owned(), "Bearer secret".to_owned()), ("user-agent".to_owned(), "mine".to_owned())].into_iter().collect(), interval }
    }
    fn exporter(url: String, interval: Duration) -> Exporter {
        exporter_with(endpoint(url, interval))
    }
    fn exporter_with(endpoint: Endpoint) -> Exporter {
        Exporter::start(ExporterOptions { label: "OtlpTracer", signal: Signal::Traces, endpoint, max_batch: Some(2), export_empty: false, shutdown_timeout: Duration::from_secs(3), body: Box::new(traces_body(resource("web", &BTreeMap::new()))) })
    }
    async fn received_bytes(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Request>) -> Request {
        tokio::time::timeout(HANG_BOUND, receiver.recv()).await.expect("the collector is posted to").unwrap()
    }
    async fn received(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<Request>) -> (Value, std::collections::HashMap<String, String>) {
        let (bytes, headers) = received_bytes(receiver).await;
        (serde_json::from_slice(&bytes).unwrap_or(Value::Null), headers)
    }
    fn span(name: &str) -> Value {
        json!({"traceId":"t","spanId":name,"name":name})
    }
    fn names(body: &Value) -> Vec<String> {
        body["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap_or_else(|| panic!("not a traces body: {body}")).iter().map(|span| span["name"].as_str().unwrap().to_owned()).collect()
    }

    #[tokio::test]
    async fn spans_are_posted_with_the_configured_headers_when_a_batch_fills_and_on_the_interval_and_at_shutdown() {
        let (url, mut collected) = collector(vec![]).await;
        let exporter = exporter(url, Duration::from_secs(10));
        exporter.push(span("a"));
        exporter.push(span("b"));
        // A batch of two goes at once, without waiting for the interval.
        let (body, headers) = received(&mut collected).await;
        assert_eq!(names(&body), ["a", "b"]);
        assert_eq!(headers["authorization"], "Bearer secret");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["user-agent"], "effect-opentelemetry-OtlpTracer/0.0.0");
        exporter.push(span("c"));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::time::resume();
        assert_eq!(names(&received(&mut collected).await.0), ["c"]);
        exporter.push(span("d"));
        exporter.shutdown().await;
        assert_eq!(names(&received(&mut collected).await.0), ["d"]);
    }

    fn protobuf_endpoint(url: String) -> Endpoint {
        Endpoint { protocol: Protocol::Protobuf, ..endpoint(url, Duration::from_secs(3600)) }
    }
    fn spans_in(body: &Value) -> Vec<Value> {
        body["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap().clone()
    }
    fn finished_span() -> Value {
        span_from_record(&json!({"type":"effect-span","name":"ws.rpc.server.getConfig","traceId":"0123456789abcdef0123456789abcdef","spanId":"0123456789abcdef","sampled":true,"kind":"internal","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000000000500","durationMs":0.0,"attributes":{"rpc.method":"server.getConfig"},"events":[],"links":[],"exit":{"_tag":"Success"}}))
    }

    #[tokio::test]
    async fn a_protobuf_endpoint_gets_the_bytes_of_the_json_body_with_the_protobuf_content_type() {
        let (url, mut collected) = collector(vec![]).await;
        let exporter = exporter_with(protobuf_endpoint(url));
        exporter.push(finished_span());
        exporter.export().await;
        let (bytes, headers) = received_bytes(&mut collected).await;
        assert_eq!(headers["content-type"], "application/x-protobuf");
        assert_eq!(headers["authorization"], "Bearer secret");
        assert_eq!(headers["user-agent"], "effect-opentelemetry-OtlpTracer/0.0.0");
        let expected = crate::otlp_protobuf::encode(Signal::Traces, &traces_body(resource("web", &BTreeMap::new()))(vec![finished_span()])).unwrap();
        assert_eq!(bytes, expected);
        // The span's name is in the message, so the body is not the JSON one.
        assert!(bytes.windows(23).any(|window| window == b"ws.rpc.server.getConfig"));
        assert!(serde_json::from_slice::<Value>(&bytes).is_err());
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn a_body_the_protobuf_encoder_cannot_read_switches_the_exporter_off_like_a_failed_post() {
        let (url, _collected) = collector(vec![]).await;
        let exporter = exporter_with(protobuf_endpoint(url));
        // Not an OTLP span (no kind, times or status).
        exporter.push(span("bad"));
        exporter.export().await;
        assert!(exporter.0.disabled_until.lock().unwrap().is_some());
        assert!(exporter.0.buffer.lock().unwrap().is_empty());
        exporter.push(finished_span());
        assert!(exporter.0.buffer.lock().unwrap().is_empty(), "nothing is taken while the exporter is off");
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn every_signal_of_a_protobuf_configuration_is_exported() {
        let endpoint = |signal: &str| Some(Endpoint { protocol: Protocol::Protobuf, ..endpoint(format!("http://127.0.0.1:9/v1/{signal}"), Duration::from_secs(3600)) });
        let config = ObservabilityConfig { traces: endpoint("traces"), metrics: None, logs: endpoint("logs"), resource_attributes: BTreeMap::new(), warnings: Vec::new() };
        let otlp = Otlp::start(&config, "web");
        assert!(otlp.traces.is_some() && otlp.logs.is_some());
        assert_eq!(otlp.traces_endpoint.map(|endpoint| endpoint.protocol), Some(Protocol::Protobuf));
    }

    #[tokio::test]
    async fn the_browser_export_is_forwarded_in_the_endpoints_protocol() {
        let (url, mut collected) = collector(vec![]).await;
        let body = traces_body(resource("web", &BTreeMap::new()))(vec![finished_span()]);
        forward(&protobuf_endpoint(url.clone()), &body).await.unwrap();
        let (bytes, headers) = received_bytes(&mut collected).await;
        assert_eq!((headers["content-type"].as_str(), bytes), ("application/x-protobuf", crate::otlp_protobuf::encode(Signal::Traces, &body).unwrap()));
        forward(&endpoint(url, Duration::from_secs(1)), &body).await.unwrap();
        let (sent, headers) = received(&mut collected).await;
        assert_eq!((headers["content-type"].as_str(), spans_in(&sent).len()), ("application/json", 1));
        // A body the encoder cannot read is an error, not a post.
        assert!(forward(&protobuf_endpoint("http://127.0.0.1:9/v1/traces".into()), &json!({})).await.is_err());
    }

    #[tokio::test]
    async fn transient_failures_are_retried_a_second_apart_and_a_retry_after_is_honored() {
        let (url, mut collected) = collector(vec![(503, None), (429, Some("3")), (200, None)]).await;
        let exporter = exporter(url, Duration::from_secs(3600));
        exporter.push(span("a"));
        let export = tokio::spawn({
            let exporter = exporter.clone();
            async move { exporter.export().await }
        });
        received(&mut collected).await;
        // The 503 is retried after one second, the 429 after the three it asked for.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::resume();
        received(&mut collected).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::time::resume();
        assert_eq!(names(&received(&mut collected).await.0), ["a"]);
        export.await.unwrap();
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn an_unrecovered_failure_drops_the_batch_and_switches_the_exporter_off_for_a_minute() {
        let (url, mut collected) = collector(vec![(400, None)]).await;
        let exporter = exporter(url, Duration::from_secs(3600));
        exporter.push(span("lost"));
        exporter.export().await;
        received(&mut collected).await;
        // Nothing is accepted while it is off, and nothing held is sent afterwards.
        exporter.push(span("ignored"));
        exporter.export().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        // The window ends when the next export runs (the interval's), and pushes are taken again after it.
        exporter.export().await;
        exporter.push(span("again"));
        exporter.export().await;
        assert_eq!(names(&received(&mut collected).await.0), ["again"]);
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn an_rpc_span_is_written_to_the_trace_file_and_exported() {
        let _turn = REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (url, mut collected) = collector(vec![]).await;
        let exporter = exporter(url, Duration::from_secs(3600));
        let directory = tempfile::tempdir().unwrap();
        let sink = crate::trace_sink::TraceSink::open(&directory.path().join("trace.ndjson"), 1 << 20, 2).unwrap();
        sink.set_otlp(exporter.clone());
        crate::rpc_spans::RpcSpan::start(Some(&sink), "server.getConfig").unwrap().succeed();
        // A browser's span (`otlp-span`) is forwarded by the route, not exported from here.
        sink.push(&json!({"type":"otlp-span","name":"browser"}));
        exporter.export().await;
        let (body, _) = received(&mut collected).await;
        assert_eq!(names(&body), ["ws.rpc.server.getConfig"]);
        let span = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!((span["kind"].clone(), span["status"].clone()), (json!(1), json!({"code":1})));
        assert_eq!(attribute(&span["attributes"], "rpc.aggregate"), Some(&json!({"stringValue":"server"})));
        exporter.shutdown().await;
    }

    #[test]
    fn a_retry_after_is_seconds_or_a_date_and_otherwise_five_seconds() {
        assert_eq!(retry_after(Some("7")), Duration::from_secs(7));
        assert_eq!(retry_after(None), Duration::from_secs(5));
        assert_eq!(retry_after(Some("soon")), Duration::from_secs(5));
        assert_eq!(retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")), Duration::from_millis(1));
    }
}
