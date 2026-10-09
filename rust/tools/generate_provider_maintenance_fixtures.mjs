// From repo root, Node >=23.6: node rust/tools/generate_provider_maintenance_fixtures.mjs
// Runs the unchanged source providerMaintenance.ts, providerCompatibility.ts and shared semver against
// isolated native filesystem trees (fake package-manager layouts and fake `brew` scripts).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as PM from "../../apps/server/src/provider/providerMaintenance.ts";
import { resolveProviderCompatibility } from "../../apps/server/src/provider/providerCompatibility.ts";
import { compareSemverVersions, satisfiesSemverRange } from "../../packages/shared/src/semver.ts";

const R = "$ROOT";
const bundled = JSON.parse(
  fs.readFileSync("apps/server/src/provider/model-manifest.json", "utf8"),
).compatibility;
// The bundled policies ship with the server so a refresh classifies without the network.
fs.writeFileSync(
  "rust/crates/server/src/provider-compatibility.json",
  JSON.stringify(bundled, null, 2) + "\n",
);

const sign = (n) => (n < 0 ? -1 : n > 0 ? 1 : 0);
const fixtures = [];

// Pure: semver comparison and range gates.
const versions = [
  "1.0.0",
  "1.0.1",
  "1.2.0",
  "2.0.0",
  "1.0.0-beta.1",
  "1.0.0-beta.2",
  "1.0.0-alpha",
  "v1.2.3",
  "1.2",
  "20",
  "junk",
  "0.156.1",
  "0.159.0",
];
for (const left of versions)
  for (const right of versions)
    fixtures.push({
      kind: "compare",
      left,
      right,
      expected: sign(compareSemverVersions(left, right)),
    });
const ranges = [
  ">=0.156.0",
  ">=0.149.0 <0.156.0",
  "<0.149.0",
  "^1.2.3",
  "^0.2.3",
  "^0.0.3",
  "=1.1.1",
  ">=2.0.0 || <1.0.0",
  ">1",
  "<=1.2",
  "bad",
  "",
];
for (const version of [
  "0.156.1",
  "0.150.0",
  "0.100.0",
  "1.2.3",
  "1.9.0",
  "0.2.9",
  "0.3.0",
  "0.0.3",
  "0.0.4",
  "1.1.1",
  "2026.05.09",
  "1.0.0-beta.1",
  "v1.2.3",
  "junk",
])
  for (const range of ranges)
    fixtures.push({
      kind: "range",
      version,
      range,
      expected: satisfiesSemverRange(version, range),
    });

// Pure: compatibility advisory against the bundled policies.
for (const driver of [
  "codex",
  "claudeAgent",
  "cursor",
  "grok",
  "opencode",
  "antigravity",
  "pi",
  "acpRegistry",
])
  for (const t3 of ["0.0.43", "0.0.45", "0.1.0", "0.0.46"])
    for (const version of [
      null,
      "0.100.0",
      "0.150.0",
      "0.156.0",
      "0.159.0",
      "v0.160.1",
      "2.1.100",
      "2.1.200",
      "2.1.280",
      "2026.05.09-abc123",
      "agy_acp_server_1.1.1",
      "1.14.19",
      "2.0.18",
      "1.0.0-beta.1",
      "junk",
    ]) {
      const advisory = resolveProviderCompatibility(bundled, driver, version, t3) ?? null;
      fixtures.push({ kind: "compatibility", driver, t3, version, expected: advisory });
    }

