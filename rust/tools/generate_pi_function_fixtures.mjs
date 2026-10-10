// Runs the original's pure Pi functions (command parsing, skill references, model slugs, the thinking ladder and
// launch arguments) over a fixed set of inputs, for the Rust ports to reproduce.
import { writeFileSync } from "node:fs";
import {
  expandPiSkillReference,
  parsePiCompactCommand,
  parsePiDiscoveredCommands,
  withPiBuiltinSlashCommands,
} from "../../apps/server/src/provider/PiCommands.ts";
import { thinkingCapabilitiesForPiModel } from "../../apps/server/src/provider/piThinkingCapabilities.ts";
import { parsePiModelSlug } from "../../apps/server/src/orchestration-v2/Adapters/PiRpc.ts";
import { resolvePiLaunchArgs } from "../../apps/server/src/orchestration-v2/Adapters/piT3McpInjection.ts";

const rows = [];
const plain = (value) => (value === undefined ? null : JSON.parse(JSON.stringify(value)));
for (const text of [
  "/compact",
  "  /compact  ",
  "/compact keep the marker",
  "/compact\tkeep it",
  "/compact   ",
  "/compactx",
  "/compactx y",
  "compact",
  "please /compact",
  "/Compact",
  "",
  "/compact\nmulti\nline",
]) {
  rows.push({ operation: "compact", input: { text }, output: plain(parsePiCompactCommand(text)) });
}
const commandPayloads = [
  { commands: [] },
  {
    commands: [
      { name: "review", description: " Review it " },
      { name: "", description: "x" },
      { name: "  " },
      { description: "no name" },
      "junk",
      null,
    ],
  },
  {
    commands: [
      {
        name: "skill:deploy",
        source: "skill",
        description: "Deploy",
        sourceInfo: { path: "/s/deploy/SKILL.md", scope: "global" },
      },
      {
        name: "plain-skill",
        source: "skill",
        path: "/p/SKILL.md",
        location: "workspace",
        displayName: "Plain",
        interface: { shortDescription: "Short" },
      },
      { name: "skill:", source: "skill" },
      { name: "nopath", source: "skill", sourceInfo: { scope: "elsewhere", displayName: "NP" } },
      { name: "skill:x", source: "skill", sourceInfo: { scope: "Personal" } },
      { name: "compact", description: "mine" },
      { name: "ext", source: "extension", description: "an extension command" },
    ],
  },
  { commands: "nope" },
  {},
  null,
];
for (const data of commandPayloads) {
  rows.push({
    operation: "discovered",
    input: { data },
    output: plain(parsePiDiscoveredCommands(data)),
  });
}
for (const commands of [
  [],
  [{ name: "a" }, { name: "compact", description: "theirs" }, { name: "b", description: "bee" }],
]) {
  rows.push({
    operation: "builtins",
    input: { commands },
    output: plain(withPiBuiltinSlashCommands(commands)),
  });
}
const skills = ["deploy", "review", "a.b"];
for (const text of [
  "no skills here",
  "$deploy",
  "$deploy and then ship",
  "ship it $deploy",
  "run $review then $deploy now",
  "$unknown $deploy",
  "$unknown only",
  "cost is $5 and $deploy",
  "$deploy $deploy twice",
  "a$deploy b",
  "  $deploy  ",
  "$deploy\n$review\nbody text",
  "tab\t$review\tafter",
  "$a.b works",
  "$ alone",
  "end $deploy",
  "$review$deploy",
]) {
  rows.push({
    operation: "expand",
    input: { text, skills },
    output: expandPiSkillReference(text, new Set(skills)),
  });
}
for (const slug of [
  "openrouter/deepseek/deepseek-v4-flash",
  "anthropic/claude-x",
  "default",
  "/model",
  "provider/",
  "a/b/c",
  "",
  "/",
  "ünï/çode",
]) {
  rows.push({ operation: "slug", input: { slug }, output: plain(parsePiModelSlug(slug)) });
}
const models = [
  [
    {
      reasoning: true,
      thinkingLevelMap: { off: "none", minimal: null, low: null, medium: "medium", high: "high" },
    },
    "high",
  ],
  [{ reasoning: true }, "medium"],
  [{ reasoning: true }, "xhigh"],
  [{ reasoning: true, thinkingLevelMap: { xhigh: "x" } }, "xhigh"],
  [{ reasoning: true, thinkingLevelMap: { xhigh: "x", max: "m" } }, "max"],
  [{ reasoning: true, thinkingLevelMap: { xhigh: null, max: "m" } }, "xhigh"],
  [
    {
      reasoning: true,
      thinkingLevelMap: { off: null, minimal: null, low: null, medium: null, high: null },
    },
    "off",
  ],
  [{ reasoning: true, thinkingLevelMap: { minimal: null, low: null } }, "minimal"],
  [{ reasoning: true, thinkingLevelMap: [] }, "low"],
  [{ reasoning: false }, "high"],
  [{}, "high"],
  [null, "high"],
  [{ reasoning: true }, "bogus"],
  [{ reasoning: true }, 5],
  [{ reasoning: true, thinkingLevelMap: { off: null, minimal: null, low: null } }, "off"],
  [{ reasoning: true, thinkingLevelMap: { medium: null, high: null } }, "high"],
];
for (const [model, level] of models) {
  rows.push({
    operation: "thinking",
    input: { model, default: level },
    output: plain(thinkingCapabilitiesForPiModel(model, level)),
  });
}
for (const launchArgs of [
  "",
  "--provider openrouter --model deepseek/deepseek-v4-flash --no-extensions --no-skills --no-prompt-templates --no-context-files --approve",
  "--mode json",
  "--session=/x",
  "-c",
  "--no-session",
  "--fork a",
  "--resume",
  "-- hello",
  "hello",
  "-z",
  "--model",
  "--provider openrouter",
  "--model=a/b --provider=x --foo=bar --flag value --bare",
  "--tools 'read,bash' --offline",
  "--flag @file --other",
  "--flag -x",
  "--extension /a.ts -e /b.ts",
  "--thinking high --name 'my session'",
  "--api-key",
  "-t read -xt write",
  "--verbose --no-tools",
  "--help",
  "--version",
  "--session-dir /tmp/x",
]) {
  const result = resolvePiLaunchArgs(launchArgs);
  rows.push({
    operation: "launch",
    input: { launchArgs },
    output: result.ok ? { args: [...result.args] } : { message: result.message },
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/pi-functions.json", import.meta.url),
  JSON.stringify(rows, null, 1) + "\n",
);
console.log(`${rows.length} pi function cases`);
process.exit(0);
