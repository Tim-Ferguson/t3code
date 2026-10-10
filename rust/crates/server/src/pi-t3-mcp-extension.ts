import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const URL_ENV = "T3_MCP_URL";
const TOKEN_ENV = "T3_MCP_BEARER_TOKEN";
const RUNTIME_MODE_ENV = "T3_PI_RUNTIME_MODE";
const ORCHESTRATION_INSTRUCTIONS =
  '## T3 Code orchestration\n\nThe `t3-code` MCP server provides app-owned orchestration. Treat these concepts distinctly:\n\n- A delegated task/subagent is child work owned by the current thread. Use `orchestrator_capabilities` to discover the current provider/model IDs from the same live catalog as the composer, including configured custom models. Do not treat a native tool\'s model list as the full list of available subagent models. Prefer native subagent tools for same-provider work only when they support the chosen model. Use `delegate_task` with that provider instance and model when native tools cannot, including for same-provider work. Also use `delegate_task` for cross-provider or explicitly T3-owned child tasks. Retain each returned `taskId`, and use `task_status` or `task_cancel` to manage it. The returned `childThreadId` is backing storage for the subagent, not the target for starting another delegated review round.\n- `t3_thread_launch` and `create_threads` create ordinary top-level T3 conversations. Use them only when the user explicitly asks for separate/new/top-level threads or conversations. Never use them merely because the user said "subagent" or requested parallel delegated work.\n- For every T3 delegated review round, call `delegate_task` again. Include the original brief, prior findings, responses, and unresolved objections in each new task prompt. Track each round by its own `taskId`. Use a distinct `clientRequestId` per round, stable across retries of that round. Do not use `t3_thread_send` on `childThreadId` to continue a delegated review.\n- `schedule_task` creates persistent recurring work in the app scheduler. Pass `schedule` as a structured object, never as JSON text: `{"type":"interval","everyMs":3600000}` for an interval, or `{"type":"fixed_time","timeOfDay":"09:00","weekdays":[1,2,3,4,5]}` for a wall-clock schedule, or `{"type":"webhook"}` to run on each request to the returned `webhookUrl` (the run sees the request only through `{{body.path}}`-style placeholders in the prompt). By default runs return to the current thread, which suits orchestrating: each trigger arrives here and you delegate or dedupe; set `bindToCurrentThread=false` only when the user wants a fresh thread for every run. After scheduling a timer, report the returned cadence and next run time; for a webhook, report its `webhookUrl`, or say T3 Connect remote access is needed if it is missing.\n- When you need a secret from the user (a token, API key, or webhook signing secret), call `request_secret` so they enter it privately, then pass the returned `secretRef` to the tool that needs it, e.g. `signature.secretRef` on a webhook task for a sender that signs requests such as GitHub. A `secretRef` works once. Never ask for a secret in chat, never invent one, and never repeat one.\n\n### Choose the workspace before starting a new thread\n\nFor independent implementation or a PR stack in its own worktree, use `t3_thread_launch` with an explicit `workspaceStrategy`. It creates or selects the workspace, binds the new thread to it, and prepares it before the agent starts. Put the task in `message`, not `prompt`:\n\n- New worktree: `{"title":"UI cleanup","workspaceStrategy":{"type":"worktree","baseRef":"feature/base","branch":"feature/ui-cleanup","startFromOrigin":false},"message":"Implement the cleanup and open a PR against feature/base."}`\n- Existing worktree: `{"title":"Continue cleanup","workspaceStrategy":{"type":"existing_worktree","worktreePath":"/absolute/path/to/worktree","branch":"feature/ui-cleanup"},"message":"Continue the cleanup."}`\n- Project\'s main checkout: `workspaceStrategy:{"type":"root"}`. Omitting workspaceStrategy also selects root; it does not inherit the caller\'s worktree.\n\nFor stacked work, set `baseRef` to the intended parent branch and `startFromOrigin:false` to use its local commits. Use `startFromOrigin:true` when you intend to fetch and start from origin. Uncommitted edits are not copied. Use `t3_worktree_list` to discover existing checkout paths. Project, model selection, and modes inherit unless supplied; a launched thread may not run with broader modes than yours.\n\n`t3_thread_launch` is the single-thread launch tool. Use `create_threads` only for a batch of threads intentionally sharing the caller\'s checkout: it always inherits the caller\'s project, branch, and worktree and has no workspace override. Asking an agent to run `git worktree add` or `cd` in its prompt does not update T3\'s thread binding. Select the workspace in the launch call instead. `t3_worktree_handoff` moves the calling thread, not another thread, and cannot move a thread already attached to a worktree.\n\n`t3_thread_launch` has no idempotency key. Retain its returned threadId and inspect it with `t3_thread_read` / `t3_thread_wait`; preparation can still be running after acceptance. If a launch fails or its response is lost, inspect `t3_thread_list` before retrying, since a thread may already exist.\n\nTool names may include a harness-normalized MCP prefix, such as `mcp__t3_code__delegate_task`; the semantics are the same. Some harnesses attach optional MCP servers lazily: if an initial tool-catalog scan does not show T3 tools, do not conclude that cross-provider delegation is unavailable. Make one bounded direct attempt using the known T3 tool name on the next tool step. In Codex code mode, for example, call `tools.mcp__t3_code__orchestrator_capabilities({})` before reporting that the capability is absent. Keep polling/wait loops bounded, do not duplicate active work, and use stable `clientRequestId` values when retrying tools that accept them.\n\nACP fallback: some ACP agents accept the injected MCP server but fail to expose its tools. When the T3 tools are absent and `T3_ACP_MCP_NODE` is present, call the same tools through the terminal: `ELECTRON_RUN_AS_NODE=1 "$T3_ACP_MCP_NODE" ${T3_ACP_MCP_ENTRYPOINT:+"$T3_ACP_MCP_ENTRYPOINT"} acp-mcp-call orchestrator_capabilities \'{}\'` (`T3_ACP_MCP_ENTRYPOINT` is unset when T3 runs as a standalone executable). Delegate with `acp-mcp-call delegate_task \'{"task":"...","target":{"providerInstanceId":"...","model":"..."},"mode":"async","clientRequestId":"..."}\'`. This is the supported T3 transport fallback, not an ordinary shell-based substitute for delegation.\n\n### Showing visuals\n\nWhen a chart, table, diagram, image collage, or mockup would say more than prose, build a self-contained HTML page, check it with `html_preview`, then publish it with `html_render` before your final reply. The reader sees the page above that reply, so don\'t announce or restate it; add only what it doesn\'t say.';