// Pure: Homebrew info parsing.
for (const [infoJson, ownership] of [
  [
    '{"formulae":[{"versions":{"stable":"1.2.4"}}]}',
    { kind: "formula", name: "codex", prefix: "/opt/homebrew" },
  ],
  ['{"casks":[{"version":"2.0.0,55"}]}', { kind: "cask", name: "codex", prefix: "/opt/homebrew" }],
  [
    '{"casks":[{"version":"2.0.0,55"}]}',
    { kind: "formula", name: "codex", prefix: "/opt/homebrew" },
  ],
  ['{"formulae":[]}', { kind: "formula", name: "codex", prefix: "/opt/homebrew" }],
  [
    '{"formulae":[{"versions":{"stable":"  "}}]}',
    { kind: "formula", name: "codex", prefix: "/opt/homebrew" },
  ],
  ["not json", { kind: "formula", name: "codex", prefix: "/opt/homebrew" }],
])
  fixtures.push({
    kind: "homebrewInfo",
    infoJson,
    ownership,
    expected: PM.parseHomebrewLatestVersion(infoJson, ownership),
  });
for (const real of [
  "/opt/homebrew/Cellar/codex/1.2.3/bin/codex",
  "/usr/local/Caskroom/codex/2.0/Codex.app/x",
  "/opt/homebrew/Cellar/codex/bin",
  "C:\\Homebrew\\CELLAR\\codex\\9\\x",
  "/usr/bin/codex",
])
  fixtures.push({
    kind: "homebrewOwnership",
    real,
    expected: PM.homebrewOwnershipFromCommandPath(real),
  });
for (const [real, pkg] of [
  ["/usr/lib/node_modules/@openai/codex/bin/codex.js", "@openai/codex"],
  ["/home/u/.npm-global/lib/node_modules/@openai/codex/bin/codex.js", "@openai/codex"],
  ["/home/u/proj/node_modules/.bin/lib/node_modules/@openai/codex/x", "@openai/codex"],
  ["/home/u/.local/share/mise/installs/node/22/lib/node_modules/@openai/codex/x", "@openai/codex"],
  ["/home/u/.local/share/mise/installs/codex/1/lib/node_modules/@openai/codex/x", "@openai/codex"],
  ["/lib/node_modules/@openai/codex/x", "@openai/codex"],
  [
    "C:\\Users\\u\\AppData\\Roaming\\npm\\Lib\\node_modules\\@openai\\codex\\bin\\codex.js",
    "@openai/codex",
  ],
  ["/usr/bin/codex", "@openai/codex"],
])
  fixtures.push({
    kind: "npmPrefix",
    real,
    packageName: pkg,
    expected: PM.npmGlobalPrefixFromCommandPath(real, pkg),
  });

// Pure: copyable commands and targeted actions.
const caps = (input) => PM.makeProviderMaintenanceCapabilities(input);
for (const platform of ["linux", "darwin", "win32"])
  for (const executable of [
    "/usr/bin/npm",
    "/opt/my tools/npm",
    "C:\\Program Files\\nodejs\\npm.cmd",
    "it's/npm",
    "npm",
  ])
    fixtures.push({
      kind: "capabilities",
      platform,
      input: {
        provider: "codex",
        packageName: "@openai/codex",
        updateExecutable: executable,
        updateArgs: ["install", "-g", "--prefix", "/opt/a b", "@openai/codex@latest"],
        updateLockKey: "npm-global:/opt/a b",
      },
      expected: caps({
        provider: "codex",
        packageName: "@openai/codex",
        updateExecutable: executable,
        updateArgs: ["install", "-g", "--prefix", "/opt/a b", "@openai/codex@latest"],
        updateLockKey: "npm-global:/opt/a b",
        platform,
      }),
    });
