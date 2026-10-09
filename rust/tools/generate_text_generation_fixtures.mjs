// Runs the original text generation sources and records what they do, for the Rust port to be held
// to. Writes crates/server/tests/fixtures/text-generation-*.jsonl:
//   prompts       the prompt each builder writes for many inputs, byte for byte
//   sanitize      what is done to answers and to what feeds a title (caps, quotes, slugs, citations)
//   settings      which provider and model write, with the provider fallback, project overrides and
//                 the dedicated source control writer
//   git-inputs    what GitManager sends for a commit: writing style, repository instructions, model
//   templates     the repository change request template the original finds
//   invocations   the arguments, directory, environment and stdin the original starts the Codex and
//                 Claude CLIs with, against recording fake executables, and what it makes of their
//                 output (including empty, malformed and failing output)
// No provider CLI is run, no network is used. Run: node tools/generate_text_generation_fixtures.mjs
import {
  writeFileSync,
  renameSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  chmodSync,
  rmSync,
  existsSync,
  realpathSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Exit from "../../apps/server/node_modules/effect/dist/Exit.js";
import * as Cause from "../../apps/server/node_modules/effect/dist/Cause.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as Prompts from "../../apps/server/src/textGeneration/TextGenerationPrompts.ts";
import * as Utils from "../../apps/server/src/textGeneration/TextGenerationUtils.ts";
import * as Operations from "../../apps/server/src/textGeneration/TextGenerationOperations.ts";
import * as TitleContext from "../../apps/server/src/textGeneration/ThreadTitleContext.ts";
import * as Citations from "../../packages/shared/src/assistantCitations.ts";
import * as GitShared from "../../packages/shared/src/git.ts";
import * as SharedSettings from "../../packages/shared/src/serverSettings.ts";
import * as ProjectSettings from "../../packages/shared/src/projectSettings.ts";
import * as Struct from "../../packages/shared/src/Struct.ts";
import * as ContractSettings from "../../packages/contracts/src/settings.ts";
import * as GitManager from "../../apps/server/src/git/GitManager.ts";
import * as GitVcsDriver from "../../apps/server/src/vcs/GitVcsDriver.ts";
import * as VcsProcess from "../../apps/server/src/vcs/VcsProcess.ts";
import * as ServerConfig from "../../apps/server/src/config.ts";
import * as ServerSettings from "../../apps/server/src/serverSettings.ts";
import * as TextGeneration from "../../apps/server/src/textGeneration/TextGeneration.ts";
import * as ProviderRegistry from "../../apps/server/src/provider/ProviderRegistry.ts";
import * as ProjectSetupScriptRunner from "../../apps/server/src/project/ProjectSetupScriptRunner.ts";
import * as ProjectionStore from "../../apps/server/src/orchestration-v2/ProjectionStore.ts";
import * as ProjectStore from "../../apps/server/src/orchestration-v2/ProjectStore.ts";
import * as SqlitePersistence from "../../apps/server/src/persistence/Sqlite.ts";
import * as SourceControlProviderRegistry from "../../apps/server/src/sourceControl/SourceControlProviderRegistry.ts";
import { detectPrTemplate } from "../../apps/server/src/sourceControl/PrTemplateDetection.ts";
import { makeClaudeTextGeneration } from "../../apps/server/src/textGeneration/ClaudeTextGeneration.ts";
import { makeCodexTextGeneration } from "../../apps/server/src/textGeneration/CodexTextGeneration.ts";

const run = (effect) => Effect.runPromise(effect);
const exitOf = async (effect) => {
  const exit = await Effect.runPromiseExit(effect);
  if (Exit.isSuccess(exit)) return { ok: true, value: exit.value };
  const error = Cause.squash(exit.cause);
  return {
    ok: false,
    error: {
      tag: error._tag ?? "Error",
      operation: error.operation,
      detail: error.detail,
      message: error.message,
    },
  };
};
const rows = {
  prompts: [],
  sanitize: [],
  settings: [],
  "git-inputs": [],
  templates: [],
  invocations: [],
};
// Long inputs are written as the pattern that makes them, so the fixtures stay small; the Rust test
// expands `{"repeat":[unit,length]}` back into the same string.
const repeated = new Map();
const long = (unit, length) => {
  const value = unit.repeat(Math.ceil(length / unit.length)).slice(0, length);
  repeated.set(value, [unit, length]);
  return value;
};
const encodeRow = (row) =>
  JSON.stringify(row, (key, value) =>
    typeof value === "string" && repeated.has(value) ? { repeat: repeated.get(value) } : value,
  );
// A long prompt is kept as its length, its ends and a digest of all of it.
const packPrompt = (prompt) =>
  prompt.length <= 3_000
    ? { prompt }
    : {
        promptLength: prompt.length,
        promptHead: prompt.slice(0, 300),
        promptTail: prompt.slice(-300),
        promptSha256: createHash("sha256").update(prompt, "utf8").digest("hex"),
      };

// ---- prompts --------------------------------------------------------------------------------------
const policyOf = (key, text) =>
  text === undefined
    ? undefined
    : { kind: "custom", inferRepositoryConventions: false, [key]: text };
for (const branch of [null, "feature/x"])
  for (const includeBranch of [false, true])
    for (const instructions of [undefined, "  Be terse.\n", "   ", long("Policy line. ", 20_500)])
      for (const [summary, patch] of [
        ["M\ta.txt", "diff --git a/a.txt b/a.txt\n+hello"],
        [long("M\tfile.txt\n", 6_500), long("+line\n", 41_000)],
      ]) {
        const built = Prompts.buildCommitMessagePrompt({
          branch,
          stagedSummary: summary,
          stagedPatch: patch,
          includeBranch,
          policy: policyOf("commitInstructions", instructions),
        });
        rows.prompts.push({
          kind: "commit",
          input: {
            branch,
            stagedSummary: summary,
            stagedPatch: patch,
            includeBranch,
            instructions: instructions ?? null,
          },
          ...packPrompt(built.prompt),
          schema: Utils.toJsonSchemaObject(built.outputSchema),
        });
      }
for (const template of [
  undefined,
  "  # Summary\n<!-- hint -->\n## Testing  ",
  "   ",
  long("Template. ", 9_000),
])
  for (const instructions of [undefined, "Keep titles short."])
    for (const long_ of [false, true]) {
      const input = {
        baseBranch: "main",
        headBranch: "feature/x",
        commitSummary: long_ ? long("abc1234 Commit\n", 13_000) : "abc Add a",
        diffSummary: long_ ? long(" a.txt | 1 +\n", 13_000) : " a.txt | 1 +",
        diffPatch: long_ ? long("+x\n", 41_000) : "+x",
        changeRequestTemplate: template,
        policy: policyOf("changeRequestInstructions", instructions),
      };
      const built = Prompts.buildPrContentPrompt(input);
      rows.prompts.push({
        kind: "changeRequest",
        input: {
          ...input,
          policy: undefined,
          changeRequestTemplate: template ?? null,
          instructions: instructions ?? null,
        },
        ...packPrompt(built.prompt),
        schema: Utils.toJsonSchemaObject(built.outputSchema),
      });
    }
const attachmentList = (count) =>
  Array.from({ length: count }, (_, index) => ({
    type: "image",
    id: `att-${index}`,
    name: `shot ${index}.png`,
    mimeType: "image/png",
    sizeBytes: 1024 * (index + 1),
  }));
for (const mode of ["static", "semantic", "custom", undefined])
  for (const instructions of ["", "  Use JIRA-123 prefixes.  ", long("Instruction. ", 20_100)])
    for (const message of ["Add search to the sidebar", long("Fix the thing. ", 9_000)])
      for (const count of [0, 1, 3]) {
        const naming = mode === undefined ? undefined : { mode, prefix: "t3", instructions };
        const built = Prompts.buildBranchNamePrompt({
          message,
          attachments: count ? attachmentList(count) : undefined,
          naming,
        });
        rows.prompts.push({
          kind: "branch",
          input: {
            message,
            attachments: attachmentList(count),
            naming: naming ?? { mode: "static", prefix: "t3", instructions: "" },
          },
          ...packPrompt(built.prompt),
          schema: Utils.toJsonSchemaObject(built.outputSchema),
        });
      }
const titleMessages = [
  "Fix the login redirect loop",
  long("A long first request. ", 9_000),
  "[Earlier content truncated]\n\nthe end of a long thread",
  `[Earlier content truncated]\n\n${long("tail ", 9_000)}`,
  long("USER:\nask\n\n", 8_001),
  long("x", 8_000),
  long("y", 8_001),
];
for (const previousTitle of [undefined, "Fix Login Redirect", 'Title with "quotes" and \\ slash'])
  for (const message of titleMessages)
    for (const linkedContext of [
      undefined,
      'https://github.com/acme/web/pull/7\n{"title":"Fix","body":""}',
    ])
      for (const count of [0, 2]) {
        const built = Prompts.buildThreadTitlePrompt({
          message,
          previousTitle,
          linkedContext,
          attachments: count ? attachmentList(count) : undefined,
        });
        rows.prompts.push({
          kind: "threadTitle",
          input: {
            message,
            previousTitle: previousTitle ?? null,
            linkedContext: linkedContext ?? null,
            attachments: attachmentList(count),
          },
          ...packPrompt(built.prompt),
          schema: Utils.toJsonSchemaObject(built.outputSchema),
        });
      }

// ---- sanitize: helpers ------------------------------------------------------------------------------
const subjects = [
  "Add search.",
  "  Add search...  \n\nbody",
  "",
  "   ",
  "\n\nAdd x",
  "A".repeat(80),
  `${"word ".repeat(20)}end.`,
  "Fix\r\nnext line",
  "été ".repeat(30),
  "😀".repeat(40),
  ". . .",
];
for (const raw of subjects)
  rows.sanitize.push({ fn: "commitSubject", raw, out: Utils.sanitizeCommitSubject(raw) });
for (const raw of [...subjects, "Title\r\nBody", "  \t "])
  rows.sanitize.push({ fn: "prTitle", raw, out: Utils.sanitizePrTitle(raw) });
const titles = [
  '"Quoted title"',
  "'single'",
  "`tick`",
  '"""triple"""',
  "  spaced   out   title ",
  "Line one\nLine two",
  "Line\r\nTwo",
  "",
  "   ",
  '""',
  '{"title":"From JSON"}',
  '{"title":"  \\"Nested\\"  ","other":1}',
  '{"title":5}',
  '["x"]',
  '"just a json string"',
  "x".repeat(130),
  `${"word ".repeat(40)}`,
  "é".repeat(121),
  "tab\there",
  "A title.",
  "a" + "😀".repeat(70),
  " nbsp ",
  "\u0085nel\u0085",
  "﻿bom﻿",
];
for (const raw of titles)
  rows.sanitize.push({ fn: "threadTitle", raw, out: Utils.sanitizeThreadTitle(raw) });
for (const raw of [
  "Add Search",
  "feat/Add Search!",
  "  ../x  ",
  "fix/login-error",
  "a".repeat(80),
  "",
  "feature/UI Update",
  "!!!",
]) {
  for (const naming of [
    { mode: "static", prefix: "t3", instructions: "" },
    { mode: "static", prefix: "my team/work items", instructions: "" },
    { mode: "static", prefix: "///", instructions: "" },
    { mode: "static", prefix: "", instructions: "" },
    { mode: "semantic", prefix: "t3", instructions: "" },
    { mode: "custom", prefix: "t3", instructions: "x" },
  ])
    rows.sanitize.push({
      fn: "branchName",
      raw,
      naming,
      out: GitShared.formatGeneratedBranchName(raw, naming),
    });
  rows.sanitize.push({ fn: "featureBranch", raw, out: GitShared.sanitizeFeatureBranchName(raw) });
}
for (const [value, max] of [
  ["short", 10],
  ["x".repeat(30), 10],
  ["éé".repeat(10), 10],
  ["😀😀😀", 4],
])
  rows.sanitize.push({ fn: "limitSection", value, max, out: Utils.limitSection(value, max) });
for (const [text, budget] of [
  ["short", 100],
  ["a".repeat(100), 60],
  ["a".repeat(100), 21],
  ["a".repeat(100), 22],
  [`${"a".repeat(50)}${"b".repeat(50)}`, 51],
  ["é".repeat(100), 40],
  ["x", 0],
])
  rows.sanitize.push({
    fn: "limitTitleMessage",
    text,
    budget,
    out: TitleContext.limitTitleMessage(text, budget),
  });

// Answers as the operations finish them, with a runner that answers what it is told to.
const scripted = (generated) => Operations.fromRunner("Scripted", () => Effect.succeed(generated));
const opCommit = async (generated, includeBranch) =>
  (
    await exitOf(
      scripted(
        includeBranch ? generated : (({ branch: _branch, ...rest }) => rest)(generated),
      ).generateCommitMessage({
        cwd: "/x",
        branch: null,
        stagedSummary: "",
        stagedPatch: "",
        includeBranch,
        modelSelection: { instanceId: "codex", model: "m" },
      }),
    )
  ).value;
for (const generated of [
  { subject: " Add things. ", body: "  - one\n- two  \n", branch: "Feature Add Things" },
  { subject: "", body: "", branch: "!!" },
  { subject: "S".repeat(100), body: "b", branch: "a/b" },
  { subject: "line1\nline2", body: "x", branch: "feature/ok" },
])
  for (const includeBranch of [false, true])
    rows.sanitize.push({
      fn: "commitOp",
      generated,
      includeBranch,
      out: await opCommit(generated, includeBranch),
    });
for (const generated of [
  { title: "  Add feature\nmore", body: "  ## Summary\n- x  " },
  { title: "", body: "" },
])
  rows.sanitize.push({
    fn: "prOp",
    generated,
    out: (
      await exitOf(
        scripted(generated).generatePrContent({
          cwd: "/x",
          baseBranch: "main",
          headBranch: "f",
          commitSummary: "",
          diffSummary: "",
          diffPatch: "",
          modelSelection: { instanceId: "codex", model: "m" },
        }),
      )
    ).value,
  });
for (const generated of [
  { title: '"Fix Login"', needsRefinement: true },
  { title: "x", needsRefinement: false },
  { title: "" },
])
  rows.sanitize.push({
    fn: "titleOp",
    generated,
    out: (
      await exitOf(
        scripted(generated).generateThreadTitle({
          cwd: "/x",
          message: "m",
          modelSelection: { instanceId: "codex", model: "m" },
        }),
      )
    ).value,
  });

// Assistant quotes in plain text.
const citation = (text, comment, over = {}) => ({
  version: 1,
  environmentId: "env-1",
  threadId: "thread 1",
  messageId: "msg-1",
  text,
  start: 3,
  end: 3 + text.length,
  prefix: "pre",
  suffix: "suf",
  ...(comment === undefined ? {} : { comment }),
  ...over,
});
const quoted = [
  citation("Selected text"),
  citation("With comment", "Please fix"),
  citation("Multi\nline & <html>", "a+b c%20d"),
  citation("é😀"),
];
const plain = [
  ...quoted.map((c) => `Before ${Citations.serializeAssistantCitation(c)} after`),
  `${Citations.serializeAssistantCitation(quoted[0])} and ${Citations.serializeAssistantCitation(quoted[1])}`,
  "no citations here",
  "[Assistant quote](t3-citation://v1/env/thread/msg?text=x&start=1&end=2&prefix=&suffix=)",
  "[Assistant quote](t3-citation://v1/env/thread/msg?text=x&start=2&end=1&prefix=&suffix=)",
  "[Assistant quote](t3-citation://v1/env/thread/msg?text=x&start=1&end=2&prefix=&suffix=&extra=1)",
  "[Assistant quote](t3-citation://v2/env/thread/msg?text=x&start=1&end=2&prefix=&suffix=)",
  "[Assistant quote](t3-citation://v1/env/thread?text=x&start=1&end=2&prefix=&suffix=)",
  "[Assistant quote](t3-citation://v1/env/thread/%E0%A4%A?text=x&start=1&end=2&prefix=&suffix=)",
];
for (const prompt of plain)
  rows.sanitize.push({
    fn: "citations",
    prompt,
    out: Citations.assistantCitationsToPlainText(prompt),
  });

// The transcript a title is regenerated from.
const msg = (role, text, attachments) => ({ role, text, ...(attachments ? { attachments } : {}) });
const ctxCases = [
  [
    msg("user", "Fix the login bug"),
    msg("assistant", "Looking now."),
    msg("user", "Also the logout"),
  ],
  [
    msg("user", "first"),
    msg("assistant", long("answer ", 5_000)),
    msg("user", "second"),
    msg("assistant", long("more ", 5_000)),
    msg("user", "third"),
  ],
  [
    msg("system", "ignored"),
    msg("reasoning", "thinking"),
    msg("user", "ask"),
    msg("assistant", ""),
  ],
  [msg("user", long("big ask ", 9_000)), msg("assistant", long("big answer ", 9_000))],
  Array.from({ length: 12 }, (_, index) =>
    msg(index % 2 === 0 ? "user" : "assistant", `${index} ${long("text ", 900)}`),
  ),
  [
    msg("user", "see images", attachmentList(2)),
    msg("assistant", "ok"),
    msg("user", "more", attachmentList(5).slice(2)),
    msg("user", "", attachmentList(1)),
  ],
  [msg("user", Citations.serializeAssistantCitation(citation("Quoted", "why?")))],
  [],
];
for (const messages of ctxCases)
  rows.sanitize.push({
    fn: "titleContext",
    messages,
    out: TitleContext.formatThreadTitleContext(messages),
  });

console.error("stage settings");
// ---- settings -------------------------------------------------------------------------------------
const encodedDefaults = Schema.encodeSync(ContractSettings.ServerSettings)(
  ContractSettings.DEFAULT_SERVER_SETTINGS,
);
const claudeSnapshot = (over = {}) => ({
  instanceId: "claudeAgent",
  driver: "claudeAgent",
  enabled: true,
  installed: true,
  version: "2.1.0",
  status: "ready",
  auth: { status: "authenticated" },
  checkedAt: "2026-01-01T00:00:00.000Z",
  models: [],
  slashCommands: [],
  skills: [],
  ...over,
});
const settingsCases = [
  { name: "defaults", overrides: {} },
  { name: "codex disabled", overrides: { providers: { codex: { enabled: false } } } },
  {
    name: "codex and claude disabled",
    overrides: { providers: { codex: { enabled: false }, claudeAgent: { enabled: false } } },
  },
  {
    name: "chosen claude",
    overrides: {
      textGenerationModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
  },
  {
    name: "chosen claude disabled",
    overrides: {
      textGenerationModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
      providers: { claudeAgent: { enabled: false } },
    },
  },
  {
    name: "chosen claude disabled by instance",
    overrides: {
      textGenerationModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
      providerInstances: { claudeAgent: { driver: "claudeAgent", enabled: false } },
    },
  },
  {
    name: "codex disabled by instance",
    overrides: { providerInstances: { codex: { driver: "codex", enabled: false } } },
  },
  {
    name: "custom instance chosen",
    overrides: {
      textGenerationModelSelection: { instanceId: "codex_work", model: "gpt-x" },
      providerInstances: { codex_work: { driver: "codex", config: {} } },
    },
  },
  {
    name: "custom instance disabled",
    overrides: {
      textGenerationModelSelection: { instanceId: "codex_work", model: "gpt-x" },
      providerInstances: { codex_work: { driver: "codex", enabled: false } },
    },
  },
  {
    name: "unknown instance chosen",
    overrides: { textGenerationModelSelection: { instanceId: "nothing_here", model: "x" } },
  },
  {
    name: "acp instance chosen",
    overrides: {
      textGenerationModelSelection: { instanceId: "acp_one", model: "x" },
      providerInstances: { acp_one: { driver: "acpRegistry", config: {} } },
    },
  },
  {
    name: "writer claude",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
    providers: [claudeSnapshot()],
  },
  {
    name: "writer claude unavailable",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
    providers: [claudeSnapshot({ availability: "unavailable" })],
  },
  {
    name: "writer claude disabled snapshot",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
    providers: [claudeSnapshot({ enabled: false })],
  },
  {
    name: "writer claude not text capable",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
    providers: [claudeSnapshot({ supportsTextGeneration: false })],
  },
  {
    name: "writer claude missing snapshot",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
    providers: [],
  },
  {
    name: "writer claude unauthenticated still used",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
    providers: [claudeSnapshot({ auth: { status: "unauthenticated" }, installed: false })],
  },
  {
    name: "writer provider disabled in settings",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
      providers: { claudeAgent: { enabled: false } },
    },
    providers: [claudeSnapshot()],
  },
  {
    name: "project overrides model",
    overrides: {
      projectSettingsOverrides: {
        "project-1": {
          textGenerationModelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-4-6" },
          branchNamingMode: "semantic",
          branchNamePrefix: "acme",
          branchNameInstructions: "Use tickets.",
          sourceControlWritingStyle: {
            mode: "custom",
            customInstructions: "Be formal.",
            followChangeRequestTemplates: false,
          },
        },
      },
    },
    projectId: "project-1",
  },
  {
    name: "project override on a disabled provider",
    overrides: {
      providers: { claudeAgent: { enabled: false } },
      projectSettingsOverrides: {
        "project-1": {
          textGenerationModelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-4-6" },
        },
      },
    },
    projectId: "project-1",
  },
  {
    name: "project writer override null",
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
      projectSettingsOverrides: { "project-1": { sourceControlWriterModelSelection: null } },
    },
    projectId: "project-1",
    providers: [claudeSnapshot()],
  },
  {
    name: "other project untouched",
    overrides: { projectSettingsOverrides: { "project-1": { branchNamingMode: "custom" } } },
    projectId: "project-2",
  },
];
for (const entry of settingsCases) {
  const decoded = Schema.decodeUnknownSync(ContractSettings.ServerSettings)(
    Struct.deepMerge(encodedDefaults, entry.overrides),
  );
  const resolved = await run(
    Effect.gen(function* () {
      return yield* (yield* ServerSettings.ServerSettingsService).getSettings;
    }).pipe(
      Effect.provide(
        ServerSettings.ServerSettingsService.layerTest(entry.overrides).pipe(
          Layer.provideMerge(NodeServices.layer),
        ),
      ),
    ),
  );
  const project = ProjectSettings.resolveProjectSettings(
    resolved,
    entry.projectId ?? null,
  ).settings;
  const encode = Schema.encodeSync(ContractSettings.ServerSettings);
  const providers = entry.providers;
  rows.settings.push({
    name: entry.name,
    settings: encode(decoded),
    projectId: entry.projectId ?? null,
    providers: providers ?? [],
    general: resolved.textGenerationModelSelection,
    project: {
      textGenerationModelSelection: project.textGenerationModelSelection,
      sourceControlWriterModelSelection: project.sourceControlWriterModelSelection,
      sourceControlWritingStyle: project.sourceControlWritingStyle,
      branchNamingMode: project.branchNamingMode,
      branchNamePrefix: project.branchNamePrefix,
      branchNameInstructions: project.branchNameInstructions,
    },
    writer: SharedSettings.resolveSourceControlWriterModelSelection(project, providers ?? []),
    stacked:
      project.sourceControlWriterModelSelection === null
        ? project.textGenerationModelSelection
        : SharedSettings.resolveSourceControlWriterModelSelection(project, providers ?? []),
  });
}

