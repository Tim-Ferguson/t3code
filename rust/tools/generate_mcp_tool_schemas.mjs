// Writes each original MCP tool's parameter schema, and the payload schema of the WebSocket RPC
// methods listed in `rpcMethods`, as the decoder sees it: the Effect Schema AST of
// `tool.parametersSchema`, with the pieces the JSON Schema in the catalog loses (refinements and
// the text they fail with, trimming, branded members, union member descriptors, exact optionality,
// transformations). `mcp_tool_input.rs` interprets this to decode a call like the original does, and
// `rpc_admission.rs` refuses an RPC payload the original's decoding refuses before its handler runs.
// A method is added to `rpcMethods` together with its cases in `generate_rpc_decode_cases.mjs`.
//
// Functions the AST holds as code (a transformation's decode, a refinement written by hand) cannot
// be written as data. Each is recorded as the hash of its source text; the port implements the
// ones it meets by hash, so a changed original function fails the port's tests instead of
// drifting. A Filter does not keep the function it wraps, so a loader hook records it on `run`.
//
// `--check` regenerates in memory and fails when the committed file is not what the original has.
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { register } from "node:module";

register(
  "data:text/javascript," +
    encodeURIComponent(`
const from = "return new Filter((input, ast, options) => SchemaIssue.normalizeFilterOutput(ast, filter(input, ast, options), input, options), annotations, aborted);";
const to = "const run = (input, ast, options) => SchemaIssue.normalizeFilterOutput(ast, filter(input, ast, options), input, options); run.userFilter = filter; return new Filter(run, annotations, aborted);";
export async function load(url, context, next) {
  const loaded = await next(url, context);
  if (url.endsWith("/effect/dist/SchemaAST.js")) {
    const source = String(loaded.source);
    if (!source.includes(from)) throw new Error("makeFilter changed: update the loader hook");
    return { ...loaded, source: source.replace(from, to) };
  }
  return loaded;
}`),
  import.meta.url,
);

const dist = "../../apps/server/node_modules/effect/dist/";
const SchemaAST = await import(dist + "SchemaAST.js");
// Loaded after the hook above, which must see every module that makes a filter.
const { WsRpcGroup } = await import("../../packages/contracts/src/rpc.ts");
const Annotations = await import(dist + "internal/schema/annotations.js");
const Tool = await import(dist + "ai/Tool.js");
export const rpcMethods = [
  "vcs.listRefs",
  "projects.createNew",
  "scheduledTasks.upsert",
  "pullRequests.comment",
  "provider.install.start",
  "orchestration.searchThreads",
  "git.runStackedAction",
  "server.upsertKeybinding",
  "attachments.createUploadUrl",
  "sourceControl.cloneRepository",
];
const toolkitFiles = [
  ["orchestrator", "OrchestratorToolkit"],
  ["thread", "ThreadToolkit"],
  ["attachment", "AttachmentToolkit"],
  ["project", "ProjectToolkit"],
  ["environment", "EnvironmentToolkit"],
  ["previewControls", "PreviewControlsToolkit"],
  ["worktree", "WorktreeToolkit"],
  ["pullRequests", "PullRequestsToolkit"],
  ["preview", "PreviewStandardToolkit", "PreviewSnapshotToolkit"],
  ["html", "HtmlRenderToolkit", "HtmlPreviewToolkit"],
  ["device", "DeviceStandardToolkit", "DeviceScreenshotToolkit"],
];
const toolkits = [];
for (const [dir, ...names] of toolkitFiles) {
  const module = await import(`../../apps/server/src/mcp/toolkits/${dir}/tools.ts`);
  for (const name of names) toolkits.push(module[name]);
}

const functions = {};
const opaque = (fn, kind) => {
  const source = fn.toString();
  const hash = createHash("sha256").update(source).digest("hex").slice(0, 16);
  functions[hash] = { kind, source };
  return hash;
};

