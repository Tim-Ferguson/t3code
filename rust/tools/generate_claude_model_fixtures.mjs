// Executes the unchanged Claude model catalog and selection compiler and records the bundled
// catalog (embedded by the Rust server) plus witnesses for the Rust port. No provider runs here.
import { writeFileSync } from "node:fs";
import * as Catalog from "../../apps/server/src/provider/ClaudeModelCatalog.ts";
import { compileClaudeModelSelection } from "../../apps/server/src/claudeModelOptions.ts";
import { providerModelsFromSettings } from "../../apps/server/src/provider/providerSnapshot.ts";
import { claudeProviderTurnTokenUsage } from "../../apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts";
import { normalizeClaudeTurnTokenUsage } from "../../apps/server/src/provider/ClaudeTurnTokenUsage.ts";
import { applyClaudePromptEffortPrefix } from "../../packages/shared/src/model.ts";

const catalog = Catalog.BUNDLED_CLAUDE_MODEL_CATALOG;
writeFileSync(
  new URL("../crates/server/src/claude-model-catalog.json", import.meta.url),
  JSON.stringify(catalog.models, null, 2) + "\n",
);

const rows = [];
const selection = (model, options) => ({
  instanceId: "claudeAgent",
  model,
  ...(options === undefined ? {} : { options }),
});
const optionSets = (entry) => {
  const sets = [undefined, []];
  for (const descriptor of entry.model.capabilities.optionDescriptors ?? []) {
    if (descriptor.type === "select") {
      for (const option of descriptor.options) sets.push([{ id: descriptor.id, value: option.id }]);
      sets.push([{ id: descriptor.id, value: "bogus-value" }]);
    } else {
      sets.push([{ id: descriptor.id, value: true }], [{ id: descriptor.id, value: false }]);
    }
  }
  const effort = (entry.model.capabilities.optionDescriptors ?? []).find((d) => d.id === "effort");
  const context = (entry.model.capabilities.optionDescriptors ?? []).find(
    (d) => d.id === "contextWindow",
  );
  if (effort && context)
    sets.push([
      { id: "effort", value: effort.options.at(-1).id },
      { id: "contextWindow", value: context.options.at(-1).id },
    ]);
  return sets;
};
for (const entry of catalog.models) {
  for (const name of [
    entry.model.slug,
    ...(entry.model.aliases ?? []),
    entry.model.slug.toUpperCase(),
  ]) {
    for (const options of optionSets(entry)) {
      const input = selection(name, options);
      rows.push({
        operation: "compile",
        input,
        output: compileClaudeModelSelection(input),
        contextWindowTokens:
          Catalog.resolveClaudeCatalogContextWindowTokens(catalog, input) ?? null,
        slug: Catalog.resolveClaudeModelSlug(catalog, name),
      });
    }
  }
}
for (const model of ["unknown-model", "", "  claude-opus-4-6  "]) {
  const input = selection(model);
  rows.push({
    operation: "compile",
    input,
    output: compileClaudeModelSelection(input),
    contextWindowTokens: Catalog.resolveClaudeCatalogContextWindowTokens(catalog, input) ?? null,
    slug: Catalog.resolveClaudeModelSlug(catalog, model),
  });
}
const versions = [
  null,
  undefined,
  "",
  "1.0.0",
  "2.0.0",
  "2.1.0",
  "2.1.100",
  "2.1.279",
  "2.1.280",
  "2.1.281",
  "2.2.0",
  "3.0.0",
  "not-a-version",
];
for (const version of versions) {
  rows.push({
    operation: "version",
    version: version ?? null,
    slugs: Catalog.resolveClaudeModelsForVersion(catalog, version).map((model) => model.slug),
    upgrade: Catalog.formatClaudeVersionUpgradeMessage(catalog, version ?? null) ?? null,
  });
}
const defaultCaps = { optionDescriptors: [] };
for (const custom of [
  [],
  ["my-model", "  spaced  ", "", "my-model", "claude-opus-4-6"],
  [
    { slug: "named", name: "Named Model" },
    { slug: "bare" },
    {
      slug: "capable",
      capabilities: { optionDescriptors: [{ id: "x", label: "X", type: "boolean" }] },
    },
  ],
  [{ slug: "opus" }, 5, null, { name: "no slug" }],
]) {
  const models = providerModelsFromSettings(
    catalog.models.map((entry) => entry.model),
    custom,
    defaultCaps,
  );
  rows.push({
    operation: "providerModels",
    custom,
    version: null,
    slugs: models.map((model) => model.slug),
    tail: models.filter((model) => model.isCustom),
  });
}
for (const [usage, model] of [
  [
    {
      input_tokens: 3,
      cache_creation_input_tokens: 2338,
      cache_read_input_tokens: 9455,
      output_tokens: 1,
    },
    selection("claude-sonnet-4-6"),
  ],
  [
    { input_tokens: 10, output_tokens: 4 },
    selection("claude-opus-4-6", [{ id: "contextWindow", value: "200k" }]),
  ],
  [
    {
      input_tokens: 10,
      cache_creation_input_tokens: null,
      cache_read_input_tokens: null,
      output_tokens: 4,
    },
    selection("unknown"),
  ],
]) {
  rows.push({
    operation: "assistantUsage",
    usage,
    model,
    output: claudeProviderTurnTokenUsage(usage, model, "2026-01-01T00:00:00.000Z"),
  });
}
for (const subtype of ["success", "error_during_execution"])
  for (const status of ["completed", "interrupted", "failed"])
    for (const hasSubagents of [false, true])
      for (const usage of [
        undefined,
        {},
        {
          input_tokens: 4,
          cache_creation_input_tokens: 12030,
          cache_read_input_tokens: 11844,
          output_tokens: 228,
        },
        { input_tokens: 0, output_tokens: 0 },
        {
          input_tokens: 5,
          cache_read_input_tokens: null,
          cache_creation_input_tokens: null,
          output_tokens: 1,
          output_tokens_details: { thinking_tokens: 7 },
        },
        { input_tokens: 5, output_tokens: 9, output_tokens_details: { thinking_tokens: 3 } },
        { input_tokens: -1, output_tokens: 1.5 },
        { output_tokens: 3 },
      ]) {
        const result = { subtype, ...(usage === undefined ? {} : { usage }) };
        rows.push({
          operation: "turnUsage",
          result,
          hasSubagents,
          status,
          output: normalizeClaudeTurnTokenUsage(result, hasSubagents, status),
        });
      }
rows.push({
  operation: "turnUsage",
  result: null,
  hasSubagents: false,
  status: "completed",
  output: normalizeClaudeTurnTokenUsage(undefined, false, "completed"),
});
for (const text of [
  "hello",
  "  hello  ",
  "",
  "   ",
  "/deploy.prod now",
  "/home/theo/app.ts",
  "Ultrathink:\nalready",
  "multi\nline",
])
  for (const effort of [undefined, null, "ultrathink", "high"])
    rows.push({
      operation: "promptEffort",
      text,
      effort: effort ?? null,
      output: applyClaudePromptEffortPrefix(text, effort),
    });

writeFileSync(
  new URL("../crates/server/tests/fixtures/claude-models.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${catalog.models.length} catalog models, ${rows.length} witnesses`);
