// From repo root, Node >=24.13.1: node rust/tools/generate_acp_registry_path_fixtures.mjs
// Executes the unchanged source managed-directory service against isolated native filesystem trees.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { stripTypeScriptTypes } from "node:module";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as Option from "../../packages/contracts/node_modules/effect/dist/Option.js";
import * as FileSystem from "../../packages/contracts/node_modules/effect/dist/FileSystem.js";
import * as Path from "../../packages/contracts/node_modules/effect/dist/Path.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import { TrimmedNonEmptyString } from "../../packages/contracts/src/baseSchemas.ts";
const source = fs.readFileSync("apps/server/src/provider/acp/AcpRegistrySupport.ts", "utf8");
function fragment(start, end) {
  const begin = source.indexOf(start),
    finish = source.indexOf(end, begin);
  if (begin < 0 || finish < begin) throw Error("Missing original markers: " + start);
  return stripTypeScriptTypes(source.slice(begin, finish), { mode: "transform" });
}
const decoders = new Function(
  "Schema",
  "TrimmedNonEmptyString",
  fragment("const BoundedAgentId =", "export const AcpRegistryErrorReason") +
    ";return {decodePackageInstallReceipt,decodeJson,decodeRegistryIndexEnvelope,decodeRegistryAgent};",
)(Schema, TrimmedNonEmptyString);
const platform = new Function(
  fragment(
    "export function resolveAcpRegistryPlatformTarget(",
    "interface ExactPackageSpec",
  ).replace("export function", "function") + ";return resolveAcpRegistryPlatformTarget;",
)();
const command = new Function(
  fragment("function normalizeRegistryCommandPath(", "function validateArchiveEntries(") +
    ";return normalizeRegistryCommandPath;",
)();
const managed = new Function(
  "Effect",
  "Option",
  "resolveAcpRegistryPlatformTarget",
  "normalizeRegistryCommandPath",
  ...Object.keys(decoders),
  fragment(
    "export const acpRegistryManagedBinaryDirectories =",
    "export interface ResolvedAcpRegistryDistribution",
  ).replace("export const", "const") + ";return acpRegistryManagedBinaryDirectories;",
)(Effect, Option, platform, command, ...Object.values(decoders));
const R = "$ROOT";
const base = {
  id: "kimi",
  name: "Kimi",
  version: "1.2.0",
  description: "Agent",
  distribution: {
    binary: { "linux-x86_64": { archive: "https://example.com/kimi", cmd: "bin/kimi" } },
  },
};
const receipt = (id = "gemini", version = "1.0.0", distribution = "npx", windows = false) => {
  const binDirectory =
    R +
    "/tools/" +
    id +
    "/" +
    encodeURIComponent(version) +
    "/" +
    (distribution === "npx" ? "npm" : "python") +
    (distribution === "npx" && windows ? "" : "/bin");
  return {
    agentId: id,
    agentVersion: version,
    distribution,
    packageSpec: "pkg@1.0.0",
    managerPath: "/usr/bin/manager",
    binDirectory,
    executablePath: binDirectory + "/agent",
  };
};
const fixtures = [];
function add(label, files, platform = "linux", architecture = "x64") {
  fixtures.push({ label, platform, architecture, files });
}
function install(p, contents = "") {
  return { path: p, ...(contents === null ? { directory: true } : { contents }) };
}
const registry = (agents = [base], version = "1.0.0") =>
  install("cache/acp-registry/registry.json", JSON.stringify({ version, agents }));
function packages(...receipts) {
  return receipts.flatMap((r, i) => [
    install("cache/acp-registry/package-installs/" + i + ".json", JSON.stringify(r)),
    install(r.executablePath.slice(R.length + 1)),
  ]);
}
const binary = [install("tools/kimi/1.2.0/linux-x86_64/bin", null)];
add("package before cached command directory", [...packages(receipt()), registry(), ...binary]);
add("binary versions numeric descending", [
  registry(),
  ...["1.2.0", "1.10.0", "1.9.0", "1.01.0", "1.1.0"].map((v) =>
    install("tools/kimi/" + v + "/linux-x86_64/bin", null),
  ),
]);
add("cached encoded version command subdirectory", [
  registry([
    {
      ...base,
      version: "1.0.0+build",
      distribution: {
        binary: { "linux-x86_64": { archive: "https://example.com", cmd: " ./nested\\agent " } },
      },
    },
  ]),
  install("tools/kimi/1.0.0%2Bbuild/linux-x86_64/nested", null),
]);
add("last cached duplicate agent wins", [
  registry([
    base,
    {
      ...base,
      distribution: {
        binary: { "linux-x86_64": { archive: "https://example.com", cmd: "second/agent" } },
      },
    },
  ]),
  ...binary,
  install("tools/kimi/1.2.0/linux-x86_64/second", null),
]);
add(
  "stable numeric-equivalent version ties",
  ["v1", "v01", "v001", "v2", "v10"].map((v) => install("tools/kimi/" + v + "/linux-x86_64", null)),
);
add("missing everything", []);
for (const contents of [
  "not json",
  "[]",
  JSON.stringify({ ...receipt(), agentId: "UPPER" }),
  JSON.stringify({ ...receipt(), agentVersion: ".." }),
  JSON.stringify({ ...receipt(), distribution: "binary" }),
  JSON.stringify({ ...receipt(), managerPath: null }),
  JSON.stringify({ ...receipt(), packageRoot: 5 }),
])
  add("invalid receipt " + contents, [
    install("cache/acp-registry/package-installs/bad.json", contents),
    install(receipt().executablePath.slice(R.length + 1)),
  ]);