// What a union reads of a member before choosing candidates: its encoded side.
const candidateOf = (ast) => {
  while (true) {
    if (ast._tag === "Suspend") return SchemaAST.unknown;
    const encoding = ast.encoding;
    if (!encoding) return ast.recur?.(candidateOf, (x) => x) ?? ast;
    if (encoding.some((link) => link.transformation._tag === "Middleware"))
      return SchemaAST.unknown;
    ast = encoding[encoding.length - 1].to;
  }
};
const candidateTypes = (ast) => {
  switch (ast._tag) {
    case "Null":
      return ["null"];
    case "Undefined":
      return ["undefined"];
    case "String":
    case "TemplateLiteral":
      return ["string"];
    case "Number":
      return ["number"];
    case "Boolean":
      return ["boolean"];
    case "Arrays":
      return ["array"];
    case "Objects":
      return ast.propertySignatures.length || ast.indexSignatures.length
        ? ["object"]
        : ["string", "number", "boolean", "symbol", "bigint", "object", "array", "function"];
    case "Literal":
      return [typeof ast.literal];
    case "Union":
      return [...new Set(ast.types.flatMap(candidateTypes))];
    default:
      return [
        "null",
        "undefined",
        "string",
        "number",
        "boolean",
        "symbol",
        "bigint",
        "object",
        "array",
        "function",
      ];
  }
};

const unsupportedAnnotations = [
  "message",
  "messageMissingKey",
  "messageUnexpectedKey",
  "identifier",
];
const checkOf = (check) => {
  for (const key of unsupportedAnnotations)
    if (check.annotations?.[key] !== undefined) throw new Error(`check annotation ${key}`);
  const base = { expected: check.annotations?.expected ?? null };
  if (check._tag === "FilterGroup") return { ...base, group: check.checks.map(checkOf) };
  const representation = check.annotations?.representation;
  const out = { ...base, aborted: check.aborted };
  switch (representation?.id) {
    case undefined:
      if (!check.run.userFilter)
        throw new Error(
          `a filter without a function or representation: ${JSON.stringify(check.annotations)}`,
        );
      return { ...out, fn: opaque(check.run.userFilter, "filter") };
    case "effect/schema/isMaxLength":
      return { ...out, maxLength: representation.payload.maxLength };
    case "effect/schema/isMinLength":
      return { ...out, minLength: representation.payload.minLength };
    case "effect/schema/isInt":
      return { ...out, int: true };
    case "effect/schema/isGreaterThan":
      return { ...out, exclusiveMinimum: representation.payload.exclusiveMinimum };
    case "effect/schema/isGreaterThanOrEqualTo":
      return { ...out, minimum: representation.payload.minimum };
    case "effect/schema/isLessThan":
      return { ...out, exclusiveMaximum: representation.payload.exclusiveMaximum };
    case "effect/schema/isLessThanOrEqualTo":
      return { ...out, maximum: representation.payload.maximum };
    case "effect/schema/isBetween":
      return {
        ...out,
        minimum: representation.payload.minimum,
        maximum: representation.payload.maximum,
        ...(representation.payload.exclusiveMinimum || representation.payload.exclusiveMaximum
          ? (() => {
              throw new Error("exclusive isBetween");
            })()
          : {}),
      };
    case "effect/schema/isPattern":
      return {
        ...out,
        pattern: representation.payload.source,
        flags: representation.payload.flags,
      };
    case "effect/schema/isFinite":
      return { ...out, finite: true };
    case "effect/schema/isTrimmed":
      return { ...out, trimmed: true };
    default:
      throw new Error(`unsupported check ${representation.id}`);
  }
};

