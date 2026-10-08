// Development-only source factory oracle. Runtime and Rust tests need no Node.
// Node24: node rust/tools/generate_acp_error_fixtures.mjs
import fs from "node:fs";
import zlib from "node:zlib";
import * as Errors from "../../packages/effect-acp/src/errors.ts";
const cases = [];
const compact = (value) =>
  Object.fromEntries(Object.entries(value).filter(([, value]) => value !== undefined));
function observe(error) {
  const output = { tag: error._tag, message: error.message, hasCause: error.cause !== undefined };
  if (error.cause?._tag) output.causeTag = error.cause._tag;
  for (const key of [
    "code",
    "method",
    "requestId",
    "operation",
    "issueCount",
    "issueKinds",
    "maximumPathDepth",
  ])
    if (error[key] !== undefined) output[key] = error[key];
  if (error._tag === "AcpRequestError") output.protocol = error.toProtocolError();
  return output;
}
function add(operation, input, error) {
  cases.push({ operation, input, output: observe(error) });
}
const cause = { secret: "private rejected payload" };
for (const command of [undefined, "", "acp --stdio"])
  add("spawn", { command }, new Errors.AcpSpawnError({ command, cause }));
for (const code of [undefined, 0, 9, -1, 1.5])
  for (const stderr of [
    undefined,
    "",
    " \n secret stderr \r\n",
    "\ufeff trim \ufeff",
    "\u001c keep \u001c",
  ])
    add(
      "exit",
      { code, stderr },
      new Errors.AcpProcessExitedError(compact({ code, stderr, pid: 7, cause })),
    );
for (const operation of ["encode-message", "decode-wire-message", "decode-notification-payload"])
  for (const method of [undefined, "", "x/test"])
    add(
      "parse",
      { operation, method },
      new Errors.AcpProtocolParseError(compact({ operation, method, requestId: "0", cause })),
    );
for (const operation of [undefined, "call-rpc", "read-input-stream", "read-process-exit-status"])
  for (const method of [undefined, "", "x/test"])
    add(
      "transport",
      { operation, method },
      new Errors.AcpTransportError({
        operation,
        method,
        detail: "private transport detail",
        pid: 7,
        cause,
      }),
    );
add("input-end", {}, new Errors.AcpInputStreamEndedError({}));
for (const operation of [
  "parseError",
  "invalidRequest",
  "invalidParams",
  "internalError",
  "authRequired",
  "resourceNotFound",
])
  for (const [message, data] of [
    [undefined, undefined],
    ["", null],
    ["custom message", { reason: true }],
  ])
    add(operation, { message, data }, Errors.AcpRequestError[operation](message, data));
add("methodNotFound", { method: "x/unknown" }, Errors.AcpRequestError.methodNotFound("x/unknown"));
for (const requestId of [0, "0", "$t3:jsonrpc:number:0"])
  for (const data of [undefined, null, { reason: true }])
    add(
      "fromProtocolError",
      { method: "x/test", requestId, data },
      Errors.AcpRequestError.fromProtocolError(
        { code: -32002, message: "remote message", data },
        { method: "x/test", requestId },
      ),
    );
for (const requestId of [0, "0"]) {
  add(
    "fromExtensionResponseFailure",
    { method: "x/test", requestId },
    Errors.AcpRequestError.fromExtensionResponseFailure("x/test", requestId, cause),
  );
  add(
    "fromExtensionResponseEncodingError",
    { method: "x/test", requestId },
    Errors.AcpRequestError.fromExtensionResponseEncodingError(
      "x/test",
      requestId,
      new Errors.AcpProtocolParseError({ operation: "encode-message", cause }),
    ),
  );
  add(
    "unsupportedStreamingResponse",
    { method: "x/test", requestId },
    Errors.AcpRequestError.unsupportedStreamingResponse("x/test", requestId),
  );
}
for (const extension of [false, true]) {
  const operation = extension ? "fromExtensionHandlerError" : "fromCoreHandlerError";
  add(
    operation,
    { method: "x/test", request: false },
    Errors.AcpRequestError[operation](new Errors.AcpTransportError({ cause }), "x/test"),
  );
  add(
    operation,
    { method: "x/test", request: true },
    Errors.AcpRequestError[operation](
      Errors.AcpRequestError.authRequired("custom auth", null),
      "x/test",
    ),
  );
}
fs.writeFileSync(
  new URL("../crates/acp/tests/fixtures/error-factories.jsonl.gz", import.meta.url),
  zlib.gzipSync(cases.map(JSON.stringify).join("\n") + "\n", { level: 9 }),
);
console.log(`${cases.length} original ACP error factory cases`);
