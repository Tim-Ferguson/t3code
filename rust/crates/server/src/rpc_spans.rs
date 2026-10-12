//! The server's own spans in the local trace file, and the RPC metrics (`observability/RpcInstrumentation.ts`).
//!
//! Each WebSocket RPC is a span named `ws.rpc.<method>` with the attributes the original gives it; a subscription's span lasts until it
//! ends, fails or is interrupted. The record is the original's `effect-span` (`spanToTraceRecord`), appended to the same file as the
//! spans browsers export, so `server.getTraceDiagnostics` sees both.
use crate::trace_sink::TraceSink;
use rand::RngCore;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::LazyLock, time::Instant};

/// `RPC_AGGREGATES`: the `rpc.aggregate` label of every method, kept as the original has them (`rpc_span_aggregates.txt`, one
/// `method<TAB>aggregate` a line, read from `RpcInstrumentation.ts`).
static AGGREGATES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    include_str!("rpc_span_aggregates.txt").lines().filter_map(|line| line.split_once('\t')).collect()
});

/// `RPC_METHODS_WITH_TRACING_DISABLED`: these record no span, for the call or anything it runs.
const TRACING_DISABLED: [&str; 4] = ["server.getTraceDiagnostics", "server.getProcessDiagnostics", "server.getProcessResourceHistory", "server.signalProcess"];

pub fn aggregate(method: &str) -> Option<&'static str> {
    AGGREGATES.get(method).copied()
}

fn hex(bytes: usize) -> String {
    let mut random = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut random);
    random.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn nanos_now() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos())
}

/// A call in progress. Dropped without an end, it was interrupted: the client let go of it, or the connection closed.
pub struct RpcSpan {
    /// The trace file; `None` for a call that records no span (its metrics are still counted).
    sink: Option<TraceSink>,
    method: String,
    aggregate: &'static str,
    trace_id: String,
    span_id: String,
    started_at: u128,
    clock: Instant,
    ended: bool,
}

impl RpcSpan {
    /// `None` when the method is not one of the group's, or there is neither a trace file to write nor a metrics exporter to count for.
    /// A method that records no span still counts in the metrics, as in the original.
    pub fn start(sink: Option<&TraceSink>, method: &str) -> Option<Self> {
        let aggregate = aggregate(method)?;
        let sink = sink.filter(|_| !TRACING_DISABLED.contains(&method)).cloned();
        if sink.is_none() && !crate::otlp::recording() {
            return None;
        }
        Some(Self { sink, method: method.to_owned(), aggregate, trace_id: hex(16), span_id: hex(8), started_at: nanos_now(), clock: Instant::now(), ended: false })
    }
    pub fn succeed(mut self) {
        self.end(json!({"_tag":"Success"}));
    }
    /// `cause` is the error as it is told to the client.
    pub fn fail(mut self, cause: &Value) {
        self.end(json!({"_tag":"Failure","cause":cause.to_string()}));
    }
    fn end(&mut self, exit: Value) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let elapsed = self.clock.elapsed();
        let outcome = match exit["_tag"].as_str() {
            Some("Success") => "success",
            Some("Failure") => "failure",
            _ => "interrupt",
        };
        crate::otlp::time("t3_rpc_request_duration", "RPC request handling duration.", &[("method", &self.method)], elapsed.as_secs_f64() * 1000.0);
        crate::otlp::count("t3_rpc_requests_total", "Total RPC requests handled by the websocket RPC server.", &[("method", &self.method), ("outcome", outcome)], 1.0);
        let Some(sink) = &self.sink else { return };
        sink.push(&json!({
            "type":"effect-span","name":format!("ws.rpc.{}", self.method),"traceId":self.trace_id,"spanId":self.span_id,"sampled":true,"kind":"internal",
            "startTimeUnixNano":self.started_at.to_string(),"endTimeUnixNano":(self.started_at + elapsed.as_nanos()).to_string(),"durationMs":elapsed.as_secs_f64() * 1000.0,
            "attributes":{"rpc.transport":"websocket","rpc.system":"effect-rpc","rpc.method":self.method,"rpc.aggregate":self.aggregate},
            "events":[],"links":[],"exit":exit,
        }));
    }
}

impl Drop for RpcSpan {
    fn drop(&mut self) {
        self.end(json!({"_tag":"Interrupted","cause":"All fibers interrupted without error"}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_has_the_label_the_original_gives_it() {
        assert_eq!(AGGREGATES.len(), 182);
        assert_eq!(aggregate("orchestration.dispatchCommand"), Some("orchestrationV2"));
        assert_eq!(aggregate("vcs.pull"), Some("git"));
        assert_eq!(aggregate("device.configure"), Some("device"));
        assert_eq!(aggregate("not.a.method"), None);
        for method in TRACING_DISABLED {
            assert!(aggregate(method).is_some(), "{method} still has a label for the metrics");
        }
    }
}
