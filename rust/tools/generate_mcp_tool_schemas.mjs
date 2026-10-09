// Writes each original MCP tool's parameter schema, and the payload schema of the WebSocket RPC
// methods (every method of the original's RPC group, as the server decodes it: through the JSON codec,
// `Schema.toCodecJson`, which reads numbers from "NaN" and "Infinity" strings and `null` as absent for
// an optional field), as the decoder sees it: the Effect Schema AST of
// `tool.parametersSchema`, with the pieces the JSON Schema in the catalog loses (refinements and
// the text they fail with, trimming, branded members, union member descriptors, exact optionality,
// transformations). `mcp_tool_input.rs` interprets this to decode a call like the original does, and
// `rpc_admission.rs` refuses an RPC payload the original's decoding refuses before its handler runs,
// and hands the handler the value that decoding makes. `generate_rpc_decode_cases.mjs` writes the cases
// the port is checked against.
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
const Schema = await import(dist + "Schema.js");
const Effect = await import(dist + "Effect.js");
const Option = await import(dist + "Option.js");
const SchemaIssue = await import(dist + "SchemaIssue.js");
const Tool = await import(dist + "ai/Tool.js");
// Every method of the original's RPC group: unary and streaming.
export const rpcMethods = [...WsRpcGroup.requests.keys()];
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

// An identifier only changes the node's expected text, which `getExpected` already folds in.
const unsupportedAnnotations = ["message", "messageMissingKey", "messageUnexpectedKey"];
// Every string literal the payload schemas hold. A function that closes over a set of such literals
// (the kinds a union knows) is recorded with the members of the set, found by trying each.
const literalPool = new Set();
{
  const seen = new Set();
  const walk = (ast) => {
    if (seen.has(ast)) return;
    seen.add(ast);
    if (ast._tag === "Literal" && typeof ast.literal === "string") literalPool.add(ast.literal);
    for (const link of ast.encoding ?? []) walk(link.to);
    if (ast._tag === "Suspend") walk(ast.thunk());
    else
      ast.recur?.(
        (child) => (walk(child), child),
        (x) => x,
      );
  };
  for (const request of WsRpcGroup.requests.values())
    walk(Schema.toCodecJson(request.payloadSchema).ast);
}
const closedOver = (fn) => {
  if (!String(fn).includes("knownKinds.has(kind)")) return {};
  const rejects = [...literalPool].filter((kind) => fn(kind) !== true);
  if (!rejects.length) throw new Error("a known-kinds filter that rejects nothing");
  return { rejects };
};
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
      return {
        ...out,
        fn: opaque(check.run.userFilter, "filter"),
        ...closedOver(check.run.userFilter),
      };
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
    case "effect/schema/isMaxProperties":
      return { ...out, maxProperties: representation.payload.maxProperties };
    case "effect/schema/isMinProperties":
      return { ...out, minProperties: representation.payload.minProperties };
    case "effect/schema/isFinite":
      return { ...out, finite: true };
    case "effect/schema/isTrimmed":
      return { ...out, trimmed: true };
    default:
      throw new Error(`unsupported check ${representation.id}`);
  }
};

