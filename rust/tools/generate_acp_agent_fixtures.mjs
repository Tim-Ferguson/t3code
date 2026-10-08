// Actual original agent protocol responses, without a test schema hiding wire
// defaults or envelopes. Development only: Node24, original dependencies.
import fs from "node:fs";
import * as Effect from "../../packages/effect-acp/node_modules/effect/dist/Effect.js";
import * as Queue from "../../packages/effect-acp/node_modules/effect/dist/Queue.js";
import * as Scope from "../../packages/effect-acp/node_modules/effect/dist/Scope.js";
import * as Layer from "../../packages/effect-acp/node_modules/effect/dist/Layer.js";
import * as Exit from "../../packages/effect-acp/node_modules/effect/dist/Exit.js";
import * as Agent from "../../packages/effect-acp/src/agent.ts";
import * as Errors from "../../packages/effect-acp/src/errors.ts";
import * as Client from "../../packages/effect-acp/src/client.ts";
import * as Fiber from "../../packages/effect-acp/node_modules/effect/dist/Fiber.js";
import * as Cause from "../../packages/effect-acp/node_modules/effect/dist/Cause.js";
import { makeInMemoryStdio } from "../../packages/effect-acp/src/_internal/stdio.ts";
const cases = [];
for (const [mode, method, params] of [
  [
    "success",
    "initialize",
    { protocolVersion: 2, capabilities: {}, info: { name: "test", version: "1" } },
  ],
  ["missing", "auth/logout", {}],
  ["request-error", "initialize", { protocolVersion: 2, info: { name: "test", version: "1" } }],
  ["transport-error", "initialize", { protocolVersion: 2, info: { name: "test", version: "1" } }],
  ["die", "initialize", { protocolVersion: 2, info: { name: "test", version: "1" } }],
  ["invalid", "initialize", { protocolVersion: "invalid" }],
  ["extension-error", "x/fail", {}],
]) {
  const input = { jsonrpc: "2.0", id: 7, method, params };
  const output = await Effect.runPromise(
    Effect.gen(function* () {
      const { stdio, input: incoming, output: outgoing } = yield* makeInMemoryStdio();
      const scope = yield* Scope.make();
      const context = yield* Layer.buildWithScope(Agent.layer(stdio), scope);
      return yield* Effect.gen(function* () {
        const agent = yield* Agent.AcpAgent;
        yield* agent.handleInitialize(() => {
          if (mode === "request-error")
            return Effect.fail(Errors.AcpRequestError.authRequired("custom auth", null));
          if (mode === "transport-error")
            return Effect.fail(new Errors.AcpTransportError({ cause: { private: true } }));
          if (mode === "die") return Effect.die(new Error("handler bug"));
          return Effect.succeed({ protocolVersion: 2, info: { name: "mock-agent", version: "1" } });
        });
        if (mode === "extension-error")
          yield* agent.handleUnknownExtRequest(() =>
            Effect.fail(Errors.AcpRequestError.authRequired("custom auth", null)),
          );
        yield* Queue.offer(incoming, new TextEncoder().encode(JSON.stringify(input) + "\n"));
        return JSON.parse(yield* Queue.take(outgoing));
      }).pipe(
        Effect.provide(context),
        Effect.provideService(Scope.Scope, scope),
        Effect.ensuring(Scope.close(scope, Exit.void)),
      );
    }),
  );
  cases.push({ mode, input, output });
}
fs.writeFileSync(
  new URL("../crates/acp/tests/fixtures/agent-wire.jsonl", import.meta.url),
  cases.map((c) => JSON.stringify(c)).join("\n") + "\n",
);
console.log(`${cases.length} original agent wire responses`);
const failures = [];
const valid = { _tag: "Fail", error: { code: -32000, message: "custom auth", data: null } };
for (const core of [false, true])
  for (const reasons of [
    [valid],
    [{ _tag: "Die", defect: "private failure" }],
    [{ _tag: "Die", defect: "private failure" }, valid],
    [{ _tag: "Fail", error: { invalid: true } }, valid],
  ]) {
    const result = await Effect.runPromise(
      Effect.gen(function* () {
        const { stdio, input, output } = yield* makeInMemoryStdio();
        const scope = yield* Scope.make();
        const context = yield* Layer.buildWithScope(
          Layer.effect(Client.AcpClient, Client.make(stdio)),
          scope,
        );
        return yield* Effect.gen(function* () {
          const client = yield* Client.AcpClient;
          const pending = yield* (
            core
              ? client.agent.initialize({
                  protocolVersion: 2,
                  clientInfo: { name: "test", version: "1" },
                })
              : client.raw.request("x/test", {})
          ).pipe(Effect.forkScoped);
          const request = JSON.parse(yield* Queue.take(output));
          const error = { _tag: "Cause", code: 0, message: "fixture failure", data: reasons };
          yield* Queue.offer(
            input,
            new TextEncoder().encode(
              JSON.stringify({ jsonrpc: "2.0", id: request.id, error }) + "\n",
            ),
          );
          const exit = yield* Fiber.await(pending);
          const failure = Cause.findErrorOption(exit.cause);
          if (failure._tag !== "Some")
            return {
              core,
              requestId: request.id,
              error,
              output: {
                tag: "Defect",
                message: String(Cause.squash(exit.cause)),
                causeShape: "defect",
              },
            };
          const actual = failure.value;
          const observed = { tag: actual._tag, message: actual.message };
          for (const key of ["code", "method", "requestId", "operation", "data"])
            if (actual[key] !== undefined) observed[key] = actual[key];
          observed.causeShape = Array.isArray(actual.cause)
            ? "array"
            : actual.cause?._tag === "RpcClientError"
              ? "rpc-client-error"
              : actual.cause?.code !== undefined
                ? "protocol-error"
                : "other";
          return { core, requestId: request.id, error, output: observed };
        }).pipe(
          Effect.provide(context),
          Effect.provideService(Scope.Scope, scope),
          Effect.ensuring(Scope.close(scope, Exit.void)),
        );
      }),
    );
    failures.push(result);
  }
fs.writeFileSync(
  new URL("../crates/acp/tests/fixtures/client-response-errors.jsonl", import.meta.url),
  failures.map((c) => JSON.stringify(c)).join("\n") + "\n",
);
console.log(`${failures.length} original client response-cause failures`);