const PROTOCOL = "2025-06-18";
const READ_ONLY_TOOLS = new Set(["read", "grep", "find", "ls"]);
const FILE_CHANGE_TOOLS = new Set(["edit", "write"]);

type RuntimeMode = "approval-required" | "auto-accept-edits" | "auto" | "full-access";

type JsonRpcResponse = {
  readonly id?: number | string;
  readonly result?: unknown;
  readonly error?: { readonly message?: string };
};

type McpTool = {
  readonly name: string;
  readonly description?: string;
  readonly inputSchema?: Record<string, unknown>;
};

function env(name: string): string | undefined {
  const value = process.env[name];
  return value && value.length > 0 ? value : undefined;
}

function runtimeMode(): RuntimeMode {
  const value = env(RUNTIME_MODE_ENV);
  return value === "approval-required" ||
    value === "auto-accept-edits" ||
    value === "auto" ||
    value === "full-access"
    ? value
    : "full-access";
}

function toolInputSummary(input: unknown): string {
  try {
    return JSON.stringify(input, null, 2).slice(0, 4_000);
  } catch {
    return String(input).slice(0, 4_000);
  }
}

function parseSseOrJson(body: string, contentType: string): JsonRpcResponse {
  if (contentType.includes("text/event-stream")) {
    for (const line of body.split("\n")) {
      const trimmed = line.startsWith("data:") ? line.slice(5).trim() : "";
      if (trimmed.length === 0) continue;
      const parsed = JSON.parse(trimmed) as JsonRpcResponse;
      if (parsed.id !== undefined || parsed.result !== undefined || parsed.error !== undefined) {
        return parsed;
      }
    }
    throw new Error("MCP SSE response had no JSON-RPC payload.");
  }
  return JSON.parse(body) as JsonRpcResponse;
}

