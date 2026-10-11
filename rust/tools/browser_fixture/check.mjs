#!/usr/bin/env node
// Checks a running fixture without a browser, over the server's own protocol, one line per check:
//
//   node check.mjs <url> <credential>
//
// The credential is a fresh pairing credential (pair.sh prints one). Exits 0 when every check passed.
import { exchangeCredential, Rpc } from "./lib/rpc.mjs";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [origin, credential] = process.argv.slice(2);
if (!origin || !credential) {
  console.error("usage: check.mjs <url> <credential>");
  process.exit(2);
}

const results = [];
function line(ok, name, detail = "") {
  results.push(ok);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  " + detail : ""}`);
}
async function check(name, run) {
  try {
    const detail = await run();
    line(true, name, typeof detail === "string" ? detail : "");
  } catch (error) {
    line(false, name, String(error.message ?? error));
  }
}
const assert = (condition, message) => {
  if (!condition) throw new Error(message);
};
const need = async (rpc, tag, payload) => {
  const result = await rpc.exit(tag, payload);
  if (!result.ok) throw new Error(result.error);
  return result.value;
};

let rpc;
await check("pair and open the WebSocket", async () => {
  rpc = await Rpc.connect(origin, await exchangeCredential(origin, credential));
});
if (!rpc) process.exit(1);

let config;
await check("server.getConfig: providers ready", async () => {
  config = await need(rpc, "server.getConfig", {});
  const ready = config.providers
    .filter((provider) => provider.enabled && provider.status === "ready")
    .map((provider) => provider.instanceId);
  for (const wanted of ["codex", "claudeAgent"])
    assert(
      ready.includes(wanted),
      `${wanted} is not ready (${config.providers.map((p) => `${p.instanceId}:${p.status}`).join(", ")})`,
    );
  // With --hook the server offers no editors; without it, whatever this machine has installed.
  return `ready: ${ready.join(", ")}; editors offered: ${(config.availableEditors ?? []).join(", ") || "none"}`;
});

// -- projects and threads -------------------------------------------------------------------------------------------
let shell;
const stream = rpc.subscribe(
  "orchestration.subscribeShell",
  { requestCompletionMarker: true },
  (values) => {
    for (const value of values) if (value.kind === "snapshot") shell = value.snapshot;
  },
);
for (let waited = 0; !shell && waited < 10_000; waited += 100)
  await new Promise((done) => setTimeout(done, 100));
stream.close();
const threads = shell?.threads ?? [];
const projects = shell?.projects ?? [];
const byTitle = (needle) => threads.find((thread) => thread.title.startsWith(needle));
const projectBy = (needle) => projects.find((project) => project.title.endsWith(needle));
await check("projects listed", async () => {
  for (const name of ["dirty", "github", "gitlab", "conversations", "commitcheck", "ship"])
    assert(projectBy(name), `no project ending in -${name}`);
  return `${projects.length} projects, ${threads.length} threads`;
});

const views = new Map();
const view = async (thread) => {
  if (!views.has(thread.id))
    views.set(
      thread.id,
      await need(rpc, "orchestration.getThreadProjection", { threadId: thread.id }),
    );
  return views.get(thread.id);
};
const items = (v, type) => v.turnItems.filter((item) => item.type === type);
const withdrawn = (v) => v.runs.map((run) => run.status);

