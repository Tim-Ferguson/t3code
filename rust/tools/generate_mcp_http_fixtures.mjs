// Development-only executable reference: the repository's patched Effect HTTP runtime.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import * as McpProtocol from "../../apps/server/node_modules/effect/dist/ai/McpProtocol.js";
import * as HttpRouter from "../../apps/server/node_modules/effect/dist/http/HttpRouter.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import { writeFileSync } from "node:fs";
const app = McpServer.layerHttp({
  name: "T3 Code",
  version: "0.0.45",
  path: "/mcp",
  protocols: [McpProtocol.v2025_06_18],
}).pipe(Layer.provide(NodeServices.layer));
const { handler, dispose } = HttpRouter.toWebHandler(app, { disableLogger: true });
const rows = [];
let session;
const base = { "content-type": "application/json", accept: "application/json, text/event-stream" };
async function call(method, headers, body) {
  const response = await handler(
    new Request("http://localhost/mcp", {
      method,
      headers,
      ...(body === undefined
        ? {}
        : { body: typeof body === "string" ? body : JSON.stringify(body) }),
    }),
  );
  const text = await response.text();
  return {
    status: response.status,
    headers: Object.fromEntries(
      [...response.headers].filter(([key]) =>
        ["content-type", "allow", "mcp-protocol-version"].includes(key),
      ),
    ),
    body: text ? JSON.parse(text) : null,
    session: response.headers.get("mcp-session-id"),
  };
}
async function record(name, method = "POST", headers = base, body) {
  const result = await call(method, headers, body);
  if (result.session) {
    session = result.session;
    result.session = "$session";
  }
  rows.push({
    name,
    method,
    headers: Object.fromEntries(
      Object.entries(headers).map(([key, value]) => [key, value === session ? "$session" : value]),
    ),
    ...(body === undefined ? {} : { input: body }),
    result,
  });
}
const initialize = {
  jsonrpc: "2.0",
  id: 1,
  method: "initialize",
  params: {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "probe", version: "1" },
  },
};
try {
  for (const method of ["GET", "PUT", "PATCH", "OPTIONS"])
    await record(`method-${method}`, method, {}, method === "GET" ? undefined : "");
  await record("origin", "POST", { ...base, origin: "http://localhost" }, initialize);
  for (const content of [
    "text/plain",
    "APPLICATION/JSON; charset=utf-8",
    "application/json;q=0",
    "application/json;q=NaN",
  ])
    await record(`content-${content}`, "POST", { ...base, "content-type": content }, initialize);
  for (const accept of [
    "application/json",
    "text/event-stream",
    "application/json;q=0,text/event-stream",
    "application/json;q=1.1,text/event-stream",
    "application/json, text/event-stream;q=0.5",
  ])
    await record(`accept-${accept}`, "POST", { ...base, accept }, initialize);
  await record("parse", "POST", base, "{");
  for (const body of [
    null,
    {},
    [],
    [initialize],
    { jsonrpc: "2.0", id: null, method: "initialize" },
    true,
  ])
    await record("invalid", "POST", base, body);
  await record("initialize", "POST", base, initialize);
  const bound = { ...base, "mcp-session-id": session, "mcp-protocol-version": "2025-06-18" };
  for (const [name, body] of [
    ["ping", { jsonrpc: "2.0", id: 0, method: "ping", params: {} }],
    ["initialized", { jsonrpc: "2.0", method: "notifications/initialized" }],
    ["tools", { jsonrpc: "2.0", id: "tools", method: "tools/list", params: {} }],
    ["unknown", { jsonrpc: "2.0", id: 2, method: "unknown", params: {} }],
    ["batch", []],
    ["initialize-bound", initialize],
  ])
    await record(name, "POST", bound, body);
  await record(
    "missing-version",
    "POST",
    { ...base, "mcp-session-id": session },
    { jsonrpc: "2.0", id: 2, method: "ping", params: {} },
  );
  await record(
    "wrong-version",
    "POST",
    { ...bound, "mcp-protocol-version": "unsupported" },
    { jsonrpc: "2.0", id: 2, method: "ping", params: {} },
  );
  await record("missing-session", "POST", base, {
    jsonrpc: "2.0",
    id: 2,
    method: "ping",
    params: {},
  });
  await record(
    "unknown-session",
    "POST",
    { ...bound, "mcp-session-id": "unknown" },
    { jsonrpc: "2.0", id: 2, method: "ping", params: {} },
  );
  await record("delete-missing", "DELETE", {});
  await record("delete-unknown", "DELETE", { "mcp-session-id": "unknown" });
  await record("delete-session", "DELETE", { "mcp-session-id": session });
  await record("deleted-session", "POST", bound, {
    jsonrpc: "2.0",
    id: 2,
    method: "ping",
    params: {},
  });
} finally {
  await dispose();
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/mcp-http.jsonl", import.meta.url),
  rows.map(JSON.stringify).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