// A decode that fails with a made-up issue, to see what a middleware does with a failure.
const probeIssue = new SchemaIssue.InvalidValue(Option.some("probe"));
const run = (effect) => Effect.runSyncExit(effect);
const some = (exit) => exit._tag === "Success" && exit.value._tag === "Some";
// What a middleware does to a decode: nothing, or turn a failure into `undefined` (catchDecoding).
const middlewareOf = (middleware) => {
  const kept = run(middleware.decode(Effect.succeed(Option.some(1)), {}));
  const failed = run(middleware.decode(Effect.fail(probeIssue), {}));
  const missing = run(middleware.decode(Effect.succeed(Option.none()), {}));
  if (!some(kept) || kept.value.value !== 1) throw new Error("middleware changes a decoded value");
  if (missing._tag !== "Success" || missing.value._tag !== "None")
    throw new Error("middleware changes a missing value");
  if (failed._tag === "Failure") return "identity";
  if (some(failed) && failed.value.value === undefined) return "catch";
  throw new Error("unsupported middleware");
};
// The `withDecodingDefault` getter: a missing or undefined input becomes the default.
const defaultOf = (getter) => {
  const text = String(getter.transform);
  if (
    !text.includes("Option.filter(o, Predicate.isNotUndefined)") ||
    !text.includes("defaultValue")
  )
    throw new Error(`unsupported optional transformation ${text}`);
  const missing = run(getter.transform(Option.none(), {}));
  const undef = run(getter.transform(Option.some(undefined), {}));
  const kept = run(getter.transform(Option.some("kept"), {}));
  if (!some(missing) || !some(undef) || !some(kept) || kept.value.value !== "kept")
    throw new Error("default getter changed");
  if (JSON.stringify(missing.value.value) !== JSON.stringify(undef.value.value))
    throw new Error("default differs");
  const value = missing.value.value;
  if (
    JSON.stringify(value) === undefined ||
    JSON.stringify(JSON.parse(JSON.stringify(value))) !== JSON.stringify(value)
  )
    throw new Error("default has no JSON form");
  return value;
};
const linkOf = (link, ast) => {
  const transformation = link.transformation;
  const to = visit(link.to);
  if (transformation._tag === "Middleware") return { to, middleware: middlewareOf(transformation) };
  const getter = transformation.decode;
  switch (getter._tag) {
    case "Passthrough":
      return { to, fn: null };
    case "Transform":
    case "TransformEffect":
      // A transformation that ignores its input has one result, which is data.
      if (getter._tag === "Transform" && getter.transform.length === 0) {
        const result = getter.transform();
        if (result !== undefined && JSON.stringify(result) === undefined)
          throw new Error("constant has no JSON form");
        return { to, constant: result === undefined ? {} : { value: result } };
      }
      // A field read as it is when the schema it decodes with accepts it, and as absent otherwise.
      if (String(getter.transform).includes("Option.isSome(decodeValue(raw))")) {
        const member = ast.types?.[0];
        const literals = (node) =>
          node._tag === "Literal" || (node._tag === "Union" && node.types.every(literals));
        if (ast._tag !== "Union" || !member || !literals(member) || member.encoding)
          throw new Error("a tolerant field over something other than literals");
        return { to, tolerant: visit(member) };
      }
      return { to, fn: opaque(getter.transform, "transform") };
    case "TransformOptionalEffect":
      return { to, default: { value: defaultOf(getter) } };
    default:
      throw new Error(`getter ${getter._tag}`);
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
// A recursive schema reaches itself through a Suspend: its slot is reserved before its target is read.
const suspended = new Map();
const declarations = ["Duration", "DateTime.Utc", "JSON value"];
const visit = (ast) => {
  for (const key of unsupportedAnnotations)
    if (ast.annotations?.[key] !== undefined) throw new Error(`annotation ${key}`);
  const node = { t: ast._tag, expected: Annotations.getExpected(ast) };
  if (ast._tag === "Suspend") {
    if (ast.checks?.length || ast.encoding) throw new Error("suspend with checks or encoding");
    if (suspended.has(ast)) return suspended.get(ast);
    const slot = nodes.length;
    nodes.push(null);
    suspended.set(ast, slot);
    node.target = visit(ast.thunk());
    nodes[slot] = node;
    return slot;
  }
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
    case "Declaration":
      // What a transformation makes (a Duration, a DateTime), or JSON itself: a decoded payload
      // always passes the declaration's own check, so only the encoding below decides.
      if (!declarations.includes(node.expected))
        throw new Error(`unsupported declaration ${node.expected}`);
      break;
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
  if (ast.encoding) node.encoding = ast.encoding.map((link) => linkOf(link, ast));
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
  // The server decodes a payload through the JSON codec (RpcSerialization.layerJson), which reads
  // numbers from "NaN" and "Infinity" strings and dates and durations from their JSON form.
  rpc[method] = visit(Schema.toCodecJson(request.payloadSchema).ast);
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