console.error("stage change request templates");
// ---- change request templates, read from real repositories -------------------------------------------
const gitRun = (cwd, args) => {
  const result = spawnSync(
    "git",
    [
      "-c",
      "user.name=Fixture",
      "-c",
      "user.email=fixture@example.com",
      "-c",
      "commit.gpgsign=false",
      ...args,
    ],
    { cwd, encoding: "utf8" },
  );
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
};
const writeFiles = (root, files) => {
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(NodePath.dirname(NodePath.join(root, path)), { recursive: true });
    writeFileSync(NodePath.join(root, path), content);
  }
};
const templateCases = [
  { name: "none", files: { "README.md": "hi\n" } },
  {
    name: "github template",
    files: { ".github/pull_request_template.md": "## Summary\n\n<!-- what -->\n" },
  },
  {
    name: "uppercase github template",
    files: { ".github/PULL_REQUEST_TEMPLATE.md": "## Upper\n" },
  },
  {
    name: "first listed path wins",
    files: {
      "docs/pull_request_template.md": "docs\n",
      "pull_request_template.md": "root\n",
      ".github/pull_request_template.md": "github\n",
    },
  },
  {
    name: "empty template falls through",
    files: { ".github/pull_request_template.md": "  \n", "pull_request_template.md": "root\n" },
  },
  {
    name: "directory with one template",
    files: {
      ".github/PULL_REQUEST_TEMPLATE/bug.md": "bug\n",
      ".github/PULL_REQUEST_TEMPLATE/notes.txt": "ignored\n",
    },
  },
  {
    name: "directory with two templates",
    files: {
      ".github/PULL_REQUEST_TEMPLATE/bug.md": "bug\n",
      ".github/PULL_REQUEST_TEMPLATE/feature.MD": "feature\n",
    },
  },
  {
    name: "directory ambiguous stops the search",
    files: {
      ".github/PULL_REQUEST_TEMPLATE/a.md": "a\n",
      ".github/PULL_REQUEST_TEMPLATE/b.md": "b\n",
      "docs/PULL_REQUEST_TEMPLATE/c.md": "c\n",
    },
  },
  {
    name: "directory with one empty and one real",
    files: { "PULL_REQUEST_TEMPLATE/a.md": "\n", "PULL_REQUEST_TEMPLATE/b.md": "real\n" },
  },
  { name: "nested files ignored", files: { "PULL_REQUEST_TEMPLATE/nested/a.md": "nested\n" } },
  {
    name: "large template truncated",
    files: { ".github/pull_request_template.md": long("Template line.\n", 9_500) },
  },
  { name: "exact size template", files: { ".github/pull_request_template.md": long("x", 8_000) } },
];
for (const entry of templateCases) {
  const root = mkdtempSync(NodePath.join(tmpdir(), "t3-template-fixture-"));
  gitRun(root, ["init", "--initial-branch=main"]);
  writeFiles(root, entry.files);
  gitRun(root, ["add", "-A"]);
  gitRun(root, ["commit", "-m", "Initial"]);
  const found = await run(
    Effect.gen(function* () {
      const driver = yield* GitVcsDriver.GitVcsDriver;
      return yield* detectPrTemplate(root, "main", driver.execute);
    }).pipe(
      Effect.provide(
        GitVcsDriver.layer.pipe(
          Layer.provideMerge(VcsProcess.layer),
          Layer.provideMerge(NodeServices.layer),
          Layer.provideMerge(ServerConfig.layerTest(process.cwd(), { prefix: "t3-template-" })),
        ),
      ),
      Effect.provide(NodeServices.layer),
    ),
  );
  rows.templates.push({
    name: entry.name,
    files: entry.files,
    template: found._tag === "Some" ? found.value : null,
  });
  rmSync(root, { recursive: true, force: true });
}