function jsonSchemaToTypebox(schema: Record<string, unknown> | undefined) {
  const unsafe = (Type as { Unsafe?: (value: unknown) => unknown }).Unsafe;
  if (typeof unsafe === "function" && schema !== undefined) {
    return unsafe(schema);
  }
  return Type.Object({}, { additionalProperties: true });
}

function formatMcpContent(result: unknown): string {
  if (result === null || result === undefined) return "";
  if (typeof result !== "object") return String(result);
  const record = result as {
    readonly content?: ReadonlyArray<{ readonly type?: string; readonly text?: string }>;
    readonly structuredContent?: unknown;
    readonly isError?: boolean;
  };
  const texts: string[] = [];
  if (Array.isArray(record.content)) {
    for (const part of record.content) {
      if (part?.type === "text" && typeof part.text === "string") texts.push(part.text);
    }
  }
  // Most T3 tools mirror structuredContent in a text block. Repeating it would
  // leave T3's own output parsing two JSON documents instead of one.
  if (record.structuredContent !== undefined) {
    const structured = JSON.stringify(record.structuredContent);
    if (!texts.includes(structured)) texts.push(structured);
  }
  if (texts.length > 0) return texts.join("\n");
  return JSON.stringify(result);
}

function isMcpToolError(result: unknown): boolean {
  return (
    typeof result === "object" && result !== null && "isError" in result && result.isError === true
  );
}

function createMcpClient(endpoint: string, token: string) {
  let nextId = 1;
  let sessionId: string | undefined;

  const headers = (): Record<string, string> => {
    const next: Record<string, string> = {
      accept: "application/json, text/event-stream",
      authorization: token.startsWith("Bearer ") ? token : `Bearer ${token}`,
      "content-type": "application/json",
      // Effect's HTTP MCP rejects post-initialize requests without this
      // (400). The worktree client in McpHttpServer tests sends the same
      // header; initialize itself does not require it.
      "mcp-protocol-version": PROTOCOL,
    };
    if (sessionId !== undefined) next["mcp-session-id"] = sessionId;
    return next;
  };

  const request = async (method: string, params?: unknown, signal?: AbortSignal) => {
    const id = nextId++;
    const response = await fetch(endpoint, {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
      signal,
    });
    const nextSession = response.headers.get("mcp-session-id");
    if (nextSession) sessionId = nextSession;
    const body = await response.text();
    if (!response.ok) {
      throw new Error(`MCP ${method} failed (${response.status}): ${body.slice(0, 400)}`);
    }
    if (body.length === 0) return undefined;
    const parsed = parseSseOrJson(body, response.headers.get("content-type") ?? "");
    if (parsed.error) {
      throw new Error(parsed.error.message ?? `MCP ${method} returned an error`);
    }
    return parsed.result;
  };

  const notify = async (method: string, params?: unknown, signal?: AbortSignal) => {
    await fetch(endpoint, {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ jsonrpc: "2.0", method, params }),
      signal,
    });
  };

  return {
    async connect(signal?: AbortSignal) {
      await request(
        "initialize",
        {
          protocolVersion: PROTOCOL,
          capabilities: {},
          clientInfo: { name: "t3-pi-mcp", version: "1.0.0" },
        },
        signal,
      );
      await notify("notifications/initialized", {}, signal).catch(() => undefined);
    },
    async listTools(signal?: AbortSignal) {
      const tools: McpTool[] = [];
      let cursor: string | undefined;
      do {
        const result = (await request(
          "tools/list",
          cursor === undefined ? {} : { cursor },
          signal,
        )) as { tools?: McpTool[]; nextCursor?: string } | undefined;
        tools.push(...(result?.tools ?? []));
        cursor = result?.nextCursor;
      } while (cursor);
      return tools;
    },
    async callTool(name: string, args: Record<string, unknown>, signal?: AbortSignal) {
      return request("tools/call", { name, arguments: args }, signal);
    },
  };
}

