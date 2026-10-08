// Run from repository root with Node 24:
// /tmp/node-v24.13.1-darwin-arm64/bin/node rust/tools/generate_acp_schemas.mjs
// Development oracle only; generated Rust artifacts never import TypeScript.
import fs from "node:fs";
import zlib from "node:zlib";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { execFileSync } from "node:child_process";
import * as Schema from "../../packages/effect-acp/node_modules/effect/dist/Schema.js";
import * as V1 from "../../packages/effect-acp/src/_generated/schema-v1.gen.ts";
import * as V2 from "../../packages/effect-acp/src/schema.ts";
import * as Compat from "../../packages/effect-acp/src/compat.ts";

const out = new URL("../crates/acp/", import.meta.url);
fs.mkdirSync(new URL("src/", out), { recursive: true });
fs.mkdirSync(new URL("tests/fixtures/", out), { recursive: true });
const nodes = [],
  seen = new Map(),
  roots = {};
function add(ast) {
  if (seen.has(ast)) return seen.get(ast);
  const id = nodes.length;
  seen.set(ast, id);
  nodes.push(null);
  const checks = (ast.checks ?? []).map((c) => {
    const r = c.annotations?.representation;
    if (!r) throw Error(`Unsupported ACP check ${JSON.stringify(c.annotations)}`);
    const kind = r.id.replace("effect/schema/", "");
    if (kind === "isPattern") {
      const source = r.payload.source;
      if (source === "^[A-Z]{3}$") return { kind: "currency" };
      const match = source.match(/^\^\(\?!\(\?:([^)]*)\)\$\)\[\\s\\S\]\*\$$/);
      if (!match) throw Error(`Unsupported ACP pattern ${source}`);
      return {
        kind: "exclude",
        values: match[1]
          .split("|")
          .map((s) =>
            s.replace(/\\x([0-9a-f]{2})/gi, (_, h) => String.fromCharCode(parseInt(h, 16))),
          ),
      };
    }
    if (
      ![
        "isInt",
        "isFinite",
        "isGreaterThanOrEqualTo",
        "isLessThanOrEqualTo",
        "isMinLength",
      ].includes(kind)
    )
      throw Error(`Unsupported ACP check ${kind}`);
    return { kind, ...r.payload };
  });
  let node = { kind: ast._tag, checks };
  switch (ast._tag) {
    case "Suspend":
      node.target = add(ast.thunk());
      break;
    case "Objects":
      node.fields = ast.propertySignatures.map((p) => ({
        name: p.name,
        node: add(p.type),
        optional: p.type.context?.isOptional === true,
      }));
      node.index = ast.indexSignatures.map((p) => ({ key: add(p.parameter), value: add(p.type) }));
      break;
    case "Arrays":
      if (ast.elements.length || ast.rest.length !== 1) throw Error("Unexpected ACP tuple");
      node.item = add(ast.rest[0]);
      break;
    case "Union":
      node.members = ast.types.map(add);
      break;
    case "Literal":
      node.value = ast.literal;
      break;
    case "Declaration":
      if (ast.annotations?.representation?.id !== "effect/schema/Json")
        throw Error("Unexpected ACP declaration");
      node.kind = "Json";
      break;
    case "String":
    case "Number":
    case "Boolean":
    case "Null":
    case "Never":
      break;
    default:
      throw Error(`Unsupported ACP AST ${ast._tag}`);
  }
  nodes[id] = node;
  return id;
}
const Compatibility = { ...Compat };
for (const name of [
  "NewSessionResponse",
  "LoadSessionResponse",
  "ResumeSessionResponse",
  "ForkSessionResponse",
])
  Compatibility[name] = Schema.Struct({
    ...V1[name].fields,
    models: Schema.optionalKey(Schema.NullOr(Compat.SessionModelState)),
  });
for (const [version, schemas] of [
  ["v1", V1],
  ["v2", V2],
  ["compat", Compatibility],
])
  for (const [name, s] of Object.entries(schemas))
    if (s?.ast) roots[`${version}.${name}`] = add(s.ast);