console.error("stage what GitManager sends");
// ---- what GitManager sends for a commit ------------------------------------------------------------------
const gitInputCases = [
  {
    name: "repo conventions with subjects and AGENTS.md",
    commits: ["Add parser", "Fix lexer bug", "Merge-free history"],
    files: { "AGENTS.md": "# Rules\nUse tabs.\n" },
  },
  { name: "repo conventions without any instructions", commits: [], files: {} },
  {
    name: "repo conventions with a claude writer reads CLAUDE.md",
    commits: ["One"],
    files: { "AGENTS.md": "agents\n", "CLAUDE.md": "claude rules\n" },
    overrides: {
      textGenerationModelSelection: { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
    },
  },
  {
    name: "repo conventions with a codex writer ignores CLAUDE.md",
    commits: ["One"],
    files: { "CLAUDE.md": "claude rules\n" },
  },
  {
    name: "claude instance writer by driver",
    commits: ["One"],
    files: { "CLAUDE.md": "claude rules\n" },
    overrides: {
      textGenerationModelSelection: { instanceId: "claude_work", model: "claude-haiku-4-5" },
      providerInstances: { claude_work: { driver: "claudeAgent", config: {} } },
    },
    providers: [claudeSnapshot({ instanceId: "claude_work" })],
  },
  {
    name: "oversize AGENTS.md ignored",
    commits: ["One"],
    files: { "AGENTS.md": long("x", 20_001) },
  },
  { name: "AGENTS.md at the size limit", commits: [], files: { "AGENTS.md": long("y", 20_000) } },
  {
    name: "conventional commits",
    commits: ["One"],
    files: { "AGENTS.md": "ignored\n" },
    overrides: { sourceControlWritingStyle: { mode: "conventional_commits" } },
  },
  {
    name: "custom instructions",
    commits: ["One"],
    files: {},
    overrides: {
      sourceControlWritingStyle: { mode: "custom", customInstructions: "Write in French." },
    },
  },
  {
    name: "custom without instructions",
    commits: ["One"],
    files: {},
    overrides: { sourceControlWritingStyle: { mode: "custom", customInstructions: "" } },
  },
  {
    name: "dedicated writer",
    commits: ["One"],
    files: {},
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-4-6" },
    },
    providers: [claudeSnapshot()],
  },
  {
    name: "dedicated writer unavailable",
    commits: ["One"],
    files: {},
    overrides: {
      sourceControlWriterModelSelection: { instanceId: "claudeAgent", model: "claude-sonnet-4-6" },
    },
    providers: [claudeSnapshot({ availability: "unavailable" })],
  },
  {
    name: "codex disabled falls back to claude",
    commits: ["One"],
    files: {},
    overrides: { providers: { codex: { enabled: false } } },
  },
  { name: "feature branch asks for a branch", commits: ["One"], files: {}, featureBranch: true },
  { name: "detached head", commits: ["One"], files: {}, detach: true },
];
for (const entry of gitInputCases) {
  const root = mkdtempSync(NodePath.join(tmpdir(), "t3-text-input-fixture-"));
  const work = NodePath.join(root, "work");
  mkdirSync(work);
  gitRun(work, ["init", "--initial-branch=main"]);
  writeFiles(work, { "README.md": "hello\n" });
  gitRun(work, ["add", "-A"]);
  gitRun(work, ["commit", "-m", "Initial commit"]);
  for (const [index, subject] of entry.commits.entries()) {
    writeFiles(work, { [`history-${index}.txt`]: `${index}\n` });
    gitRun(work, ["add", "-A"]);
    gitRun(work, ["commit", "-m", subject]);
  }
  writeFiles(work, entry.files);
  if (entry.detach) gitRun(work, ["checkout", "--detach"]);
  writeFiles(work, { "change.txt": "a change\n" });
  const calls = [];
  const textGeneration = TextGeneration.TextGeneration.of({
    generateCommitMessage: (input) =>
      Effect.sync(() => {
        calls.push({ ...input, cwd: "<cwd>" });
        return {
          subject: "Subject",
          body: "",
          ...(input.includeBranch ? { branch: "feature/x" } : {}),
        };
      }),
    generatePrContent: () => Effect.succeed({ title: "t", body: "b" }),
    generateBranchName: () => Effect.succeed({ branch: "b" }),
    generateThreadTitle: () => Effect.succeed({ title: "t" }),
  });
  const registry = SourceControlProviderRegistry.SourceControlProviderRegistry.of({
    resolveLink: () => undefined,
    get: () => Effect.die("unused"),
    resolveHandle: () => Effect.die("unused"),
    resolve: () => Effect.die("unused"),
    discover: Effect.succeed([]),
  });
  const layerVcs = GitVcsDriver.layer.pipe(
    Layer.provideMerge(VcsProcess.layer),
    Layer.provideMerge(NodeServices.layer),
    Layer.provideMerge(ServerConfig.layerTest(process.cwd(), { prefix: "t3-text-input-" })),
  );
  const layerManager = Layer.mergeAll(
    Layer.succeed(TextGeneration.TextGeneration, textGeneration),
    Layer.mock(ProviderRegistry.ProviderRegistry)({
      getProviders: Effect.succeed(entry.providers ?? []),
    }),
    Layer.succeed(ProjectSetupScriptRunner.ProjectSetupScriptRunner, {
      runForThread: () => Effect.succeed({ status: "no-script" }),
    }),
    layerVcs,
    ServerSettings.ServerSettingsService.layerTest(entry.overrides ?? {}),
  ).pipe(
    Layer.provideMerge(
      Layer.succeed(SourceControlProviderRegistry.SourceControlProviderRegistry, registry),
    ),
    Layer.provideMerge(NodeServices.layer),
  );
  const layerStores = Layer.merge(ProjectionStore.layer, ProjectStore.layer).pipe(
    Layer.provideMerge(SqlitePersistence.layerMemory),
  );
  const outcome = await exitOf(
    Effect.scoped(
      Effect.gen(function* () {
        const stores = yield* Layer.build(layerStores);
        const manager = yield* GitManager.make.pipe(
          Effect.provide(layerManager),
          Effect.provideContext(stores),
        );
        return yield* manager.runStackedAction(
          {
            actionId: "a",
            cwd: work,
            action: "commit",
            ...(entry.featureBranch ? { featureBranch: true } : {}),
          },
          { actionId: "a", progressReporter: { publish: () => Effect.void } },
        );
      }),
    ),
  );
  const settings = Schema.encodeSync(ContractSettings.ServerSettings)(
    Schema.decodeUnknownSync(ContractSettings.ServerSettings)(
      Struct.deepMerge(encodedDefaults, entry.overrides ?? {}),
    ),
  );
  const call = calls[0];
  rows["git-inputs"].push({
    name: entry.name,
    commits: entry.commits,
    files: entry.files,
    detach: entry.detach === true,
    featureBranch: entry.featureBranch === true,
    settings,
    providers: entry.providers ?? [],
    called: call !== undefined,
    input:
      call === undefined
        ? null
        : {
            branch: call.branch,
            stagedSummary: call.stagedSummary,
            stagedPatch: call.stagedPatch,
            includeBranch: call.includeBranch === true,
            policy:
              JSON.stringify(call.policy ?? null).length < 3_000 ? (call.policy ?? null) : "<long>",
            modelSelection: call.modelSelection,
          },
    ...(call === undefined
      ? { prompt: null }
      : packPrompt(
          Prompts.buildCommitMessagePrompt({
            branch: call.branch,
            stagedSummary: call.stagedSummary,
            stagedPatch: call.stagedPatch,
            includeBranch: call.includeBranch === true,
            policy: call.policy,
          }).prompt,
        )),
    ok: outcome.ok,
  });
  rmSync(root, { recursive: true, force: true });
}

