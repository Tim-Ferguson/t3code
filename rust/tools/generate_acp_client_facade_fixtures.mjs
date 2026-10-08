// Development oracle: Node24 and pinned original source/dependencies.
// node rust/tools/generate_acp_client_facade_fixtures.mjs
import fs from "node:fs";
import * as Effect from "../../packages/effect-acp/node_modules/effect/dist/Effect.js";
import * as Queue from "../../packages/effect-acp/node_modules/effect/dist/Queue.js";
import * as Fiber from "../../packages/effect-acp/node_modules/effect/dist/Fiber.js";
import * as Exit from "../../packages/effect-acp/node_modules/effect/dist/Exit.js";
import * as Cause from "../../packages/effect-acp/node_modules/effect/dist/Cause.js";
import * as Layer from "../../packages/effect-acp/node_modules/effect/dist/Layer.js";
import * as Client from "../../packages/effect-acp/src/client.ts";
import * as Stdio from "../../packages/effect-acp/src/_internal/stdio.ts";
const cases = [];
const v1Config = {
  type: "select",
  id: "model",
  name: "Model",
  currentValue: "m",
  options: [
    {
      group: "g",
      name: "Group",
      options: [{ value: "m", name: "Model", description: null }],
      _meta: null,
    },
  ],
  description: null,
  _meta: null,
};
const v2Config = {
  type: "select",
  configId: "model",
  name: "Model",
  currentValue: "m",
  options: [
    {
      groupId: "g",
      name: "Group",
      options: [{ value: "m", name: "Model", description: null }],
      _meta: null,
    },
  ],
  description: null,
  _meta: null,
};
for (const generation of [1, 2]) {
  await Effect.runPromise(
    Effect.scoped(
      Effect.gen(function* () {
        const io = yield* Stdio.makeInMemoryStdio();
        const services = yield* Layer.build(Layer.effect(Client.AcpClient, Client.make(io.stdio)));
        yield* Effect.gen(function* () {
          const client = yield* Client.AcpClient;
          const invoke = Effect.fn(function* (method, input, response, unsupported = false) {
            const work = yield* client.agent[method](input).pipe(Effect.forkScoped);
            let wire = null;
            if (!unsupported) {
              wire = JSON.parse(yield* Queue.take(io.output));
              if (method !== "cancel") {
                let frames = [{ jsonrpc: "2.0", id: wire.id, result: response }];
                if (method === "prompt" && generation === 2)
                  frames.push({
                    jsonrpc: "2.0",
                    method: "session/update",
                    params: {
                      sessionId: "s",
                      update: {
                        sessionUpdate: "state_update",
                        state: "idle",
                        stopReason: "end_turn",
                        usage: null,
                        _meta: null,
                      },
                    },
                  });
                yield* Queue.offer(
                  io.input,
                  new TextEncoder().encode(frames.map(JSON.stringify).join("\n") + "\n"),
                );
              }
            }
            const exit = yield* Fiber.await(work);
            // Effect creates fresh tracing IDs per call. Preserve their presence
            // and sampled flag while canonicalizing only volatile identifiers.
            if (wire?.traceId !== undefined) wire.traceId = "original-trace-id";
            if (wire?.spanId !== undefined) wire.spanId = "original-span-id";
            cases.push({
              generation,
              method,
              input,
              response,
              wire,
              valid: Exit.isSuccess(exit),
              ...(Exit.isSuccess(exit)
                ? { output: exit.value ?? null }
                : { errorCode: Cause.squash(exit.cause).code }),
            });
          });
          const initialize = {
            protocolVersion: 1,
            clientInfo: { name: "fixture", version: "1", title: null },
            clientCapabilities: {
              auth: { terminal: true, _meta: null },
              elicitation: null,
              _meta: null,
            },
            _meta: null,
          };
          const initialized =
            generation === 1
              ? {
                  protocolVersion: 1,
                  agentInfo: null,
                  agentCapabilities: {
                    loadSession: true,
                    promptCapabilities: { image: true, _meta: null },
                    sessionCapabilities: { list: { _meta: null }, fork: null, _meta: null },
                    auth: { logout: null, _meta: null },
                    positionEncoding: "utf-8",
                    _meta: null,
                  },
                  authMethods: [{ id: "legacy", name: "Legacy", description: null, _meta: null }],
                  _meta: null,
                }
              : {
                  protocolVersion: 2,
                  info: { name: "agent", version: "2", title: null },
                  capabilities: { session: { prompt: { image: {} }, mcp: { stdio: {} } } },
                  authMethods: [
                    {
                      type: "terminal",
                      methodId: "terminal",
                      name: "Terminal",
                      args: ["--auth"],
                      env: [{ name: "A", value: "1" }],
                      _meta: null,
                    },
                    {
                      type: "env_var",
                      methodId: "env",
                      name: "Environment",
                      vars: [{ name: "TOKEN", label: "Token" }],
                      link: null,
                    },
                    { type: "agent", methodId: "agent", name: "Agent" },
                  ],
                  _meta: null,
                };
          yield* invoke("initialize", initialize, initialized);
          const setup =
            generation === 1
              ? {
                  sessionId: "s",
                  models: {
                    currentModelId: "m",
                    availableModels: [{ modelId: "m", name: "Model", description: null }],
                    _meta: null,
                  },
                  modes: {
                    currentModeId: "mode",
                    availableModes: [{ id: "mode", name: "Mode", description: null }],
                    _meta: null,
                  },
                  configOptions: [v1Config],
                  _meta: null,
                }
              : { sessionId: "s", configOptions: [v2Config], _meta: null };
          const loaded = { ...setup };
          delete loaded.sessionId;
          const methods = [
            ["authenticate", { methodId: "agent", _meta: null }, { _meta: null }],
            ["logout", { _meta: null }, { _meta: null }],
            [
              "createSession",
              {
                cwd: "/workspace",
                additionalDirectories: ["/other"],
                mcpServers: [
                  { name: "fixture", command: "fixture", args: [], env: [], _meta: null },
                ],
                _meta: null,
              },
              setup,
            ],
            [
              "loadSession",
              { sessionId: "s", cwd: "/workspace", mcpServers: [], _meta: null },
              loaded,
            ],
            [
              "listSessions",
              { cursor: null, cwd: null, _meta: null },
              {
                sessions: [
                  { sessionId: "s", cwd: "/workspace", title: null, updatedAt: null, _meta: null },
                ],
                nextCursor: null,
                _meta: null,
              },
            ],
            ["forkSession", { sessionId: "s", cwd: "/workspace", _meta: null }, setup],
            [
              "resumeSession",
              { sessionId: "s", cwd: "/workspace", replayFrom: null, _meta: null },
              loaded,
            ],
            ["closeSession", { sessionId: "s", _meta: null }, { _meta: null }],
            ["deleteSession", { sessionId: "s", _meta: null }, { _meta: null }, generation === 1],
            ["listProviders", { _meta: null }, { providers: [], _meta: null }, generation === 1],
            [
              "setProvider",
              {
                providerId: "p",
                apiType: "openai",
                baseUrl: "https://example.test",
                headers: { Authorization: "fixture" },
                _meta: null,
              },
              { _meta: null },
              generation === 1,
            ],
            [
              "disableProvider",
              { providerId: "p", _meta: null },
              { _meta: null },
              generation === 1,
            ],
            [
              "setSessionModel",
              { sessionId: "s", modelId: "m", _meta: null },
              { _meta: null },
              generation === 2,
            ],
            [
              "setSessionMode",
              { sessionId: "s", modeId: "mode", _meta: null },
              { _meta: null },
              generation === 2,
            ],
            [
              "setSessionConfigOption",
              { sessionId: "s", configId: "model", value: "m", _meta: null },
              { configOptions: [generation === 1 ? v1Config : v2Config], _meta: null },
            ],
            [
              "setSessionConfigOption",
              { type: "boolean", sessionId: "s", configId: "safe", value: false, _meta: null },
              { configOptions: [], _meta: null },
            ],
            [
              "prompt",
              {
                sessionId: "s",
                prompt: [{ type: "text", text: "hello", _meta: null }],
                _meta: null,
              },
              generation === 1
                ? { stopReason: "end_turn", usage: null, _meta: null }
                : { _meta: null },
            ],
            ["cancel", { sessionId: "s", _meta: null }, null],
          ];
          for (const args of methods) yield* invoke(...args);
        }).pipe(Effect.provide(services));
      }),
    ).pipe(Effect.timeout("30 seconds")),
  );
}
fs.writeFileSync(
  new URL("../crates/acp/tests/fixtures/client-facade.jsonl", import.meta.url),
  cases.map(JSON.stringify).join("\n") + "\n",
);
console.log(`${cases.length} original negotiated client facade cases`);
