// Runs the unchanged `decodeOtlpTraceRecords` (packages/shared/src/observability.ts) over trace exports of every shape a browser
// or a hand-made payload can take, and records the trace records (or that the decoder threw), for the Rust port to replay
// (crates/server/src/trace_sink.rs).
import { writeFileSync } from "node:fs";
import { decodeOtlpTraceRecords } from "../../packages/shared/src/observability.ts";

const str = (value) => ({ stringValue: value });
const attribute = (key, value) => ({ key, value });
const span = (extra = {}) => ({
  traceId: "0af7651916cd43dd8448eb211c80319c",
  spanId: "b7ad6b7169203331",
  name: "http.request",
  kind: 3,
  startTimeUnixNano: "1700000000000000000",
  endTimeUnixNano: "1700000000012500000",
  attributes: [],
  events: [],
  links: [],
  status: { code: 1 },
  ...extra,
});
const payload = (spans, extra = {}) => ({
  resourceSpans: [
    {
      resource: { attributes: [attribute("service.name", str("web"))] },
      scopeSpans: [
        {
          scope: {
            name: "browser",
            version: "1.2.3",
            attributes: [attribute("scope.key", str("v"))],
          },
          spans,
        },
      ],
      ...extra,
    },
  ],
});
const long = (length) => "x".repeat(length);

const cases = [
  ["one span", payload([span()])],
  ["kinds", payload([0, 1, 2, 3, 4, 5, 6, 99].map((kind) => span({ kind, spanId: `s${kind}` })))],
  [
    "parents",
    payload([span({ parentSpanId: "p1" }), span({ parentSpanId: "" }), span({ spanId: "s3" })]),
  ],
  [
    "value types",
    payload([
      span({
        attributes: [
          attribute("s", str("text")),
          attribute("b", { boolValue: false }),
          attribute("i", { intValue: "42" }),
          attribute("n", { intValue: 7 }),
          attribute("d", { doubleValue: 1.5 }),
          attribute("y", { bytesValue: "AAEC" }),
          attribute("a", {
            arrayValue: { values: [str("one"), { intValue: "2" }, { arrayValue: { values: [] } }] },
          }),
          attribute("k", {
            kvlistValue: {
              values: [
                attribute("inner", str("v")),
                attribute("deep", {
                  kvlistValue: { values: [attribute("x", { boolValue: true })] },
                }),
              ],
            },
          }),
          attribute("empty", {}),
          attribute("nullish", null),
          attribute("dup", str("first")),
          attribute("dup", str("second")),
        ],
      }),
    ]),
  ],
  [
    "long strings",
    payload([
      span({
        attributes: [
          attribute("exactly", str(long(500))),
          attribute("over", str(long(501))),
          attribute("db.query.text", str(long(200))),
          attribute("db.query.text", str(long(201))),
          attribute("other.db.query.text", str(long(300))),
          attribute("list", { arrayValue: { values: [str(long(600)), str("short")] } }),
          attribute("nested", { kvlistValue: { values: [attribute("deep", str(long(700)))] } }),
          attribute("wide", str("é".repeat(498) + "😀" + "tail".repeat(10))),
        ],
      }),
    ]),
  ],
  [
    "events links and status",
    payload([
      span({
        events: [
          {
            name: "log",
            timeUnixNano: "1700000000001000000",
            attributes: [attribute("message", str("hello"))],
          },
          { name: "bare", attributes: [] },
        ],
        links: [{ traceId: "t2", spanId: "s2", attributes: [attribute("kind", str("follows"))] }],
        status: { code: 2, message: "boom" },
      }),
      span({ status: { code: 0, message: "" } }),
      span({ status: {} }),
    ]),
  ],
  [
    "durations",
    payload([
      span({ startTimeUnixNano: "5", endTimeUnixNano: "1000005" }),
      span({ startTimeUnixNano: "10", endTimeUnixNano: "3" }),
      span({ startTimeUnixNano: "x", endTimeUnixNano: "2000000" }),
      span({ startTimeUnixNano: 100, endTimeUnixNano: 2000100 }),
    ]),
  ],
  [
    "missing fields on the span",
    payload([{ attributes: [], events: [], links: [], status: { code: 1 } }]),
  ],
  [
    "scopes and resources",
    {
      resourceSpans: [
        { scopeSpans: [{ scope: {}, spans: [span()] }] },
        {
          resource: {},
          scopeSpans: [{ scope: { name: "", version: "" }, spans: [span({ spanId: "second" })] }],
        },
        {
          resource: { attributes: [attribute("a", str("b"))] },
          scopeSpans: [
            { scope: { name: "n", attributes: "no" }, spans: [] },
            { scope: { version: 3 }, spans: [span({ spanId: "third" })] },
          ],
        },
      ],
    },
  ],
  ["no resource spans", { resourceSpans: [] }],
  // Payloads the decoder throws on, in whole.
  ["resourceSpans missing", {}],
  ["scopeSpans missing", { resourceSpans: [{ resource: {} }] }],
  ["spans missing", { resourceSpans: [{ scopeSpans: [{ scope: {} }] }] }],
  ["events missing", payload([{ ...span(), events: undefined }])],
  ["links missing", payload([{ ...span(), links: undefined }])],
  ["attributes missing", payload([{ ...span(), attributes: undefined }])],
  ["status missing", payload([{ ...span(), status: undefined }])],
  ["a good span before a bad one", payload([span(), { ...span({ spanId: "bad" }), links: null }])],
];

const rows = cases.map(([name, input]) => {
  const plain = JSON.parse(JSON.stringify(input));
  try {
    return {
      name,
      payload: plain,
      records: JSON.parse(JSON.stringify(decodeOtlpTraceRecords(input))),
    };
  } catch {
    return { name, payload: plain, error: true };
  }
});
writeFileSync(
  new URL("../crates/server/tests/fixtures/trace-otlp-records.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} payloads`);
