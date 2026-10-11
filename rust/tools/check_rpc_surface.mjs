#!/usr/bin/env node
// Compares what the original server exposes with what the Rust server serves, derived from source on both sides:
//   rpc       every method of the original's `WsRpcGroup` (contracts `rpc.ts`, `orchestrationV2.ts`) against the methods
//             the Rust server's handlers name;
//   http      every HTTP route of the original (`environmentHttp.ts` endpoints, `HttpRouter.add` routes, and the prefixes
//             they use) against the Rust router's `.route(...)` calls;
//   mcp       every tool the original's MCP server lists (`Tool.make` in `apps/server/src/mcp`) against the catalogs the
//             Rust server lists, and which of those answer "no service";
//   settings  every key of the original's `ServerSettings` against the Rust settings struct and the server code that acts
//             on it (a key that is stored but never read counts as not ported);
//   env       every capability flag of the environment descriptor against the ones the Rust server advertises.
// Usage: node rust/tools/check_rpc_surface.mjs [--section rpc|http|mcp|settings|env] [--json]
// Statuses: handled | partial (handled, but a part answers "not supported"; reason in the allowlist) | pending (known gap, to
// be ported) | omitted (deliberate, reason in the allowlist) | MISSING (a gap the allowlist does not know: exits 1).
// The allowlist is `check_rpc_surface_allowlist.json` beside this file; every entry carries a reason.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../..");
const read = (file) => fs.readFileSync(path.join(root, file), "utf8");
const allow = JSON.parse(
  fs.readFileSync(path.join(here, "check_rpc_surface_allowlist.json"), "utf8"),
);
const args = process.argv.slice(2);
const only = args.includes("--section") ? args[args.indexOf("--section") + 1] : null;
const asJson = args.includes("--json");

const serverSrc = "rust/crates/server/src";
const rustFiles = fs
  .readdirSync(path.join(root, serverSrc))
  .filter((f) => f.endsWith(".rs"))
  // Declaration, decoding and test files name every method without handling it.
  .filter(
    (f) =>
      !/(_tests?\.rs$|^rpc_handler_decoders|^rpc_response|^rpc_admission|^test_support|^fork_parity|^reducer_bench|^fixture\.rs)/.test(
        f,
      ),
  );
const rustText = Object.fromEntries(rustFiles.map((f) => [f, read(`${serverSrc}/${f}`)]));
// Without `#[cfg(test)] mod ... { }` blocks: a method only a test names is not handled.
const stripTests = (text) => {
  let out = "";
  let rest = text;
  for (;;) {
    const m = /#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))? )?mod \w+ \{/.exec(rest);
    if (!m) return out + rest;
    out += rest.slice(0, m.index);
    let depth = 1;
    let i = m.index + m[0].length;
    for (; i < rest.length && depth > 0; i += 1)
      depth += rest[i] === "{" ? 1 : rest[i] === "}" ? -1 : 0;
    rest = rest.slice(i);
  }
};
const handlerText = Object.fromEntries(
  Object.entries(rustText).map(([f, t]) => [f, stripTests(t)]),
);

const result = {};
let unknown = 0;
const classify = (section, name, handled, where) => {
  const rules = allow[section] ?? {};
  let status = handled ? "handled" : "MISSING";
  let reason = where ?? "";
  if (handled && rules.partial?.[name]) [status, reason] = ["partial", rules.partial[name]];
  if (!handled && rules.pending?.[name]) [status, reason] = ["pending", rules.pending[name]];
  if (!handled && rules.omitted?.[name]) [status, reason] = ["omitted", rules.omitted[name]];
  if (status === "MISSING") unknown += 1;
  (result[section] ??= []).push({ name, status, reason });
};