console.error("stage how the CLIs");
// ---- how the CLIs are started ---------------------------------------------------------------------------
const bin = mkdtempSync(NodePath.join(tmpdir(), "t3-text-bin-"));
const recorder = `#!/usr/bin/env node
const fs = require("node:fs");
const chunks = [];
process.stdin.on("data", (chunk) => chunks.push(chunk));
process.stdin.on("end", () => {
  const argv = process.argv.slice(2);
  const read = (flag) => { const at = argv.indexOf(flag); return at < 0 ? null : argv[at + 1]; };
  const schemaPath = read("--output-schema");
  const outputPath = read("--output-last-message");
  const capture = { argv, cwd: fs.realpathSync(process.cwd()), env: { CLAUDE_CONFIG_DIR: process.env.CLAUDE_CONFIG_DIR ?? null, CODEX_HOME: process.env.CODEX_HOME ?? null }, stdin: Buffer.concat(chunks).toString("utf8"), schema: schemaPath ? fs.readFileSync(schemaPath, "utf8") : null };
  fs.writeFileSync(process.env.T3_CAPTURE, JSON.stringify(capture));
  if (process.env.T3_STDERR) process.stderr.write(process.env.T3_STDERR);
  if (outputPath && process.env.T3_OUTPUT_FILE !== undefined) fs.writeFileSync(outputPath, process.env.T3_OUTPUT_FILE);
  process.stdout.write(process.env.T3_STDOUT ?? "");
  process.exit(Number(process.env.T3_EXIT ?? 0));
});
`;
writeFileSync(NodePath.join(bin, "recorder"), recorder);
chmodSync(NodePath.join(bin, "recorder"), 0o755);
const recorderPath = NodePath.join(bin, "recorder");
const workDir = mkdtempSync(NodePath.join(tmpdir(), "t3-text-cwd-"));
const realWork = realpathSync(workDir);
const claudeSelections = [
  { instanceId: "claudeAgent", model: "claude-haiku-4-5" },
  {
    instanceId: "claudeAgent",
    model: "claude-haiku-4-5",
    options: [{ id: "thinking", value: true }],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-haiku-4-5",
    options: [{ id: "thinking", value: false }],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-sonnet-4-6",
    options: [{ id: "effort", value: "high" }],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-sonnet-4-6",
    options: [
      { id: "effort", value: "max" },
      { id: "contextWindow", value: "1m" },
    ],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-sonnet-4-6",
    options: [{ id: "effort", value: "ultrathink" }],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-opus-4-6",
    options: [
      { id: "contextWindow", value: "1m" },
      { id: "fastMode", value: true },
      { id: "effort", value: "ultrathink" },
    ],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-opus-4-8",
    options: [{ id: "effort", value: "ultracode" }],
  },
  {
    instanceId: "claudeAgent",
    model: "claude-opus-4-8",
    options: [{ id: "fastMode", value: false }],
  },
  { instanceId: "claudeAgent", model: "sonnet" },
  { instanceId: "claudeAgent", model: "haiku-4.5" },
  { instanceId: "claudeAgent", model: "claude-fable-5-1" },
  {
    instanceId: "claudeAgent",
    model: "claude-opus-4-7",
    options: [{ id: "effort", value: "xhigh" }],
  },
  { instanceId: "claudeAgent", model: "custom-model-slug" },
  {
    instanceId: "claudeAgent",
    model: "custom-model-slug",
    options: [{ id: "effort", value: "high" }],
  },
];
const codexSelections = [
  { instanceId: "codex", model: "gpt-6-luna" },
  { instanceId: "codex", model: "gpt-6-luna", options: [{ id: "reasoningEffort", value: "high" }] },
  {
    instanceId: "codex",
    model: "gpt-6-astra",
    options: [
      { id: "reasoningEffort", value: "xhigh" },
      { id: "serviceTier", value: "flex" },
    ],
  },
  { instanceId: "codex", model: "gpt-6-astra", options: [{ id: "fastMode", value: true }] },
  {
    instanceId: "codex",
    model: "gpt-6-astra",
    options: [
      { id: "fastMode", value: false },
      { id: "reasoningEffort", value: "low" },
    ],
  },
  { instanceId: "codex", model: "openai.gpt-5.6-sol" },
];
const answers = {
  commit: { subject: "Add a thing", body: "- detail" },
  commitBranch: { subject: "Add a thing", body: "", branch: "Feature Thing" },
  pr: { title: "Add a thing", body: "## Summary\n- x" },
  branch: { branch: "Add Search" },
  title: { title: "Search Sidebar", needsRefinement: false },
};
const generationInputs = (selection) => ({
  generateCommitMessage: {
    cwd: realWork,
    branch: "feature/x",
    stagedSummary: "M\ta.txt",
    stagedPatch: "+x",
    includeBranch: false,
    modelSelection: selection,
  },
  generatePrContent: {
    cwd: realWork,
    baseBranch: "main",
    headBranch: "feature/x",
    commitSummary: "abc Add",
    diffSummary: "a.txt | 1 +",
    diffPatch: "+x",
    modelSelection: selection,
  },
  generateBranchName: {
    cwd: realWork,
    message: "Add search",
    naming: { mode: "static", prefix: "t3", instructions: "" },
    modelSelection: selection,
  },
  generateThreadTitle: {
    cwd: realWork,
    message: "Fix the login bug",
    linkedContext: "none",
    modelSelection: selection,
  },
});
const answerFor = {
  generateCommitMessage: answers.commit,
  generatePrContent: answers.pr,
  generateBranchName: answers.branch,
  generateThreadTitle: answers.title,
};
const makeOriginal = (provider, config, environment) =>
  Effect.gen(function* () {
    return provider === "claude"
      ? yield* makeClaudeTextGeneration(
          {
            enabled: true,
            binaryPath: recorderPath,
            homePath: "",
            customModels: [],
            launchArgs: "",
            autoCompactWindow: "",
            ...config,
          },
          environment,
        )
      : yield* makeCodexTextGeneration(
          {
            enabled: true,
            binaryPath: recorderPath,
            homePath: "",
            shadowHomePath: "",
            launchArgs: "",
            customModels: [],
            ...config,
          },
          environment,
        );
  }).pipe(
    Effect.provide(NodeServices.layer),
    Effect.provide(
      ServerConfig.layerTest(process.cwd(), { prefix: "t3-text-run-" }).pipe(
        Layer.provideMerge(NodeServices.layer),
      ),
    ),
  );