const table = { roots, nodes };
fs.writeFileSync(new URL("src/schema_table.json", out), JSON.stringify(table) + "\n");
// Match the repository's commit formatter exactly without changing its config.
// This is a development-time tool dependency only, like the source TS oracle.
execFileSync(
  process.execPath,
  [
    new URL("../../node_modules/vite-plus/bin/vp", import.meta.url).pathname,
    "fmt",
    new URL("src/schema_table.json", out).pathname,
    "--threads=1",
  ],
  {
    cwd: new URL("../../", import.meta.url).pathname,
    stdio: "pipe",
  },
);
function minimal(id, stack = new Set()) {
  if (stack.has(id)) return null;
  stack = new Set(stack).add(id);
  const n = nodes[id];
  switch (n.kind) {
    case "Suspend":
      return minimal(n.target, stack);
    case "Objects":
      return Object.fromEntries(
        n.fields.filter((f) => !f.optional).map((f) => [f.name, minimal(f.node, stack)]),
      );
    case "Arrays":
      return Array.from(
        { length: n.checks.find((c) => c.kind === "isMinLength")?.minLength ?? 0 },
        () => minimal(n.item, stack),
      );
    case "Union":
      return minimal(n.members[0], stack);
    case "Literal":
      return n.value;
    case "String":
      return n.checks.some((c) => c.kind === "currency") ? "USD" : "test";
    case "Number":
      return 0;
    case "Boolean":
      return false;
    case "Json":
    case "Null":
      return null;
    case "Never":
      return null;
  }
}
const cases = [];
for (const [key, id] of Object.entries(roots)) {
  const [version, name] = key.split(".");
  const schema = (version === "v1" ? V1 : version === "v2" ? V2 : Compatibility)[name];
  const base = minimal(id);
  const samples = [base, null, {}, [], 0, "", true];
  if (base && typeof base === "object" && !Array.isArray(base)) {
    samples.push({ ...base, unknownExtra: { keep: "only records" } });
    for (const field of Object.keys(base)) {
      const missing = { ...base };
      delete missing[field];
      samples.push(
        missing,
        { ...base, [field]: null },
        { ...base, [field]: [] },
        { ...base, [field]: {} },
      );
    }
  }
  if (name === "ContentBlock")
    samples.push(
      { type: "text", text: "hello" },
      { type: "text" },
      { type: "image", data: "data" },
      { type: "future_content", payload: { value: 1 } },
    );
  if (name === "CreateElicitationRequest")
    samples.push(
      {
        sessionId: "s",
        mode: "form",
        message: "choose",
        requestedSchema: { type: "object", properties: {} },
      },
      { sessionId: "s", mode: "url", message: "sign in" },
    );
  const unique = new Set();
  for (const input of samples) {
    const encoded = JSON.stringify(input);
    if (unique.has(encoded)) continue;
    unique.add(encoded);
    try {
      const decoded = Schema.decodeUnknownSync(schema)(input);
      const output = Schema.encodeSync(schema)(decoded);
      cases.push({ schema: key, input, valid: true, output });
    } catch {
      cases.push({ schema: key, input, valid: false });
    }
  }
}
fs.writeFileSync(
  new URL("tests/fixtures/source-codecs.jsonl.gz", out),
  zlib.gzipSync(cases.map((c) => JSON.stringify(c)).join("\n") + "\n", { level: 9 }),
);
const names = Object.keys(roots)
  .filter((k) => k.startsWith("v2."))
  .map((k) => k.slice(3));
const aliases = names
  .map(
    (name) =>
      `pub type ${name} = crate::schema::Wire<${name}Schema>;\npub enum ${name}Schema {}\nimpl crate::schema::SchemaName for ${name}Schema { const NAME: &'static str = "v2.${name}"; }`,
  )
  .join("\n");
fs.writeFileSync(
  new URL("src/v2.rs", out),
  "// Generated from the checked-in ACP v2 Effect schemas. Regenerate with rust/tools/generate_acp_schemas.mjs.\n" +
    aliases +
    "\n",
);
const aliasesV1 = Object.keys(roots)
  .filter((k) => k.startsWith("v1."))
  .map((k) => k.slice(3))
  .map(
    (name) =>
      `pub type ${name} = crate::schema::Wire<${name}Schema>;\npub enum ${name}Schema {}\nimpl crate::schema::SchemaName for ${name}Schema { const NAME: &'static str = "v1.${name}"; }`,
  )
  .join("\n");
fs.writeFileSync(
  new URL("src/v1.rs", out),
  "// Generated from the checked-in ACP v1 Effect schemas.\n" + aliasesV1 + "\n",
);

