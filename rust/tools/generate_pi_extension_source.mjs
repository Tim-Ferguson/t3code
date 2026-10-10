// Writes the extension T3 Code loads into every Pi session (permission hook and T3 MCP bridge) exactly as the
// original's `PI_T3_MCP_EXTENSION_SOURCE` builds it, so the Rust server writes the same file.
import { writeFileSync } from "node:fs";
import { PI_T3_MCP_EXTENSION_SOURCE } from "../../apps/server/src/orchestration-v2/Adapters/piT3McpExtensionSource.ts";

writeFileSync(
  new URL("../crates/server/src/pi-t3-mcp-extension.ts.txt", import.meta.url),
  PI_T3_MCP_EXTENSION_SOURCE,
);
console.log("wrote the Pi extension source");
process.exit(0);
