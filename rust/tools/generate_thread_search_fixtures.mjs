// Runs the original thread search (the SQL ranking over the V2 projection tables, snippet building,
// input schema and error envelope) and records what it produces. The Rust tests rebuild each
// scenario's threads and messages in the Rust store and require the same matches in the same order.
// The snippet helpers private to ThreadSearch.ts are cut out of that file verbatim.
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as SqlClient from "../../apps/server/node_modules/effect/dist/sql/SqlClient.js";
import * as SearchContracts from "../../packages/contracts/src/threadSearch.ts";
import * as SqlitePersistence from "../../apps/server/src/persistence/Sqlite.ts";
import * as ProjectionStore from "../../apps/server/src/orchestration-v2/ProjectionStore.ts";
import * as ProjectStore from "../../apps/server/src/orchestration-v2/ProjectStore.ts";
import * as ThreadSearch from "../../apps/server/src/orchestration-v2/ThreadSearch.ts";

const rows = [];
const json = (value) => JSON.parse(JSON.stringify(value));

// ---- snippets --------------------------------------------------------------------------------
const source = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/ThreadSearch.ts", import.meta.url),
  "utf8",
);
const cutStart = source.indexOf("function escapeLikePattern");
const cutEnd = source.indexOf("/**\n * Searches the finished");
if (cutStart < 0 || cutEnd < 0) throw new Error("cannot find the snippet helpers");
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-search-fixtures-"));
const helpers = NodePath.join(scratch, "helpers.ts");
writeFileSync(
  helpers,
  `${source.slice(cutStart, cutEnd)}\nexport { buildSearchSnippet, escapeLikePattern };\n`,
);
const { buildSearchSnippet, escapeLikePattern } = await import(pathToFileURL(helpers).href);
rmSync(scratch, { recursive: true, force: true });

const filler = (length, word = "lorem ") =>
  word.repeat(Math.ceil(length / word.length)).slice(0, length);
