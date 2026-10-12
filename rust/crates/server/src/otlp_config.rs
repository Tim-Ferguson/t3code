//! Which signals the server exports over OTLP, where, and how (`packages/shared/src/otelEnvironment.ts`, the `T3CODE_OTLP_*` variables of
//! `apps/server/src/cli/config.ts`, and `loadPersistedObservabilitySettings`).
//!
//! Every read falls back instead of failing: a value that does not read leaves its signal not exported, with a warning that never echoes
//! the value (these variables carry credentials).
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Json,
    Protobuf,
}
impl Protocol {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "http/json" => Some(Self::Json),
            "http/protobuf" => Some(Self::Protobuf),
            _ => None,
        }
    }
}

/// `SignalExport` and the URL it goes to: everything one signal's exporter needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub protocol: Protocol,
    pub headers: BTreeMap<String, String>,
    pub interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
}
impl Signal {
    fn upper(self) -> &'static str {
        match self {
            Self::Traces => "TRACES",
            Self::Metrics => "METRICS",
            Self::Logs => "LOGS",
        }
    }
    fn path(self) -> &'static str {
        match self {
            Self::Traces => "v1/traces",
            Self::Metrics => "v1/metrics",
            Self::Logs => "v1/logs",
        }
    }
}

/// `OtelSignal`: what the OTEL variables say about one signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OtelSignal {
    Unset,
    Off,
    Export { url: String, protocol: Protocol, headers: Option<BTreeMap<String, String>> },
}

#[derive(Clone, Debug)]
pub struct OtelEnvironment {
    pub disabled: bool,
    pub warnings: Vec<String>,
    pub resource_attributes: BTreeMap<String, String>,
    pub traces: OtelSignal,
    pub metrics: OtelSignal,
    pub logs: OtelSignal,
}