const thread = (title) => {
  const found = byTitle(title);
  assert(found, `no thread titled "${title}..."`);
  return found;
};
const seeded = [
  [
    "Finished: a tool call and a reply",
    (t, v) =>
      assert(
        withdrawn(v).join() === "completed" &&
          items(v, "command_execution").length > 0 &&
          items(v, "assistant_message").length === 1,
        `runs ${withdrawn(v)}`,
      ),
  ],
  [
    "Plan card, todo list and reasoning",
    (t, v) =>
      assert(
        v.plans.length > 0 && items(v, "todo_list").length > 0 && items(v, "reasoning").length > 0,
        `plans ${v.plans.length}, todo ${items(v, "todo_list").length}`,
      ),
  ],
  [
    "Pending approval: a command waits",
    (t, v) =>
      assert(
        v.runtimeRequests.some((r) => r.status === "pending") &&
          items(v, "approval_request").length > 0,
        "no pending approval",
      ),
  ],
  [
    "Pending question: the agent asks",
    (t, v) =>
      assert(
        v.runtimeRequests.some((r) => r.status === "pending" && r.kind === "user_input"),
        "no pending question",
      ),
  ],
  [
    "Pending file-change approval",
    (t, v) =>
      assert(
        v.runtimeRequests.some((r) => r.status === "pending" && r.kind === "file-change"),
        "no pending file-change request",
      ),
  ],
  [
    "Run held open with two queued messages",
    (t, v) => assert(withdrawn(v).join() === "running,queued,queued", `runs ${withdrawn(v)}`),
  ],
  [
    "Forked thread",
    (t, v) =>
      assert(
        v.thread.lineage?.parentThreadId === thread("Finished: a tool call and a reply").id,
        "not a fork of the finished thread",
      ),
  ],
  ["Pinned thread", (t) => assert(t.pinnedAt, "not pinned")],
  ["Snoozed thread", (t) => assert(t.snoozedUntil, "not snoozed")],
  ["Settled thread", (t) => assert(t.settledAt, "not settled")],
  [
    "Background work: a command left running (Codex)",
    (t) => assert((t.pendingBackgroundTasks ?? []).length > 0, "no background task"),
  ],
  [
    "Background work: a tool that outlives its turn (Codex)",
    (t, v) => assert(items(v, "dynamic_tool").length > 0, "no tool item"),
  ],
  [
    "MCP and dynamic tool calls",
    (t, v) => assert(items(v, "dynamic_tool").length >= 2, "fewer than two tool calls"),
  ],
  [
    "Reconnecting banner, then a reply",
    (t, v) =>
      assert(
        items(v, "error").length > 0 && withdrawn(v).join() === "completed",
        "no retry notice",
      ),
  ],
  [
    "Usage limit reached (not armed)",
    (t) =>
      assert(
        t.lastErrorClass === "usage_limit" && t.usageLimitResetAt,
        "not stopped on a usage limit",
      ),
  ],
  [
    "Usage limit reached, recovery armed",
    async (t, v) =>
      assert(
        t.lastErrorClass === "usage_limit" && v.thread.limitRecovery?.autoResume === true,
        "recovery not armed",
      ),
  ],
  [
    "Long thread",
    (t, v) =>
      // A read of the thread is the newest window of it (as the original's is), so the 36 turns are not all in it: the
      // newest run is the 36th, and every run the window holds is finished.
      assert(
        Math.max(...v.runs.map((r) => r.ordinal)) === 36 &&
          v.runs.length >= 10 &&
          v.runs.every((r) => r.status === "completed"),
        `${v.runs.length} runs, newest ${Math.max(...v.runs.map((r) => r.ordinal))}`,
      ),
  ],
  ["Dirty tree", (t, v) => assert(withdrawn(v).join() === "completed", "")],
  [
    "Linked to a GitHub pull request",
    (t) =>
      assert(
        JSON.stringify(t.pullRequests ?? t.pullRequestLinks ?? []).includes("acme/widgets"),
        "no PR link",
      ),
  ],
  [
    "Linked to a GitLab merge request",
    (t) =>
      assert(
        JSON.stringify(t.pullRequests ?? t.pullRequestLinks ?? []).includes("acme/lab"),
        "no MR link",
      ),
  ],
  [
    "Claude: thinking, a command and a todo list",
    (t, v) =>
      assert(
        items(v, "reasoning").length > 0 && items(v, "todo_list").length > 0,
        "no reasoning/todo",
      ),
  ],
  [
    "Claude: a subagent with a child thread",
    (t, v) =>
      assert(
        v.subagents.length > 0 && v.runtimeRequests.some((r) => r.status === "pending"),
        "no subagent or no pending approval",
      ),
  ],
  [
    "Claude: a background command",
    (t) => assert((t.pendingBackgroundTasks ?? []).length > 0, "no background task"),
  ],
  [
    "Claude: a failed run",
    (t, v) =>
      assert(
        withdrawn(v).join() === "failed" && items(v, "error").length > 0,
        `runs ${withdrawn(v)}`,
      ),
  ],
  [
    "Claude: usage limit reached",
    (t) => assert(t.lastErrorClass === "usage_limit", `class ${t.lastErrorClass}`),
  ],
  [
    "Claude: a question waits",
    (t, v) =>
      assert(
        v.runtimeRequests.some((r) => r.status === "pending" && r.kind === "user_input"),
        "no pending question",
      ),
  ],
];
for (const [title, verify] of seeded) {
  await check(`thread "${title}"`, async () => {
    const t = thread(title);
    await verify(t, await view(t));
  });
}
await check("a subagent has its child thread", async () => {
  const parent = thread("Claude: a subagent with a child thread");
  const child = threads.find((t) => t.lineage?.parentThreadId === parent.id);
  assert(child, "no thread has the subagent thread as its parent");
  return `child "${child.title}"`;
});
if (threads.some((t) => t.title.startsWith("Grok:"))) {
  await check("thread Grok: a finished turn", async () => {
    const v = await view(thread("Grok: a finished turn"));
    assert(withdrawn(v).join() === "completed", `runs ${withdrawn(v)}`);
  });
  // Grok keeps no roster of background tasks (that is Claude and Codex): the adapter holds the run open while the
  // command lives, so the run is still running and the command's item is still running.
  await check("thread Grok: a background command", async () => {
    const v = await view(thread("Grok: a background command"));
    assert(
      withdrawn(v).join() === "running" &&
        items(v, "command_execution").some((item) => item.status === "running"),
      `runs ${withdrawn(v)}, no command left running`,
    );
  });
}