const snippetCases = [
  ["", "ab"],
  ["short text with needle", "needle"],
  ["x".repeat(240), "xx"],
  ["x".repeat(241), "xx"],
  [`needle${"x".repeat(300)}`, "needle"],
  [`${"x".repeat(300)}needle`, "needle"],
  [`${"x".repeat(100)}needle${"y".repeat(300)}`, "needle"],
  [`${"x".repeat(72)}needle${"y".repeat(300)}`, "needle"],
  [`${"x".repeat(73)}needle${"y".repeat(300)}`, "needle"],
  [`${"x".repeat(71)}needle${"y".repeat(300)}`, "needle"],
  [`${filler(400)}NEEDLE${filler(400)}`, "needle"],
  [`${filler(400)}needle${filler(400)}`, "NEEDLE"],
  [`${filler(400)}Needle`, "nEEdle"],
  [`  leading and   internal\n\n whitespace\t\t needle   ${filler(300)}`, "needle"],
  [`${filler(300)} needle here${filler(300)}`, "needle"],
  ["a   b   c ".repeat(60), "b   c"],
  ["a   b   c ".repeat(60), "b c"],
  ["a\n\nb\n\nc ".repeat(80), "b\n\nc"],
  [filler(500), "zzzz"],
  [filler(500), "lorem"],
  [`${"é".repeat(150)}needle${"ü".repeat(150)}`, "needle"],
  [`${"日本語".repeat(100)}needle${"日本語".repeat(100)}`, "needle"],
  [`${"😀".repeat(60)}needle${"😀".repeat(60)}`, "needle"],
  [`${"😀".repeat(200)}`, "😀"],
  ["İstanbul ".repeat(40), "istanbul"],
  ["ǅ".repeat(300), "ǅ"],
  [`${"a".repeat(236)}needle`, "needle"],
  [`${"a".repeat(240)}`, "aa"],
  [" ".repeat(500), "ab"],
  [`${" ".repeat(300)}needle${" ".repeat(300)}`, "needle"],
  ["x".repeat(239) + " needle", "needle"],
  ["needle " + "x".repeat(239), "needle"],
  [`${filler(210)}needle${filler(210)}`, "needle"],
  [`${"a".repeat(172)}needle${"b".repeat(5)}`, "needle"],
  [`${"a ".repeat(130)}needle`, "needle"],
  ["%_!% literal ".repeat(40), "%_!%"],
  ["ABC ".repeat(80), "abc abc"],
  ["ab cd ".repeat(70), "ab cd"],
  ["a\u0085b ".repeat(100), "a\u0085b"],
  ["﻿needle" + "x".repeat(300), "needle"],
];
for (const [text, query] of snippetCases) {
  const snippet = buildSearchSnippet(text, query);
  // A cut through a surrogate pair leaves a lone surrogate, which no Rust string can hold.
  if (/[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/.test(snippet)) {
    rows.push({ type: "snippet-split", text, query });
    continue;
  }
  rows.push({ type: "snippet", text, query, snippet });
}
for (const value of ["plain", "50%", "a_b", "x!y", "!%_", "%%", "__", "!!", "", "日本%語"])
  rows.push({ type: "escape", value, escaped: escapeLikePattern(value) });

// ---- input schema ----------------------------------------------------------------------------
const decodeInput = Schema.decodeUnknownExit(
  Schema.toCodecJson(SearchContracts.OrchestrationSearchThreadsInput),
);
const inputs = [
  { query: "ab" },
  { query: "a" },
  { query: "" },
  { query: "  ab  " },
  { query: " a " },
  { query: "   " },
  { query: "x".repeat(200) },
  { query: "x".repeat(201) },
  { query: ` ${"x".repeat(200)} ` },
  { query: "😀".repeat(100) },
  { query: "😀".repeat(101) },
  { query: " ab " },
  { query: "﻿ab" },
  { query: "ab", limit: 1 },
  { query: "ab", limit: 50 },
  { query: "ab", limit: 51 },
  { query: "ab", limit: 0 },
  { query: "ab", limit: -1 },
  { query: "ab", limit: 2.5 },
  { query: "ab", limit: 5.0 },
  { query: "ab", limit: "5" },
  { query: "ab", limit: null },
  { query: "ab", limit: 1e1 },
  { query: "ab", extra: true },
  {},
  { limit: 3 },
  { query: 5 },
  { query: null },
  null,
  "ab",
  5,
  [],
  [{ query: "ab" }],
];
for (const payload of inputs) {
  const exit = decodeInput(json(payload));
  rows.push({
    type: "input",
    payload: json(payload),
    ok: exit._tag === "Success" ? json(exit.value) : undefined,
    error: exit._tag === "Success" ? undefined : true,
  });
}

// ---- error envelope --------------------------------------------------------------------------
{
  const cause = new ThreadSearch.ThreadSearchError({
    operation: "query",
    cause: new Error("boom"),
  });
  const error = new SearchContracts.OrchestrationSearchThreadsError({
    message: "Failed to search threads",
    cause,
  });
  rows.push({
    type: "error",
    operation: "query",
    wire: json(
      Schema.encodeSync(Schema.toCodecJson(SearchContracts.OrchestrationSearchThreadsError))(error),
    ),
  });
  const decode = new ThreadSearch.ThreadSearchError({
    operation: "decode",
    cause: new Error("boom"),
  });
  rows.push({
    type: "error",
    operation: "decode",
    wire: json(
      Schema.encodeSync(Schema.toCodecJson(SearchContracts.OrchestrationSearchThreadsError))(
        new SearchContracts.OrchestrationSearchThreadsError({
          message: "Failed to search threads",
          cause: decode,
        }),
      ),
    ),
  });
}

// ---- the ranked search over real stores ------------------------------------------------------
const providerInstanceId = "codex";
const at = (minute) => DateTime.makeUnsafe(Date.UTC(2026, 8, 27, 0, minute));
const iso = (minute) => DateTime.formatIso(at(minute));
const createProject = (projectId) =>
  Effect.flatMap(ProjectStore.ProjectStoreV2, (projects) =>
    projects.apply({
      sequence: 0,
      eventId: `created:${projectId}`,
      aggregateKind: "project",
      aggregateId: projectId,
      occurredAt: iso(0),
      commandId: null,
      causationEventId: null,
      correlationId: null,
      metadata: {},
      type: "project.created",
      payload: {
        projectId,
        title: projectId,
        workspaceRoot: `/work/${projectId}`,
        defaultModelSelection: null,
        scripts: [],
        createdAt: iso(0),
        updatedAt: iso(0),
      },
    }),
  );
const deleteProject = (projectId) =>
  Effect.flatMap(ProjectStore.ProjectStoreV2, (projects) =>
    projects.apply({
      sequence: 0,
      eventId: `deleted:${projectId}`,
      aggregateKind: "project",
      aggregateId: projectId,
      occurredAt: iso(2),
      commandId: null,
      causationEventId: null,
      correlationId: null,
      metadata: {},
      type: "project.deleted",
      payload: { projectId, deletedAt: iso(2) },
    }),
  );
const threadEvent = (thread) => ({
  id: `created:${thread.id}`,
  type: "thread.created",
  threadId: thread.id,
  providerInstanceId,
  occurredAt: at(0),
  payload: {
    createdBy: "user",
    creationSource: "web",
    id: thread.id,
    projectId: thread.project,
    title: thread.id,
    providerInstanceId,
    modelSelection: { instanceId: providerInstanceId, model: "gpt-5" },
    runtimeMode: "full-access",
    interactionMode: "default",
    branch: null,
    worktreePath: null,
    activeProviderThreadId: null,
    lineage: { parentThreadId: null, relationshipToParent: null, rootThreadId: thread.id },
    forkedFrom: null,
    createdAt: at(0),
    updatedAt: at(thread.updated ?? 0),
    archivedAt: thread.archived ? at(1) : null,
    settledOverride: null,
    settledAt: null,
    lastVisitedAt: null,
    deletedAt: thread.deleted ? at(1) : null,
  },
});
const messageEvent = (message) => ({
  id: `message:${message.id}`,
  type: "message.updated",
  threadId: message.thread,
  providerInstanceId,
  occurredAt: at(message.minute ?? 1),
  payload: {
    createdBy: message.role === "user" ? "user" : "agent",
    creationSource: message.role === "user" ? "web" : "provider",
    id: message.id,
    threadId: message.thread,
    runId: null,
    nodeId: null,
    role: message.role,
    text: message.text,
    attachments: [],
    streaming: message.streaming ?? false,
    createdAt: at(message.minute ?? 1),
    updatedAt: at(message.minute ?? 1),
  },
});

const many = (count, make) => Array.from({ length: count }, (_, index) => make(index));
const scenarios = [
  {
    name: "the original test",
    projects: [{ id: "project:search" }, { id: "project:search-deleted", deleted: true }],
    threads: [
      { id: "thread:both", project: "project:search" },
      { id: "thread:assistant", project: "project:search" },
      { id: "thread:archived", project: "project:search", archived: true },
      { id: "thread:deleted", project: "project:search", deleted: true },
      { id: "thread:orphaned", project: "project:search-deleted" },
    ],
    messages: [
      {
        thread: "thread:both",
        id: "both-assistant",
        role: "assistant",
        text: "needle from the answer",
        minute: 3,
      },
      {
        thread: "thread:both",
        id: "both-user-old",
        role: "user",
        text: "older needle question",
        minute: 1,
      },
      {
        thread: "thread:both",
        id: "both-user-new",
        role: "user",
        text: "newer needle question",
        minute: 2,
      },
      {
        thread: "thread:assistant",
        id: "assistant-only",
        role: "assistant",
        text: "needle in an answer",
      },
      {
        thread: "thread:assistant",
        id: "assistant-streaming",
        role: "user",
        text: "needle still typing",
        streaming: true,
      },
      {
        thread: "thread:assistant",
        id: "assistant-system",
        role: "system",
        text: "needle system prompt",
      },
      { thread: "thread:archived", id: "archived", role: "user", text: "needle archived" },
      { thread: "thread:deleted", id: "deleted", role: "user", text: "needle deleted" },
      { thread: "thread:orphaned", id: "orphaned", role: "user", text: "needle orphaned" },
    ],
    searches: [
      { query: "NEEDLE", limit: 20 },
      { query: "needle", limit: 1 },
      { query: "ne%le" },
      { query: "needle" },
      { query: "  needle  " },
      { query: "nope" },
      { query: "ne" },
    ],
  },
  {
    name: "ordering: match kind, then thread recency, then thread id; one match per thread",
    projects: [{ id: "p" }],
    threads: [
      { id: "t:a", project: "p", updated: 5 },
      { id: "t:b", project: "p", updated: 9 },
      { id: "t:c", project: "p", updated: 5 },
      { id: "t:d", project: "p", updated: 20 },
      { id: "t:e", project: "p", updated: 1 },
      { id: "t:f", project: "p", updated: 9 },
    ],
    messages: [
      { thread: "t:a", id: "a1", role: "assistant", text: "alpha answer", minute: 4 },
      { thread: "t:b", id: "b1", role: "user", text: "alpha question b", minute: 2 },
      { thread: "t:c", id: "c1", role: "user", text: "alpha question c", minute: 2 },
      { thread: "t:d", id: "d1", role: "assistant", text: "alpha answer d", minute: 6 },
      { thread: "t:e", id: "e1", role: "user", text: "alpha question e", minute: 3 },
      { thread: "t:f", id: "f1", role: "assistant", text: "alpha answer f", minute: 7 },
      { thread: "t:f", id: "f2", role: "user", text: "alpha question f newest", minute: 8 },
      { thread: "t:f", id: "f3", role: "user", text: "alpha question f oldest", minute: 1 },
      { thread: "t:a", id: "a2", role: "user", text: "alpha tie y", minute: 10 },
      { thread: "t:a", id: "a3", role: "user", text: "alpha tie x", minute: 10 },
    ],
    searches: [
      { query: "alpha" },
      { query: "alpha", limit: 3 },
      { query: "alpha", limit: 1 },
      { query: "alpha tie" },
      { query: "ANSWER" },
      { query: "question f" },
    ],
  },
  {
    name: "LIKE semantics",
    projects: [{ id: "p" }],
    threads: many(8, (i) => ({ id: `t:${i}`, project: "p", updated: i })),
    messages: [
      { thread: "t:0", id: "m0", role: "user", text: "Plain ASCII Text" },
      { thread: "t:1", id: "m1", role: "user", text: "50% of the _ way! done" },
      { thread: "t:2", id: "m2", role: "user", text: "Éclair and éclair" },
      { thread: "t:3", id: "m3", role: "user", text: "tab\tsep and new\nline" },
      { thread: "t:4", id: "m4", role: "user", text: "日本語のテキスト" },
      { thread: "t:5", id: "m5", role: "user", text: "wild%card and under_score" },
      { thread: "t:6", id: "m6", role: "assistant", text: "double  space" },
      { thread: "t:7", id: "m7", role: "user", text: "İstanbul ǅ" },
    ],
    searches: [
      { query: "ascii text" },
      { query: "PLAIN" },
      { query: "50%" },
      { query: "% of" },
      { query: "_ way" },
      { query: "way!" },
      { query: "!" },
      { query: "éclair" },
      { query: "ÉCLAIR" },
      { query: "Éclair" },
      { query: "tab\tsep" },
      { query: "new\nline" },
      { query: "日本語" },
      { query: "テキ" },
      { query: "wild%" },
      { query: "d%c" },
      { query: "under_" },
      { query: "under\\_" },
      { query: "double  space" },
      { query: "double space" },
      { query: "çlair" },
      { query: "stanbul" },
      { query: "istanbul" },
      { query: "%%" },
      { query: "__" },
      { query: "!!" },
      { query: "ab" },
      { query: "te" },
    ],
  },
  {
    name: "snippets from real messages",
    projects: [{ id: "p" }],
    threads: [
      { id: "t:1", project: "p" },
      { id: "t:2", project: "p", updated: 1 },
      { id: "t:3", project: "p", updated: 2 },
    ],
    messages: [
      {
        thread: "t:1",
        id: "long-middle",
        role: "user",
        text: `${filler(300)}needle${filler(300)}`,
      },
      { thread: "t:2", id: "long-end", role: "assistant", text: `${filler(500)} needle` },
      {
        thread: "t:3",
        id: "ws",
        role: "user",
        text: `  spaced \n\n  out   needle \t text ${filler(260)}`,
      },
    ],
    searches: [{ query: "needle" }],
  },
  {
    name: "the default limit is 50",
    projects: [{ id: "p" }],
    threads: many(55, (i) => ({
      id: `t:${String(i).padStart(2, "0")}`,
      project: "p",
      updated: i % 7,
    })),
    messages: many(55, (i) => ({
      thread: `t:${String(i).padStart(2, "0")}`,
      id: `m${i}`,
      role: i % 3 === 0 ? "assistant" : "user",
      text: `bulk needle ${i}`,
      minute: 1 + (i % 5),
    })),
    searches: [
      { query: "bulk needle" },
      { query: "bulk needle", limit: 50 },
      { query: "bulk needle", limit: 7 },
    ],
  },
  {
    name: "no messages, no threads",
    projects: [{ id: "p" }],
    threads: [{ id: "t:empty", project: "p" }],
    messages: [],
    searches: [{ query: "anything" }],
  },
  {
    name: "an unreadable match is a decode failure",
    projects: [{ id: "p" }],
    threads: [{ id: "t:corrupt", project: "p" }],
    messages: [{ thread: "t:corrupt", id: "corrupt", role: "user", text: "20260927" }],
    corruptText: "corrupt",
    searches: [{ query: "0260927" }, { query: "nomatch" }],
  },
];
const layer = Layer.mergeAll(ThreadSearch.layer, ProjectionStore.layer, ProjectStore.layer).pipe(
  Layer.provideMerge(SqlitePersistence.layerMemory),
);
for (const scenario of scenarios) {
  const results = await Effect.runPromise(
    Effect.gen(function* () {
      const projections = yield* ProjectionStore.ProjectionStoreV2;
      const search = yield* ThreadSearch.ThreadSearch;
      const sql = yield* SqlClient.SqlClient;
      for (const project of scenario.projects) yield* createProject(project.id);
      for (const project of scenario.projects)
        if (project.deleted) yield* deleteProject(project.id);
      yield* Effect.forEach(
        [...scenario.threads.map(threadEvent), ...scenario.messages.map(messageEvent)],
        projections.apply,
        { discard: true },
      );
      if (scenario.corruptText)
        yield* sql`UPDATE orchestration_v2_projection_messages SET payload_json = json_set(payload_json, '$.text', 20260927) WHERE message_id = ${scenario.corruptText}`;
      // The original bumps a thread's `updated_at` to each event it applies, so the value ranking
      // uses is whatever the last applied event left; record it rather than assume.
      const stored =
        yield* sql`SELECT thread_id AS "threadId", updated_at AS "updatedAt" FROM orchestration_v2_projection_threads`;
      const out = [];
      out.stored = Object.fromEntries(stored.map((row) => [row.threadId, row.updatedAt]));
      for (const input of scenario.searches) {
        const exit = yield* Effect.exit(search.search(input));
        out.push(
          exit._tag === "Success"
            ? { input, matches: json(exit.value.matches) }
            : { input, error: exit.cause.reasons[0].error.operation },
        );
      }
      return out;
    }).pipe(Effect.provide(layer)),
  );
  rows.push({
    type: "search",
    scenario: {
      name: scenario.name,
      projects: scenario.projects,
      threads: scenario.threads.map((t) => ({
        ...t,
        updatedAt: results.stored[t.id],
        createdAt: iso(0),
        archivedAt: t.archived ? iso(1) : null,
        deletedAt: t.deleted ? iso(1) : null,
      })),
      messages: scenario.messages.map((m) => ({
        ...m,
        createdAt: iso(m.minute ?? 1),
        streaming: m.streaming ?? false,
      })),
      corruptText: scenario.corruptText,
    },
    results,
  });
}

const target = new URL("../crates/server/tests/fixtures/thread-search.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ rows: rows.length }));
