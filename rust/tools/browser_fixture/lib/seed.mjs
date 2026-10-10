// The conversations. Each is made with the commands the UI sends (`projects.mutate`, `orchestration.launchThread`,
// `orchestration.dispatchCommand`) and each title says what the thread demonstrates. The provider behind a thread is
// a fake whose behaviour is chosen by a tag in the prompt: see fakes/fake_codex.py (the repository's scripted
// app-server), fakes/fake_claude.py and the Grok script in lib/server.mjs.
import { randomUUID } from "node:crypto";

const TERMINAL = new Set(["completed", "failed", "interrupted", "cancelled", "canceled"]);

export async function seed(rpc, { paths, say, resetAtSeconds, withGrok, hosts }) {
  const created = [];
  const dispatch = async (command) => {
    const exit = await rpc.exit("orchestration.dispatchCommand", {
      commandId: randomUUID(),
      ...command,
    });
    if (!exit.ok) throw new Error(`${command.type}: ${exit.error}`);
    return exit.value;
  };
  const projection = (threadId) => rpc.call("orchestration.getThreadProjection", { threadId });
  const until = async (threadId, what, check, timeoutMs = 90_000) => {
    const deadline = Date.now() + timeoutMs;
    let last;
    while (Date.now() < deadline) {
      last = await projection(threadId);
      const found = check(last);
      if (found) return found === true ? last : found;
      await new Promise((done) => setTimeout(done, 150));
    }
    const runs = (last?.runs ?? []).map((run) => run.status).join(",");
    throw new Error(
      `thread ${threadId} never reached: ${what} (runs: ${runs || "none"}; requests: ${(last?.runtimeRequests ?? []).map((r) => r.status).join(",")})`,
    );
  };
  // A run is named by its ordinal, not by its place in the list: a read of a long thread is the newest window of it (as the
  // original's is), so a thread with many runs lists only the last of them.
  const runOrdinal = (view) => Math.max(0, ...(view.runs ?? []).map((run) => run.ordinal ?? 0));
  const runStatus = (view, index) =>
    (view.runs ?? []).find((run) => run.ordinal === index + 1)?.status;
  const settled = (index) => (view) => TERMINAL.has(runStatus(view, index));
  const pending = (view) =>
    (view.runtimeRequests ?? []).some((request) => request.status === "pending");

  const projectIds = {
    dirty: "fixture-dirty",
    github: "fixture-github",
    gitlab: "fixture-gitlab",
    conversations: "fixture-conversations",
    commitcheck: "fixture-commitcheck",
  };
  for (const [key, id] of Object.entries(projectIds)) {
    const title = paths.projects[key].split("/").pop();
    const exit = await rpc.exit("projects.mutate", {
      type: "project.create",
      commandId: randomUUID(),
      projectId: id,
      title,
      workspaceRoot: paths.projects[key],
      createWorkspaceRootIfMissing: false,
      defaultModelSelection: null,
    });
    if (!exit.ok) throw new Error(`project ${title}: ${exit.error}`);
  }
  say(`  ${Object.keys(projectIds).length} projects registered`);

  const models = { codex: "fixture-model", claudeAgent: "claude-sonnet-4-6", grok: "grok-4.7" };
  async function launch({
    project = "conversations",
    title,
    provider = "codex",
    mode = "full-access",
    text,
    wait = "settled",
  }) {
    const exit = await rpc.exit("orchestration.launchThread", {
      commandId: randomUUID(),
      projectId: projectIds[project],
      title,
      generateTitle: false,
      modelSelection: { instanceId: provider, model: models[provider] },
      runtimeMode: mode,
      interactionMode: "default",
      creationSource: "web",
      workspaceStrategy: { type: "root" },
      initialMessage: { messageId: randomUUID(), text, attachments: [] },
    });
    if (!exit.ok) throw new Error(`launch "${title}": ${exit.error}`);
    const id = exit.value.threadId;
    created.push({ id, title, provider, project });
    if (wait === "settled") await until(id, "the first run to end", settled(0));
    else if (wait === "pending") await until(id, "a request waiting for an answer", pending);
    else if (wait === "running")
      await until(id, "the first run to be running", (view) => runStatus(view, 0) === "running");
    say(`  ${title}`);
    return id;
  }
  async function send(threadId, text, dispatchMode = { type: "start_immediately" }) {
    const before = runOrdinal(await projection(threadId));
    const messageId = randomUUID();
    await dispatch({
      type: "message.dispatch",
      threadId,
      messageId,
      text,
      attachments: [],
      createdBy: "user",
      creationSource: "web",
      dispatchMode,
    });
    await until(threadId, "the new run to exist", (view) => runOrdinal(view) > before);
    return before;
  }

  // -- conversations ------------------------------------------------------------------------------------
  const finished = await launch({
    title: "Finished: a tool call and a reply",
    text: "Show me the project files.",
  });
  await launch({
    title: "Plan card, todo list and reasoning",
    text: "Plan the migration of the config loader. [think] [todo] [plan]",
  });
  await launch({
    title: "Pending approval: a command waits",
    text: "Run the cleanup script. [approval]",
    mode: "approval-required",
    wait: "pending",
  });
  await launch({
    title: "Pending question: the agent asks",
    text: "Which fixture should I use? [question]",
    mode: "approval-required",
    wait: "pending",
  });
  await launch({
    title: "Pending file-change approval",
    text: "Write the generated file. [file]",
    mode: "approval-required",
    wait: "pending",
  });

  const held = await launch({
    title: "Run held open with two queued messages",
    text: "Work on the long task. [hold]",
    wait: "running",
  });
  await send(held, "Queued follow-up one: also update the docs.", { type: "queue_after_active" });
  await send(held, "Queued follow-up two: then run the tests.", { type: "queue_after_active" });
  say("    two messages queued behind the held run");

  const fork = randomUUID();
  await dispatch({
    type: "thread.fork",
    sourceThreadId: finished,
    targetThreadId: fork,
    sourcePoint: { type: "latest_stable" },
    title: "Forked thread: lineage back to 'Finished'",
    createdBy: "user",
    creationSource: "web",
  });
  created.push({
    id: fork,
    title: "Forked thread: lineage back to 'Finished'",
    provider: "codex",
    project: "conversations",
  });
  const forkRun = await send(fork, "Continue from the fork: now list only the TypeScript files.");
  await until(fork, "the fork's run to end", settled(forkRun));
  say("  Forked thread: lineage back to 'Finished'");

  const pinned = await launch({ title: "Pinned thread", text: "Remember this one." });
  await dispatch({ type: "thread.pin", threadId: pinned });
  const snoozed = await launch({
    title: "Snoozed thread (wakes tomorrow)",
    text: "Come back to this later.",
  });
  await dispatch({
    type: "thread.snooze",
    threadId: snoozed,
    snoozedUntil: new Date(Date.now() + 24 * 3600 * 1000).toISOString(),
  });
  const settledThread = await launch({ title: "Settled thread", text: "This one is done." });
  await dispatch({ type: "thread.settle", threadId: settledThread });

  await launch({
    title: "Background work: a command left running (Codex)",
    text: "Start the dev server. [background]",
  });
  await launch({
    title: "Background work: a tool that outlives its turn (Codex)",
    text: "Watch the logs. [persistent-tool]",
  });
  await launch({ title: "MCP and dynamic tool calls", text: "Create the issue. [mcp-tool]" });
  await launch({
    title: "Reconnecting banner, then a reply",
    text: "Summarise the changelog. [retry]",
  });

  // The usage-limit stop: the fake Codex reports a limit that resets in three hours.
  await launch({ title: "Usage limit reached (not armed)", text: "Keep going. [usage-limit]" });
  const limited = await launch({
    title: "Usage limit reached, recovery armed",
    text: "Keep going. [usage-limit]",
  });
  // The run is failed a moment before its failure is in the projection; arming a recovery needs both.
  const limitedView = await until(limited, "the usage-limit failure to be recorded", (view) =>
    (view.turnItems ?? []).some(
      (item) =>
        item.type === "error" && item.status === "failed" && item.failure?.class === "usage_limit",
    ),
  );
  const limitRun = limitedView.runs.at(-1);
  await dispatch({
    type: "thread.metadata.update",
    threadId: limited,
    limitRecovery: {
      runId: limitRun.id,
      resetAt: new Date(resetAtSeconds * 1000).toISOString().replace(/\.\d+Z$/, ".000Z"),
      autoResume: true,
      snooze: true,
    },
  });

  // -- a long conversation -------------------------------------------------------------------------------
  const longThread = await launch({
    title: "Long thread: 36 turns, for scrolling",
    text: "Turn 1: describe the project layout.",
  });
  const topics = [
    "the build pipeline",
    "error handling",
    "the config loader",
    "logging",
    "the retry helper",
    "tests",
    "the release process",
    "the docs",
  ];
  for (let turn = 2; turn <= 36; turn += 1) {
    const topic = topics[turn % topics.length];
    const extra = turn % 6 === 0 ? ` [think]` : turn % 9 === 0 ? ` [todo]` : "";
    const body =
      turn % 4 === 0
        ? `Turn ${turn}: here is a snippet about ${topic}.\n\n\`\`\`ts\nexport function step${turn}(input: string) {\n  return input.trim().toLowerCase() + "-${turn}";\n}\n\`\`\`\n\nPlease review it.`
        : `Turn ${turn}: tell me more about ${topic}.${turn % 5 === 0 ? " This one is a longer question so that the rows differ in height: consider the failure modes, the retry budget, the logging that would be needed to diagnose them in production, and how the answer changes for a single-user install." : ""}`;
    const index = await send(longThread, body + extra);
    await until(longThread, `run ${index + 1} to end`, settled(index));
  }
  say("    36 turns done");

  // -- a thread in the project with uncommitted changes ----------------------------------------------------
  await launch({
    project: "dirty",
    title: "Dirty tree: open the commit dialog and diff panel from here",
    text: "Summarise what is uncommitted.",
  });

  // -- threads linked to the fake hosts' pull requests -----------------------------------------------------------
  const prThread = await launch({
    project: "github",
    title: "Linked to a GitHub pull request (#42)",
    text: "Review the retry change.",
  });
  await dispatch({
    type: "thread.pull-request.link",
    threadId: prThread,
    host: hosts.github,
    repository: "acme/widgets",
    number: 42,
    url: `https://${hosts.github}/acme/widgets/pull/42`,
    source: "manual",
  });
  const mrThread = await launch({
    project: "gitlab",
    title: "Linked to a GitLab merge request (!7)",
    text: "Review the retry change.",
  });
  await dispatch({
    type: "thread.pull-request.link",
    threadId: mrThread,
    host: hosts.gitlab,
    repository: "acme/lab",
    number: 7,
    url: `https://${hosts.gitlab}/acme/lab/-/merge_requests/7`,
    source: "manual",
  });

  // -- Claude ------------------------------------------------------------------------------------------------------
  await launch({
    provider: "claudeAgent",
    title: "Claude: thinking, a command and a todo list",
    text: "Check the build.",
  });
  await launch({
    provider: "claudeAgent",
    title: "Claude: a subagent with a child thread and a pending approval",
    text: "Inspect the repo with a subagent. [subagent]",
    mode: "approval-required",
    wait: "pending",
  });
  await launch({
    provider: "claudeAgent",
    title: "Claude: a background command outlives the turn",
    text: "Start the build. [background]",
  });
  await launch({
    provider: "claudeAgent",
    title: "Claude: a failed run (HTTP 429)",
    text: "Summarise the logs. [error]",
  });
  await launch({
    provider: "claudeAgent",
    title: "Claude: usage limit reached",
    text: "Keep going. [limit]",
  });
  await launch({
    provider: "claudeAgent",
    title: "Claude: a question waits for an answer",
    text: "Set up the database. [question]",
    mode: "approval-required",
    wait: "pending",
  });

  // -- Grok (the script plays its prompts in order: a finished turn, then a background command) -------------------------
  if (withGrok) {
    await launch({
      provider: "grok",
      title: "Grok: a finished turn with a tool call",
      text: "List the files.",
    });
    // Grok holds the run open while its background command lives, so it is running, not settled.
    const building = await launch({
      provider: "grok",
      title: "Grok: a background command",
      text: "Start the build.",
      wait: "running",
    });
    await until(building, "the background command to be running", (view) =>
      (view.turnItems ?? []).some(
        (item) => item.type === "command_execution" && item.status === "running",
      ),
    );
  }

  say(`  ${created.length} threads created`);
  return { projects: projectIds, threads: created, limitRunId: limitRun.id };
}
