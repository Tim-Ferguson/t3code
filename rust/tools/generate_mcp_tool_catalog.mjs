// Register every original T3 MCP toolkit with inert handlers, and write each tool as the model reads it.
// Gate metadata is read from each toolkit's handlers file separately; this file is only descriptions and schemas.
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as McpServer from "../../apps/server/node_modules/effect/dist/ai/McpServer.js";
import { OrchestratorToolkit } from "../../apps/server/src/mcp/toolkits/orchestrator/tools.ts";
import { ThreadToolkit } from "../../apps/server/src/mcp/toolkits/thread/tools.ts";
import { AttachmentToolkit } from "../../apps/server/src/mcp/toolkits/attachment/tools.ts";
import { ProjectToolkit } from "../../apps/server/src/mcp/toolkits/project/tools.ts";
import { EnvironmentToolkit } from "../../apps/server/src/mcp/toolkits/environment/tools.ts";
import { PreviewControlsToolkit } from "../../apps/server/src/mcp/toolkits/previewControls/tools.ts";
import { WorktreeToolkit } from "../../apps/server/src/mcp/toolkits/worktree/tools.ts";
import { PullRequestsToolkit } from "../../apps/server/src/mcp/toolkits/pullRequests/tools.ts";
import {
  PreviewSnapshotToolkit,
  PreviewStandardToolkit,
} from "../../apps/server/src/mcp/toolkits/preview/tools.ts";
import {
  HtmlPreviewToolkit,
  HtmlRenderToolkit,
} from "../../apps/server/src/mcp/toolkits/html/tools.ts";
import {
  DeviceScreenshotToolkit,
  DeviceStandardToolkit,
} from "../../apps/server/src/mcp/toolkits/device/tools.ts";
import { readFileSync, writeFileSync } from "node:fs";
const toolkits = [
  ["orchestrator", OrchestratorToolkit],
  ["thread", ThreadToolkit],
  ["attachment", AttachmentToolkit],
  ["project", ProjectToolkit],
  ["environment", EnvironmentToolkit],
  ["previewControls", PreviewControlsToolkit],
  ["worktree", WorktreeToolkit],
  ["pullRequests", PullRequestsToolkit],
  ["previewStandard", PreviewStandardToolkit],
  ["previewSnapshot", PreviewSnapshotToolkit],
  ["htmlRender", HtmlRenderToolkit],
  ["htmlPreview", HtmlPreviewToolkit],
  ["deviceStandard", DeviceStandardToolkit],
  ["deviceScreenshot", DeviceScreenshotToolkit],
];
const out = [];
for (const [group, toolkit] of toolkits) {
  const handlers = Object.fromEntries(
    Object.keys(toolkit.tools).map((name) => [name, () => Effect.die("catalog only")]),
  );
  const layer = McpServer.toolkit(toolkit).pipe(
    Layer.provide(toolkit.toLayer(handlers)),
    Layer.provideMerge(McpServer.McpServer.layer),
  );
  const tools = await Effect.runPromise(
    Effect.scoped(
      Effect.gen(function* () {
        const server = yield* McpServer.McpServer;
        return server.tools.map(({ tool }) => JSON.parse(JSON.stringify(tool)));
      }),
    ).pipe(Effect.provide(layer)),
  );
  for (const tool of tools) {
    // The three image tools are registered by hand in McpHttpServer.ts, which declares no output schema.
    if (["preview_snapshot", "html_preview", "device_screenshot"].includes(tool.name))
      delete tool.outputSchema;
    out.push({ group, tool });
  }
}
const target = new URL("../crates/server/src/mcp_tool_catalog.json", import.meta.url);
const text = JSON.stringify(out, null, 1) + "\n";
if (process.argv.includes("--check")) {
  // Regenerates in memory and fails when the committed catalog is not what the original registers.
  // Parsed, so a formatter's whitespace is not a difference.
  const same = JSON.stringify(JSON.parse(readFileSync(target, "utf8"))) === JSON.stringify(out);
  console.log(JSON.stringify({ tools: out.length, same }));
  process.exit(same ? 0 : 1);
}
writeFileSync(target, text);
console.log(JSON.stringify({ tools: out.length }));