for (const lockKey of [
  "npm-global:/x",
  "bun-global",
  "pnpm-global",
  "vite-plus-global",
  "yarn-global",
  "volta",
  "homebrew",
  "codex-native",
]) {
  const pkg = "@openai/codex";
  const argSets = [
    ["install", "-g", "--prefix", "/x", "--allow-scripts=@openai/codex", `${pkg}@latest`],
    ["add", "-g", pkg],
    ["update"],
    ["i", "-g", `${pkg}@latest`],
  ];
  for (const args of argSets) {
    const capabilities = caps({
      provider: "codex",
      packageName: pkg,
      updateExecutable: "/usr/bin/tool",
      updateArgs: args,
      updateLockKey: lockKey,
      platform: "linux",
    });
    for (const version of ["1.2.3", "1.2", "1.2.3-beta", "v1.2.3"])
      fixtures.push({
        kind: "targeted",
        capabilities,
        version,
        expected: PM.makeTargetedProviderUpdateAction(capabilities, version),
      });
  }
}
fixtures.push({
  kind: "targeted",
  capabilities: caps({
    provider: "x",
    packageName: null,
    updateExecutable: "/usr/bin/npm",
    updateArgs: ["i"],
    updateLockKey: "bun-global",
    platform: "linux",
  }),
  version: "1.2.3",
  expected: null,
});
fixtures.push({
  kind: "targeted",
  capabilities: PM.makeManualOnlyProviderMaintenanceCapabilities({
    provider: "codex",
    packageName: "@openai/codex",
  }),
  version: "1.2.3",
  expected: null,
});

// Pure: advisories.
for (const [current, latest] of [
  ["1.0.0", "1.0.1"],
  ["1.0.1", "1.0.0"],
  ["1.0.0", "1.0.0"],
  [null, "1.0.0"],
  ["1.0.0", null],
  ["1.0.0-beta", "1.0.0"],
  ["v1.0.0", "1.0.0"],
  ["junk", "1.0.0"],
])
  for (const withUpdate of [true, false]) {
    const capabilities = withUpdate
      ? caps({
          provider: "codex",
          packageName: "@openai/codex",
          updateExecutable: "/usr/bin/npm",
          updateArgs: ["install", "-g", "@openai/codex@latest"],
          updateLockKey: "npm-global:/usr",
          platform: "linux",
        })
      : PM.makeManualOnlyProviderMaintenanceCapabilities({
          provider: "codex",
          packageName: "@openai/codex",
        });
    fixtures.push({
      kind: "advisory",
      currentVersion: current,
      latestVersion: latest,
      checkedAt: "2026-01-01T00:00:00.000Z",
      capabilities,
      expected: PM.createProviderVersionAdvisory({
        driver: "codex",
        currentVersion: current,
        latestVersion: latest,
        checkedAt: "2026-01-01T00:00:00.000Z",
        maintenanceCapabilities: capabilities,
      }),
    });
  }

// Filesystem: ownership resolution over fake installs.
const isCodexStandalone = (p) => PM.normalizeCommandPath(p).includes("/packages/standalone/");
const isClaudeNative = (p) => {
  const n = PM.normalizeCommandPath(p);
  return (
    n.endsWith("/.local/bin/claude") ||
    n.endsWith("/.local/bin/claude.exe") ||
    n.includes("/.local/share/claude/")
  );
};
const definitions = {
  codex: {
    provider: "codex",
    npmPackageName: "@openai/codex",
    nativeUpdate: {
      args: ["update"],
      isCommandPath: isCodexStandalone,
      env: { CODEX_HOME: R + "/codexhome" },
    },
  },
  claude: {
    provider: "claudeAgent",
    npmPackageName: "@anthropic-ai/claude-code",
    nativeUpdate: { args: ["update"], isCommandPath: isClaudeNative },
  },
  plain: {
    provider: "codex",
    npmPackageName: "@example/package-tool",
    nativeUpdate: { args: ["self-update"] },
  },
  manual: { provider: "codex", npmPackageName: "@example/package-tool", nativeUpdate: null },
};
const file = (p, contents = "#!/bin/sh\nexit 0\n", mode = 0o755) => ({ path: p, contents, mode });
const dir = (p) => ({ path: p, directory: true });
const link = (p, target) => ({ path: p, link: target });
const brew = (prefix, info) =>
  file(
    "brew/bin/brew",
    `#!/bin/sh\ncase "$1" in\n--prefix) echo "${prefix}";;\ninfo) echo '${info}';;\nesac\n`,
  );
const formulaInfo = '{"formulae":[{"versions":{"stable":"1.2.4"}}]}';
const caskInfo = '{"casks":[{"version":"2.0.0,55"}]}';
const cases = [];
const add = (label, definition, binaryPath, files, env = {}) =>
  cases.push({ label, definition, binaryPath, files, env });
