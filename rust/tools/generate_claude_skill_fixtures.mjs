// Runs the unchanged Claude skill discovery over directory trees it builds in a temp directory, and
// the unchanged `$skill` dispatch planner over prompts, and records the results for the Rust port.
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import {
  discoverClaudeSkills,
  skillOverrideSettingsPaths,
} from "../../apps/server/src/provider/Drivers/ClaudeSkills.ts";
import { planClaudeSkillDispatch } from "../../apps/server/src/provider/Drivers/ClaudeSkillDispatch.ts";

const skill = (frontmatter, body = "Body\n") => `---\n${frontmatter}\n---\n${body}`;
const cases = [
  {
    name: "user and project skills, user wins a collision, folder name is the skill name",
    homePath: "$ROOT/config",
    environment: {},
    cwd: "work",
    files: {
      "config/skills/alpha/SKILL.md": skill("name: alpha\ndescription: User alpha"),
      "work/.claude/skills/alpha/SKILL.md": skill("description: Project alpha"),
      "work/.claude/skills/beta/SKILL.md": skill('name: other-name\ndescription: "Quoted: beta"'),
      "work/.claude/skills/gamma/SKILL.md": "No frontmatter at all\n",
      "work/.claude/skills/delta/SKILL.md": skill(
        "description: |\n  Block text\n  second line\ndisable-model-invocation: yes",
      ),
      "work/.claude/skills/eps/SKILL.md": skill(
        "description: >\n  Folded\n  text\nuser-invocable: no",
      ),
      "work/.claude/skills/zeta/SKILL.md": skill(
        "description: plain: with colon # trailing comment",
      ),
      "work/.claude/skills/broken/SKILL.md": skill('description: "unterminated'),
      "work/.claude/skills/listy/SKILL.md": skill("- a\n- b"),
      "work/.claude/skills/nodoc/notes.txt": "not a skill",
      "work/.agents/skills/codexonly/SKILL.md": skill("description: Codex location"),
    },
  },
  {
    name: "skillOverrides across settings files, a repository root local file, and an invalid map",
    homePath: "",
    environment: { CLAUDE_CONFIG_DIR: "$ROOT/cfg" },
    cwd: "repo/pkg",
    files: {
      "cfg/skills/one/SKILL.md": skill("description: one"),
      "cfg/skills/two/SKILL.md": skill("description: two"),
      "cfg/skills/three/SKILL.md": skill("description: three"),
      "cfg/settings.json": JSON.stringify({
        skillOverrides: { one: "off", two: "user-invocable-only" },
      }),
      "repo/.git/HEAD": "ref: refs/heads/main\n",
      "repo/.claude/settings.local.json": '{ // comment\n "skillOverrides": { "three": "off", }, }',
      "repo/pkg/.claude/settings.local.json": JSON.stringify({ skillOverrides: { three: "on" } }),
      "repo/pkg/.claude/settings.json": JSON.stringify({ skillOverrides: { two: "bogus" } }),
    },
  },
  {
    name: "a relative CLAUDE_CONFIG_DIR resolves against the workspace",
    homePath: "",
    environment: { CLAUDE_CONFIG_DIR: "rel-config" },
    cwd: "work",
    files: { "work/rel-config/skills/rel/SKILL.md": skill("description: relative") },
  },
];
const root = realpathSync(mkdtempSync(join(tmpdir(), "t3-skills-")));
const discovery = [];
try {
  for (const [index, entry] of cases.entries()) {
    const base = join(root, String(index));
    for (const [relative, contents] of Object.entries(entry.files)) {
      mkdirSync(dirname(join(base, relative)), { recursive: true });
      writeFileSync(join(base, relative), contents);
    }
    const environment = Object.fromEntries(
      Object.entries(entry.environment).map(([key, value]) => [key, value.replace("$ROOT", base)]),
    );
    const cwd = join(base, entry.cwd);
    const skills = await Effect.runPromise(
      discoverClaudeSkills(
        { homePath: entry.homePath.replace("$ROOT", base) },
        cwd,
        environment,
      ).pipe(Effect.provide(NodeServices.layer)),
    );
    discovery.push({
      name: entry.name,
      files: entry.files,
      homePath: entry.homePath,
      environment: entry.environment,
      cwd: entry.cwd,
      output: JSON.parse(JSON.stringify(skills).replaceAll(base, "$ROOT")),
    });
  }
} finally {
  rmSync(root, { recursive: true, force: true });
}
const dispatch = [];
const names = ["review", "plan", "a-b:c", "x"];
for (const prompt of [
  "$review README.md",
  "please $review this",
  "$plan then $review it",
  "$unknown stays",
  "price is $5 and $2k",
  "$HOME is $x done",
  "  $review",
  "line one\n$review\nline three",
  "$review",
  "text $review",
  "a $plan b $review c",
  "$a-b:c go",
  "$1 $review",
  "€review is not it",
  "£review now",
  "$review\n",
  "no mention",
  "$x$x $x",
])
  dispatch.push({
    prompt,
    skills: names,
    output: planClaudeSkillDispatch(prompt, new Set(names)) ?? null,
  });
const path = await import("../../apps/server/node_modules/effect/dist/Path.js");
const settingsPaths = [];
const pathService = await Effect.runPromise(
  Effect.gen(function* () {
    return yield* path.Path;
  }).pipe(Effect.provide(NodeServices.layer)),
);
for (const input of [
  ["/cfg", "/w", "linux", {}, undefined],
  ["/cfg", "/w", "linux", {}, "/w"],
  ["/cfg", "/w/pkg", "linux", {}, "/w"],
  ["/cfg", undefined, "darwin", {}, undefined],
  ["/cfg", "/w", "win32", {}, undefined],
]) {
  const [configDir, cwd, platform, environment, root] = input;
  settingsPaths.push({
    configDir,
    cwd: cwd ?? null,
    platform,
    root: root ?? null,
    output: skillOverrideSettingsPaths(pathService, configDir, cwd, platform, environment, root),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/claude-skills.json", import.meta.url),
  JSON.stringify({ discovery, dispatch, settingsPaths }, null, 1) + "\n",
);
console.log(`${discovery.length} discovery cases, ${dispatch.length} dispatch prompts`);
process.exit(0);