for (const changes of [
  { packageRoot: null, packageVersion: null },
  { agentVersion: " 1.0.0 " },
  { agentVersion: "1.0.0\n" },
  { agentVersion: "1.0.0\u0085" },
  { agentVersion: "1.0.0/" },
  { agentVersion: "." },
  { executablePath: receipt().binDirectory + "/./agent" },
  { executablePath: receipt().binDirectory + "//agent" },
  { extra: "ignored" },
  { binDirectory: R + "/outside" },
  { executablePath: R + "/outside/agent" },
])
  add("receipt variants " + JSON.stringify(changes), packages({ ...receipt(), ...changes }));
add("missing receipt executable", [
  install("cache/acp-registry/package-installs/ok.json", JSON.stringify(receipt())),
]);
add("package duplicates stable deduplicated", packages(receipt(), receipt()));
add(
  "package receipts filename UTF16 order",
  packages(receipt("z-last"), receipt("a-first"), receipt("python", "1.0.0", "uvx")),
);
add(
  "unsupported target still package dirs",
  [...packages(receipt(), receipt("python", "1.0.0", "uvx")), registry(), ...binary],
  "freebsd",
  "x64",
);
add(
  "Windows npm prefix executable",
  [...packages(receipt("gemini", "1.0.0", "npx", true))],
  "win32",
  "x64",
);
add("Windows wrong POSIX npm receipt", packages(receipt()), "win32", "x64");
for (const contents of [
  "invalid",
  JSON.stringify({ version: "..", agents: [base] }),
  JSON.stringify({ version: "1.0.0", agents: null }),
  JSON.stringify({ version: "1.0.0", agents: Array(513).fill(base) }),
])
  add("damaged cache falls back to install root " + contents.slice(0, 45), [
    install("cache/acp-registry/registry.json", contents),
    ...binary,
  ]);
for (const cmd of [
  "../escape",
  "/absolute",
  "C:\\absolute",
  ".",
  "./bin//agent",
  "bin/missing/agent",
])
  add("cached command " + cmd, [
    registry([
      {
        ...base,
        distribution: { binary: { "linux-x86_64": { archive: "https://example.com", cmd } } },
      },
    ]),
    ...binary,
  ]);
add("invalid agent discarded but install still listed", [
  registry([{ ...base, id: "BAD" }]),
  ...binary,
]);
add("missing cached directory omitted", [
  registry(),
  install("tools/kimi/1.2.0/linux-x86_64", null),
]);
add("legacy install without cache retains platform root", binary);
for (const f of fixtures) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "t3port-managed-path-oracle-"));
  try {
    for (const file of f.files) {
      const destination = path.join(root, file.path);
      if (file.directory) fs.mkdirSync(destination, { recursive: true });
      else {
        fs.mkdirSync(path.dirname(destination), { recursive: true });
        fs.writeFileSync(destination, file.contents.replaceAll(R, root));
      }
    }
    const output = await Effect.runPromise(
      Effect.gen(function* () {
        const fileSystem = yield* FileSystem.FileSystem,
          path = yield* Path.Path;
        return yield* managed({
          fileSystem,
          path,
          cacheDir: root + "/cache",
          toolsDir: root + "/tools",
          platform: f.platform,
          architecture: f.architecture,
        });
      }).pipe(Effect.provide(NodeServices.layer)),
    );
    f.output = output.map((p) => p.replaceAll(root, R));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}
fs.writeFileSync(
  "rust/crates/server/tests/fixtures/acp-registry-path.jsonl",
  fixtures.map(JSON.stringify).join("\n") + "\n",
);
console.log(fixtures.length + " unchanged-source native managed PATH witnesses");
