// Runs the unchanged OTLP exporters of effect (the ones apps/server/src/observability/Observability.ts and serverLogger.ts install)
// against a recording HTTP client and writes the requests they make, for the Rust port to compare with
// (crates/server/src/otlp.rs, tests/fixtures/otlp-exports.jsonl). No network is used. Span ids, times and fiber ids are random in
// the original, so the Rust test compares everything else and only the shape of those.
import { writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Scope from "../../apps/server/node_modules/effect/dist/Scope.js";
import * as Exit from "../../apps/server/node_modules/effect/dist/Exit.js";
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
const client = HttpClient.make((request) =>
  Effect.sync(() => {
    const body = request.body;
    requests.push({
      url: request.url,
      method: request.method,
      headers: request.headers,
      contentType: body.contentType,
      body: JSON.parse(new TextDecoder().decode(body.body)),
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
const base = Layer.mergeAll(
  Layer.succeed(HttpClient.HttpClient, client),
  OtlpSerialization.layerJson,
  OtlpExporter.layerFlusher,
);
const run = (effect) => Effect.runPromise(Effect.scoped(effect).pipe(Effect.provide(base)));

const cases = [];
const record = (name) => {
  const taken = requests.splice(0);
  cases.push({ name, requests: taken });
};

// Spans: a success with attributes, a failure with a tagged error, an interrupted one and a child.
await run(
  Effect.gen(function* () {
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
  }),
);
record("spans");

// Logs
await run(
  Effect.gen(function* () {
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
  }),
);
record("logs");

// Metrics
await run(
  Effect.gen(function* () {
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
    yield* Metric.update(
      Metric.withAttributes(duration, [["rpc.method", "server.getConfig"]]),
      300,
    );
  }),
);
record("metrics");

writeFileSync(
  new URL("../crates/server/tests/fixtures/otlp-exports.jsonl", import.meta.url),
  cases.map((entry) => JSON.stringify(entry)).join("\n") + "\n",
);
console.log(cases.map((entry) => `${entry.name}: ${entry.requests.length} request(s)`).join("\n"));
