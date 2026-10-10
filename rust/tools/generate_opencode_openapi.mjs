// Development oracle: the OpenCode 2 HTTP surface the Rust adapter uses, as the original's protocol package
// (`@opencode/protocol`, the same HttpApi the original's client is generated from) describes it. The fake server
// checks every request the Rust client sends against it (path, query, body). Written next to the replay fixtures.
import fs from "node:fs";

const base = new URL("../../node_modules/.pnpm/", import.meta.url).pathname;
const find = (prefix) => fs.readdirSync(base).find((name) => name.startsWith(prefix));
const proto = `${base}${find("@opencode+protocol")}/node_modules/@opencode/protocol/dist/`;
const eff = `${base}${find("effect@4")}/node_modules/effect/dist/`;
const { makeDefaultApi } = await import(`${proto}api.js`);
const { HttpApiMiddleware, OpenApi } = await import(`${eff}http-api/index.js`);
const middleware = (name) => class extends HttpApiMiddleware.Service()(`oc/${name}`) {};
const api = makeDefaultApi({
  locationMiddleware: middleware("location"),
  formLocationMiddleware: middleware("form"),
  sessionLocationMiddleware: middleware("session"),
});
const spec = OpenApi.fromApi(api);

const USED = [
  /^\/api\/info$/,
  /^\/api\/event$/,
  /^\/api\/model$/,
  /^\/api\/agent$/,
  /^\/api\/command$/,
  /^\/api\/skill$/,
  /^\/api\/session(\/|$)/,
  /^\/api\/experimental\/mcp\/\{server\}$/,
  /^\/api\/experimental\/session\/\{sessionID\}\/instructions/,
];
const paths = {};
for (const [path, item] of Object.entries(spec.paths)) {
  if (!USED.some((pattern) => pattern.test(path))) continue;
  paths[path] = item;
}
// Keep only the schemas the used operations reach.
const schemas = spec.components?.schemas ?? {};
const keep = new Set();
const visit = (node) => {
  if (Array.isArray(node)) return node.forEach(visit);
  if (node === null || typeof node !== "object") return;
  if (typeof node.$ref === "string") {
    const name = node.$ref.replace("#/components/schemas/", "");
    if (!keep.has(name) && schemas[name] !== undefined) {
      keep.add(name);
      visit(schemas[name]);
    }
  }
  Object.values(node).forEach(visit);
};
visit(paths);
const pruned = Object.fromEntries([...keep].sort().map((name) => [name, schemas[name]]));
const operations = Object.entries(paths).flatMap(([path, item]) =>
  Object.entries(item)
    .filter(([method]) => ["get", "post", "put", "patch", "delete"].includes(method))
    .map(([method, operation]) => ({
      method: method.toUpperCase(),
      path,
      id: operation.operationId,
      parameters: (operation.parameters ?? []).map((parameter) => ({
        name: parameter.name,
        in: parameter.in,
        required: parameter.required === true,
        schema: parameter.schema,
      })),
      body: operation.requestBody?.content?.["application/json"]?.schema ?? null,
      bodyRequired: operation.requestBody?.required === true,
    })),
);
const target = new URL("../crates/server/tests/fixtures/opencode2-openapi.json", import.meta.url);
fs.writeFileSync(
  target,
  JSON.stringify({
    source: "@opencode/protocol makeDefaultApi, OpenApi.fromApi",
    operations,
    schemas: pruned,
  }) + "\n",
);
console.log(
  `${operations.length} operations, ${Object.keys(pruned).length} schemas, ${fs.statSync(target).size} bytes`,
);