// -- the dirty working tree --------------------------------------------------------------------------------------------
const dirty = projectBy("dirty");
await check("vcs.refreshStatus: the dirty project has changes of every kind", async () => {
  const status = await need(rpc, "vcs.refreshStatus", { cwd: dirty.workspaceRoot });
  const paths = status.workingTree.files.map((file) => file.path);
  assert(status.hasWorkingTreeChanges && status.refName === "main", "clean or off main");
  for (const wanted of [
    "src/app.ts",
    "src/old.ts",
    "src/feature.ts",
    "assets/logo.png",
    "data/table.csv",
    "data/large.log",
    "docs/notes.md",
  ])
    assert(paths.includes(wanted), `${wanted} is not in the status`);
  return `${paths.length} files, +${status.workingTree.insertions} -${status.workingTree.deletions}`;
});
await check("review.getDiffPreview: the working tree diff", async () => {
  const preview = await need(rpc, "review.getDiffPreview", { cwd: dirty.workspaceRoot });
  const sources = preview.sources ?? [];
  assert(sources.length > 0, "no diff sources");
  const tree =
    sources.find((source) => source.kind === "working_tree" || source.kind === "workingTree") ??
    sources[0];
  return `${sources.length} sources (${sources.map((s) => s.kind).join(", ")})`;
});