const nodes = [];
const index = new Map();
const intern = (node) => {
  const key = JSON.stringify(node);
  if (!index.has(key)) {
    index.set(key, nodes.length);
    nodes.push(node);
  }
  return index.get(key);
};
const visit = (ast) => {
  for (const key of unsupportedAnnotations)
    if (ast.annotations?.[key] !== undefined) throw new Error(`annotation ${key}`);
  const node = { t: ast._tag, expected: Annotations.getExpected(ast) };
  switch (ast._tag) {
    case "Literal":
      node.literal = ast.literal;
      break;
    case "Objects":
      node.props = ast.propertySignatures.map((property) => {
        if (typeof property.name !== "string") throw new Error("symbol property");
        for (const key of unsupportedAnnotations)
          if (property.type.context?.annotations?.[key] !== undefined)
            throw new Error(`context annotation ${key}`);
        return {
          name: property.name,
          optional: property.type.context?.isOptional === true,
          type: visit(property.type),
        };
      });
      node.indexes = ast.indexSignatures.map((signature) => ({
        key: visit(signature.parameter),
        value: visit(signature.type),
      }));
      break;
    case "Arrays":
      node.elements = ast.elements.map((element) => ({
        optional: element.context?.isOptional === true,
        type: visit(element),
      }));
      node.rest = ast.rest.map(visit);
      break;
    case "Union":
      if (ast.options?.mode === "oneOf") throw new Error("oneOf union");
      node.types = ast.types.map(visit);
      node.members = ast.types.map((type) => {
        const candidate = candidateOf(type);
        return {
          never: candidate._tag === "Never",
          literal: candidate._tag === "Literal" ? candidate.literal : undefined,
          sentinels: SchemaAST.collectSentinels(candidate).map(({ key, literal }) => ({
            key,
            literal,
          })),
          types: candidateTypes(candidate),
        };
      });
      break;
    case "TemplateLiteral": {
      // A literal prefix and suffix around one part: the part is checked on what lies between.
      const [prefix, middle, suffix] = ast.parts;
      if (
        ast.parts.length !== 3 ||
        prefix._tag !== "Literal" ||
        suffix._tag !== "Literal" ||
        middle._tag !== "String"
      )
        throw new Error("unsupported template literal");
      node.prefix = String(prefix.literal);
      node.suffix = String(suffix.literal);
      node.middle = visit(middle);
      break;
    }
    case "String":
    case "Number":
    case "Boolean":
    case "Null":
    case "Undefined":
    case "Never":
    case "Unknown":
      break;
    default:
      throw new Error(`unsupported node ${ast._tag}`);
  }
  if (ast.checks?.length) node.checks = ast.checks.map(checkOf);
  if (ast.encodingChecks?.length) throw new Error("encoding checks");
  if (ast.encoding)
    node.encoding = ast.encoding.map((link) => {
      const getter = link.transformation.decode;
      if (link.transformation._tag === "Middleware") throw new Error("middleware");
      if (getter._tag === "Passthrough") return { to: visit(link.to), fn: null };
      if (getter._tag !== "TransformEffect") throw new Error(`getter ${getter._tag}`);
      return { to: visit(link.to), fn: opaque(getter.transform, "transform") };
    });
  return intern(node);
};

const tools = {};
for (const toolkit of toolkits)
  for (const tool of Object.values(toolkit.tools)) {
    if (Tool.getStrictMode(tool)) throw new Error(`${tool.name} is strict`);
    tools[tool.name] = visit(tool.parametersSchema.ast);
  }
const rpc = {};
for (const method of rpcMethods) {
  const request = WsRpcGroup.requests.get(method);
  if (!request) throw new Error(`no such method ${method}`);
  rpc[method] = visit(request.payloadSchema.ast);
}
const out = { nodes, tools, rpc, functions };

const target = new URL("../crates/server/src/mcp_tool_schemas.json", import.meta.url);
if (process.argv.includes("--check")) {
  const same = JSON.stringify(JSON.parse(readFileSync(target, "utf8"))) === JSON.stringify(out);
  console.log(JSON.stringify({ tools: Object.keys(tools).length, same }));
  process.exit(same ? 0 : 1);
}
writeFileSync(target, JSON.stringify(out, null, 1) + "\n");
console.log(
  JSON.stringify({
    tools: Object.keys(tools).length,
    rpc: Object.keys(rpc).length,
    nodes: nodes.length,
    functions: Object.keys(functions).length,
  }),
);
