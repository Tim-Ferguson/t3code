// Development oracle for MCP Apps documents: the origins an app declares, the policy built from
// them, its injection into the stored document, and the tool-visibility rule. The Rust helpers
// replay every row. Run: node rust/tools/generate_mcp_app_fixtures.mjs
import { renameSync, writeFileSync } from "node:fs";
import {
  injectMcpAppCsp,
  mcpAppContentSecurityPolicy,
  mcpAppToolCallableByApp,
  readMcpAppCsp,
  readMcpAppPermissions,
} from "../../packages/shared/src/mcpApp.ts";

const csps = [
  undefined,
  null,
  [],
  {},
  {
    connectDomains: [
      "https://api.example.com",
      "wss://live.example.com:8443",
      "http://localhost:3000",
      "*.example.com",
      "https://*.cdn.example.com",
      "https://bad host",
      "ftp://x.test",
      5,
    ],
  },
  {
    resourceDomains: [
      "https://cdn.example.com",
      "javascript:alert(1)",
      "https://a.b.c.d.example.com:1",
      "https://-x.test",
      "HTTPS://UPPER.EXAMPLE.COM",
    ],
    frameDomains: ["https://embed.example.com"],
    baseUriDomains: ["https://base.example.com"],
  },
  { connectDomains: "https://not-an-array.test", resourceDomains: [] },
  { connectDomains: Array.from({ length: 40 }, (_, index) => `https://host${index}.example.com`) },
  { connectDomains: [`https://${"a".repeat(260)}.com`, "https://ok.example.com"] },
  {
    frameDomains: ['https://x.test"; script-src *'],
    resourceDomains: ["https://y.test'><script>"],
  },
];
const documents = [
  "<html><body>x</body></html>",
  "<!DOCTYPE html><html></html>",
  "﻿  <!doctype html>\n<p>",
  '<!doctype html public "x">rest',
  "",
  'no doctype & <b>"quoted"</b>',
];
const permissions = [
  undefined,
  null,
  {},
  { camera: {}, microphone: {}, geolocation: {}, clipboardWrite: {} },
  { camera: true, microphone: {}, other: {} },
  { geolocation: [] },
];
const metas = [
  undefined,
  null,
  {},
  { ui: {} },
  { ui: { visibility: ["model"] } },
  { ui: { visibility: ["app"] } },
  { ui: { visibility: ["model", "app"] } },
  { ui: { visibility: [] } },
  { ui: { visibility: "app" } },
  { ui: { visibility: "model" } },
  { ui: null },
  "text",
  5,
];
const rows = {
  csp: csps.map((input) => ({
    input: input ?? null,
    undefinedInput: input === undefined,
    read: readMcpAppCsp(input) ?? null,
    policy: mcpAppContentSecurityPolicy(readMcpAppCsp(input)),
  })),
  injection: csps.flatMap((input) =>
    documents.map((html) => ({
      csp: input ?? null,
      html,
      injected: injectMcpAppCsp(html, readMcpAppCsp(input)),
    })),
  ),
  permissions: permissions.map((input) => ({
    input: input ?? null,
    read: readMcpAppPermissions(input) ?? null,
  })),
  callable: metas.map((meta) => ({ meta: meta ?? null, callable: mcpAppToolCallableByApp(meta) })),
};
const target = new URL("../crates/server/tests/fixtures/mcp-app-documents.json", import.meta.url);
writeFileSync(new URL(target.href + ".tmp"), JSON.stringify(rows, null, 1));
renameSync(new URL(target.href + ".tmp"), target);
console.log(
  JSON.stringify(
    Object.fromEntries(Object.entries(rows).map(([key, value]) => [key, value.length])),
  ),
);
