// Development oracle: executes the unchanged provider failure implementation.
import fs from "node:fs";
import { makeProviderFailure } from "../../apps/server/src/orchestration-v2/ProviderFailure.ts";
process.env.TZ = "UTC"; // Explicit date-parser locale for portable source fixtures.
const rows = [];
function add(name, input) {
  rows.push({ name, input, output: makeProviderFailure(input) });
}
for (const input of [
  {},
  { message: "" },
  { message: "   " },
  { cause: "raw secret defect" },
  { cause: { message: "raw secret defect", code: "native_code" } },
  { message: "explicit safe text", cause: { _tag: "ProviderAdapterTurnStartError" } },
  { code: null, cause: { code: "fallback" } },
  { code: "", cause: { code: "fallback" } },
])
  add("defaults", input);
const tags = [
  "ContextHandoffBudgetError",
  "ClaudeBackgroundWorkBlocksQueryReplacementError",
  "ContextHandoffDeliveryUncertainError",
  "ProviderAdapterTurnStartError",
  "ProviderAdapterEventStreamError",
  "ProviderAdapterOpenSessionError",
  "ProviderAdapterResumeThreadError",
  "ArbitraryProviderDefect",
];
for (const tag of tags) {
  for (const message of [undefined, "Known explanation token=secret", ""])
    add("known cause category", { cause: { _tag: tag, message, code: "cause-code" } });
  for (const depth of [0, 1, 2, 15, 16, 17]) {
    let cause = { _tag: tag, message: "known reason" };
    for (let n = 0; n < depth; n++) cause = { cause };
    add("nested cause depth", { cause });
  }
  for (const inner of tags)
    add("nested known precedence", {
      cause: { _tag: tag, cause: { _tag: inner, message: "nested reason" } },
    });
}
const texts = [
  'request failed: Authorization: Bearer bearer-secret https://user:pass@example.test/path?access_token=url-secret#fragment {"token":"json-secret"} api_key=key-secret sk-abcdefghijklmnop',
  "before\u0000\u0007\t\nafter\u007f",
  "secret=value, password=\"hello world\"; credential='hello world' token : v",
  "Bearer secret Basic secret bearer SECRET BASIC a,bearer next;Basic last",
  "fooBearer Bearer secret Basic secret",
  "éBearer secret ſBearer secret KBearer secret _Bearer secret 1Bearer secret",
  "https://user:pass@EXAMPLE.test:443/path?token=secret#hash).,!?",
  "HTTP://user:pass@EXAMPLE.test HTTP://[::1]/?token=secret http://%invalid",
  "http://example.test/a(b)c., http://example.test http://user@example.test:8080/?secret=x",
  "tokenization=preserve étoken=redact tokené=keep _token=keep token_=keep",
  '"TOKEN" : "value" \'api-key\':\'secret\' "password":\'mixed\' "api_key":"one"',
  "sk-abcdefghijklmnop sk-abcdefghijklmno sk-abcdefghijklmnop_ xsk-abcdefghijklmnop ésk-abcdefghijklmnop sk-abcdefghijklmnopé",
];
for (const space of [
  " ",
  "\t",
  "\n",
  "\u0085",
  "\u00a0",
  "\u1680",
  "\u2000",
  "\u2028",
  "\u2029",
  "\ufeff",
]) {
  texts.push(`${space}Bearer${space}secret${space}after${space}`);
  texts.push(`${space}token${space}=${space}secret${space}after${space}`);
  texts.push(`https://example.test/path${space}token=x`);
  texts.push(`"token"${space}:${space}"secret"`);
}
for (const text of texts)
  for (const field of ["message", "code"])
    add("credential/control/url redaction", { [field]: text });
for (const maximum of [128, 4096])
  for (const length of [maximum - 2, maximum - 1, maximum, maximum + 1, maximum + 2, maximum * 2]) {
    const field = maximum === 128 ? "code" : "message";
    for (const suffix of ["", "😀", "😀tail", "𐐀", "a😀", "…", "\uFEFF"])
      add("UTF16 boundary", { [field]: "a".repeat(length) + suffix });
    for (const position of [maximum - 3, maximum - 2, maximum - 1, maximum])
      add("astral truncation boundary", { [field]: "x".repeat(position) + "😀" + "y".repeat(20) });
  }
for (const classification of [
  "unknown",
  "validation_error",
  "transport_error",
  "provider_error",
  "usage_limit",
]) {
  for (const retryable of [undefined, null, false, true])
    add("class/retryable", { class: classification, retryable, message: "safe" });
  for (const resetAt of [
    undefined,
    null,
    "",
    "not-a-date",
    "2026-10-08T12:34:56.789Z",
    "2026-10-08T12:34:56+02:30",
    "2026-10-08T12:34:56",
    "2026-10-08T12:34",
    "2026-10-08T24:00:00Z",
    "2026-10-08T24:00:00.0000Z",
    "2026-10-08T24:00:00.0001Z",
    "2026-10-08T12:34:56.123456789123Z",
    "2026-02-30T12:34:56Z",
    "2026-10-08T12:34:60Z",
    "2026-10-08T12:34:56+24:00",
    "10/08/2026",
    "2026/10/08",
    "10/08/2026 12:34:56",
    "2026-10-08 12:34:56",
    "+010000-10-08T12:34:56Z",
    "-000001-10-08T12:34:56Z",
    "-000000-10-08T12:34:56Z",
    "2026-10-08",
    "2026-10",
    "2026",
    "2026-02-30",
    "2026-13-01",
    "Thu, 08 Oct 2026 12:34:56 GMT",
  ])
    add("reset date", { class: classification, resetAt });
}
fs.writeFileSync(
  new URL("../crates/server/tests/fixtures/provider-failures.json", import.meta.url),
  JSON.stringify(rows, null, 2) + "\n",
);
console.log(`${rows.length} unchanged-source provider failure witnesses`);