// ---- rpc
const rpcSection = () => {
  const block = (source, name) => {
    const m = source.match(new RegExp(`export const ${name} = \\{([\\s\\S]*?)\\} as const`));
    return Object.fromEntries(
      [...m[1].matchAll(/^\s*(\w+):\s*"([^"]+)"/gm)].map((x) => [x[1], x[2]]),
    );
  };
  const rpc = read("packages/contracts/src/rpc.ts");
  const ws = block(rpc, "WS_METHODS");
  const orchestration = block(
    read("packages/contracts/src/orchestrationV2.ts"),
    "ORCHESTRATION_V2_WS_METHODS",
  );
  const defined = [
    ...rpc.matchAll(/const (\w+) = Rpc\.make\(\s*(WS_METHODS|ORCHESTRATION_V2_WS_METHODS)\.(\w+)/g),
  ].map((m) => [m[1], m[2] === "WS_METHODS" ? ws[m[3]] : orchestration[m[3]]]);
  const group = rpc.slice(rpc.indexOf("export const WsRpcGroup"));
  const methods = defined
    .filter(([constant]) => new RegExp(`\\b${constant}\\b`).test(group))
    .map(([, method]) => method)
    .sort();
  for (const method of methods) {
    const files = Object.keys(handlerText).filter((f) => handlerText[f].includes(`"${method}"`));
    classify("rpc", method, files.length > 0, files.slice(0, 3).join(", "));
  }
};

// ---- http
const normalizePath = (p) =>
  p
    .replace(/:(\w+)/g, "{$1}")
    .replace(/\{\*\w+\}/g, "*")
    .replace(/\/\*$/, "/*")
    .replace(/\{\w+\}/g, "{}");
const httpSection = () => {
  const routes = new Map();
  const add = (method, p, from) => routes.set(`${method.toUpperCase()} ${normalizePath(p)}`, from);
  const contract = read("packages/contracts/src/environmentHttp.ts");
  const webhook = contract.match(/WEBHOOK_PATH = "([^"]+)"/)[1];
  for (const m of contract.matchAll(
    /HttpApiEndpoint\.(get|post|put|patch|delete)\(\s*"(\w+)",\s*(WEBHOOK_PATH|"[^"]+")/g,
  ))
    add(m[1], m[3] === "WEBHOOK_PATH" ? webhook : m[3].slice(1, -1), "environmentHttp.ts");
  for (const m of contract.matchAll(
    /\.add\(HttpApiEndpoint\.(get|post)\("(\w+)",\s*"([^"]+)"\)\)/g,
  ))
    add(m[1], m[3], "environmentHttp.ts");
  const constants = {
    OTLP_TRACES_PROXY_PATH: read("apps/server/src/http.ts").match(
      /OTLP_TRACES_PROXY_PATH = "([^"]+)"/,
    )[1],
    ASSET_ROUTE_PREFIX: read("apps/server/src/assets/AssetAccess.ts").match(
      /ASSET_ROUTE_PREFIX = "([^"]+)"/,
    )[1],
    ATTACHMENT_UPLOAD_ROUTE_PREFIX: read("apps/server/src/assets/AttachmentUpload.ts").match(
      /ATTACHMENT_UPLOAD_ROUTE_PREFIX = "([^"]+)"/,
    )[1],
    DEVICE_HUB_ROUTE_PREFIX: read("apps/server/src/device/DeviceService.ts").match(
      /DEVICE_HUB_ROUTE_PREFIX = "([^"]+)"/,
    )[1],
    PREVIEW_STREAM_ROUTE_PREFIX: read("apps/server/src/preview/ServerBrowserStream.ts").match(
      /PREVIEW_STREAM_ROUTE_PREFIX = "([^"]+)"/,
    )[1],
  };
  const resolve = (expr) =>
    (constants[expr] ?? expr)
      .replace(/\$\{(?:\w+\.)?(\w+)\}/g, (_, n) => constants[n] ?? "?")
      .replace(/^`|`$/g, "")
      .replace(/^"|"$/g, "");
  const files = [
    "http.ts",
    "ws.ts",
    "mcp/McpHttpServer.ts",
    "device/DeviceHubProxy.ts",
    "preview/ServerBrowserStream.ts",
    "scheduledTasks/webhookRoute.ts",
  ];
  for (const file of files) {
    const text = read(`apps/server/src/${file}`);
    for (const m of text.matchAll(/HttpRouter\.add\(\s*"(\w+|\*)",\s*([`"][^,]+[`"]|\w+)/g))
      add(m[1], resolve(m[2]), file);
    for (const m of text.matchAll(/router\.add\("(\w+)",\s*([`"][^,]+[`"])/g))
      add(m[1], resolve(m[2]), file);
  }
  add("ANY", "/mcp", "mcp/McpHttpServer.ts (McpServer.layerHttp)");
  // The static and dev route is the router's fallback.
  add("GET", "*", "http.ts layerStaticAndDevRoute");
  const rustRoutes = new Set();
  for (const [, text] of Object.entries(handlerText))
    for (const m of text.matchAll(/\.route\(\s*"([^"]+)"/g)) rustRoutes.add(normalizePath(m[1]));
  for (const m of handlerText["attachment_http.rs"].matchAll(
    /(ATTACHMENT_UPLOAD_ROUTE|ASSET_ROUTE): &str = "([^"]+)"/g,
  ))
    rustRoutes.add(normalizePath(m[2]));
  rustRoutes.add("*"); // `.fallback(get(static_asset))`
  for (const [key, from] of [...routes].sort()) {
    const [method, p] = [key.split(" ")[0], key.slice(key.indexOf(" ") + 1)];
    classify(
      "http",
      key,
      rustRoutes.has(p) ||
        (p.endsWith("/*") &&
          [...rustRoutes].some((r) => r === p || r === p.replace(/\/\*$/, "/*"))),
      from,
    );
  }
  for (const rust of [...rustRoutes].sort())
    if (![...routes.keys()].some((k) => k.endsWith(` ${rust}`)))
      (result.httpExtra ??= []).push(rust);
};

// ---- mcp
const mcpSection = () => {
  const names = new Set();
  const walk = (dir) => {
    for (const entry of fs.readdirSync(path.join(root, dir), { withFileTypes: true })) {
      const p = `${dir}/${entry.name}`;
      if (entry.isDirectory()) walk(p);
      else if (p.endsWith(".ts") && !/\.(test|testkit)\.ts$/.test(p)) {
        const text = read(p);
        for (const m of text.matchAll(/Tool\.make\(\s*(?:"([^"]+)"|(\w+))/g)) {
          if (m[1]) names.add(m[1]);
          else
            names.add(
              text.match(new RegExp(`${m[2]} = "([^"]+)"`))?.[1] ??
                read("packages/shared/src/htmlRender.ts").match(
                  new RegExp(`${m[2]} = "([^"]+)"`),
                )?.[1] ??
                m[2],
            );
        }
      }
    }
  };
  walk("apps/server/src/mcp");
  const listed = new Set();
  for (const f of ["mcp_tool_catalog.json", "mcp_control_catalog.json", "mcp_device_catalog.json"])
    for (const e of JSON.parse(read(`${serverSrc}/${f}`))) listed.add((e.tool ?? e).name);
  const extra = rustText["mcp_extra.rs"];
  const unavailable = new Set(
    [
      ...extra
        .slice(
          extra.indexOf("pub(crate) const UNAVAILABLE"),
          extra.indexOf("pub(crate) fn unavailable_tool"),
        )
        .matchAll(/\("(\w+)",\s*Gate/g),
    ].map((m) => m[1]),
  );
  for (const name of [...names].sort()) {
    if (!listed.has(name)) classify("mcp", name, false, "");
    else if (unavailable.has(name))
      classify("mcp", name, true, "listed; answers the original's no-host failure");
    else classify("mcp", name, true, "handled");
  }
  for (const name of unavailable) {
    const row = result.mcp.find((r) => r.name === name);
    if (row && allow.mcp?.partial?.[name])
      [row.status, row.reason] = ["partial", allow.mcp.partial[name]];
    else if (row) {
      row.status = allow.mcp?.pending?.[name] ? "pending" : "MISSING";
      row.reason = allow.mcp?.pending?.[name] ?? row.reason;
      if (row.status === "MISSING") unknown += 1;
    }
  }
};

// ---- settings
const settingsSection = () => {
  const source = read("packages/contracts/src/settings.ts");
  const start = source.indexOf("export const ServerSettings = Schema.Struct({");
  const body = source.slice(start, source.indexOf("export type ServerSettings", start));
  const keys = [...body.matchAll(/^  (\w+):/gm)].map((m) => m[1]);
  const snake = (k) => k.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);
  const model = read("rust/crates/contracts/src/settings.rs");
  const consumers = Object.entries(handlerText).filter(
    ([f]) => !/^(server_settings|settings)/.test(f),
  );
  for (const key of keys) {
    const stored = model.includes(`pub ${snake(key)}:`);
    const users = consumers
      .filter(
        ([, t]) =>
          t.includes(`.${snake(key)}`) || t.includes(`["${key}"]`) || t.includes(`"${key}"`),
      )
      .map(([f]) => f);
    const rules = allow.settings ?? {};
    let status = stored && users.length > 0 ? "handled" : "MISSING";
    let reason = users.slice(0, 4).join(", ");
    if (!stored) reason = "not in the Rust settings struct";
    if (status === "handled" && rules.partial?.[key])
      [status, reason] = ["partial", rules.partial[key]];
    if (status !== "handled" && status !== "partial") {
      if (rules.clientOnly?.[key])
        [status, reason] = ["handled", `client-side key: ${rules.clientOnly[key]}`];
      else if (rules.pending?.[key]) [status, reason] = ["pending", rules.pending[key]];
      else if (rules.omitted?.[key]) [status, reason] = ["omitted", rules.omitted[key]];
    }
    if (status === "MISSING") unknown += 1;
    (result.settings ??= []).push({ name: key, status, reason });
  }
};

// ---- environment capabilities
const envSection = () => {
  const text = read("apps/server/src/environment/ServerEnvironment.ts");
  const block = text.slice(text.indexOf("capabilities: {"), text.indexOf("serverBrowser: true,"));
  const flags = [...block.matchAll(/^\s{6}(\w+):/gm)]
    .map((m) => m[1])
    .concat(["serverBrowser", "agentActivityPublishing"]);
  const advertised = rustText["environment_capabilities.rs"].slice(
    0,
    rustText["environment_capabilities.rs"].indexOf("#[cfg(test)]"),
  );
  const connect = rustText["connect.rs"] ?? "";
  for (const flag of [...new Set(flags)])
    classify("env", flag, advertised.includes(`"${flag}"`) || connect.includes(`"${flag}"`), "");
};

if (!only || only === "rpc") rpcSection();
if (!only || only === "http") httpSection();
if (!only || only === "mcp") mcpSection();
if (!only || only === "settings") settingsSection();
if (!only || only === "env") envSection();

if (asJson) console.log(JSON.stringify(result, null, 2));
else {
  for (const [section, rows] of Object.entries(result)) {
    if (section === "httpExtra") {
      console.log(`\n== Rust-only routes (extensions of the port)\n  ${rows.join("\n  ")}`);
      continue;
    }
    const counts = {};
    for (const row of rows) counts[row.status] = (counts[row.status] ?? 0) + 1;
    console.log(`\n== ${section}: ${rows.length} ${JSON.stringify(counts)}`);
    for (const row of rows)
      if (row.status !== "handled")
        console.log(
          `  ${row.status.padEnd(8)} ${row.name}${row.reason ? `  -- ${row.reason}` : ""}`,
        );
  }
}
if (unknown > 0) {
  console.error(
    `\n${unknown} gap(s) the allowlist does not know. Port them, or add them to check_rpc_surface_allowlist.json with a reason.`,
  );
  process.exit(1);
}
