// Runs the original's composer trigger detection and slash/skill search over many prompts and
// queries and records what it answers, for `crates/client/tests/composer_menu_parity.rs`. The
// functions are cut out of the original sources unchanged (those files import React, effect and
// contract types a plain script does not need).
//   detectComposerTrigger            apps/web/src/composer-logic.ts
//   scoreQueryMatch, normalizeSearchQuery, insertRankedSearchResult   packages/shared/src/searchRanking.ts
//   searchSlashCommandItems          apps/web/src/components/chat/composerSlashCommandSearch.ts
//   searchProviderSkills             apps/web/src/providerSkillSearch.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(`../../${path}`, import.meta.url), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = end === null ? source.length : source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const logic = read("apps/web/src/composer-logic.ts");
const ranking = read("packages/shared/src/searchRanking.ts");
const slash = read("apps/web/src/components/chat/composerSlashCommandSearch.ts");
const skills = read("apps/web/src/providerSkillSearch.ts");
const skillLib = read("packages/client-runtime/src/providerSkills.ts");
const picker = read("apps/web/src/components/chat/modelPickerSearch.ts");

const body = [
  ranking,
  cut(picker, "const MODEL_PICKER_FAVORITE_SCORE_BOOST", null),
  cut(logic, "function clampCursor", "export function expandCollapsedComposerCursor"),
  cut(logic, "export function detectComposerTrigger", "/** Caret and trigger after"),
  cut(skillLib, "function titleCaseWords", "function normalizePathSeparators"),
  cut(
    skillLib,
    "export function formatProviderSkillDisplayName",
    "export function getProviderSkillsForSlashMenu",
  ),
  cut(skills, "export function scoreProviderSkill", null),
  cut(slash, "export function slashCommandItemsForPromptPosition", null),
].join("\n");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-composer-menu-"));
const file = NodePath.join(scratch, "menu.mjs");
writeFileSync(
  file,
  `${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { detectComposerTrigger, searchSlashCommandItems, slashCommandItemsForPromptPosition, searchProviderSkills, scoreModelPickerSearch, buildModelPickerSearchText };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const rows = [];
const record = (fn, input, output) => rows.push({ fn, input, output: output ?? null });

for (const text of [
  "",
  "/",
  "/mod",
  "/model now",
  "hello /x",
  "line one\n/pl",
  "see @src/li",
  "@",
  "a @b c",
  "ping #123",
  "#",
  "#a-b",
  "use $deploy",
  "$",
  "€price",
  "text @a\nnext",
  "  /x",
  "x/y",
  "/plan\n@file",
  "mail me@host",
  "#12 and @path",
]) {
  for (const cursor of [text.length, Math.floor(text.length / 2), 0, 999]) {
    record("trigger", { text, cursor }, original.detectComposerTrigger(text, cursor));
  }
}

const builtIn = [
  {
    id: "slash:model",
    type: "slash-command",
    command: "model",
    label: "/model",
    description: "Switch response model for this thread",
  },
  {
    id: "slash:plan",
    type: "slash-command",
    command: "plan",
    label: "/plan",
    description: "Switch this thread into plan mode",
  },
  {
    id: "slash:default",
    type: "slash-command",
    command: "default",
    label: "/default",
    description: "Switch this thread back to normal build mode",
  },
];
const provider = (name, description) => ({
  id: `provider:${name}`,
  type: "provider-slash-command",
  provider: "codex",
  command: { name, description },
  label: `/${name}`,
  description,
});
const skill = (name, extra = {}) => ({
  id: `skill:${name}`,
  type: "skill",
  provider: "codex",
  skill: { name, enabled: true, path: `/skills/${name}`, ...extra },
  label: `$${name}`,
  description: extra.shortDescription ?? "",
});
const items = [
  ...builtIn,
  provider("compact", "Summarize the conversation to free context"),
  provider("review", "Review the current changes"),
  provider("init", "Create an AGENTS.md file"),
  provider("model-config", "Configure models"),
  skill("deploy-app", { displayName: "Deploy App", shortDescription: "Ship to production" }),
  skill("review_pr", { description: "Review a pull request thoroughly", scope: "personal" }),
];
for (const query of [
  "",
  "/",
  "m",
  "mod",
  "/mod",
  "plan",
  "pl",
  "rev",
  "review",
  "comp",
  "cmp",
  "ini",
  "xyz",
  "default",
  "ship",
  "deploy",
  "skill",
  "skill:dep",
  "config",
  "free context",
  "-config",
  "pull",
  "PLAN",
  "  rev  ",
]) {
  const result = original.searchSlashCommandItems(items, query);
  record(
    "slash",
    { query },
    result.map((item) => item.id),
  );
}
for (const atStart of [true, false]) {
  record(
    "position",
    { atStart },
    original.slashCommandItemsForPromptPosition(items, atStart).map((item) => item.id),
  );
}
const skillList = [
  {
    name: "deploy-app",
    displayName: "Deploy App",
    shortDescription: "Ship to production",
    enabled: true,
    path: "/a",
  },
  {
    name: "review_pr",
    description: "Review a pull request thoroughly",
    scope: "personal",
    enabled: true,
    path: "/b",
  },
  { name: "Review_PR", enabled: true, path: "/c" },
  { name: "hidden", enabled: false, path: "/d" },
  { name: "reserved", enabled: true, userInvocable: false, path: "/e" },
  { name: "write-docs", shortDescription: "Docs and READMEs", enabled: true, path: "/f" },
];
for (const query of ["", "$", "dep", "$dep", "ship", "pr", "review", "docs", "hid", "xyz", "wd"]) {
  record(
    "skills",
    { query, skills: skillList },
    original.searchProviderSkills(skillList, query).map((entry) => entry.path),
  );
}
const models = [
  { driverKind: "codex", providerDisplayName: "Codex", name: "GPT-5.4", shortName: "5.4" },
  {
    driverKind: "codex",
    providerDisplayName: "Codex Personal",
    name: "GPT-5.3 Codex",
    subProvider: "OpenAI",
  },
  {
    driverKind: "claudeAgent",
    providerDisplayName: "Claude",
    name: "Claude Opus 4.1",
    shortName: "Opus",
  },
  {
    driverKind: "claudeAgent",
    providerDisplayName: "Claude",
    name: "Claude Sonnet 4.5",
    isFavorite: true,
  },
  {
    driverKind: "opencode",
    providerDisplayName: "OpenCode",
    name: "deepseek-chat",
    subProvider: "DeepSeek",
  },
  { driverKind: "cursor", providerDisplayName: "Cursor", name: "Composer 1" },
];
for (const query of [
  "",
  "  ",
  "gpt",
  "5.4",
  "opus",
  "claude",
  "claude son",
  "codex personal",
  "openai",
  "deep",
  "dsc",
  "co",
  "cmp",
  "composer 1",
  "xyz",
  "GPT 5",
  "sonnet opus",
]) {
  for (const model of models) {
    record("pickerScore", { model, query }, original.scoreModelPickerSearch(model, query));
  }
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/composer_menu.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} rows`);