const normalize = (capture, expectedCwd) => ({
  ...capture,
  argv: capture.argv.map((arg) =>
    existsSync(arg) && arg.includes("t3code-codex-")
      ? arg.includes("schema")
        ? "<schema>"
        : "<output>"
      : arg,
  ),
  cwd:
    capture.cwd === expectedCwd
      ? "<cwd>"
      : capture.cwd.includes("t3code-claude-title-")
        ? "<title-dir>"
        : capture.cwd,
});
const normalizeArgs = (args) =>
  args.map((arg, index) =>
    args[index - 1] === "--output-schema"
      ? "<schema>"
      : args[index - 1] === "--output-last-message"
        ? "<output>"
        : arg,
  );
let captureIndex = 0;
const record = async ({
  provider,
  op,
  selection,
  config = {},
  stdout = "",
  outputFile,
  exit = 0,
  stderr,
  label,
  path,
}) => {
  const capture = NodePath.join(bin, `capture-${captureIndex++}.json`);
  const environment = {
    ...process.env,
    T3_CAPTURE: capture,
    T3_STDOUT: stdout,
    T3_EXIT: String(exit),
    ...(stderr === undefined ? {} : { T3_STDERR: stderr }),
    ...(outputFile === undefined ? {} : { T3_OUTPUT_FILE: outputFile }),
  };
  if (path !== undefined) environment.PATH = path;
  delete environment.CLAUDE_CONFIG_DIR;
  delete environment.CODEX_HOME;
  delete environment.T3CODE_CODEX_LAUNCH_ARGS;
  const input = generationInputs(selection)[op];
  const outcome = await exitOf(
    Effect.gen(function* () {
      const service = yield* makeOriginal(provider, config, environment);
      return yield* service[op](input);
    }).pipe(Effect.scoped),
  );
  const captured = existsSync(capture) ? JSON.parse(readFileSync(capture, "utf8")) : null;
  rows.invocations.push({
    label,
    provider,
    op,
    selection,
    config,
    stdout,
    outputFile: outputFile ?? null,
    exit,
    stderr: stderr ?? null,
    input: { ...input, cwd: "<cwd>" },
    capture:
      captured === null
        ? null
        : {
            ...captured,
            argv: normalizeArgs(captured.argv),
            cwd: normalize(captured, realWork).cwd,
          },
    ...(outcome.ok
      ? { ok: true, result: outcome.value }
      : {
          ok: false,
          error: {
            tag: outcome.error.tag,
            operation: outcome.error.operation,
            detail: outcome.error.detail,
          },
        }),
  });
};
const claudeEnvelope = (value) => JSON.stringify({ type: "result", structured_output: value });
for (const selection of claudeSelections)
  for (const op of ["generateCommitMessage", "generateThreadTitle"])
    await record({
      provider: "claude",
      op,
      selection,
      stdout: claudeEnvelope(answerFor[op]),
      label: "selection",
    });