// Export pure transforms from a temporary copy of the actual original client.
// The original checkout is untouched and this oracle is never shipped.
const clientUrl = new URL("../../packages/effect-acp/src/client.ts", import.meta.url);
const require = createRequire(clientUrl);
const transforms = [
  "toNegotiatingInitializeRequest",
  "normalizeInitializeResponse",
  "normalizeConfigOption",
  "normalizeV2SessionSetupResponse",
  "normalizeSessionUpdate",
  "normalizeV1SessionUpdate",
  "normalizePermissionRequest",
  "toV2McpServer",
  "toV2ResumeRequest",
  "toV2ForkRequest",
];
let clientSource = fs
  .readFileSync(clientUrl, "utf8")
  .replace(
    /from (["'])([^"']+)\1/g,
    (_, quote, specifier) =>
      `from ${quote}${specifier.startsWith(".") ? new URL(specifier, clientUrl).href : pathToFileURL(require.resolve(specifier)).href}${quote}`,
  );
clientSource += `\nexport const __rustOracle = {${transforms.join(",")}};\nexport const __rustIdentity = AcpProtocol.acpRequestIdentity;\n`;
const temp = new URL(`file:///private/tmp/t3port-acp-oracle-${process.pid}.ts`);
fs.writeFileSync(temp, clientSource);
const normalization = [];
try {
  const original = await import(temp.href);
  const oracle = original.__rustOracle;
  const addTransform = (transform, input, ...args) =>
    normalization.push({
      transform,
      input,
      args,
      output: oracle[transform](input, ...args) ?? null,
    });
  for (const caps of [
    undefined,
    {},
    {
      fs: { readTextFile: true },
      terminal: true,
      auth: { terminal: true },
      elicitation: { form: {} },
      _meta: { custom: true },
    },
  ]) {
    addTransform("toNegotiatingInitializeRequest", {
      protocolVersion: 1,
      ...(caps === undefined ? {} : { clientCapabilities: caps }),
    });
    addTransform("toNegotiatingInitializeRequest", {
      protocolVersion: 1,
      clientInfo: { name: "fixture", version: "1" },
      clientCapabilities: caps ?? {},
      _meta: { source: true },
    });
  }
  const info = { name: "fixture", version: "1" };
  for (const capabilities of [
    undefined,
    {},
    {
      session: {
        prompt: { image: {}, audio: null, embeddedContext: {} },
        mcp: { stdio: {}, http: {}, acp: {} },
        delete: {},
        fork: {},
        additionalDirectories: {},
      },
      providers: {},
    },
  ]) {
    const response = {
      protocolVersion: 2,
      info,
      ...(capabilities === undefined ? {} : { capabilities }),
    };
    addTransform("normalizeInitializeResponse", response);
    for (const authMethods of [
      [],
      [{ type: "agent", methodId: "login", name: "Login" }],
      [
        {
          type: "terminal",
          methodId: "term",
          name: "Terminal",
          args: ["login", 4],
          env: [{ name: "KEY", value: "secret" }],
        },
      ],
      [
        {
          type: "env_var",
          methodId: "key",
          name: "Key",
          vars: [{ name: "API_KEY", label: "API key" }],
          link: "https://example.test",
        },
      ],
      [{ type: "future_auth", methodId: "future", name: "Future" }],
    ])
      addTransform("normalizeInitializeResponse", { ...response, authMethods });
  }
  for (const option of [
    {
      type: "select",
      configId: "model",
      name: "Model",
      currentValue: "m",
      options: [{ value: "m", name: "Model" }],
    },
    {
      type: "boolean",
      configId: "safe",
      name: "Safe",
      currentValue: false,
      description: null,
      category: "policy",
      _meta: { x: 1 },
    },
    { type: "future_config", configId: "f", name: "Future", payload: { nested: true } },
  ]) {
    addTransform("normalizeConfigOption", option);
    addTransform("normalizeV2SessionSetupResponse", {
      sessionId: "s",
      configOptions: [option],
      _meta: { x: true },
    });
  }
  addTransform("normalizeV2SessionSetupResponse", { sessionId: "s" });
  function resolveNode(id) {
    while (nodes[id].kind === "Suspend") id = nodes[id].target;
    return id;
  }
  for (const [version, name, transform] of [
    ["v1", "SessionUpdate", "normalizeV1SessionUpdate"],
    ["v2", "SessionUpdate", "normalizeSessionUpdate"],
  ]) {
    const union = nodes[resolveNode(roots[`${version}.${name}`])];
    for (const id of union.members) {
      const update = minimal(id);
      const input = { sessionId: "s", update, _meta: { kept: true } };
      try {
        Schema.decodeUnknownSync(
          version === "v1" ? V1.SessionNotification : V2.UpdateSessionNotification,
        )(input);
        addTransform(transform, input);
      } catch {}
    }
  }
  for (const update of [
    {
      sessionUpdate: "agent_message_chunk",
      content: { type: "future_content", payload: { large: [1, 2] } },
    },
    {
      sessionUpdate: "tool_call_content_chunk",
      toolCallId: "tool",
      content: { type: "future_tool", payload: { x: 1 } },
    },
    { sessionUpdate: "future_update", extra: { nested: true } },
    {
      sessionUpdate: "available_commands_update",
      availableCommands: [
        { name: "cmd", description: "Command", input: { type: "future_input", data: 1 } },
      ],
    },
  ])
    addTransform("normalizeSessionUpdate", { sessionId: "s", update });
  for (const subject of [
    undefined,
    null,
    { type: "command", toolCallId: "command-id", command: "echo" },
    {
      type: "tool_call",
      toolCall: { toolCallId: "tool", content: [{ type: "diff", changes: [], patch: null }] },
    },
    { type: "future_subject", data: { x: 1 } },
  ])
    addTransform(
      "normalizePermissionRequest",
      {
        sessionId: "s",
        title: "Allow?",
        options: [],
        ...(subject === undefined ? {} : { subject }),
      },
      { requestId: "$t3:jsonrpc:number:0", method: "session/request_permission" },
    );
  for (const server of [
    { name: "legacy", command: "mcp", args: [], env: [] },
    { type: "http", name: "remote", url: "https://example.test", headers: [] },
    { type: "stdio", name: "stdio", command: "mcp", args: [] },
  ])
    addTransform("toV2McpServer", server);
  for (const request of [
    { sessionId: "s", cwd: "/workspace" },
    { sessionId: "s", cwd: "/workspace", mcpServers: [] },
    { sessionId: "s", cwd: "/workspace", mcpServers: [{ name: "legacy", command: "mcp" }] },
  ]) {
    addTransform("toV2ResumeRequest", request);
    addTransform("toV2ResumeRequest", request, { type: "start" });
    addTransform("toV2ForkRequest", request);
  }
  const boundaryCase = (name, input) => {
    const [version, key] = name.split(".");
    const schema = (version === "v1" ? V1 : V2)[key];
    try {
      const decoded = Schema.decodeUnknownSync(schema)(input);
      cases.push({ schema: name, input, valid: true, output: Schema.encodeSync(schema)(decoded) });
    } catch {
      cases.push({ schema: name, input, valid: false });
    }
  };
  boundaryCase("v2.ResumeSessionRequest", { sessionId: "s", cwd: "/workspace", mcpServers: null });
  for (const version of ["v1", "v2"]) {
    for (const code of [
      -9007199254740992, -9007199254740991, -1.5, 1, 1.5, 9007199254740991, 9007199254740992,
    ])
      boundaryCase(`${version}.Error`, { code, message: "fixture" });
    for (const number of [-1, 0, 65535, 65536, 1.5, 9007199254740992])
      boundaryCase(`${version}.ProtocolVersion`, number);
  }
  const identityInputs = [
    "1",
    "1.0",
    "-0",
    "-0.0",
    "0.000001",
    "1e-7",
    "1e20",
    "1e21",
    "-1e21",
    "9007199254740991",
    "9007199254740993",
    "18446744073709551615",
    "1000000000000000128",
    "1.2345678901234568",
    '"1"',
    '"$t3:jsonrpc:number:1"',
  ];
  for (const number of [1.1, 1.23456789, 0.3333333333333333, 9.999999999999998])
    for (const power of [-12, -7, -6, 0, 7, 20, 21, 30])
      identityInputs.push(String(number * 10 ** power));
  fs.writeFileSync(
    new URL("tests/fixtures/request-identities.jsonl", out),
    identityInputs
      .map((wire) => JSON.stringify({ wire, identity: original.__rustIdentity(JSON.parse(wire)) }))
      .join("\n") + "\n",
  );
} finally {
  fs.unlinkSync(temp);
}
fs.writeFileSync(
  new URL("tests/fixtures/source-codecs.jsonl.gz", out),
  zlib.gzipSync(cases.map((c) => JSON.stringify(c)).join("\n") + "\n", { level: 9 }),
);
fs.writeFileSync(
  new URL("tests/fixtures/normalization.jsonl", out),
  normalization.map((c) => JSON.stringify(c)).join("\n") + "\n",
);
console.log(`${normalization.length} direct original client normalization cases`);
console.log(
  `${Object.keys(roots).length} ACP schemas, ${nodes.length} nodes, ${cases.length} original decode/encode cases`,
);
execFileSync("rustfmt", [
  "--edition",
  "2024",
  new URL("src/v1.rs", out).pathname,
  new URL("src/v2.rs", out).pathname,
]);