export default async function t3McpExtension(pi: ExtensionAPI) {
  // Workaround for an upstream Pi context-budgeting bug: pi-ai reuses the
  // previous response's usage even when a fork's instructions/tools differ,
  // then reserves almost all remaining context for output. OpenRouter can
  // reject even a short conversation. Remove this cap when Pi accounts for
  // the current request prefix reliably (api/simple-options + utils/estimate).
  pi.on("before_provider_request", (event, ctx) => {
    if (ctx.model?.provider !== "openrouter") return;
    const payload = event.payload;
    if (typeof payload !== "object" || payload === null || Array.isArray(payload)) return;
    const replacement = { ...payload } as Record<string, unknown>;
    let changed = false;
    for (const key of ["max_tokens", "max_completion_tokens"]) {
      const limit = replacement[key];
      if (typeof limit === "number" && Number.isFinite(limit) && limit > 32_768) {
        replacement[key] = 32_768;
        changed = true;
      }
    }
    if (changed) return replacement;
  });

  // Pi deliberately leaves permission policy to extensions. T3's injected
  // bridge uses Pi's public blocking tool hook so the shared runtime modes
  // keep their normal meaning without replacing or shadowing Pi's runtime.
  pi.on("tool_call", async (event, ctx) => {
    const mode = runtimeMode();
    if (mode === "full-access" || READ_ONLY_TOOLS.has(event.toolName)) return;
    if (mode === "auto-accept-edits" && FILE_CHANGE_TOOLS.has(event.toolName)) {
      return;
    }
    const approved = await ctx.ui.confirm(
      `Allow ${event.toolName}?`,
      toolInputSummary(event.input),
    );
    if (!approved) {
      return { block: true, reason: `${event.toolName} was declined in T3 Code.` };
    }
  });

  const endpoint = env(URL_ENV);
  const token = env(TOKEN_ENV);
  if (endpoint === undefined || token === undefined) {
    pi.on("session_start", async (_event, ctx) => {
      ctx.ui.notify(
        "t3-code MCP unavailable: T3_MCP_URL or T3_MCP_BEARER_TOKEN is missing.",
        "warning",
      );
    });
    return;
  }

  const client = createMcpClient(endpoint, token);
  let started: Promise<void> | undefined;

  const ensureStarted = () => {
    if (started !== undefined) return started;
    const attempt = (async () => {
      const signal = AbortSignal.timeout(10_000);
      await client.connect(signal);
      const tools = await client.listTools(signal);
      for (const tool of tools) {
        const name = tool.name;
        const registeredName = `mcp__t3-code__${name}`;
        const description = tool.description ?? name;
        pi.registerTool({
          name: registeredName,
          label: name,
          description,
          promptSnippet: description.split("\n")[0] ?? name,
          promptGuidelines: [
            `Use ${registeredName} from the t3-code MCP server when the user asks for T3 orchestration that this tool covers.`,
          ],
          parameters: jsonSchemaToTypebox(tool.inputSchema),
          async execute(_toolCallId, params, signal) {
            const result = await client.callTool(
              name,
              (params ?? {}) as Record<string, unknown>,
              signal,
            );
            const text = formatMcpContent(result);
            return {
              content: [{ type: "text", text }],
              details: { server: "t3-code", tool: name },
              ...(isMcpToolError(result) ? { isError: true } : {}),
            };
          },
        });
      }
    })();
    started = attempt;
    void attempt.catch(() => {
      if (started === attempt) started = undefined;
    });
    return attempt;
  };

  // Await here so tools exist before session_start and the first prompt.
  // session_start is a retry if the process later reloads the extension.
  // Best effort during extension load. A failed first connection is retried
  // below on session_start instead of pinning this process to the failure.
  await ensureStarted().catch(() => undefined);

  pi.on("session_start", async (_event, ctx) => {
    try {
      await ensureStarted();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      ctx.ui.notify(`t3-code MCP unavailable: ${message}`, "warning");
    }
  });

  // Deliver orchestration guidance through pi's real system-prompt channel.
  // Wrapping the first user message instead would stop it from starting
  // with "/" and silently break slash-command expansion.
  pi.on("before_agent_start", (event) => ({
    systemPrompt: event.systemPrompt + "\n\n" + ORCHESTRATION_INSTRUCTIONS,
  }));
}
