// Development oracle: the capability constants the original adapters declare for Codex, Claude,
// an ACP agent and Grok (before negotiation). The Rust capability files are compared with them and
// may differ only where the port documents a feature it has not ported.
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { ProviderInstanceId } from "../../packages/contracts/src/index.ts";
import { CodexProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts";
import { ClaudeProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts";
import { AcpProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts";
import { GrokProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/GrokAdapterV2.ts";
import { makePiAdapterV2 } from "../../apps/server/src/orchestration-v2/Adapters/PiAdapterV2.ts";

const plain = (value) => JSON.parse(JSON.stringify(value));
// OpenCode 2.x declares its capabilities as a module constant that is not exported. The literal holds only plain
// values, so it is read from the source and evaluated alone. (1.x declares another set, below.)
const openCode2 = () => {
  const source = readFileSync(
    new URL(
      "../../apps/server/src/orchestration-v2/Adapters/OpenCode2AdapterV2.ts",
      import.meta.url,
    ),
    "utf8",
  );
  const start = source.indexOf("const OpenCode2ProviderCapabilities = {");
  const end = source.indexOf("} satisfies OrchestrationV2ProviderCapabilities;", start);
  if (start < 0 || end < 0) throw new Error("OpenCode2ProviderCapabilities was not found");
  const literal = source.slice(source.indexOf("{", start), end + 1);
  return plain(new Function(`return (${literal});`)());
};
// OpenCode 1.x declares its capabilities the same way, in its own adapter.
const openCode1 = () => {
  const source = readFileSync(
    new URL(
      "../../apps/server/src/orchestration-v2/Adapters/OpenCodeAdapterV2.ts",
      import.meta.url,
    ),
    "utf8",
  );
  const start = source.indexOf("const OpenCodeProviderCapabilitiesV2 = {");
  const end = source.indexOf("} satisfies OrchestrationV2ProviderCapabilities;", start);
  if (start < 0 || end < 0) throw new Error("OpenCodeProviderCapabilitiesV2 was not found");
  const literal = source.slice(source.indexOf("{", start), end + 1);
  return plain(new Function(`return (${literal});`)());
};
// Pi declares its capabilities on the adapter it builds; nothing is spawned to read them.
const pi = makePiAdapterV2({
  instanceId: ProviderInstanceId.make("pi"),
  settings: {},
  environment: {},
  spawner: undefined,
  fileSystem: undefined,
  idAllocator: undefined,
  serverConfig: undefined,
});
const rows = {
  codex: plain(CodexProviderCapabilitiesV2),
  claudeAgent: plain(ClaudeProviderCapabilitiesV2),
  acpRegistry: plain(AcpProviderCapabilitiesV2),
  grok: plain(GrokProviderCapabilitiesV2),
  pi: plain(await Effect.runPromise(pi.getCapabilities())),
  opencode: openCode2(),
  opencode1: openCode1(),
};
const target = new URL(
  "../crates/server/tests/fixtures/provider-capabilities.json",
  import.meta.url,
);
const staging = new URL(
  "../crates/server/tests/fixtures/provider-capabilities.json.tmp",
  import.meta.url,
);
// Rows other providers added to the file are kept: only the sets generated here are replaced.
const existing = JSON.parse(readFileSync(target, "utf8"));
writeFileSync(staging, JSON.stringify({ ...existing, ...rows }, null, 1) + "\n");
renameSync(staging, target);
console.log(`wrote ${Object.keys(rows).length} capability sets`);
process.exit(0);
