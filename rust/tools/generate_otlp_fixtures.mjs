// Runs the unchanged OTLP exporters of effect (the ones apps/server/src/observability/Observability.ts and serverLogger.ts install)
// against a recording HTTP client and writes the requests they make, for the Rust port to compare with
// (crates/server/src/otlp.rs, otlp_protobuf.rs, tests/fixtures/otlp-exports.jsonl). No network is used. Span ids, times and fiber ids are
// random in the original, so the Rust test compares everything else and only the shape of those.
//
// The JSON cases ("spans", "logs", "metrics") record the JSON body. The protobuf cases ("*-protobuf", "rich-*") run through the original's
// `OtlpSerialization.layerProtobuf` and record, per request, the data structure it was given (as JSON) next to the bytes it made of it, so
// the Rust encoder is compared on exactly the input the original encoded.
import { writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Fiber from "../../apps/server/node_modules/effect/dist/Fiber.js";
import * as Logger from "../../apps/server/node_modules/effect/dist/Logger.js";
import * as Metric from "../../apps/server/node_modules/effect/dist/Metric.js";
import * as Tracer from "../../apps/server/node_modules/effect/dist/Tracer.js";
import {
  HttpClient,
  HttpClientResponse,
} from "../../apps/server/node_modules/effect/dist/http/index.js";
import {
  OtlpExporter,
  OtlpLogger,
  OtlpMetrics,
  OtlpSerialization,
  OtlpTracer,
} from "../../apps/server/node_modules/effect/dist/observability/index.js";

const requests = [];
// The data structures the protobuf serialization was handed, in order, to pair with the bytes it made.
const handed = [];
const PROTOBUF = "application/x-protobuf";
const client = HttpClient.make((request) =>
  Effect.sync(() => {
    const body = request.body;
    const protobuf = body.contentType === PROTOBUF;
    requests.push({
      url: request.url,
      method: request.method,
      headers: request.headers,
      contentType: body.contentType,
      body: protobuf ? handed.shift() : JSON.parse(new TextDecoder().decode(body.body)),
      ...(protobuf ? { protobuf: Buffer.from(body.body).toString("base64") } : {}),
    });
    return HttpClientResponse.fromWeb(request, new Response("{}"));
  }),
);
const resource = {
  serviceName: "t3code-server",
  attributes: {
    "service.namespace": "t3code",
    "service.runtime": "t3-server",
    "service.mode": "web",
  },
};
const environment = { OTEL_RESOURCE_ATTRIBUTES: "deployment.environment=test%20lab,team=t3" };
for (const [key, value] of Object.entries(environment)) process.env[key] = value;
const headers = { authorization: "Bearer secret", "x-team": "t3" };
const recordingProtobuf = Layer.effect(
  OtlpSerialization.OtlpSerialization,
  Effect.map(OtlpSerialization.OtlpSerialization, (real) => {
    const hand = (data) => handed.push(JSON.parse(JSON.stringify(data)));
    return {
      traces: (data) => (hand(data), real.traces(data)),
      metrics: (data) => (hand(data), real.metrics(data)),
      logs: (data) => (hand(data), real.logs(data)),
    };
  }),
).pipe(Layer.provide(OtlpSerialization.layerProtobuf));
const baseWith = (serialization) =>
  Layer.mergeAll(
    Layer.succeed(HttpClient.HttpClient, client),
    serialization,
    OtlpExporter.layerFlusher,
  );
const baseJson = baseWith(OtlpSerialization.layerJson);
const baseProtobuf = baseWith(recordingProtobuf);
const run = (effect, base = baseJson) =>
  Effect.runPromise(Effect.scoped(effect).pipe(Effect.provide(base)));

const cases = [];
const record = (name) => {
  const taken = requests.splice(0);
  cases.push({ name, requests: taken });
};

// Spans: a success with attributes, a failure with a tagged error, an interrupted one and a child.
const spans = Effect.gen(function* () {
  const tracer = yield* OtlpTracer.make({
    url: "http://collector.test/v1/traces",
    exportInterval: "10 seconds",
    headers,
    resource,
  });
  const provide = Effect.provideService(Tracer.Tracer, tracer);
  yield* Effect.void.pipe(
    Effect.withSpan("ws.rpc.server.getConfig", {
      kind: "internal",
      attributes: {
        "rpc.transport": "websocket",
        "rpc.system": "effect-rpc",
        "rpc.method": "server.getConfig",
        "rpc.aggregate": "server",
      },
    }),
    provide,
  );
  class Rejected extends Error {
    _tag = "Rejected";
  }
  yield* Effect.fail(Object.assign(new Rejected("no such thing"), {})).pipe(
    Effect.withSpan("ws.rpc.thread.get", {
      kind: "internal",
      attributes: { "rpc.method": "thread.get", count: 3, ratio: 0.5, flag: true },
    }),
    Effect.ignore,
    provide,
  );
  const fiber = yield* Effect.never.pipe(
    Effect.withSpan("ws.rpc.subscribeThread", { kind: "internal" }),
    provide,
    Effect.forkChild,
  );
  yield* Effect.yieldNow;
  yield* Fiber.interrupt(fiber);
});
await run(spans);
record("spans");

// Logs
const logs = Effect.gen(function* () {
  const logger = yield* OtlpLogger.make({
    url: "http://collector.test/v1/logs",
    exportInterval: "10 seconds",
    headers,
    resource,
  });
  yield* Effect.logInfo("hello", { thread: "t-1" }).pipe(
    Effect.provide(Logger.layer([logger], { mergeWithExisting: false })),
  );
  yield* Effect.logWarning("careful").pipe(
    Effect.provide(Logger.layer([logger], { mergeWithExisting: false })),
  );
});
await run(logs);
record("logs");

// Metrics
const metrics = Effect.gen(function* () {
  yield* OtlpMetrics.make({
    url: "http://collector.test/v1/metrics",
    exportInterval: "10 seconds",
    headers,
    resource,
  });
  const requests_ = Metric.counter("t3_rpc_requests_total", {
    description: "Total RPC requests handled by the websocket RPC server.",
  });
  const duration = Metric.timer("t3_rpc_request_duration", {
    description: "RPC request handling duration.",
  });
  yield* Metric.update(
    Metric.withAttributes(requests_, [
      ["rpc.method", "server.getConfig"],
      ["outcome", "success"],
    ]),
    1,
  );
  yield* Metric.update(
    Metric.withAttributes(requests_, [
      ["rpc.method", "server.getConfig"],
      ["outcome", "success"],
    ]),
    1,
  );
  yield* Metric.update(
    Metric.withAttributes(requests_, [
      ["rpc.method", "thread.get"],
      ["outcome", "failure"],
    ]),
    1,
  );
  yield* Metric.update(Metric.withAttributes(duration, [["rpc.method", "server.getConfig"]]), 12);
  yield* Metric.update(Metric.withAttributes(duration, [["rpc.method", "server.getConfig"]]), 300);
});
await run(metrics);
record("metrics");

// The same three through the protobuf serialization.
await run(spans, baseProtobuf);
record("spans-protobuf");
await run(logs, baseProtobuf);
record("logs-protobuf");
await run(metrics, baseProtobuf);
record("metrics-protobuf");

// Values the three above do not reach: every attribute type, a nested array, a negative and a zero integer, an empty and a non-ASCII
// string, a span link, an event with attributes, a child span (a parent id), and log annotations.
const rich = Effect.gen(function* () {
  const tracer = yield* OtlpTracer.make({
    url: "http://collector.test/v1/traces",
    exportInterval: "10 seconds",
    headers,
    resource,
  });
  const provide = Effect.provideService(Tracer.Tracer, tracer);
  const linked = yield* Effect.currentSpan.pipe(Effect.withSpan("linked"), provide);
  yield* Effect.gen(function* () {
    yield* Effect.annotateCurrentSpan({
      list: [1, "two", [true, 2.5]],
      negative: -7,
      zero: 0,
      empty: "",
      text: "café ☃ \u{1F600}",
      big: 4294967296,
      fraction: -0.125,
    });
    const span = yield* Effect.currentSpan;
    span.event("checkpoint", 1_700_000_000_000_000_000n, { phase: "one", n: 3 });
    yield* Effect.withSpan("child", { kind: "client" })(Effect.void);
  }).pipe(
    Effect.withSpan("parent", {
      kind: "server",
      links: [{ span: linked, attributes: { why: "x" } }],
    }),
    provide,
  );
});
await run(rich, baseProtobuf);
record("rich-spans-protobuf");
await run(
  Effect.gen(function* () {
    const logger = yield* OtlpLogger.make({
      url: "http://collector.test/v1/logs",
      exportInterval: "10 seconds",
      headers,
      resource,
    });
    yield* Effect.logError("failed", {
      count: -2,
      ratio: 0.25,
      list: ["a", 1],
      ok: false,
      empty: "",
    }).pipe(
      Effect.annotateLogs({ thread: "t-1" }),
      Effect.provide(Logger.layer([logger], { mergeWithExisting: false })),
    );
    yield* Effect.logDebug("one", "two").pipe(
      Effect.provide(Logger.layer([logger], { mergeWithExisting: false })),
    );
  }),
  baseProtobuf,
);
record("rich-logs-protobuf");

writeFileSync(
  new URL("../crates/server/tests/fixtures/otlp-exports.jsonl", import.meta.url),
  cases.map((entry) => JSON.stringify(entry)).join("\n") + "\n",
);
console.log(cases.map((entry) => `${entry.name}: ${entry.requests.length} request(s)`).join("\n"));
