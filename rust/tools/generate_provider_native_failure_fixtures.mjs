// Executes source adapter classifiers; no provider process or credentials involved.
import fs from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import { AcpRequestError } from "../../packages/effect-acp/src/errors.ts";
import { makeProviderFailure } from "../../apps/server/src/orchestration-v2/ProviderFailure.ts";
const source = fs.readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts", import.meta.url),
  "utf8",
);
function between(text, start, end, from = 0) {
  const a = text.indexOf(start, from),
    b = text.indexOf(end, a);
  if (a < 0 || b < 0) throw Error(`Missing source classifier ${start}`);
  return text.slice(a, b);
}
const code = new Function(
  `${stripTypeScriptTypes(between(source, "function codexErrorInfoCode(", "interface ActiveCodexTurnContext"))}return codexErrorInfoCode;`,
)();
const handler = source.indexOf('client.handleServerNotification("error"');
const nonRetry = between(source, "const notificationCode =", "const updatedAt =", handler).replace(
  "return;",
  "return context.latestProviderFailure.failure;",
);
const notification = new Function(
  "payload",
  "makeProviderFailure",
  "codexErrorInfoCode",
  `const context={};${nonRetry}`,
);
const retryBody = between(
  source,
  "const code = codexErrorInfoCode(payload.error.codexErrorInfo);",
  "const itemOrdinal =",
  handler,
);
const retry = new Function(
  "payload",
  "makeProviderFailure",
  "codexErrorInfoCode",
  `${retryBody}return failure;`,
);
const terminalStart = source.indexOf("const makeRootTerminalEvent =");
const terminalBody = between(source, "const previousFailure =", "return {", terminalStart);
const terminal = new Function("input", "makeProviderFailure", `${terminalBody}return failure;`);
const registry = fs.readFileSync(
  new URL(
    "../../apps/server/src/orchestration-v2/Adapters/AcpRegistryAdapterV2.ts",
    import.meta.url,
  ),
  "utf8",
);
const prompt = new Function(
  "makeProviderFailure",
  "isAcpRequestError",
  `const MISTRAL_VIBE_RATE_LIMITED=-31001;${stripTypeScriptTypes(between(registry, "export function acpRegistryPromptFailure(", "function makeAcpRegistryRuntime")).replace("export function", "function")}return acpRegistryPromptFailure;`,
)(makeProviderFailure, Schema.is(AcpRequestError));
const rows = [];
const infos = [
  undefined,
  null,
  "usageLimitExceeded",
  "rateLimitExceeded",
  "httpConnectionFailed",
  "responseStreamConnectionFailed",
  "invalidRequest",
  "internalServerError",
  { usageLimitExceeded: {} },
  { httpConnectionFailed: { httpStatusCode: 503 } },
  {},
  [],
  ["usageLimitExceeded"],
  { z: 1, 2: 2, 1: 1 },
  "sk-abcdefghijklmnop",
];
for (const info of infos)
  for (const message of [
    "Native failure",
    "token=secret https://user:pass@example.test/?token=x",
    "",
  ])
    for (const details of [undefined, "", " \ufeffExtra detail token=secret\ufeff ", "\u0085"])
      for (const willRetry of [false, true]) {
        const error = { message, additionalDetails: details, codexErrorInfo: info };
        rows.push({
          operation: "codexError",
          input: { error, willRetry },
          output: (willRetry ? retry : notification)(
            { error, willRetry },
            makeProviderFailure,
            code,
          ),
        });
      }
for (const info of infos)
  for (const message of [undefined, "Native failure", "Different failure"])
    for (const previous of [
      undefined,
      {
        nativeMessage: "Native failure",
        failure: makeProviderFailure({
          message: "Extra detail",
          code: code(info),
          class: "usage_limit",
        }),
      },
      {
        nativeMessage: "Other native failure",
        failure: makeProviderFailure({
          message: "Other",
          code: "other",
          class: "transport_error",
          retryable: true,
        }),
      },
    ]) {
      const error = { message, codexErrorInfo: info };
      rows.push({
        operation: "codexTerminal",
        input: { error, previous },
        output: terminal(
          {
            context: { latestProviderFailure: previous },
            failureMessage: message,
            failureCode: code(info),
          },
          makeProviderFailure,
        ),
      });
    }
for (const agentId of ["", "local:mistral-vibe", "mistral-vibe", "other"])
  for (const code of [-31001, -32603, -32602, 401])
    for (const message of [
      "Native failure",
      "Authorization: Bearer secret https://user:pass@example.test/?token=x",
      "",
    ]) {
      const cause = new AcpRequestError({ code, errorMessage: message });
      rows.push({
        operation: "acpPrompt",
        input: { agentId, request: { code, message } },
        output: prompt(agentId, cause),
      });
    }
for (const agentId of ["", "mistral-vibe"])
  for (const cause of [
    undefined,
    "secret transport defect",
    { message: "arbitrary provider defect" },
  ])
    rows.push({ operation: "acpPrompt", input: { agentId }, output: prompt(agentId, cause) });
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/provider-native-failures.json", import.meta.url),
  JSON.stringify(rows, null, 2) + "\n",
);
console.log(`${rows.length} unchanged-source native provider failure witnesses`);