/// A set but blank value reads as unset, so the source under it can answer.
fn blank_as_unset(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

#[derive(Clone)]
struct Setting<A> {
    value: Option<A>,
    warning: Option<String>,
}
impl<A> Setting<A> {
    fn unset() -> Self {
        Self { value: None, warning: None }
    }
    fn claimed(&self) -> bool {
        self.value.is_some() || self.warning.is_some()
    }
}
fn claimed<A: Clone>(own: &Setting<A>, generic: &Setting<A>) -> Setting<A> {
    if own.claimed() { own.clone() } else { generic.clone() }
}

fn read_or_warn<A>(env: &dyn Fn(&str) -> Option<String>, name: &str, parse: impl Fn(&str) -> Option<A>, warning: String) -> Setting<A> {
    match blank_as_unset(env(name)) {
        None => Setting::unset(),
        Some(raw) => match parse(&raw) {
            Some(value) => Setting { value: Some(value), warning: None },
            None => Setting { value: None, warning: Some(warning) },
        },
    }
}

const NOT_EXPORTED: &str = "so the signals it configures are not exported";

fn parse_http_url(raw: &str) -> Option<url::Url> {
    url::Url::parse(raw).ok().filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// `OtlpHeadersFromString`: `key=value` pairs separated by commas, values percent-encoded, a literal `=` inside a value surviving.
pub fn parse_headers(input: &str) -> Option<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    for pair in input.split(',') {
        if pair.trim().is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=')?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        let value = percent_encoding::percent_decode_str(value.trim()).decode_utf8().ok()?;
        headers.insert(key.to_owned(), value.into_owned());
    }
    Some(headers)
}

fn parse_resource_attributes(raw: &str) -> Option<BTreeMap<String, String>> {
    let mut attributes = BTreeMap::new();
    for pair in raw.split(',') {
        let (key, value) = pair.split_once('=')?;
        let decode = |text: &str| percent_encoding::percent_decode_str(text).decode_utf8().ok().map(|text| text.into_owned());
        attributes.insert(decode(key.trim())?, decode(value.trim())?);
    }
    Some(attributes)
}

struct Settings {
    endpoint: Setting<url::Url>,
    protocol: Setting<Protocol>,
    headers: Setting<BTreeMap<String, String>>,
}
fn settings(env: &dyn Fn(&str) -> Option<String>, prefix: &str) -> Settings {
    let name = |suffix: &str| format!("{prefix}{suffix}");
    Settings {
        endpoint: read_or_warn(env, &name("ENDPOINT"), parse_http_url, format!("{} is not an http or https URL, {NOT_EXPORTED}", name("ENDPOINT"))),
        protocol: read_or_warn(env, &name("PROTOCOL"), |raw| Protocol::parse(&raw.to_lowercase()), format!("{} is not http/protobuf or http/json, {NOT_EXPORTED}", name("PROTOCOL"))),
        headers: read_or_warn(env, &name("HEADERS"), parse_headers, format!("{} is not a list of key=value pairs with percent-encoded values, {NOT_EXPORTED}", name("HEADERS"))),
    }
}

/// A flag that accepts `truthy` and `falsy`, ignoring case and padding. Any other value is ignored with a warning.
fn flag(env: &dyn Fn(&str) -> Option<String>, name: &str, truthy: &[&str], falsy: &[&str], invalid: impl Fn(&str) -> String) -> Setting<bool> {
    let Some(raw) = env(name) else { return Setting::unset() };
    let value = raw.trim().to_lowercase();
    if truthy.contains(&value.as_str()) {
        return Setting { value: Some(true), warning: None };
    }
    if falsy.contains(&value.as_str()) {
        return Setting { value: Some(false), warning: None };
    }
    if raw.trim().is_empty() { Setting::unset() } else { Setting { value: None, warning: Some(invalid(raw.trim())) } }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Exporter {
    Otlp,
    None,
}
/// `OTEL_<SIGNAL>_EXPORTER`: a case-insensitive list whose default is `otlp`. Entries T3 Code has no exporter for are named in a warning
/// and dropped, and a list left with nothing to honor reads as unset.
fn exporter(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Setting<Exporter> {
    let raw = env(name).unwrap_or_default();
    let entries: Vec<String> = raw.split(',').map(|entry| entry.trim().to_lowercase()).filter(|entry| !entry.is_empty()).collect();
    let mut ignored: Vec<&str> = Vec::new();
    for entry in &entries {
        if !matches!(entry.as_str(), "otlp" | "none") && !ignored.contains(&entry.as_str()) {
            ignored.push(entry);
        }
    }
    let value = if entries.iter().any(|entry| entry == "otlp") { Some(Exporter::Otlp) } else if entries.iter().any(|entry| entry == "none") { Some(Exporter::None) } else { None };
    let warning = (!ignored.is_empty()).then(|| format!("{name} names {}, which T3 Code does not export to, so {} ignored", ignored.join(", "), if ignored.len() == 1 { "it was" } else { "they were" }));
    Setting { value, warning }
}

fn with_signal_path(signal: Signal, base: &url::Url) -> url::Url {
    let mut url = base.clone();
    let slash = if url.path().ends_with('/') { "" } else { "/" };
    let path = format!("{}{slash}{}", url.path(), signal.path());
    url.set_path(&path);
    url
}

struct Resolved {
    signal: OtelSignal,
    used: Vec<Option<String>>,
}
fn resolve_signal(signal: Signal, exporter: &Setting<Exporter>, own: &Settings, generic: &Settings) -> Resolved {
    if exporter.value == Some(Exporter::None) {
        return Resolved { signal: OtelSignal::Off, used: vec![exporter.warning.clone()] };
    }
    let own_endpoint = own.endpoint.claimed();
    let endpoint = if own_endpoint { &own.endpoint } else { &generic.endpoint };
    let mut used = vec![exporter.warning.clone(), endpoint.warning.clone()];
    let Some(base) = &endpoint.value else {
        let resolved = if endpoint.warning.is_none() { OtelSignal::Unset } else { OtelSignal::Off };
        return Resolved { signal: resolved, used };
    };
    let protocol = claimed(&own.protocol, &generic.protocol);
    let headers = claimed(&own.headers, &generic.headers);
    used.extend([protocol.warning.clone(), headers.warning.clone()]);
    if protocol.warning.is_some() || headers.warning.is_some() {
        return Resolved { signal: OtelSignal::Off, used };
    }
    let url = if own_endpoint { base.clone() } else { with_signal_path(signal, base) };
    // The OTEL variables name protobuf as the default; T3 Code's own say JSON.
    Resolved { signal: OtelSignal::Export { url: url.to_string(), protocol: protocol.value.unwrap_or(Protocol::Protobuf), headers: headers.value }, used }
}

/// `OtelEnvironment.load`
pub fn load_environment(env: &dyn Fn(&str) -> Option<String>) -> OtelEnvironment {
    let t3 = flag(env, "T3CODE_OTEL_SDK_DISABLED", &["true", "yes", "on", "1", "y"], &["false", "no", "off", "0", "n"], |value| format!("T3CODE_OTEL_SDK_DISABLED={value} is not a yes or a no and was ignored"));
    let spec = flag(env, "OTEL_SDK_DISABLED", &["true"], &["false"], |value| format!("OTEL_SDK_DISABLED={value} was read as false; the OpenTelemetry specification recognizes only the string true, so use OTEL_SDK_DISABLED=true or T3CODE_OTEL_SDK_DISABLED to say it any other way"));
    let mut resource_warning = None;
    let resource_attributes = match blank_as_unset(env("OTEL_RESOURCE_ATTRIBUTES")) {
        None => BTreeMap::new(),
        Some(raw) => parse_resource_attributes(&raw).unwrap_or_else(|| {
            // The value is left out because attributes can carry credentials.
            resource_warning = Some("OTEL_RESOURCE_ATTRIBUTES is not a list of percent-encoded key=value pairs and was ignored".to_owned());
            BTreeMap::new()
        }),
    };
    let generic = settings(env, "OTEL_EXPORTER_OTLP_");
    let (own_traces, own_metrics, own_logs) = (settings(env, "OTEL_EXPORTER_OTLP_TRACES_"), settings(env, "OTEL_EXPORTER_OTLP_METRICS_"), settings(env, "OTEL_EXPORTER_OTLP_LOGS_"));
    let disabled = t3.value.or(spec.value).unwrap_or(false);
    let mut warnings: Vec<String> = [t3.warning.clone(), spec.warning.clone(), resource_warning].into_iter().flatten().collect();
    let (traces, metrics, logs) = if disabled {
        (OtelSignal::Unset, OtelSignal::Unset, OtelSignal::Unset)
    } else {
        let resolved = [
            resolve_signal(Signal::Traces, &exporter(env, "OTEL_TRACES_EXPORTER"), &own_traces, &generic),
            resolve_signal(Signal::Metrics, &exporter(env, "OTEL_METRICS_EXPORTER"), &own_metrics, &generic),
            resolve_signal(Signal::Logs, &exporter(env, "OTEL_LOGS_EXPORTER"), &own_logs, &generic),
        ];
        // A generic variable read by several signals warns once.
        for warning in resolved.iter().flat_map(|resolved| resolved.used.iter().flatten()) {
            if !warnings.contains(warning) {
                warnings.push(warning.clone());
            }
        }
        let [traces, metrics, logs] = resolved.map(|resolved| resolved.signal);
        (traces, metrics, logs)
    };
    if disabled {
        warnings.push(if t3.value == Some(true) {
            "T3CODE_OTEL_SDK_DISABLED is set, so no telemetry is exported, whatever configured it".to_owned()
        } else {
            "OTEL_SDK_DISABLED is set, so no telemetry is exported, whatever configured it; set T3CODE_OTEL_SDK_DISABLED=false to export anyway".to_owned()
        });
    }
    OtelEnvironment { disabled, warnings, resource_attributes, traces, metrics, logs }
}

/// The `T3CODE_OTLP_*` variables: the one answer they give is the answer for all three signals.
#[derive(Clone, Debug)]
pub struct T3Export {
    pub protocol: Protocol,
    pub headers: Option<BTreeMap<String, String>>,
    pub interval: Duration,
}
pub fn t3_export(env: &dyn Fn(&str) -> Option<String>, warnings: &mut Vec<String>) -> T3Export {
    let protocol = match blank_as_unset(env("T3CODE_OTLP_PROTOCOL")) {
        None => Protocol::Json,
        Some(raw) => Protocol::parse(&raw).unwrap_or_else(|| {
            warnings.push("T3CODE_OTLP_PROTOCOL is not http/protobuf or http/json and was ignored".into());
            Protocol::Json
        }),
    };
    let headers = blank_as_unset(env("T3CODE_OTLP_HEADERS")).and_then(|raw| {
        let parsed = parse_headers(&raw);
        if parsed.is_none() {
            warnings.push("T3CODE_OTLP_HEADERS is not a list of key=value pairs with percent-encoded values and was ignored".into());
        }
        parsed
    });
    let interval = blank_as_unset(env("T3CODE_OTLP_EXPORT_INTERVAL_MS")).and_then(|raw| raw.parse::<u64>().ok()).map_or(Duration::from_millis(10_000), Duration::from_millis);
    T3Export { protocol, headers, interval }
}

/// `resolveSignalEndpoint`: `T3CODE_OTLP_*_URL` wins outright with T3 Code's own export, then an OTEL endpoint with its own headers and
/// protocol, then the first of `fallbacks` (the desktop bootstrap's, then settings') with T3 Code's own export.
pub fn resolve_endpoint(otel: &OtelEnvironment, signal: Signal, t3_url: Option<String>, t3: &T3Export, fallbacks: &[Option<String>]) -> Option<Endpoint> {
    if otel.disabled {
        return None;
    }
    let own = |url: String| Endpoint { url, protocol: t3.protocol, headers: t3.headers.clone().unwrap_or_default(), interval: t3.interval };
    if let Some(url) = blank_as_unset(t3_url) {
        return Some(own(url));
    }
    let state = match signal {
        Signal::Traces => &otel.traces,
        Signal::Metrics => &otel.metrics,
        Signal::Logs => &otel.logs,
    };
    match state {
        OtelSignal::Export { url, protocol, headers } => Some(Endpoint { url: url.clone(), protocol: *protocol, headers: headers.clone().unwrap_or_default(), interval: t3.interval }),
        OtelSignal::Off => None,
        OtelSignal::Unset => fallbacks.iter().find_map(|url| blank_as_unset(url.clone())).map(own),
    }
}

/// What the server exports: the resolved endpoint of each signal.
#[derive(Clone, Debug, Default)]
pub struct ObservabilityConfig {
    pub traces: Option<Endpoint>,
    pub metrics: Option<Endpoint>,
    pub logs: Option<Endpoint>,
    pub resource_attributes: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

/// The configuration the environment, the desktop bootstrap's URLs and the persisted settings' URLs give (`resolveServerConfig`).
pub fn resolve(env: &dyn Fn(&str) -> Option<String>, bootstrap: [Option<String>; 3], persisted: [Option<String>; 3]) -> ObservabilityConfig {
    let otel = load_environment(env);
    let mut warnings = otel.warnings.clone();
    let t3 = t3_export(env, &mut warnings);
    let [bootstrap_traces, bootstrap_metrics, bootstrap_logs] = bootstrap;
    let [persisted_traces, persisted_metrics, persisted_logs] = persisted;
    let endpoint = |signal: Signal, bootstrap: Option<String>, persisted: Option<String>| resolve_endpoint(&otel, signal, env(&format!("T3CODE_OTLP_{}_URL", signal.upper())), &t3, &[bootstrap, persisted]);
    ObservabilityConfig {
        traces: endpoint(Signal::Traces, bootstrap_traces, persisted_traces),
        metrics: endpoint(Signal::Metrics, bootstrap_metrics, persisted_metrics),
        logs: endpoint(Signal::Logs, bootstrap_logs, persisted_logs),
        resource_attributes: otel.resource_attributes,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect();
        move |key| map.get(key).cloned()
    }
    fn none() -> [Option<String>; 3] {
        [None, None, None]
    }

    #[test]
    fn t3_variables_win_and_the_protocol_defaults_to_json() {
        let config = resolve(&env(&[("T3CODE_OTLP_TRACES_URL", " http://t3.test/traces "), ("T3CODE_OTLP_HEADERS", "authorization=Bearer%20x,x-team=t3"), ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://otel.test")]), none(), none());
        let traces = config.traces.unwrap();
        assert_eq!((traces.url.as_str(), traces.protocol), ("http://t3.test/traces", Protocol::Json));
        assert_eq!(traces.headers["authorization"], "Bearer x");
        assert_eq!(traces.interval, Duration::from_secs(10));
        // The OTEL endpoint answers for the signals T3 Code's variables do not name, with its own path, protocol and headers.
        let metrics = config.metrics.unwrap();
        assert_eq!((metrics.url.as_str(), metrics.protocol, metrics.headers.is_empty()), ("http://otel.test/v1/metrics", Protocol::Protobuf, true));
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn bootstrap_then_settings_answer_when_the_environment_is_silent() {
        let config = resolve(&env(&[]), [Some("http://boot.test/t".into()), None, Some("  ".into())], [Some("http://saved.test/t".into()), Some("http://saved.test/m".into()), Some("http://saved.test/l".into())]);
        assert_eq!(config.traces.unwrap().url, "http://boot.test/t");
        assert_eq!(config.metrics.unwrap().url, "http://saved.test/m");
        assert_eq!(config.logs.unwrap().url, "http://saved.test/l");
    }

    #[test]
    fn the_kill_switch_and_a_signals_exporter_none_turn_export_off() {
        let off = resolve(&env(&[("OTEL_SDK_DISABLED", "true"), ("T3CODE_OTLP_TRACES_URL", "http://t3.test/traces")]), none(), none());
        assert!(off.traces.is_none() && off.warnings.iter().any(|warning| warning.contains("OTEL_SDK_DISABLED is set")));
        let some = resolve(&env(&[("OTEL_SDK_DISABLED", "true"), ("T3CODE_OTEL_SDK_DISABLED", "no"), ("T3CODE_OTLP_TRACES_URL", "http://t3.test/traces")]), none(), none());
        assert!(some.traces.is_some());
        let none_exporter = resolve(&env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://otel.test"), ("OTEL_LOGS_EXPORTER", "none"), ("OTEL_TRACES_EXPORTER", "console")]), none(), [Some("http://saved.test/t".into()), None, Some("http://saved.test/l".into())]);
        assert!(none_exporter.logs.is_none());
        assert_eq!(none_exporter.traces.unwrap().url, "http://otel.test/v1/traces");
        assert!(none_exporter.warnings.iter().any(|warning| warning.contains("console")));
    }

    #[test]
    fn a_value_that_does_not_read_claims_its_signal_without_exporting_and_is_never_echoed() {
        let config = resolve(&env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "ftp://secret-host"), ("OTEL_EXPORTER_OTLP_LOGS_HEADERS", "=oops")]), none(), [Some("http://saved.test/t".into()), None, None]);
        assert!(config.traces.is_none() && config.logs.is_none());
        assert!(config.warnings.iter().all(|warning| !warning.contains("secret-host") && !warning.contains("oops")));
        assert_eq!(config.warnings.len(), 1);
        // A signal's own headers are read once its endpoint is, and not echoed either.
        let headers = resolve(&env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://otel.test"), ("OTEL_EXPORTER_OTLP_LOGS_HEADERS", "=oops")]), none(), none());
        assert!(headers.logs.is_none() && headers.traces.is_some());
        assert!(headers.warnings.len() == 1 && !headers.warnings[0].contains("oops"));
        let attributes = load_environment(&env(&[("OTEL_RESOURCE_ATTRIBUTES", "deployment.environment=test%20lab,team=t3")]));
        assert_eq!(attributes.resource_attributes["deployment.environment"], "test lab");
        assert!(load_environment(&env(&[("OTEL_RESOURCE_ATTRIBUTES", "broken")])).warnings[0].contains("OTEL_RESOURCE_ATTRIBUTES"));
    }
}