add("blank binary path", "codex", "  ", []);
add("binary missing", "codex", R + "/nope/codex", []);
add("bare name not on PATH", "codex", "codex", [dir("empty")], { PATH: R + "/empty" });
add("bare name found on PATH", "plain", "tool", [file("bin/tool")], { PATH: R + "/bin" });
add("codex standalone install", "codex", R + "/h/.codex/packages/standalone/bin/codex", [
  file("h/.codex/packages/standalone/bin/codex"),
]);
add("claude native launcher", "claude", R + "/h/.local/bin/claude", [file("h/.local/bin/claude")]);
add("claude native versioned", "claude", R + "/h/.local/share/claude/versions/1/claude", [
  file("h/.local/share/claude/versions/1/claude"),
]);
add("npm global posix", "codex", R + "/npm/bin/codex", [
  file("npm/lib/node_modules/@openai/codex/bin/codex.js"),
  link("npm/bin/codex", "../lib/node_modules/@openai/codex/bin/codex.js"),
]);
add("npm global with spaces", "codex", R + "/np m/bin/codex", [
  file("np m/lib/node_modules/@openai/codex/bin/codex.js"),
  link("np m/bin/codex", "../lib/node_modules/@openai/codex/bin/codex.js"),
]);
add("project node_modules is manual", "codex", R + "/proj/node_modules/.bin/codex", [
  file("proj/node_modules/.bin/codex"),
]);
add("project node_modules without native", "manual", R + "/proj/node_modules/.bin/tool", [
  file("proj/node_modules/.bin/tool"),
]);
add("plain local install uses native updater", "plain", R + "/bin/tool", [file("bin/tool")]);
add("plain local install without native updater", "manual", R + "/bin/tool", [file("bin/tool")]);
add("pnpm global", "codex", R + "/.local/share/pnpm/codex", [file(".local/share/pnpm/codex")]);
add("bun global", "codex", R + "/.bun/bin/codex", [file(".bun/bin/codex")]);
add("vite-plus global", "codex", R + "/.vite-plus/bin/codex", [file(".vite-plus/bin/codex")]);
add("yarn global", "codex", R + "/.config/yarn/global/node_modules/.bin/codex", [
  file(".config/yarn/global/node_modules/.bin/codex"),
]);
add("volta package", "codex", R + "/volta/bin/codex", [
  file("volta/bin/volta-shim"),
  link("volta/bin/codex", "volta-shim"),
  dir("volta/tools/image/packages/@openai/codex"),
]);
add("volta shim without the package", "codex", R + "/volta/bin/codex", [
  file("volta/bin/volta-shim"),
  link("volta/bin/codex", "volta-shim"),
]);
add("mise install", "codex", R + "/mise/installs/codex/1.0/bin/codex", [
  file("mise/installs/codex/1.0/bin/codex"),
]);
add("mise wrapper script", "codex", R + "/bin/codex", [
  file("bin/codex", '#!/bin/sh\nexec mise x codex -- codex "$@"\n'),
]);
add("homebrew formula", "codex", R + "/brew/bin/codex", [
  file("brew/Cellar/codex/1.2.3/bin/codex"),
  link("brew/bin/codex", "../Cellar/codex/1.2.3/bin/codex"),
  brew(R + "/brew", formulaInfo),
]);
add("homebrew cask", "codex", R + "/brew/bin/codex", [
  file("brew/Caskroom/codex/2.0.0/bin/codex"),
  link("brew/bin/codex", "../Caskroom/codex/2.0.0/bin/codex"),
  brew(R + "/brew", caskInfo),
]);
add("homebrew prefix mismatch", "codex", R + "/brew/bin/codex", [
  file("brew/Cellar/codex/1.2.3/bin/codex"),
  link("brew/bin/codex", "../Cellar/codex/1.2.3/bin/codex"),
  brew(R + "/elsewhere", formulaInfo),
]);
add("homebrew prefix mismatch with native fallback", "plain", R + "/brew/bin/codex", [
  file("brew/Cellar/codex/1.2.3/bin/codex"),
  link("brew/bin/codex", "../Cellar/codex/1.2.3/bin/codex"),
  brew(R + "/elsewhere", formulaInfo),
]);
add(
  "homebrew without brew",
  "plain",
  R + "/brew2/bin/codex",
  [
    file("brew2/Cellar/codex/1.2.3/bin/codex"),
    link("brew2/bin/codex", "../Cellar/codex/1.2.3/bin/codex"),
  ],
  { PATH: R + "/empty" },
);
add("homebrew mise formula", "codex", R + "/brew/bin/mise", [
  file("brew/Cellar/mise/1.0/bin/mise"),
  link("brew/bin/mise", "../Cellar/mise/1.0/bin/mise"),
  brew(R + "/brew", formulaInfo),
]);
add("homebrew node globals are npm's", "codex", R + "/brew/bin/codex", [
  file("brew/Cellar/node/22/lib/node_modules/@openai/codex/bin/codex.js"),
  link("brew/bin/codex", "../Cellar/node/22/lib/node_modules/@openai/codex/bin/codex.js"),
  brew(R + "/brew", formulaInfo),
]);
add("homebrew info failure", "codex", R + "/brew/bin/codex", [
  file("brew/Cellar/codex/1.2.3/bin/codex"),
  link("brew/bin/codex", "../Cellar/codex/1.2.3/bin/codex"),
  brew(R + "/brew", "not json"),
]);