for (const op of [
  "generatePrContent",
  "generateBranchName",
  "generateThreadTitle",
  "generateCommitMessage",
])
  await record({
    provider: "claude",
    op,
    selection: claudeSelections[3],
    stdout: claudeEnvelope(answerFor[op]),
    label: "operation",
  });
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  config: { homePath: "/tmp/claude-home" },
  stdout: claudeEnvelope(answers.commit),
  label: "home",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: JSON.stringify([
    { type: "system" },
    { type: "result", structured_output: answers.commit },
  ]),
  label: "verbose array",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: JSON.stringify([
    { type: "result", structured_output: { subject: "first", body: "" } },
    { type: "assistant" },
    { type: "result", structured_output: answers.commit },
  ]),
  label: "last result wins",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: JSON.stringify([{ type: "system" }]),
  label: "array without result",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: JSON.stringify([{ nope: 1 }]),
  label: "array of non-messages",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: "",
  label: "empty output",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: "not json",
  label: "malformed output",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: JSON.stringify({ result: "text" }),
  label: "object without structured output",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: claudeEnvelope({ subject: "only" }),
  label: "missing field",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: claudeEnvelope({ subject: 1, body: "" }),
  label: "wrong type",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: claudeEnvelope(null),
  label: "null structured output",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: claudeEnvelope({ subject: "x", body: "", extra: true }),
  label: "extra field ignored",
});
await record({
  provider: "claude",
  op: "generateThreadTitle",
  selection: claudeSelections[0],
  stdout: claudeEnvelope({ title: "No refinement flag" }),
  label: "title without flag",
});
await record({
  provider: "claude",
  op: "generateThreadTitle",
  selection: claudeSelections[0],
  stdout: claudeEnvelope({ title: "Bad flag", needsRefinement: "yes" }),
  label: "title with bad flag",
});
await record({
  provider: "claude",
  op: "generateThreadTitle",
  selection: claudeSelections[0],
  stdout: claudeEnvelope({ title: "Unclear", needsRefinement: true }),
  label: "title needing refinement",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: "boom out",
  stderr: "claude is not signed in",
  exit: 1,
  label: "failure with stderr",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: "only stdout",
  exit: 2,
  label: "failure with stdout only",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  stdout: "",
  exit: 3,
  label: "failure with nothing",
});
await record({
  provider: "claude",
  op: "generateCommitMessage",
  selection: claudeSelections[0],
  config: { binaryPath: "/nonexistent/claude-binary" },
  stdout: "",
  label: "binary missing",
});
for (const selection of codexSelections)
  for (const op of ["generateCommitMessage", "generateThreadTitle"])
    await record({
      provider: "codex",
      op,
      selection,
      outputFile: JSON.stringify(answerFor[op]),
      label: "selection",
    });
