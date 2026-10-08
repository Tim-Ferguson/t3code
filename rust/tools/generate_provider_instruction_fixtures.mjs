// Development-only source oracle. Run with Node24 against the unchanged checkout.
import fs from "node:fs";
import {
  buildCodexDeveloperInstructions,
  buildCodexAdditionalContext,
} from "../../apps/server/src/provider/CodexDeveloperInstructions.ts";
import { buildRuntimeInstructions } from "../../apps/server/src/provider/RuntimeInstructions.ts";
import { t3AcpPromptWithInstructions } from "../../apps/server/src/provider/T3OrchestrationInstructions.ts";
import { withAgentDeviceEnvironment } from "../../apps/server/src/mcp/McpProviderSession.ts";

const write = (path, value) =>
  fs.writeFileSync(new URL(path, import.meta.url), JSON.stringify(value, null, 2) + "\n");
const acpMode = (mode) =>
  t3AcpPromptWithInstructions({ prompt: "", state: { interactionMode: mode, hasT3Mcp: false } })
    .split("<t3_code_instructions>\n")[1]
    .split("\n</t3_code_instructions>")[0];
const context = buildCodexAdditionalContext({ model: "auto", reasoningEffort: "" }, false);
write("../crates/server/src/provider-instructions.json", {
  codexPlan: buildCodexDeveloperInstructions("plan"),
  codexDefault: buildCodexDeveloperInstructions("default"),
  orchestration: context.t3_code_orchestration.value,
  browser: buildCodexAdditionalContext(
    { model: "auto", reasoningEffort: "" },
    { browser: true, device: false },
  ).t3_code_tools.value,
  device: buildCodexAdditionalContext(
    { model: "auto", reasoningEffort: "" },
    { browser: false, device: true },
  ).t3_code_tools.value,
  runtimeSuffix: buildRuntimeInstructions({ harness: "" }).split("\n\n").slice(1).join("\n\n"),
  acpPlan: acpMode("plan"),
  acpDefault: acpMode("default"),
});
const rows = [];
for (const harness of ["Codex", "acpRegistry", " Example\nHarness ", "\uFEFF Unicode \u0085"]) {
  for (const model of ["auto", "default", "model", " model\n name ", ""]) {
    for (const modelName of [undefined, "Model display", "model"]) {
      for (const reasoningEffort of [undefined, "medium", "\uFEFF high \n"]) {
        const input = { harness, model, modelName, reasoningEffort };
        rows.push({ operation: "runtime", input, output: buildRuntimeInstructions(input) });
      }
    }
  }
}
for (const model of ["auto", "model", " model\n"])
  for (const reasoningEffort of ["medium", "", " high\n"]) {
    for (const browser of [true, false])
      for (const device of [true, false]) {
        const input = { model, reasoningEffort, browser, device };
        rows.push({
          operation: "codexContext",
          input,
          output: buildCodexAdditionalContext({ model, reasoningEffort }, { browser, device }),
        });
      }
  }
for (const interactionMode of ["plan", "default"])
  for (const hasT3Mcp of [true, false]) {
    for (const prompt of ["hello", " /help", "\uFEFF/help", "\u0085/help", ""]) {
      for (const previousState of [
        undefined,
        { interactionMode, hasT3Mcp },
        { interactionMode: interactionMode === "plan" ? "default" : "plan", hasT3Mcp },
      ]) {
        const input = { prompt, state: { interactionMode, hasT3Mcp }, previousState };
        rows.push({ operation: "acpPrompt", input, output: t3AcpPromptWithInstructions(input) });
      }
    }
  }
for (const base of [
  { PATH: "/provider/bin:/usr/bin", PROVIDER_KEY: "fixture" },
  { Path: "C:\\provider;C:\\system" },
  { PATH: "", Path: "fallback" },
  {},
]) {
  for (const extra of [
    undefined,
    {},
    {
      PATH: "/device/bin",
      AGENT_DEVICE_DAEMON_BASE_URL: "http://127.0.0.1:1",
      AGENT_DEVICE_DAEMON_AUTH_TOKEN: "isolated",
    },
    { PATH: "C:\\device", PATH_SEPARATOR: ";" },
    { PATH: "", PATH_SEPARATOR: ";", OTHER: "x" },
  ]) {
    const input = { base, extra };
    rows.push({
      operation: "deviceEnvironment",
      input,
      output: withAgentDeviceEnvironment(
        base,
        extra === undefined ? undefined : { agentDeviceEnvironment: extra },
      ),
    });
  }
}
write("../crates/server/tests/fixtures/provider-instructions.json", rows);
console.log(`${rows.length} original provider instruction/environment cases`);
