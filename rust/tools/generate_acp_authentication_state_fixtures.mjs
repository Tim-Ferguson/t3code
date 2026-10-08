// Node24 development oracle; runtime and tests never execute TypeScript.
// PATH=/tmp/node-v24.13.1-darwin-arm64/bin:$PATH node rust/tools/generate_acp_authentication_state_fixtures.mjs
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import { AcpRegistrySettings } from "../../packages/contracts/src/settings.ts";
import { makeAcpRegistryAuthenticationState } from "../../apps/server/src/provider/acp/AcpRegistryAuthenticationState.ts";

const base = {
  instanceId: "acp_devin",
  settings: Schema.decodeSync(AcpRegistrySettings)({ agentId: "devin" }),
  environment: [
    { name: "Z_TOKEN", value: "test-only-secret", sensitive: true },
    { name: "A_TOKEN", value: "second-secret", sensitive: false },
  ],
  processEnvironment: { HOME: "/home/test" },
};
const cases = [
  ["unchanged", {}],
  [
    "cosmetic-enabled-models",
    {
      settings: { ...base.settings, enabled: !base.settings.enabled, customModels: ["new-model"] },
    },
  ],
  ["registry-ignores-command-args", { settings: { ...base.settings, commandArgs: ["ignored"] } }],
  ["agent", { settings: { ...base.settings, agentId: "other-agent" } }],
  ["method", { settings: { ...base.settings, authMethodId: "enterprise" } }],
  ["executable", { settings: { ...base.settings, commandPath: "/other/devin" } }],
  ["distribution", { settings: { ...base.settings, distribution: "binary" } }],
  ["local-empty", { settings: { ...base.settings, source: "local" } }],
  [
    "local-args",
    { settings: { ...base.settings, source: "local", commandArgs: ["--name", "😃", ""] } },
  ],
  [
    "environment-value",
    { environment: [{ ...base.environment[0], value: "different-secret" }, base.environment[1]] },
  ],
  [
    "environment-sensitive",
    { environment: [{ ...base.environment[0], sensitive: false }, base.environment[1]] },
  ],
  ["environment-order", { environment: [...base.environment].reverse() }],
  [
    "environment-equal-collation",
    {
      environment: [
        { name: "a", value: "one", sensitive: false },
        { name: "A", value: "two", sensitive: true },
        { name: "a", value: "three", sensitive: false },
      ],
    },
  ],
  [
    "environment-redaction-flag",
    { environment: [{ ...base.environment[0], valueRedacted: false }, base.environment[1]] },
  ],
  ["other-instance", { instanceId: "acp_other" }],
  ["profile-empty", { processEnvironment: { HOME: "" } }],
  ["profile-missing", { processEnvironment: {} }],
  ...["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "APPDATA", "LOCALAPPDATA"].map(
    (name) => [
      "profile-" + name,
      { processEnvironment: { ...base.processEnvironment, [name]: "/another/profile" } },
    ],
  ),
  [
    "unrelated-process-env",
    { processEnvironment: { ...base.processEnvironment, OTHER: "irrelevant" } },
  ],
];
const rows = [];
for (const [label, patch] of cases) {
  const cacheDir = fs.mkdtempSync(path.join(os.tmpdir(), "t3port-acp-auth-state-"));
  try {
    const input = { ...base, ...patch };
    const output = await Effect.runPromise(
      Effect.gen(function* () {
        const first = yield* makeAcpRegistryAuthenticationState({ ...base, cacheDir });
        const initial = yield* first.get;
        yield* first.set(true);
        const beforeName = fs.readdirSync(cacheDir)[0];
        const before = JSON.parse(fs.readFileSync(path.join(cacheDir, beforeName), "utf8"));
        const changed = yield* makeAcpRegistryAuthenticationState({ ...input, cacheDir });
        const confirmed = yield* changed.get;
        yield* changed.set(true);
        const names = fs.readdirSync(cacheDir);
        const after = names.map((name) => ({
          name,
          ...JSON.parse(fs.readFileSync(path.join(cacheDir, name), "utf8")),
        }));
        const restored = yield* makeAcpRegistryAuthenticationState({ ...base, cacheDir });
        return {
          initial,
          before: { name: beforeName, ...before },
          confirmed,
          after,
          restored: yield* restored.get,
        };
      }).pipe(Effect.provide(NodeServices.layer)),
    );
    rows.push({ label, base, input, output });
  } finally {
    fs.rmSync(cacheDir, { recursive: true, force: true });
  }
}
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/acp-authentication-state.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log("Generated " + rows.length + " actual-source authentication confirmation witnesses");