for (const op of ["generatePrContent", "generateBranchName"])
  await record({
    provider: "codex",
    op,
    selection: codexSelections[0],
    outputFile: JSON.stringify(answerFor[op]),
    label: "operation",
  });
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  config: { homePath: "/tmp/codex-home" },
  outputFile: JSON.stringify(answers.commit),
  label: "home",
});
for (const launchArgs of [
  "--config model_verbosity=low -c foo=bar --enable web --disable shell --strict-config --profile work -m x",
  "--config=a=b -c=c=d --enable=x --disable=y",
  "--config --enable web",
  "-c -x --disable",
  "--search --sandbox danger-full-access",
])
  await record({
    provider: "codex",
    op: "generateCommitMessage",
    selection: codexSelections[0],
    config: { launchArgs },
    outputFile: JSON.stringify(answers.commit),
    label: "launch args",
  });
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  outputFile: "",
  label: "empty output file",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  outputFile: "not json",
  label: "malformed output file",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  outputFile: JSON.stringify({ subject: "only" }),
  label: "missing field",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  outputFile: `Here you go: ${JSON.stringify(answers.commit)}`,
  label: "output wrapped in prose",
});
await record({
  provider: "codex",
  op: "generateThreadTitle",
  selection: codexSelections[0],
  outputFile: JSON.stringify({ title: "No flag" }),
  label: "title without flag",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  stdout: "oops",
  stderr: "not logged in",
  exit: 1,
  outputFile: "",
  label: "failure with stderr",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  stdout: "stdout words",
  exit: 4,
  label: "failure with stdout only",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  exit: 5,
  label: "failure with nothing",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  config: { binaryPath: "/nonexistent/codex-binary" },
  label: "binary missing",
});
await record({
  provider: "codex",
  op: "generateCommitMessage",
  selection: codexSelections[0],
  outputFile: undefined,
  label: "no output file written",
});
rmSync(bin, { recursive: true, force: true });
rmSync(workDir, { recursive: true, force: true });

// ---- write -----------------------------------------------------------------------------------------------
const summary = {};
for (const [name, list] of Object.entries(rows)) {
  const target = new URL(
    `../crates/server/tests/fixtures/text-generation-${name}.jsonl`,
    import.meta.url,
  );
  const temporary = new URL(`${target.href}.tmp`);
  writeFileSync(temporary, `${list.map(encodeRow).join("\n")}\n`);
  renameSync(temporary, target);
  summary[name] = list.length;
}
console.log(JSON.stringify(summary));
