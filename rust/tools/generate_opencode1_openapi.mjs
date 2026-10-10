// Development oracle: the OpenCode 1.x HTTP surface the Rust adapter uses, as the SDK the original drives it with
// (`@opencode-ai/sdk/v2`) describes it. The SDK ships its generated types, not an OpenAPI document, so each
// operation's request (`<Operation>Data`: body, path and query) is converted from those types to JSON Schema and
// pruned to the schemas the used operations reach. The fake server checks every request the Rust client sends
// against it (path, query, body), as it does for 2.x with `opencode2-openapi.json`. Written next to the replay fixtures.
import fs from "node:fs";

const base = new URL("../../node_modules/.pnpm/", import.meta.url).pathname;
const find = (prefix) => fs.readdirSync(base).find((name) => name.startsWith(prefix));
const sdk = `${base}${find("@opencode-ai+sdk@")}/node_modules/@opencode-ai/sdk/dist/v2/gen/`;
const { default: ts } = await import(
  `${base}${find("typescript@6")}/node_modules/typescript/lib/typescript.js`
);

const source = ts.createSourceFile(
  "types.gen.d.ts",
  fs.readFileSync(`${sdk}types.gen.d.ts`, "utf8"),
  ts.ScriptTarget.Latest,
  true,
);
const aliases = new Map();
for (const statement of source.statements) {
  if (ts.isTypeAliasDeclaration(statement)) aliases.set(statement.name.text, statement.type);
}

// The operations the adapter and the instance use (the SDK's class and method name, and its request type).
const OPERATIONS = [
  ["session.create", "SessionCreateData"],
  ["session.get", "SessionGetData"],
  ["session.update", "SessionUpdateData"],
  ["session.children", "SessionChildrenData"],
  ["session.messages", "SessionMessagesData"],
  ["session.status", "SessionStatusData"],
  ["session.promptAsync", "SessionPromptAsyncData"],
  ["session.command", "SessionCommandData"],
  ["session.summarize", "SessionSummarizeData"],
  ["session.abort", "SessionAbortData"],
  ["session.fork", "SessionForkData"],
  ["permission.reply", "PermissionReplyData"],
  ["question.reply", "QuestionReplyData"],
  ["mcp.status", "McpStatusData"],
  ["mcp.add", "McpAddData"],
  ["mcp.disconnect", "McpDisconnectData"],
  ["command.list", "CommandListData"],
  ["provider.list", "ProviderListData"],
  ["app.agents", "AppAgentsData"],
  ["app.skills", "AppSkillsData"],
];

