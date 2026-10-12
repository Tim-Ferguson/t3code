// Writes crates/contracts/tests/fixtures/preview.jsonl: what the original's Effect codec does with the
// preview contracts (packages/contracts/src/preview.ts and previewAutomation.ts), as cases tests/preview.rs
// checks the Rust types against, and the test's dispatch table. Development-only; the Rust tests never run TypeScript.
//
// The original is read from T3_ORACLE_ROOT when set (a checkout with node_modules installed), else from this
// checkout; fixtures and the test are written to this checkout.
import { execFileSync } from "node:child_process";
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const oracle = (process.env.T3_ORACLE_ROOT ?? root).replace(/\/$/, "");
const Schema = await import(
  pathToFileURL(oracle + "/packages/contracts/node_modules/effect/dist/Schema.js")
);
const rustFiles = ["preview_session", "preview_automation"];
const rustSource = rustFiles
  .map((f) => readFileSync(root + "/rust/crates/contracts/src/" + f + ".rs", "utf8"))
  .join("\n");
const rustNames = new Set(
  [
    ...rustSource.matchAll(
      /pub (?:struct|enum|type) (\w+)|(?:vocabulary|tagged_union)!\s*[({]\s*(?:\/\/\/[^\n]*\n\s*)*(\w+)/g,
    ),
  ].map((match) => match[1] ?? match[2]),
);
// Types the contract shares with client settings, which have their own fixtures of the same codec.
for (const shared of [
  "PreviewViewportSetting",
  "PreviewZoomFactor",
  "PreviewAppearancePreference",
  "PreviewTabId",
])
  rustNames.add(shared);
const fixtures = [];
// The text an agent reads, which the original's error classes compute: checked against `message()` in the Rust types.
const messages = [];
// Reuse the bounded mutation cases of the broad oracle without changing its dispatcher or corpus.
const generator = readFileSync(root + "/rust/tools/generate_contract_fixtures.mjs", "utf8");
const helpers = generator
  .slice(generator.indexOf("function seed("), generator.indexOf("const skipped = []"))
  .replace(
    "const all = s.anyOf ?? s.oneOf;",
    "const all = s.anyOf ?? s.oneOf; if(all.some(x=>x.properties?._tag?.enum?.includes('None')))return {_tag:'None'};",
  );
const { seed, codecCases } = new Function(
  "Schema",
  "fixtures",
  "strictObjectTypes",
  "generatingProviderSetup",
  helpers + "\nreturn {seed,codecCases};",
)(Schema, fixtures, rustNames, false);
const modules = {};
for (const file of ["preview", "previewAutomation"]) {
  modules[file] = await import(pathToFileURL(oracle + "/packages/contracts/src/" + file + ".ts"));
}
const exported = Object.assign({}, modules.preview, modules.previewAutomation);
const mapping = {};
const skipped = [];
for (const [name, schema] of Object.entries(exported)) {
  if (!rustNames.has(name) || !Schema.isSchema(schema)) continue;
  const doc = Schema.toJsonSchemaDocument(schema);
  if (codecCases(name, schema, doc, seed(doc.schema, doc.definitions))) mapping[name] = name;
  else skipped.push(name);
}
function witness(name, input, label) {
  const codec = Schema.toCodecJson(exported[name]);
  let decoded;
  try {
    decoded = Schema.decodeUnknownSync(codec)(input);
  } catch {
    fixtures.push({ schema: name, label, input, valid: false, decoded_valid: false });
    return;
  }
  if (decoded instanceof Error) messages.push({ schema: name, input, message: decoded.message });
  try {
    fixtures.push({
      schema: name,
      label,
      input,
      valid: true,
      decoded_valid: true,
      output: Schema.encodeUnknownSync(codec)(decoded),
    });
  } catch {
    fixtures.push({ schema: name, label, input, valid: false, decoded_valid: true });
  }
}
const seedOf = (name) => {
  const doc = Schema.toJsonSchemaDocument(exported[name]);
  return seed(doc.schema, doc.definitions);
};
const utf16 = (unit, count) => unit.repeat(count);

// Strings with a length bound counted in UTF-16 units, after trimming.
for (const input of [
  "tab_1",
  "",
  " ",
  " tab ",
  "﻿tab ",
  "a".repeat(128),
  "a".repeat(129),
  utf16("😀", 64),
  utf16("😀", 64) + "x",
  " " + "a".repeat(128) + " ",
  false,
  null,
  [],
])
  witness("PreviewTabId", input, "tab id boundary");

// Viewports: each side 240..3840, area at most 3840x2160, preset ids from the catalog or the legacy list.
const viewports = [
  { _tag: "fill" },
  { _tag: "fill", width: 1 },
  { _tag: "freeform", width: 240, height: 240 },
  { _tag: "freeform", width: 239, height: 240 },
  { _tag: "freeform", width: 3840, height: 2160 },
  { _tag: "freeform", width: 3840, height: 2161 },
  { _tag: "freeform", width: 3840, height: 3840 },
  { _tag: "freeform", width: 1024.5, height: 768 },
  { _tag: "freeform", width: 1024.0, height: 768 },
  { _tag: "freeform", width: "1024", height: 768 },
  { _tag: "freeform", width: 1024 },
  { _tag: "freeform", width: null, height: 768 },
  { _tag: "preset", width: 390, height: 844, presetId: "iphone-12-pro" },
  { _tag: "preset", width: 390, height: 844, presetId: "iphone-15-pro" },
  { _tag: "preset", width: 390, height: 844, presetId: "desktop-1920x1080" },
  { _tag: "preset", width: 390, height: 844, presetId: "iPhone-SE" },
  { _tag: "preset", width: 390, height: 844, presetId: "unknown" },
  { _tag: "preset", width: 390, height: 844 },
  { _tag: "preset", width: 3840, height: 2161, presetId: "ipad-pro" },
  { _tag: "Fill" },
  { _tag: "grid" },
  { width: 800, height: 600 },
  [],
  null,
];
for (const input of viewports) witness("PreviewViewportSetting", input, "viewport setting");
for (const input of [0.25, 0.33, 0.5, 1, 1.0, 1.1, 5, 5.0, 0.3, 0, -1, 6, "1", null, true, 1e0, 2.5])
  witness("PreviewZoomFactor", input, "zoom ladder");
for (const input of ["system", "light", "dark", "System", "", null, 1])
  witness("PreviewAppearancePreference", input, "appearance");

// Navigation status: titles are bounded but not trimmed, urls are trimmed and bounded.
const url = "https://example.test/";
const navCases = [
  { _tag: "Idle" },
  { _tag: "Idle", url },
  { _tag: "Loading", url, title: "" },
  { _tag: "Loading", url },
  { _tag: "Loading", url: "", title: "" },
  { _tag: "Loading", url: "  " + url + "  ", title: "  padded  " },
  { _tag: "Loading", url: "https://e.test/" + "a".repeat(2048 - 15), title: "" },
  { _tag: "Loading", url: "https://e.test/" + "a".repeat(2048 - 14), title: "" },
  { _tag: "Loading", url, title: utf16("😀", 256) },
  { _tag: "Loading", url, title: utf16("😀", 256) + "x" },
  { _tag: "Success", url, title: "ok" },
  { _tag: "LoadFailed", url, title: "t", code: -105, description: "net::ERR_NAME_NOT_RESOLVED" },
  { _tag: "LoadFailed", url, title: "t", code: 1.5, description: "" },
  { _tag: "LoadFailed", url, title: "t", code: 9007199254740992, description: "" },
  { _tag: "LoadFailed", url, title: "t", code: "1", description: "" },
  { _tag: "LoadFailed", url, title: "t", code: 1 },
  { _tag: "Unknown" },
  { title: "x" },
  "Idle",
  [],
];
for (const input of navCases) witness("PreviewNavStatus", input, "nav status");

// Snapshots: every optional member, nulls for them, and the reveal request.
const snapshot = {
  threadId: "thread-1",
  tabId: "tab_1",
  navStatus: { _tag: "Idle" },
  canGoBack: false,
  canGoForward: true,
  updatedAt: "2026-10-11T00:00:00.000Z",
};
const full = {
  ...snapshot,
  viewport: { _tag: "fill" },
  colorScheme: "dark",
  zoomFactor: 1.25,
  profileId: "default",
  runtime: "server",
  automationOwner: "env\u0000session",
  reveal: true,
  revealRequest: { id: "r1", force: false },
};
witness("PreviewSessionSnapshot", snapshot, "snapshot minimal");
witness("PreviewSessionSnapshot", full, "snapshot full");
for (const [key, value] of [
  ["viewport", null],
  ["colorScheme", null],
  ["zoomFactor", null],
  ["profileId", null],
  ["runtime", null],
  ["automationOwner", null],
  ["reveal", null],
  ["revealRequest", null],
  ["revealRequest", { id: "r1" }],
  ["revealRequest", { id: "r1", force: null }],
  ["runtime", "desktop"],
  ["runtime", "browser"],
  ["profileId", " work "],
  ["profileId", ""],
  ["profileId", "a".repeat(65)],
  ["profileId", "a\u0001b"],
  ["threadId", " "],
  ["tabId", "a".repeat(129)],
  ["updatedAt", 5],
])
  witness("PreviewSessionSnapshot", { ...full, [key]: value }, `snapshot ${key}`);

// Inputs.
const thread = "thread-1";
for (const [name, inputs] of Object.entries({
  PreviewOpenInput: [
    { threadId: thread },
    { threadId: thread, url: " localhost:5173 ", runtime: "server", reveal: false },
    { threadId: thread, url: "" },
    { threadId: thread, url: null },
    { threadId: thread, viewport: { _tag: "freeform", width: 800, height: 600 }, profileId: "work" },
    { threadId: thread, viewport: { _tag: "fill", extra: 1 } },
    { threadId: "", runtime: "desktop" },
    { threadId: " t ", runtime: "mobile" },
    { runtime: "server" },
  ],
  PreviewNavigateInput: [
    { threadId: thread, tabId: "tab_1", url },
    { threadId: thread, tabId: "tab_1", url, resolvedTitle: "x".repeat(512) },
    { threadId: thread, tabId: "tab_1", url, resolvedTitle: "x".repeat(513) },
    { threadId: thread, tabId: "tab_1", url, resolvedTitle: null },
    { threadId: thread, tabId: "tab_1", url: "   " },
    { threadId: thread, tabId: "tab_1" },
  ],
  PreviewReportStatusInput: [
    { threadId: thread, tabId: "tab_1", navStatus: { _tag: "Idle" }, canGoBack: true, canGoForward: false },
    { threadId: thread, tabId: "tab_1", navStatus: { _tag: "Idle" }, canGoBack: 1, canGoForward: false },
    { threadId: thread, tabId: "tab_1", navStatus: { _tag: "Gone" }, canGoBack: true, canGoForward: false },
    { threadId: thread, tabId: "tab_1", navStatus: { _tag: "Idle" }, canGoBack: true },
  ],
  PreviewRefreshInput: [{ threadId: thread, tabId: "tab_1" }, { threadId: thread }],
  PreviewResizeInput: [
    { threadId: thread, tabId: "tab_1", viewport: { _tag: "fill" } },
    { threadId: thread, tabId: "tab_1", viewport: { _tag: "freeform", width: 100, height: 100 } },
    { threadId: thread, tabId: "tab_1" },
  ],
  PreviewAdjustInput: [
    { threadId: thread, tabId: "tab_1" },
    { threadId: thread, tabId: "tab_1", colorScheme: "light", zoomFactor: 0.5, hardReload: true, clear: "cookies" },
    { threadId: thread, tabId: "tab_1", clear: "storage" },
    { threadId: thread, tabId: "tab_1", zoomFactor: 1.3 },
    { threadId: thread, tabId: "tab_1", hardReload: null, clear: null },
  ],
  PreviewCloseInput: [
    { threadId: thread },
    { threadId: thread, tabId: "tab_1" },
    { threadId: thread, tabId: "" },
    { threadId: thread, tabId: null },
  ],
  PreviewClearProfileInput: [{ profileId: "work" }, { profileId: "" }, { profileId: "a\u0000b" }, {}],
  PreviewListInput: [{ threadId: thread }, { threadId: "" }, {}],
  PreviewListResult: [
    { sessions: [], serverEpoch: "e", revision: 0 },
    { sessions: [full], serverEpoch: "e", revision: 7 },
    { sessions: [], serverEpoch: " ", revision: 0 },
    { sessions: [], serverEpoch: "e", revision: -1 },
    { sessions: [null], serverEpoch: "e", revision: 0 },
    { sessions: [], serverEpoch: "e" },
  ],
}))
  for (const input of inputs) witness(name, input, "input shape");

// Events: each member with its own fields, the revision floor, and a one-off request riding on a resize.
const base = { threadId: thread, tabId: "tab_1", createdAt: "x", serverEpoch: "e", revision: 1 };
for (const input of [
  { ...base, type: "opened", snapshot },
  { ...base, type: "navigated", snapshot: full },
  { ...base, type: "resized", snapshot },
  { ...base, type: "resized", snapshot, request: { hardReload: true, clear: "cache" } },
  { ...base, type: "resized", snapshot, request: {} },
  { ...base, type: "resized", snapshot, request: { clear: "all" } },
  { ...base, type: "failed", url, title: "t", code: -2, description: "d" },
  { ...base, type: "failed", url, title: "t", code: 1.5, description: "d" },
  { ...base, type: "closed" },
  { ...base, type: "closed", revision: 0 },
  { ...base, type: "closed", revision: 1.5 },
  { ...base, type: "closed", serverEpoch: "" },
  { ...base, type: "paused" },
  { ...base, snapshot },
  { type: "closed" },
])
  witness("PreviewEvent", input, "event");

// Errors, as they travel in an RPC failure.
for (const [name, inputs] of Object.entries({
  PreviewSessionLookupError: [
    { _tag: "PreviewSessionLookupError", threadId: "t", tabId: "b" },
    { _tag: "PreviewSessionLookupError", threadId: "t" },
    { threadId: "t", tabId: "b" },
  ],
  PreviewInvalidUrlError: [
    { _tag: "PreviewInvalidUrlError", inputLength: 3, reason: "parse", protocol: "ftp:", cause: {} },
    { _tag: "PreviewInvalidUrlError", inputLength: 3, reason: "empty", cause: { name: "E", message: "m" } },
    { _tag: "PreviewInvalidUrlError", inputLength: 3, reason: "other", cause: {} },
    { _tag: "PreviewInvalidUrlError", inputLength: 3, reason: "parse" },
  ],
  PreviewControlRequiredError: [
    { _tag: "PreviewControlRequiredError", tabId: "b" },
    { _tag: "PreviewControlRequiredError" },
  ],
  PreviewClearProfileError: [
    { _tag: "PreviewClearProfileError", profileId: "p", cause: { name: "E", message: "m" } },
    { _tag: "PreviewClearProfileError", profileId: "p" },
  ],
  PreviewError: [
    { _tag: "PreviewSessionLookupError", threadId: "t", tabId: "b" },
    { _tag: "PreviewClearProfileError", profileId: "p", cause: {} },
    { _tag: "Other" },
  ],
}))
  for (const input of inputs) witness(name, input, "error shape");

// Automation: operations, the host handshake, requests and responses.
for (const input of [
  "status",
  "waitFor",
  "recordingStop",
  "setColorScheme",
  "dialog",
  "close",
  "drag",
  "Status",
  "evaluate ",
  "",
  null,
])
  witness("PreviewAutomationOperation", input, "operation");
const identity = { clientId: "server-browser", environmentId: "env-1" };
for (const input of [
  identity,
  { ...identity, supportedOperations: ["status", "open"] },
  { ...identity, supportedOperations: [] },
  { ...identity, supportedOperations: ["status", "fly"] },
  { ...identity, supportedOperations: null },
  { clientId: "a".repeat(128), environmentId: "env-1" },
  { clientId: "a".repeat(129), environmentId: "env-1" },
  { clientId: " c ", environmentId: " e " },
  { clientId: "", environmentId: "env-1" },
])
  witness("PreviewAutomationHost", input, "host");
const focus = { ...identity, connectionId: "c1", focused: true };
for (const input of [
  focus,
  { ...focus, liveTabs: [] },
  { ...focus, liveTabs: [{ threadId: thread, tabId: "tab_1" }] },
  { ...focus, liveTabs: [{ threadId: thread, tabId: "tab_1", visible: false }] },
  { ...focus, liveTabs: [{ threadId: thread }] },
  { ...focus, connectionId: "c".repeat(64) },
  { ...focus, connectionId: "c".repeat(65) },
])
  witness("PreviewAutomationHostFocus", input, "host focus");
const request = {
  requestId: "preview-0",
  threadId: thread,
  operation: "click",
  input: { selector: "button" },
  timeoutMs: 15000,
};
for (const input of [
  request,
  { ...request, input: null },
  { ...request, input: undefined },
  { ...request, tabId: "tab_1", tabIdExplicit: true, agentSessionId: "env\u0000session" },
  { ...request, tabIdExplicit: null },
  { ...request, timeoutMs: 0 },
  { ...request, timeoutMs: 1.5 },
  { ...request, operation: "fly" },
])
  witness("PreviewAutomationRequest", input, "request");
const connection = { clientId: "server-browser", connectionId: "c1", requestId: "preview-0" };
for (const input of [
  { ...connection, ok: true },
  { ...connection, ok: true, result: null },
  { ...connection, ok: true, result: { tabId: "tab_1" } },
  { ...connection, ok: false, error: { _tag: "PreviewAutomationTimeoutError", message: "m" } },
  { ...connection, ok: false, error: { _tag: "X", message: "m", detail: null } },
  { ...connection, ok: false, error: { _tag: "X", message: "m", detail: { staleRef: true } } },
  { ...connection, ok: false, error: { _tag: " ", message: "m" } },
  { ...connection, ok: false, error: { _tag: "X" } },
  { ...connection, ok: false },
  { ...connection, ok: "yes" },
])
  witness("PreviewAutomationResponse", input, "response");
for (const input of [
  { type: "connected", connectionId: "c1" },
  { type: "request", connectionId: "c1", request },
  { type: "request", connectionId: "c1" },
  { type: "other", connectionId: "c1" },
])
  witness("PreviewAutomationStreamEvent", input, "stream event");
const status = {
  available: true,
  visible: false,
  tabId: null,
  url: null,
  title: null,
  loading: false,
};
for (const input of [
  status,
  { ...status, tabId: "tab_1", url: "u", title: "t" },
  { ...status, tabId: undefined },
  { ...status, control: { owner: "agent", ownedByCaller: true, generation: 3 } },
  { ...status, control: { owner: "robot", ownedByCaller: true, generation: 3 } },
  { ...status, dialog: null },
  { ...status, dialog: { type: "alert", message: "m", defaultValue: "" } },
  { ...status, dialog: { type: "alert", message: "m" } },
  { ...status, viewportSetting: { _tag: "fill" }, viewport: { width: 100, height: 50 } },
  { ...status, viewport: { width: 0, height: 50 } },
  { ...status, viewport: { width: 100.5, height: 50 } },
  { ...status, fileChooser: null },
  { ...status, fileChooser: { multiple: false, accept: "" } },
  { ...status, downloads: [{ fileName: "a", path: "/p", sizeBytes: 1, url: "u", completedAt: "t" }] },
  { ...status, downloads: [{ fileName: "a" }] },
  { ...status, tabs: [{ tabId: "tab_1", url: null }, { tabId: "tab_2", url: "u", openerTabId: "tab_1" }] },
  { ...status, tabs: [{ tabId: "tab_1" }] },
  { ...status, tabs: [{ tabId: "", url: null }] },
])
  witness("PreviewAutomationStatus", input, "status");
for (const input of [
  { tabId: "tab_1" },
  {},
  { tabId: null },
  { tabId: "" },
  { tabId: "a".repeat(129) },
])
  witness("PreviewAutomationTabTargetInput", input, "tab target");
const scope = {
  operation: "click",
  environmentId: "env-1",
  threadId: thread,
  providerSessionId: "ps-1",
  providerInstanceId: "codex",
};
const requestFields = { ...scope, clientId: "server-browser", connectionId: "c1", requestId: "preview-0", timeoutMs: 15000 };
const remote = { remoteTag: "X", remoteMessageLength: 3, cause: { _tag: "X", message: "m" } };
for (const [name, inputs] of Object.entries({
  PreviewAutomationNoAvailableHostError: [scope, { ...scope, clientId: "c", timeoutMs: 5, remoteTag: "X", cause: {} }, { ...scope, timeoutMs: 0 }],
  PreviewAutomationUnsupportedClientError: [{ ...requestFields, ...remote }, requestFields],
  PreviewAutomationTabNotFoundError: [{ ...requestFields, ...remote, tabId: "tab_1" }, { ...requestFields, ...remote }],
  PreviewAutomationTimeoutError: [requestFields, { ...requestFields, ...remote, remoteDetailKind: "array" }, { ...requestFields, remoteDetailKind: "map" }],
  PreviewAutomationControlInterruptedError: [{ ...requestFields, ...remote, reason: "tabLimit" }, { ...requestFields, ...remote }, { ...requestFields, ...remote, reason: "other" }],
  PreviewAutomationExecutionError: [{ ...requestFields, ...remote, reason: "net::ERR" }, { ...requestFields, ...remote }, { ...requestFields, ...remote, remoteMessageLength: -1 }],
  PreviewAutomationInvalidSelectorError: [{ ...requestFields, ...remote, selectorKind: "locator", selectorLength: 4, staleRef: true }, { ...requestFields, ...remote }, { ...requestFields, ...remote, selectorKind: "css" }],
  PreviewAutomationTargetNotEditableError: [{ ...requestFields, ...remote, selectorKind: "focused-element", selectorLength: 0 }, { ...requestFields, ...remote, selectorKind: "locator" }, { ...requestFields, ...remote, selectorKind: "element" }],
  PreviewAutomationResultTooLargeError: [{ ...requestFields, ...remote, maximumBytes: 10 }, { ...requestFields, ...remote, maximumBytes: 0 }, { ...requestFields, ...remote }],
  PreviewAutomationClientDisconnectedError: [requestFields, { ...requestFields, extra: 1 }, scope],
  PreviewAutomationRequestQueueClosedError: [requestFields, scope],
  PreviewAutomationRemoteUnavailableError: [{ ...requestFields, ...remote }, requestFields],
  PreviewAutomationMalformedResponseError: [requestFields, scope],
  PreviewAutomationRecordingTransferError: [{ threadId: thread }, { threadId: thread, cause: { name: "E", message: "m" } }, {}],
  PreviewAutomationRecordingTooLargeError: [{ threadId: thread }],
  PreviewAutomationUnavailableError: [
    { capability: "preview", environmentId: "env-1" },
    { capability: "preview", environmentId: "env-1", threadId: thread, providerSessionId: "ps", providerInstanceId: "codex" },
    { capability: "files", environmentId: "env-1" },
  ],
}))
  for (const input of inputs)
    witness(name, name === "PreviewAutomationRecordingTransferError" || name === "PreviewAutomationRecordingTooLargeError" || name === "PreviewAutomationUnavailableError" ? { _tag: name, ...input } : { _tag: name, ...input }, "automation error");
for (const input of [
  { _tag: "PreviewAutomationTimeoutError", ...requestFields },
  { _tag: "PreviewAutomationClientDisconnectedError", ...requestFields },
  { _tag: "PreviewAutomationExecutionError", ...requestFields, ...remote },
  { _tag: "PreviewAutomationRecordingDeadlineExpiredError", threadId: thread },
  { _tag: "PreviewAutomationUnknownError", ...requestFields },
  { ...requestFields },
])
  witness("PreviewAutomationError", input, "automation error union");
for (const input of [
  { requestedUrl: "a", resolvedUrl: "b", resolutionKind: "direct", environmentId: "env-1" },
  { requestedUrl: "a", resolvedUrl: "b", resolutionKind: "direct-private-network", environmentId: "env-1" },
  { requestedUrl: "a", resolvedUrl: "b", resolutionKind: "proxy", environmentId: "env-1" },
])
  witness("PreviewUrlResolution", input, "url resolution");

const unique = [...new Map(fixtures.map((f) => [JSON.stringify([f.schema, f.input]), f])).values()];
const fixturePath = root + "/rust/crates/contracts/tests/fixtures/preview.jsonl";
writeFileSync(fixturePath + ".tmp", unique.map((f) => JSON.stringify(f)).join("\n") + "\n");
renameSync(fixturePath + ".tmp", fixturePath);
const messagesPath = root + "/rust/crates/contracts/tests/fixtures/preview-messages.jsonl";
writeFileSync(
  messagesPath,
  [...new Map(messages.map((m) => [JSON.stringify([m.schema, m.input]), m])).values()]
    .map((m) => JSON.stringify(m))
    .join("\n") + "\n",
);
const names = [...new Set(unique.map((f) => f.schema))].sort();
const testPath = root + "/rust/crates/contracts/tests/preview.rs";
let test = readFileSync(testPath, "utf8");
const begin = test.indexOf("// BEGIN DISPATCH") + "// BEGIN DISPATCH".length;
const end = test.indexOf("// END DISPATCH", begin);
if (begin < "// BEGIN DISPATCH".length || end < 0) throw Error("Missing dispatcher markers");
test =
  test.slice(0, begin) +
  "\n" +
  names.map((name) => `"${name}"=>roundtrip::<${name}>(fixture.input.clone()),`).join("\n") +
  "\n" +
  test.slice(end);
writeFileSync(testPath, test);
execFileSync("rustfmt", ["--edition", "2024", testPath]);
console.log(JSON.stringify({ cases: unique.length, schemas: names.length, skipped }));