const definitionFor = (name, root) => {
  const base = definitions[name];
  const env =
    base.nativeUpdate?.env &&
    Object.fromEntries(
      Object.entries(base.nativeUpdate.env).map(([k, v]) => [k, v.replaceAll(R, root)]),
    );
  return {
    ...base,
    nativeUpdate: base.nativeUpdate && { ...base.nativeUpdate, ...(env ? { env } : {}) },
  };
};
for (const c of cases) {
  const root = fs.realpathSync(
    fs.mkdtempSync(path.join(os.tmpdir(), "t3port-maintenance-oracle-")),
  );
  try {
    for (const f of c.files) {
      const destination = path.join(root, f.path);
      if (f.directory) fs.mkdirSync(destination, { recursive: true });
      else if (f.link !== undefined) {
        fs.mkdirSync(path.dirname(destination), { recursive: true });
        fs.symlinkSync(f.link, destination);
      } else {
        fs.mkdirSync(path.dirname(destination), { recursive: true });
        fs.writeFileSync(destination, f.contents.replaceAll(R, root));
        fs.chmodSync(destination, f.mode);
      }
    }
    const env = Object.fromEntries(
      Object.entries(c.env).map(([k, v]) => [k, v.replaceAll(R, root)]),
    );
    const resolver = PM.makePackageManagedProviderMaintenanceResolver(
      definitionFor(c.definition, root),
    );
    const out = await Effect.runPromise(
      PM.resolveProviderMaintenanceCapabilitiesEffect(resolver, {
        binaryPath: c.binaryPath.replaceAll(R, root),
        env,
      }).pipe(Effect.provide(NodeServices.layer)),
    );
    // Lock keys are lowercased by the source, so the root also appears in lowercase ($root).
    const normalized = JSON.parse(
      JSON.stringify(out).replaceAll(root, R).replaceAll(root.toLowerCase(), "$root"),
    );
    fixtures.push({
      kind: "resolve",
      label: c.label,
      definition: c.definition,
      binaryPath: c.binaryPath,
      files: c.files,
      env: c.env,
      expected: normalized,
    });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}
fs.writeFileSync(
  "rust/crates/server/tests/fixtures/provider-maintenance.jsonl",
  fixtures.map((f) => JSON.stringify(f)).join("\n") + "\n",
);
console.log(fixtures.length + " unchanged-source provider maintenance witnesses");