const schemas = {};
const converting = new Set();
const convert = (node) => {
  if (ts.isParenthesizedTypeNode(node)) return convert(node.type);
  switch (node.kind) {
    case ts.SyntaxKind.StringKeyword:
      return { type: "string" };
    case ts.SyntaxKind.NumberKeyword:
      return { type: "number" };
    case ts.SyntaxKind.BooleanKeyword:
      return { type: "boolean" };
    case ts.SyntaxKind.NullKeyword:
      return { type: "null" };
    case ts.SyntaxKind.UnknownKeyword:
    case ts.SyntaxKind.AnyKeyword:
      return {};
    case ts.SyntaxKind.NeverKeyword:
      return { not: {} };
    case ts.SyntaxKind.UndefinedKeyword:
      return { type: "null" };
  }
  if (ts.isLiteralTypeNode(node)) {
    if (node.literal.kind === ts.SyntaxKind.NullKeyword) return { type: "null" };
    if (node.literal.kind === ts.SyntaxKind.TrueKeyword) return { const: true };
    if (node.literal.kind === ts.SyntaxKind.FalseKeyword) return { const: false };
    if (ts.isNumericLiteral(node.literal)) return { const: Number(node.literal.text) };
    return { const: node.literal.text };
  }
  if (ts.isArrayTypeNode(node)) return { type: "array", items: convert(node.elementType) };
  if (ts.isUnionTypeNode(node)) {
    const options = node.types
      .filter((type) => type.kind !== ts.SyntaxKind.UndefinedKeyword)
      .map(convert);
    return options.length === 1 ? options[0] : { anyOf: options };
  }
  if (ts.isIntersectionTypeNode(node)) return { allOf: node.types.map(convert) };
  if (ts.isTypeLiteralNode(node)) {
    const properties = {};
    const required = [];
    let additional = false;
    for (const member of node.members) {
      if (ts.isIndexSignatureDeclaration(member)) {
        additional = convert(member.type);
      } else if (ts.isPropertySignature(member)) {
        const name = member.name.text ?? member.name.getText(source).replace(/^["']|["']$/g, "");
        properties[name] = member.type ? convert(member.type) : {};
        if (!member.questionToken) required.push(name);
      }
    }
    return {
      type: "object",
      properties,
      ...(required.length > 0 ? { required } : {}),
      additionalProperties: additional,
    };
  }
  if (ts.isTypeReferenceNode(node)) {
    const name = node.typeName.getText(source);
    if (name === "Array" && node.typeArguments?.length === 1) {
      return { type: "array", items: convert(node.typeArguments[0]) };
    }
    if (name === "Record" && node.typeArguments?.length === 2) {
      return { type: "object", additionalProperties: convert(node.typeArguments[1]) };
    }
    const target = aliases.get(name);
    if (target === undefined) throw new Error(`unknown type ${name}`);
    if (!schemas[name] && !converting.has(name)) {
      converting.add(name);
      schemas[name] = convert(target);
      converting.delete(name);
    }
    return { $ref: `#/components/schemas/${name}` };
  }
  throw new Error(`cannot convert ${ts.SyntaxKind[node.kind]}: ${node.getText(source)}`);
};

const member = (literal, name) => {
  const found = literal.members.find(
    (entry) => ts.isPropertySignature(entry) && entry.name.text === name,
  );
  return found;
};
const operations = [];
for (const [id, type] of OPERATIONS) {
  const data = aliases.get(type);
  if (data === undefined || !ts.isTypeLiteralNode(data)) throw new Error(`${type} was not found`);
  const url = member(data, "url").type.literal.text;
  const method = sdkMethod(id);
  const parameters = [];
  for (const place of ["path", "query"]) {
    const entry = member(data, place);
    if (entry === undefined || entry.type.kind === ts.SyntaxKind.NeverKeyword) continue;
    for (const property of entry.type.members) {
      parameters.push({
        name: property.name.text,
        in: place,
        required: !property.questionToken,
        schema: convert(property.type),
      });
    }
  }
  const body = member(data, "body");
  const hasBody = body !== undefined && body.type.kind !== ts.SyntaxKind.NeverKeyword;
  // What a success answers with, so the fake can check its own answers against the protocol too.
  const responses = aliases.get(type.replace(/Data$/, "Responses"));
  const success =
    responses && ts.isTypeLiteralNode(responses) ? member(responses, "200") : undefined;
  operations.push({
    method,
    path: url,
    id,
    parameters,
    body: hasBody ? convert(body.type) : null,
    bodyRequired: hasBody && !body.questionToken,
    response: success?.type ? convert(success.type) : null,
  });
}

// The HTTP verb of each operation, from the client the SDK generates.
function sdkMethod(id) {
  const text = fs.readFileSync(`${sdk}sdk.gen.js`, "utf8");
  const [group, name] = id.split(".");
  const classes = {
    session: "Session2",
    permission: "Permission",
    question: "Question",
    mcp: "Mcp",
    command: "Command",
    provider: "Provider",
    app: "App",
  };
  const start = text.indexOf(`export class ${classes[group]} extends`);
  if (start < 0) throw new Error(`no class for ${id}`);
  const end = text.indexOf("\nexport class ", start + 1);
  const body = text.slice(start, end < 0 ? text.length : end);
  const at = body.search(new RegExp(`\\n    ${name}\\(`));
  if (at < 0) throw new Error(`no method for ${id}`);
  const verb = /\)\.(get|post|put|patch|delete)\(\{/.exec(body.slice(at));
  if (!verb) throw new Error(`no verb for ${id}`);
  return verb[1].toUpperCase();
}

const target = new URL("../crates/server/tests/fixtures/opencode1-openapi.json", import.meta.url);
fs.writeFileSync(
  target,
  JSON.stringify({
    source: "@opencode-ai/sdk/v2 types.gen.d.ts, request types of the used operations",
    operations,
    schemas: Object.fromEntries(
      Object.keys(schemas)
        .sort()
        .map((name) => [name, schemas[name]]),
    ),
  }) + "\n",
);
console.log(
  `${operations.length} operations, ${Object.keys(schemas).length} schemas, ${fs.statSync(target).size} bytes`,
);