// -- pull requests ---------------------------------------------------------------------------------------------------------
const gitlab = projectBy("gitlab");
const github = projectBy("github");
const reference = (project, repository, number) => ({
  projectId: project.id,
  repository,
  number,
  allowStale: false,
});
await check("GitLab: pullRequests.list", async () => {
  const page = await need(rpc, "pullRequests.list", { state: "all" });
  const mine = page.entries.filter((entry) => entry.provider === "gitlab");
  assert(mine.length >= 4, `${mine.length} merge requests listed`);
  const states = new Set(mine.map((entry) => (entry.isDraft ? "draft" : entry.state)));
  for (const wanted of ["open", "draft", "merged", "closed"])
    assert(states.has(wanted), `no ${wanted} merge request (${[...states]})`);
  return `${mine.length} entries: ${[...states].join(", ")}`;
});
await check("GitLab: pullRequests.detail", async () => {
  const detail = await need(rpc, "pullRequests.detail", reference(gitlab, "acme/lab", 7));
  assert(
    detail.reviewers.length === 2 && detail.labels.length === 2,
    "reviewers or labels missing",
  );
  return `"${detail.title}", ${detail.reviewers.length} reviewers`;
});
await check("GitLab: pullRequests.checks", async () => {
  const checks = await need(rpc, "pullRequests.checks", reference(gitlab, "acme/lab", 7));
  assert(checks.checks.length >= 1, "no checks");
  return checks.checks.map((c) => `${c.name}:${c.status}`).join(", ");
});
await check("GitLab: pullRequests.activity", async () => {
  const activity = await need(rpc, "pullRequests.activity", reference(gitlab, "acme/lab", 7));
  assert(
    activity.comments.length >= 3 &&
      activity.reviewThreads.length >= 2 &&
      activity.commits.length === 4,
    `${activity.comments.length} comments, ${activity.reviewThreads.length} threads, ${activity.commits.length} commits`,
  );
  return `${activity.comments.length} comments, ${activity.reviewThreads.length} threads, ${activity.commits.length} commits`;
});
const hostHint = 'Start the fixture with --hook (README, "Server hooks")';
await check("GitHub: pullRequests.list", async () => {
  const page = await need(rpc, "pullRequests.list", { state: "all", projectId: github.id });
  const mine = page.entries.filter((entry) => entry.provider === "github");
  if (mine.length === 0)
    throw new Error(
      `no entries; provider note: ${JSON.stringify(page.providers.find((p) => p.kind === "github")?.detail)}. ${hostHint}`,
    );
  return `${mine.length} entries`;
});
await check("GitHub: pullRequests.detail", async () => {
  try {
    const detail = await need(rpc, "pullRequests.detail", reference(github, "acme/widgets", 42));
    assert(detail.checks.length >= 5, `${detail.checks.length} checks`);
    return `"${detail.title}", ${detail.checks.length} checks`;
  } catch (error) {
    throw new Error(`${error.message}. ${hostHint}`);
  }
});
await check("GitHub: pullRequests.checks", async () => {
  try {
    const checks = await need(rpc, "pullRequests.checks", reference(github, "acme/widgets", 42));
    return checks.checks.map((c) => `${c.name}:${c.status}`).join(", ");
  } catch (error) {
    throw new Error(`${error.message}. ${hostHint}`);
  }
});
await check("GitHub: pullRequests.activity", async () => {
  try {
    const activity = await need(
      rpc,
      "pullRequests.activity",
      reference(github, "acme/widgets", 42),
    );
    return `${activity.comments.length} comments, ${activity.reviewThreads.length} threads`;
  } catch (error) {
    throw new Error(`${error.message}. ${hostHint}`);
  }
});

// -- text generation -------------------------------------------------------------------------------------------------------
await check("git.runStackedAction: a commit message is written and committed", async () => {
  const project = projectBy("commitcheck");
  const file = join(project.workspaceRoot, `change-${Date.now()}.txt`);
  writeFileSync(file, "a change made by check.mjs\n");
  const result = await rpc.stream("git.runStackedAction", {
    actionId: `check-${Date.now()}`,
    cwd: project.workspaceRoot,
    action: "commit",
  });
  if (!result.ok) throw new Error(result.error);
  const finished =
    result.events.find((event) => event.kind === "action_finished") ?? result.events.at(-1);
  const subject = result.value?.commit?.subject ?? finished?.result?.commit?.subject;
  assert(subject, `no commit subject in ${JSON.stringify(result.value ?? finished).slice(0, 300)}`);
  return `subject: "${subject}"`;
});

rpc.close();
const failed = results.filter((ok) => !ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
