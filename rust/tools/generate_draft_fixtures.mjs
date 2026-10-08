// Development-only oracle. Extract unchanged pure functions from the original
// store so its React/platform imports never run; Rust tests read JSON fixtures.
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url)).replace(/\/$/, "");
const contracts = await import(pathToFileURL(root + "/packages/contracts/src/index.ts"));
const model = await import(pathToFileURL(root + "/packages/shared/src/model.ts"));
const refs = await import(
  pathToFileURL(root + "/packages/shared/src/composerContextReferences.ts")
);
const Schema = await import(
  pathToFileURL(root + "/packages/contracts/node_modules/effect/dist/Schema.js")
);
function functions(file, names) {
  const source = readFileSync(root + "/" + file, "utf8");
  return [...source.matchAll(/^(?:export )?function (\w+)\(/gm)]
    .filter((match) => !names || names.includes(match[1]))
    .map((match) => {
      const tail = source.slice(match.index);
      const end = /^}\s*$/m.exec(tail);
      if (!end) throw Error("Missing function boundary " + match[1]);
      return tail.slice(0, end.index + 1).replace(/^export /, "");
    })
    .join("\n");
}
function variables(file, names) {
  const source = readFileSync(root + "/" + file, "utf8");
  return names
    .map((name) => {
      const match = new RegExp("^(?:export )?const " + name + "\\b", "m").exec(source);
      if (!match) throw Error("Missing constant " + name);
      const end = source.indexOf(";", match.index);
      return source.slice(match.index, end + 1).replace(/^export /, "");
    })
    .join("\n");
}
const storeFunctions = [
  "cloneModelSelection",
  "compactModelSelectionByProvider",
  "compactStickyOptionsByModel",
  "seedStickyOptionsByModel",
  "normalizeProviderDriverKind",
  "normalizeProviderInstanceId",
  "coerceProviderOptionSelections",
  "normalizeProviderModelOptions",
  "normalizeModelSelection",
  "legacySyncModelSelectionOptions",
  "legacyMergeModelSelectionIntoProviderModelOptions",
  "legacyReplaceProviderModelOptions",
  "legacyToModelSelectionByProvider",
  "normalizePersistedAttachment",
  "normalizePersistedTerminalContextDraft",
  "normalizeDraftThreadEnvMode",
  "projectDraftKey",
  "logicalProjectDraftKey",
  "composerTargetKey",
  "normalizeLegacyComposerStorageKey",
  "composerThreadRefFromKey",
  "normalizePersistedDraftThreads",
  "normalizePersistedDraftsByThreadId",
  "persistedComposerDraftHasUserContent",
  "stripLegacyModelSeedsFromEmptyDraftSessions",
  "normalizeCurrentPersistedComposerDraftStoreState",
  "migratePersistedComposerDraftStoreState",
];
const contextSource = functions("apps/web/src/lib/composerContextReferences.ts");
const recordSource = functions("apps/web/src/lib/composerContextRecords.ts", [
  "basename",
  "reviewCommentContextLabel",
  "isPullRequestSummaryContext",
  "pullRequestContextNumber",
  "previewAnnotationContextLabel",
  "terminalContextReference",
  "reviewCommentContextId",
  "previewAnnotationContextId",
  "reviewCommentContextReference",
  "previewAnnotationContextReference",
  "threadContextReference",
  "fileContextReference",
]);
const scopedSource = functions("packages/client-runtime/src/environment/scoped.ts");
const body = stripTypeScriptTypes(
  "function makeOracle(){\n" +
    [
      variables("apps/web/src/reviewCommentContext.ts", [
        "ReviewCommentSelectionSchema",
        "ReviewCommentContextSchema",
      ]),
      contextSource,
      recordSource,
      scopedSource,
      functions("apps/web/src/lib/terminalContext.ts"),
      functions("apps/web/src/lib/elementContext.ts", ["elementContextToPreviewAnnotation"]),
      functions("apps/web/src/composerDraftStore.ts", storeFunctions),
      `const isProviderDriverKind=Schema.is(ProviderDriverKind),isRuntimeMode=Schema.is(RuntimeMode),isSnapShotSource=Schema.is(SnapShotSource),isReviewCommentContext=Schema.is(ReviewCommentContextSchema),isThreadContextRecord=Schema.is(ThreadContextRecord),isPreviewAnnotationPayload=Schema.is(PreviewAnnotationPayloadSchema);`,
      variables("apps/web/src/composerDraftStore.ts", [
        "PROVIDER_INSTANCE_ID_PATTERN",
        "EMPTY_PERSISTED_DRAFT_STORE_STATE",
        "PersistedComposerDraftFileAttachment",
      ]),
      `const isPersistedComposerDraftFileAttachment=Schema.is(PersistedComposerDraftFileAttachment);`,
      `return {normalizeCurrentPersistedComposerDraftStoreState,migratePersistedComposerDraftStoreState};`,
    ].join("\n") +
    "\n}",
);
const fixedNow = "2026-10-08T12:00:00.000Z";
class FixedDate extends Date {
  constructor(...args) {
    super(...(args.length ? args : [fixedNow]));
  }
}
const bindings = {
  Schema,
  Date: FixedDate,
  ...Object.fromEntries(
    [
      "ProviderDriverKind",
      "RuntimeMode",
      "SnapShotSource",
      "ThreadContextRecord",
      "PreviewAnnotationPayloadSchema",
      "ElementContextDetails",
      "PullRequestContextMetadata",
      "PastedTextAttachmentSource",
      "EnvironmentId",
      "ProjectId",
      "ThreadId",
      "DEFAULT_MODEL",
      "DEFAULT_MODEL_BY_PROVIDER",
      "defaultInstanceIdForDriver",
    ].map((name) => [name, contracts[name]]),
  ),
  ...Object.fromEntries(
    ["normalizeModelSlug", "createModelSelection"].map((name) => [name, model[name]]),
  ),
  ...Object.fromEntries(
    [
      "collectComposerContextReferences",
      "formatComposerContextReference",
      "replaceComposerContextReferences",
      "sanitizeComposerContextLabel",
    ].map((name) => [name, refs[name]]),
  ),
  DEFAULT_RUNTIME_MODE: "full-access",
  DEFAULT_INTERACTION_MODE: "default",
  CONTEXT_ID_PATTERN: /^[a-z0-9_-]{1,128}$/i,
  PREVIEW_LABEL_MAX_CHARS: 48,
  INLINE_TERMINAL_CONTEXT_PLACEHOLDER: "\uFFFC",
};
const oracle = new Function(...Object.keys(bindings), body + "\nreturn makeOracle();")(
  ...Object.values(bindings),
);
const row = (prompt = "", extra = {}) => ({ prompt, attachments: [], ...extra });
const session = (extra = {}) => ({
  threadId: "draft-thread",
  environmentId: "environment-a",
  projectId: "project",
  createdAt: fixedNow,
  runtimeMode: "approval-required",
  interactionMode: "default",
  branch: "main",
  worktreePath: null,
  envMode: "worktree",
  startFromOrigin: false,
  ...extra,
});
const states = [
  null,
  false,
  [],
  {},
  {
    draftsByThreadKey: {
      "environment-a:thread": row("Keep unsent prompt", { runtimeMode: "approval-required" }),
    },
  },
  {
    draftsByThreadId: {
      "draft-id": row("Legacy prompt", {
        provider: "codex",
        model: "5.3",
        effort: "high",
        codexFastMode: true,
      }),
    },
    draftThreadsByThreadId: { "draft-id": session() },
    projectDraftThreadIdByProjectKey: { "environment-a:project": "draft-id" },
  },
  {
    draftsByThreadKey: {
      "draft-id": row("", {
        modelSelectionByProvider: { codex: { instanceId: "codex", model: "gpt-5.4" } },
        activeProvider: "codex",
        runtimeMode: "approval-required",
      }),
    },
    draftThreadsByThreadKey: { "draft-id": session() },
    logicalProjectDraftThreadKeyByLogicalProjectKey: { "environment-a:/workspace": "draft-id" },
  },
  {
    draftsByThreadKey: {
      "draft-id": row("", {
        modelSelectionByProvider: { codex: { instanceId: "codex", model: "gpt-5.4" } },
        activeProvider: "codex",
        modelSelectionExplicit: true,
      }),
    },
    draftThreadsByThreadKey: { "draft-id": session() },
    logicalProjectDraftThreadKeyByLogicalProjectKey: { "environment-a:project": "draft-id" },
  },
  { draftsByThreadKey: { "environment-a:thread": row("A"), "environment-b:thread": row("B") } },
  { stickyModel: "gpt-5.6-terra", stickyModelOptions: { codex: { reasoningEffort: "xhigh" } } },
  {
    stickyProvider: "claudeAgent",
    stickyModel: " sonnet ",
    stickyModelOptions: { claudeAgent: { effort: "max" } },
  },
  {
    stickyModelSelectionByProvider: {
      codex_personal: {
        instanceId: "codex_personal",
        model: "owned",
        options: [{ id: "custom", value: false }],
      },
    },
    stickyActiveProvider: "codex_personal",
  },
  {
    draftThreadsByThreadKey: {
      "environment-a:draft": session({
        projectId: "concrete-id",
        logicalProjectKey: "environment-a:/workspace",
      }),
    },
    logicalProjectDraftThreadKeyByLogicalProjectKey: {
      "environment-a:/workspace": "environment-a:draft",
    },
  },
  {
    projectDraftThreadIdByProjectKey: { "environment-a:project": "recovered" },
    draftsByThreadId: { recovered: row("Recover missing session") },
  },
  {
    draftsByThreadKey: {
      "draft-id": row("", {
        files: [{ id: "f", name: "file.txt", mimeType: "text/plain", sizeBytes: 2 }],
        modelSelectionByProvider: { codex: { instanceId: "codex", model: "gpt-5.4" } },
        activeProvider: "codex",
      }),
    },
    draftThreadsByThreadKey: { "draft-id": session() },
    logicalProjectDraftThreadKeyByLogicalProjectKey: { "environment-a:project": "draft-id" },
  },
  {
    draftsByThreadKey: {
      "environment-a:thread": row("Run \uFFFC now", {
        terminalContexts: [
          {
            id: "terminal:producer.1",
            threadId: "thread",
            createdAt: fixedNow,
            terminalId: " term ",
            terminalLabel: " Build ",
            lineStart: -1.2,
            lineEnd: 0.7,
            text: "kept\r\nraw",
          },
        ],
      }),
    },
  },
  {
    draftsByThreadKey: {
      "environment-a:thread": row("![old](t3-context://v1/image/image.1)", {
        attachments: [
          {
            id: "image.1",
            name: "image.png",
            mimeType: "image/png",
            sizeBytes: 3,
            dataUrl: "data:image/png;base64,AQID",
          },
        ],
      }),
    },
  },
  {
    draftsByThreadKey: {
      "environment-a:thread": row("", {
        reviewComments: [
          {
            id: "r:1",
            sectionId: "section",
            sectionTitle: "Comment",
            filePath: "/repo/file.rs",
            startIndex: 0,
            endIndex: 1,
            rangeLabel: "L1",
            text: "Review",
            diff: "line",
          },
        ],
      }),
    },
  },
  { draftsByThreadKey: { "environment-a:thread": row("", ["bad"]) } },
];

