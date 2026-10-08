// Development-only reference generator. Requires the original Node dependencies.
// Rust runtime and Rust tests consume the checked-in JSON and do not execute TS.
// From repository root, after installing original dependencies:
//   node --version  # requires Node >=24.13.1 with built-in TypeScript stripping
//   node rust/tools/generate_contract_fixtures.mjs
//   cargo fmt --manifest-path rust/Cargo.toml --all
// No separate TypeScript loader is required; original relative imports use .ts.
import { fileURLToPath, pathToFileURL } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const Schema = await import(
  pathToFileURL(root + "/packages/contracts/node_modules/effect/dist/Schema.js")
);
import { readFileSync, writeFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
const fixtures = [],
  mapping = {};
let generatingProviderSetup = false;
const files = [
  "auth",
  "providerInstance",
  "model",
  "threadPullRequest",
  "orchestrationV2",
  "provider",
  "environment",
  "server",
  "providerUsageLimits",
  "acpRegistry",
  "providerSetup",
  "settings",
  "device",
  "project",
  "keybindings",
  "editor",
  "chatAttachment",
  "composerContext",
  "providerRuntime",
  "providerPolicy",
  "filesystem",
  "terminal",
];
const rustNames = new Set(
  [
    "auth",
    "provider",
    "thread_command",
    "orchestration",
    "server_config",
    "settings",
    "api_config",
    "messages",
    "execution",
    "turn_items",
    "history",
    "filesystem",
    "terminal",
    "provider_runtime",
    "preview",
    "acp_registry",
    "provider_setup",
  ].flatMap((f) =>
    [
      ...readFileSync(root + "/rust/crates/contracts/src/" + f + ".rs", "utf8").matchAll(
        /pub (?:struct|enum|type) (\w+)/g,
      ),
    ].map((m) => m[1]),
  ),
);
const strictObjectTypes = new Set(
  [
    "history",
    "filesystem",
    "terminal",
    "provider_runtime",
    "preview",
    "acp_registry",
    "provider_setup",
  ].flatMap((file) =>
    [
      ...readFileSync(root + "/rust/crates/contracts/src/" + file + ".rs", "utf8").matchAll(
        /pub (?:struct|enum|type) (\w+)/g,
      ),
    ].map((m) => m[1]),
  ),
);
strictObjectTypes.add("AcpRegistryUrlAuthAction");
function seed(s, defs, key = "", depth = 0) {
  if (!s) return {};
  if (depth > 20) return null;
  if (s.$ref) return seed(defs[s.$ref.split("/").pop()], defs, key, depth + 1);
  if (/ModelSelection$/.test(key) || key === "modelSelection")
    return { instanceId: "codex", model: "gpt-6-astra" };
  if (key === "mimeType") return "image/png";
  if (key === "clientId" && generatingProviderSetup) return "oaiapp_fixture";
  if (key === "canvas" || key === "accent") return "#123abc";
  if (key === "autoCompactWindow") return "300000";
  if (s.const !== undefined) return s.const;
  if (s.enum) return s.enum[0];
  if (s.anyOf || s.oneOf) {
    const all = s.anyOf ?? s.oneOf;
    return seed(all.find((x) => x.type !== "null") ?? all[0], defs, key, depth + 1);
  }
  if (s.allOf && !s.type) return seed(s.allOf[0], defs, key, depth + 1);
  if (s.type === "object") {
    const o = {};
    for (const p of s.required ?? []) o[p] = seed(s.properties[p], defs, p, depth + 1);
    return o;
  }
  if (s.type === "array") return [];
  if (s.type === "boolean") return false;
  if (s.type === "number" || s.type === "integer") return Math.max(1, s.minimum ?? 0);
  if (key === "modelSelection") return { instanceId: "codex", model: "gpt-6-astra" };
  if (key === "providerInstanceHistory") return [];
  if (s.type === "string") {
    if (/At$|Until$/.test(key)) return "2026-10-07T13:04:05Z";
    return "sample";
  }
  return {};
}
function codecCases(name, schema, schemaDoc, initial) {
  const codec = Schema.toCodecJson(schema),
    clone = (x) => JSON.parse(JSON.stringify(x));
  const test = (input, label) => {
    let decoded;
    try {
      decoded = Schema.decodeUnknownSync(codec)(input);
    } catch {
      fixtures.push({ schema: name, label, input, valid: false, decoded_valid: false });
      return false;
    }
    try {
      const output = Schema.encodeUnknownSync(codec)(decoded);
      fixtures.push({ schema: name, label, input, valid: true, decoded_valid: true, output });
      return true;
    } catch {
      fixtures.push({ schema: name, label, input, valid: false, decoded_valid: true });
      return false;
    }
  };
  if (!test(initial, "baseline")) {
    fixtures.pop();
    return false;
  }
  if (strictObjectTypes.has(name) && schemaDoc.schema.type === "object") {
    test([], "object rejects empty array");
    test(Object.values(initial), "object rejects positional array");
  }
  test({ ...initial, ignoredFutureField: { hello: true } }, "unknown field");
  if (schemaDoc.schema.type === "array") {
    const member = seed(schemaDoc.schema.items, schemaDoc.definitions);
    test([member], "array member");
    test([member, null, { future: true }], "mixed array");
  }
  const variants = schemaDoc.schema.anyOf ?? [schemaDoc.schema];
  for (const variant of variants) {
    const candidate = seed(variant, schemaDoc.definitions);
    test(candidate, "union variant");
    for (const [key, s] of Object.entries(variant.properties ?? {})) {
      const missing = clone(candidate);
      delete missing[key];
      test(missing, "missing " + key);
      for (const [label, v] of [
        ["null", null],
        ["number", 42],
        ["value", seed(s, schemaDoc.definitions, key)],
        ["empty", ""],
        ["boolean", false],
      ])
        test({ ...clone(candidate), [key]: v }, label + " " + key);
      const present = seed(s, schemaDoc.definitions, key);
      if (s.type === "string")
        test({ ...clone(candidate), [key]: " sample " }, "trim boundary " + key);
      if (s.type === "array") {
        const member = seed(s.items, schemaDoc.definitions, key);
        test({ ...clone(candidate), [key]: [member] }, "array member " + key);
        test(
          { ...clone(candidate), [key]: [member, null, { future: true }] },
          "mixed array " + key,
        );
      }
      if (typeof present === "number")
        for (const value of [-1, 0, 1.5, 9007199254740991, 9007199254740992])
          test({ ...clone(candidate), [key]: value }, "numeric boundary " + key);
      if (/At$|Until$/.test(key))
        for (const value of [
          "2026-10-07",
          "2026-10-07T13:04:05.123456Z",
          "2026-10-07T15:04:05+02:00",
          "not-a-date",
        ])
          test({ ...clone(candidate), [key]: value }, "date boundary " + key);
      if (present && typeof present === "object" && !Array.isArray(present)) {
        for (const nested of Object.keys(present)) {
          for (const value of [null, 42, ""])
            test(
              { ...clone(candidate), [key]: { ...clone(present), [nested]: value } },
              "nested " + key + "." + nested,
            );
          const missing = clone(present);
          delete missing[nested];
          test({ ...clone(candidate), [key]: missing }, "nested missing " + key + "." + nested);
        }
      }
    }
  }
  if (name === "ThreadProjection") {
    const unknown = { type: "future_item", opaque: true };
    test({ ...clone(initial), turnItems: [unknown] }, "unknown timeline type omitted");
    test({ ...clone(initial), turnItems: [{ type: 42 }] }, "malformed timeline tag rejected");
    test(
      { ...clone(initial), turnItems: [{ type: "assistant_message" }] },
      "malformed known timeline member rejected",
    );
    test(
      {
        ...clone(initial),
        visibleTurnItems: [
          {
            position: 1,
            visibility: "local",
            sourceThreadId: "thread",
            sourceItemId: "item",
            item: unknown,
          },
        ],
      },
      "unknown projected timeline type omitted",
    );
    test(
      {
        ...clone(initial),
        visibleTurnItems: [
          {
            position: -1,
            visibility: "local",
            sourceThreadId: "thread",
            sourceItemId: "item",
            item: unknown,
          },
        ],
      },
      "invalid projection envelope still fails for unknown item",
    );
  }
  if (["TerminalOpenInput", "TerminalAttachInput", "TerminalRestartInput"].includes(name)) {
    const keep128 = Object.fromEntries(Array.from({ length: 128 }, (_, i) => ["KEY_" + i, ""]));
    for (const env of [
      { "INVALID-NAME": 42 },
      { " NAME": "x" },
      { ["N".repeat(129)]: false },
      { KEY: "😀".repeat(4096) },
      { KEY: "😀".repeat(4097) },
      keep128,
      { ...keep128, KEY_OVERFLOW: "" },
      { ...keep128, "INVALID-NAME": 42 },
      Object.fromEntries(Array.from({ length: 129 }, (_, i) => ["INVALID-" + i, 42])),
    ])
      test({ ...clone(initial), env }, "terminal env filtering/value/count boundary");
  }
  if (name === "TerminalWriteInput") {
    for (const data of ["", "\r\n\u001b[31m😀", "😀".repeat(32768), "😀".repeat(32769)])
      test({ ...clone(initial), data }, "raw terminal write UTF16 boundary");
  }
  if (name === "ProjectSearchContentsInput") {
    for (const query of ["", "  ", " foo ", "😀".repeat(128), "😀".repeat(129)])
      test({ ...clone(initial), query }, "untrimmed content query UTF16 boundary");
  }
  if (name === "RuntimeEventRawSource") {
    for (const source of [
      "acp..extension",
      "acp.custom-driver.extension",
      "acp.future/命名.extension",
      "acp.\n.extension",
      "acp.extension",
      "acp.custom.extension.extra",
      " codex.eventmsg ",
      "future.notification",
    ])
      test(source, "ACP template delimiters and extension compatibility");
  }
  if (name === "ThreadHistoryPage") {
    const row = {
      position: 1,
      visibility: "local",
      sourceThreadId: "thread",
      sourceItemId: "item",
      item: { type: "future_item" },
    };
    test({ ...clone(initial), items: [row] }, "unknown projected timeline type omitted");
    test(
      { ...clone(initial), items: [{ ...row, position: -1 }] },
      "unknown item retains strict envelope validation",
    );
    test(
      { ...clone(initial), items: [{ ...row, item: { type: "assistant_message" } }] },
      "known malformed item rejects page",
    );
    test(
      { ...clone(initial), items: [{ ...row, item: { type: 42 } }] },
      "non-string item tag rejected",
    );
    test(
      { ...clone(initial), items: [[1, "local", "thread", "item", { type: "future_item" }]] },
      "projected envelope rejects positional array",
    );
  }
  if (name === "ThreadStreamItem") {
    test(
      { kind: "event", sequence: 1, event: { type: "future.event" } },
      "future event is decode only",
    );
    test(
      { kind: "event", sequence: 9007199254740992, event: { type: "future.event" } },
      "future event rejects unsafe cursor",
    );
    test(
      {
        kind: "event",
        sequence: 1,
        event: { type: "turn-item.updated", payload: { type: "future_item" } },
      },
      "future timeline event is decode only",
    );
    test(
      { kind: "event", sequence: 1, event: { type: "turn-item.updated", payload: { type: 42 } } },
      "unknown non-string timeline tag follows source envelope fallback",
    );
  }
  if (name === "OrchestrationMessageContext") {
    const record = {
      version: 1,
      contextId: "id",
      label: "image",
      kind: "image",
      attachmentId: "attachment",
      name: "image.png",
      mimeType: "image/png",
      sizeBytes: 1,
    };
    test(
      { version: 1, records: [record, { ...record, contextId: "other", attachmentId: "other" }] },
      "distinct composer records",
    );
    test({ version: 1, records: [record, record] }, "duplicate composer record identity rejected");
    test(
      { version: 1, records: [record, { kind: "terminal", contextId: "broken" }] },
      "malformed composer members dropped",
    );
    test(
      { version: 1, records: Array(201).fill({ future: true }) },
      "raw composer count checked before filtering",
    );
  }
  if (name === "AcpRegistrySetProviderInput") {
    for (const headers of [
      { "": "x" },
      { "   ": "x" },
      { ["x".repeat(129)]: "x" },
      { " Authorization ": "x" },
      { "": 42 },
      { "   ": 42 },
      { valid: 42 },
      { " a ": "first", a: "second" },
      { a: "first", " a ": "second" },
      { "X-Boundary": "x".repeat(8192) },
      { "X-Boundary": "x".repeat(8193) },
      { "X-Unicode": "😀".repeat(4096) },
      { "X-Unicode": "😀".repeat(4097) },
    ])
      test(
        { ...clone(initial), headers },
        "transformed/bounded header record " + JSON.stringify(headers).slice(0, 100),
      );
  }
  if (name === "ProviderAuthInteraction") {
    for (const input of [
      {
        type: "browser",
        id: "browser",
        url: " https://example.test/auth ",
        requiresConsent: true,
        acceptsCallback: false,
      },
      {
        type: "deviceCode",
        id: "code",
        url: "https://example.test/auth",
        userCode: " sample-code ",
      },
      { type: "terminal", id: "terminal", output: " raw\n", outputOffset: 1.0 },
      {
        type: "credentials",
        id: "credentials",
        fields: [{ name: " token ", label: " API token ", secret: true }],
      },
      { type: "terminal", id: "terminal", output: "", outputOffset: 9007199254740992 },
    ])
      test(input, "auth interaction variant boundary");
  }
  if (name === "ProviderAuthResponse") {
    for (const values of [
      { "": "x" },
      { " a ": "first", a: "last" },
      { a: "first", " a ": "last" },
      Object.fromEntries(Array.from({ length: 17 }, (_, i) => ["name" + i, "x"])),
      Object.fromEntries(Array.from({ length: 17 }, (_, i) => [" ".repeat(i) + "same", "x"])),
    ])
      test(
        { type: "credentials", values },
        "credential record admission and transformed cardinality",
      );
    for (const size of [
      undefined,
      null,
      { cols: 1, rows: 200 },
      { cols: 500, rows: 1 },
      { cols: 501, rows: 1 },
      { cols: 1.5, rows: 2 },
      { cols: 1, rows: 201 },
    ])
      test(
        { type: "terminal", data: " ", ...(size === undefined ? {} : { size }) },
        "terminal dimensions optional and boundary",
      );
  }
  if (name === "ProviderAuthState") {
    for (const interaction of [
      undefined,
      null,
      { type: "future", id: "future" },
      { type: "browser", id: "x" },
    ])
      test(
        { ...clone(initial), ...(interaction === undefined ? {} : { interaction }) },
        "forward authentication interaction",
      );
    for (const methods of [
      null,
      [{ id: "x", name: "x", description: null, type: "future" }],
      Array(33).fill({ id: "x", name: "x", description: null, type: "agent" }),
      Array(33).fill({ type: "future" }),
    ])
      test({ ...clone(initial), methods }, "authentication methods filtered then bounded");
  }
  if (name === "ChatGptReconnectProfile") {
    for (const clientId of [
      "oaiapp_a",
      "oaiapp_é",
      "oaiapp_x\n",
      "oaiapp_x\r\n",
      "oaiapp_x\n\n",
      "oaiapp_x\u2028",
      "oaiapp_x-",
    ])
      test({ ...clone(initial), clientId }, "registration ID exact source regex");
    for (const redirectUri of [
      "http://127.0.0.1:1/auth/callback",
      "http://localhost:99999/auth/callback",
      "http://localhost:01/auth/callback",
      "http://localhost:1/auth/callback\n",
      "http://localhost:1/auth/callback\r\n",
      "https://localhost:1/auth/callback",
    ])
      test({ ...clone(initial), redirectUri }, "registration redirect exact source regex");
  }
  return true;
}
const skipped = [];
for (const file of files) {
  generatingProviderSetup = file === "providerSetup";
  const mod = await import(pathToFileURL(root + "/packages/contracts/src/" + file + ".ts"));
  for (const [name, s] of Object.entries(mod)) {
    if (!rustNames.has(name) || !Schema.isSchema(s)) continue;
    const doc = Schema.toJsonSchemaDocument(s);
    let initial = seed(doc.schema, doc.definitions);
    if (name === "LimitRecoveryUpdate")
      initial = { runId: "run", resetAt: "time", autoResume: false };
    if (codecCases(name, s, doc, initial)) mapping[name] = name;
    else skipped.push(file + "." + name);
  }
}
generatingProviderSetup = false;
const orch = await import(pathToFileURL(root + "/packages/contracts/src/orchestrationV2.ts"));
const ipc = await import(pathToFileURL(root + "/packages/contracts/src/IPC.ts"));
for (const [schemaName, schema] of Object.entries(ipc)) {
  let name = schemaName.replace(/Schema$/, "");
  if (name === "PreviewAnnotationStyleChange") name = "PreviewAnnotationCaptureStyleChange";
  if (!rustNames.has(name) || !Schema.isSchema(schema)) continue;
  const doc = Schema.toJsonSchemaDocument(schema);
  if (codecCases(name, schema, doc, seed(doc.schema, doc.definitions))) mapping[name] = name;
  else skipped.push("IPC." + schemaName);
}
const runtime = await import(pathToFileURL(root + "/packages/contracts/src/providerRuntime.ts"));
const runtimeMembers = runtime.ProviderRuntimeEventV2.members;
const runtimePayloads = {};
const runtimeSource = readFileSync(root + "/packages/contracts/src/providerRuntime.ts", "utf8");
function addRuntimeCodec(name, schema) {
  if (!rustNames.has(name)) return;
  const doc = Schema.toJsonSchemaDocument(schema);
  if (codecCases(name, schema, doc, seed(doc.schema, doc.definitions))) mapping[name] = name;
  else skipped.push("providerRuntime." + name);
}
function unwrapRuntimeOptional(schema) {
  return schema.schema.members.find((member) => member.ast._tag !== "Undefined");
}
for (const match of runtimeSource.matchAll(
  /const (ProviderRuntime\w+Event) = Schema.Struct\(\{[\s\S]*?\btype:\s*(\w+),\s*payload:\s*(\w+),/g,
)) {
  const [, name, tagName, payloadName] = match;
  const tagMarker = "const " + tagName + ' = Schema.Literal("';
  const tagIndex = runtimeSource.indexOf(tagMarker);
  if (tagIndex < 0) throw new Error("Missing runtime tag declaration " + tagName);
  const tag = runtimeSource.slice(tagIndex + tagMarker.length).split('"')[0];
  const member = runtimeMembers.find((member) => member.fields.type.literal === tag);
  if (!member) throw new Error("Missing runtime event member for " + name);
  addRuntimeCodec(name, member);
  runtimePayloads[payloadName] = member.fields.payload;
}
for (const [name, schema] of Object.entries(runtimePayloads)) addRuntimeCodec(name, schema);
const baseFields = { ...runtimeMembers[0].fields };
delete baseFields.type;
delete baseFields.payload;
addRuntimeCodec("ProviderRuntimeEventBase", Schema.Struct(baseFields));
const refs = unwrapRuntimeOptional(baseFields.providerRefs);
const planStep = runtimePayloads.TurnPlanUpdatedPayload.fields.plan.value;
for (const [name, schema] of Object.entries({
  ProviderRefs: refs,
  RuntimeEventRawSource: runtime.RuntimeEventRaw.fields.source,
  RuntimeSessionState: runtimePayloads.SessionStateChangedPayload.fields.state,
  RuntimeThreadState: runtimePayloads.ThreadStateChangedPayload.fields.state,
  RuntimeTurnState: runtimePayloads.TurnCompletedPayload.fields.state,
  RuntimePlanStep: planStep,
  RuntimePlanStepStatus: planStep.fields.status,
  RuntimeItemStatus: unwrapRuntimeOptional(runtime.ItemLifecyclePayload.fields.status),
  RuntimeContentStreamKind: runtimePayloads.ContentDeltaPayload.fields.streamKind,
  RuntimeSessionExitKind: unwrapRuntimeOptional(
    runtimePayloads.SessionExitedPayload.fields.exitKind,
  ),
  RuntimeErrorClass: unwrapRuntimeOptional(runtimePayloads.RuntimeErrorPayload.fields.class),
  RuntimeUserInputQuestionOption: runtime.UserInputQuestion.fields.options.value,
}))
  addRuntimeCodec(name, schema);
const mapped = {
  ProviderRef: "OrchestrationV2ProviderRef",
  ProviderThreadNativeMetadata: "OrchestrationV2ProviderThreadNativeMetadata",
  LimitRecovery: "OrchestrationV2LimitRecovery",
  LimitRecoveryUpdate: "OrchestrationV2LimitRecoveryUpdate",
  PendingBackgroundTask: "OrchestrationV2PendingBackgroundTask",
  ThreadForkSourcePoint: "OrchestrationV2ThreadForkSourcePoint",
  ConversationMessage: "OrchestrationV2ConversationMessage",
  RuntimeRequest: "OrchestrationV2RuntimeRequest",
  ThreadProjection: "OrchestrationV2ThreadProjection",
  DomainEvent: "OrchestrationV2DomainEvent",
  ThreadStreamItem: "OrchestrationV2ThreadStreamItem",
  ThreadDetailSnapshot: "OrchestrationV2ThreadDetailSnapshot",
  ThreadBoundedSnapshot: "OrchestrationV2ThreadBoundedSnapshot",
  ThreadHistoryPage: "OrchestrationV2ThreadHistoryPage",
  GetTurnItemInput: "OrchestrationV2GetTurnItemInput",
  GetTurnItemResult: "OrchestrationV2GetTurnItemResult",
  GetThreadProjectionInput: "OrchestrationV2GetThreadProjectionInput",
  SubscribeThreadInput: "OrchestrationV2SubscribeThreadInput",
  SubscribeShellInput: "OrchestrationV2SubscribeShellInput",
};
const executionNames = new Set(
  ["execution", "turn_items"].flatMap((file) =>
    [
      ...readFileSync(root + "/rust/crates/contracts/src/" + file + ".rs", "utf8").matchAll(
        /pub (?:struct|enum|type) (\w+)/g,
      ),
    ].map((m) => m[1]),
  ),
);
for (const rust of executionNames) {
  if (orch["OrchestrationV2" + rust] && !mapped[rust] && !mapping[rust])
    mapped[rust] = "OrchestrationV2" + rust;
}
mapped.ProviderSessionV2 = "OrchestrationV2ProviderSession";
mapped.UserInputQuestionV2 = "OrchestrationV2UserInputQuestion";
for (const [rust, source] of Object.entries(mapped)) {
  const s = orch[source];
  if (!s) continue;
  const doc = Schema.toJsonSchemaDocument(s);
  let initial = seed(doc.schema, doc.definitions);
  if (rust === "LimitRecoveryUpdate")
    initial = { runId: "run", resetAt: "time", autoResume: false };
  if (codecCases(rust, s, doc, initial)) mapping[rust] = rust;
  else skipped.push(source);
}
const http = await import(pathToFileURL(root + "/packages/contracts/src/environmentHttp.ts"));
const historyEndpoint = http.EnvironmentHttpApi.groups.orchestration.endpoints.threadHistoryPage;
for (const [name, s] of [
  ["EnvironmentOrchestrationThreadSnapshotParams", historyEndpoint.params],
  ["EnvironmentOrchestrationThreadHistoryQuery", historyEndpoint.query],
]) {
  const doc = Schema.toJsonSchemaDocument(s);
  if (codecCases(name, s, doc, seed(doc.schema, doc.definitions))) mapping[name] = name;
}
const doc = Schema.toJsonSchemaDocument(orch.OrchestrationV2Command);
const tags = [
  ...readFileSync(root + "/rust/crates/contracts/src/thread_command.rs", "utf8").matchAll(
    /serde\(rename = "(thread\.[^"]+)"/g,
  ),
].map((m) => m[1]);
for (const variant of doc.schema.anyOf) {
  const tag = variant.properties?.type?.const ?? variant.properties?.type?.enum?.[0];
  if (!tags.includes(tag)) continue;
  let initial = seed(variant, doc.definitions);
  initial.modelSelection =
    initial.modelSelection === undefined
      ? undefined
      : { instanceId: "codex", model: "gpt-6-astra" };
  if (
    codecCases(
      "ThreadCommand",
      orch.OrchestrationV2Command,
      { schema: variant, definitions: doc.definitions },
      JSON.parse(JSON.stringify(initial)),
    )
  )
    mapping.ThreadCommand = "ThreadCommand";
  else skipped.push(tag);
}
const executionTags = [
  ...readFileSync(root + "/rust/crates/contracts/src/execution.rs", "utf8").matchAll(
    /serde\(rename\s*=\s*"([^"]+)"/g,
  ),
].map((m) => m[1]);
for (const variant of doc.schema.anyOf) {
  const tag = variant.properties?.type?.const ?? variant.properties?.type?.enum?.[0];
  if (!executionTags.includes(tag)) continue;
  const initial = seed(variant, doc.definitions);
  if (
    codecCases(
      "ProviderCommand",
      orch.OrchestrationV2Command,
      { schema: variant, definitions: doc.definitions },
      initial,
    )
  )
    mapping.ProviderCommand = "ProviderCommand";
  else skipped.push(tag);
}
const unique = [...new Map(fixtures.map((f) => [JSON.stringify([f.schema, f.input]), f])).values()];
writeFileSync(
  root + "/rust/crates/contracts/tests/fixtures/expanded-codecs.jsonl.gz",
  gzipSync(unique.map((f) => JSON.stringify(f)).join("\n") + "\n", { level: 9 }),
);
const testPath = root + "/rust/crates/contracts/tests/original_codec_parity.rs";
let rustTest = readFileSync(testPath, "utf8");
const markerIndex = rustTest.indexOf("// BEGIN ORIGINAL CODEC DISPATCH");
if (markerIndex < 0) throw new Error("Missing Rust fixture dispatcher beginning marker");
const begin = markerIndex + "// BEGIN ORIGINAL CODEC DISPATCH".length;
const end = rustTest.indexOf("// END ORIGINAL CODEC DISPATCH", begin);
if (begin < 0 || end < 0) throw new Error("Missing Rust fixture dispatcher markers");
rustTest =
  rustTest.slice(0, begin) +
  "\n" +
  Object.entries(mapping)
    .map(
      ([name, type]) =>
        `            "${name}" => checked_roundtrip::<${type}>(fixture.input.clone()),\n`,
    )
    .join("") +
  "            " +
  rustTest.slice(end);
writeFileSync(testPath, rustTest);
console.log(
  JSON.stringify(
    { fixtures: unique.length, codecs: Object.keys(mapping).length, skipped },
    null,
    2,
  ),
);

// Runtime default artifact: generated once from source, loaded by pure Rust.
const { DEFAULT_RESOLVED_KEYBINDINGS } = await import(
  pathToFileURL(root + "/packages/shared/src/keybindings.ts")
);
const { ResolvedKeybindingsConfig } = await import(
  pathToFileURL(root + "/packages/contracts/src/keybindings.ts")
);
writeFileSync(
  root + "/rust/crates/contracts/assets/default-keybindings.json",
  JSON.stringify(
    Schema.encodeUnknownSync(Schema.toCodecJson(ResolvedKeybindingsConfig))(
      DEFAULT_RESOLVED_KEYBINDINGS,
    ),
    null,
    2,
  ) + "\n",
);
