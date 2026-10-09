// Development oracle: the capability constants the original adapters declare for Codex, Claude
// and an ACP agent (before negotiation). The Rust capability files are compared with them and
// may differ only where the port documents a feature it has not ported.
import { renameSync, writeFileSync } from "node:fs";
import { CodexProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts";
import { ClaudeProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts";
import { AcpProviderCapabilitiesV2 } from "../../apps/server/src/orchestration-v2/Adapters/AcpAdapterV2.ts";

const plain = (value) => JSON.parse(JSON.stringify(value));
const rows = {
  codex: plain(CodexProviderCapabilitiesV2),
  claudeAgent: plain(ClaudeProviderCapabilitiesV2),
  acpRegistry: plain(AcpProviderCapabilitiesV2),
};
const target = new URL(
  "../crates/server/tests/fixtures/provider-capabilities.json",
  import.meta.url,
);
const staging = new URL(
  "../crates/server/tests/fixtures/provider-capabilities.json.tmp",
  import.meta.url,
);
writeFileSync(staging, JSON.stringify(rows, null, 1) + "\n");
renameSync(staging, target);
console.log("wrote 3 capability sets");