const element = {
  id: "legacy:element",
  pickedAt: fixedNow,
  pageUrl: "https://example.test",
  pageTitle: null,
  tagName: "button",
  selector: null,
  htmlPreview: "<button />",
  componentName: null,
  source: null,
  styles: "",
};
const preview = {
  id: "annotation",
  pageUrl: "https://example.test",
  pageTitle: null,
  comment: "",
  elements: [],
  regions: [],
  strokes: [],
  styleChanges: [],
  screenshot: null,
  createdAt: fixedNow,
};
const fields = {
  attachments: [{ id: "i", name: "", mimeType: "", sizeBytes: 0, dataUrl: "x" }],
  files: [{ id: "f", name: "", mimeType: "", sizeBytes: 0 }],
  terminalContexts: [
    {
      id: "a:b",
      threadId: "thread",
      createdAt: fixedNow,
      terminalId: " t ",
      terminalLabel: " label ",
      lineStart: 1.9,
      lineEnd: 8.9,
    },
  ],
  reviewComments: [
    {
      id: "r",
      sectionId: "s",
      sectionTitle: "",
      filePath: "",
      startIndex: 0,
      endIndex: 1,
      rangeLabel: "",
      text: "",
      diff: "",
    },
  ],
  elementContexts: [element],
  previewAnnotations: [preview],
};
for (const [field, records] of Object.entries(fields)) {
  states.push({ draftsByThreadKey: { "environment-a:thread": row("", { [field]: records }) } });
  for (const [property, original] of Object.entries(records[0]))
    for (const candidate of [undefined, null, false, 0, "", " invalid ", {}, []]) {
      const record = { ...records[0], [property]: candidate };
      states.push({
        draftsByThreadKey: { "environment-a:thread": row("", { [field]: [record] }) },
      });
    }
}
for (const options of [
  null,
  false,
  "bad",
  {},
  [],
  [{ id: "", value: true }],
  [
    { id: "a", value: "x" },
    { id: "a", value: false },
  ],
  { reasoningEffort: "low" },
]) {
  states.push({
    stickyModelSelectionByProvider: { codex: { instanceId: "codex", model: "5.3", options } },
    stickyActiveProvider: "codex",
  });
  states.push({
    draftsByThreadKey: {
      "environment-a:thread": row("kept", {
        modelSelectionByProvider: { codex: { instanceId: "codex", model: "5.3", options } },
        activeProvider: "codex",
      }),
    },
  });
  states.push({
    draftsByThreadKey: {
      "environment-a:thread": row("kept", {
        modelSelection: { provider: "codex", model: "5.3", options },
      }),
    },
  });
}

for (const ws of ["\uFEFF", "\u0085", "\u00A0", "\u2028", "\u001C"]) {
  states.push({
    draftsByThreadKey: {
      "draft-id": row(ws, {
        modelSelectionByProvider: { codex: { instanceId: "codex", model: "gpt-5.4" } },
        activeProvider: "codex",
      }),
    },
    draftThreadsByThreadKey: { "draft-id": session() },
  });
  states.push({
    draftsByThreadKey: {
      "environment-a:thread": row("\uFFFC", {
        terminalContexts: [
          {
            id: "t:1",
            threadId: "thread",
            createdAt: fixedNow,
            terminalId: ws + "terminal" + ws,
            terminalLabel: ws + "label" + ws,
            lineStart: 1,
            lineEnd: 2,
          },
        ],
      }),
    },
  });
  states.push({
    draftsByThreadKey: {
      "environment-a:thread": row("![" + ws + "label" + ws + "](t3-context://v1/image/image.1)", {
        attachments: [
          { id: "image.1", name: "i", mimeType: "image/png", sizeBytes: 1, dataUrl: "x" },
        ],
      }),
    },
  });
}
const picked = { ...element, stack: [] };
delete picked.id;
const nested = {
  elements: [{ id: "e", element: picked, rect: { x: 0, y: 0, width: 1, height: 1 } }],
  regions: [{ id: "r", rect: { x: 0, y: 0, width: 1, height: 1 }, label: null }],
  strokes: [
    {
      id: "s",
      points: [{ x: 0, y: 0 }],
      color: "red",
      width: 1,
      bounds: { x: 0, y: 0, width: 1, height: 1 },
    },
  ],
  styleChanges: [
    { targetId: "e", selector: null, property: "color", previousValue: "blue", value: "red" },
  ],
  screenshot: { dataUrl: "x", width: 1, height: 1, cropRect: { x: 0, y: 0, width: 1, height: 1 } },
};
for (const [field, records] of Object.entries(nested)) {
  states.push({
    draftsByThreadKey: {
      "environment-a:thread": row("", { previewAnnotations: [{ ...preview, [field]: records }] }),
    },
  });
  for (const bad of [null, false, {}, [], { id: "bad" }, ["bad"]])
    states.push({
      draftsByThreadKey: {
        "environment-a:thread": row("", { previewAnnotations: [{ ...preview, [field]: bad }] }),
      },
    });
}
for (const number of [1e20, 1e21, 1e100, 1e308, 9007199254740992, 1.2345678912345678e25])
  states.push({
    draftsByThreadKey: {
      "a:t": row("\uFFFC", {
        terminalContexts: [
          {
            id: "t:1",
            threadId: "t",
            createdAt: fixedNow,
            terminalId: "t",
            terminalLabel: "label",
            lineStart: number,
            lineEnd: number,
          },
        ],
      }),
    },
  });

for (const [field, optional] of [
  ["files", ["attachmentId", "environmentId", "source"]],
  ["reviewComments", ["fenceLanguage", "selection", "pullRequest"]],
]) {
  for (const property of optional)
    for (const candidate of [null, false, 0, "", " a ", {}, [], { _tag: "pasted-text" }])
      states.push({
        draftsByThreadKey: {
          "a:t": row("kept", { [field]: [{ ...fields[field][0], [property]: candidate }] }),
        },
      });
}
const fixtures = [];
for (const version of [1, 2, 8, 9, 10])
  for (const state of states) {
    let expected, error;
    try {
      expected =
        version === 9
          ? oracle.normalizeCurrentPersistedComposerDraftStoreState(state)
          : oracle.migratePersistedComposerDraftStoreState(state);
    } catch (e) {
      error = String(e);
    }
    fixtures.push({ version, state, now: fixedNow, expected, ...(error ? { error } : {}) });
  }
mkdirSync(root + "/rust/crates/client/tests/fixtures", { recursive: true });
writeFileSync(
  root + "/rust/crates/client/tests/fixtures/draft-recovery.jsonl",
  fixtures.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(`Generated ${fixtures.length} original draft recovery cases\n`);
