// Development-only: the recordings the original's OpenCode 2 text generation tests replay
// (`OpenCode2TextGeneration.test.ts`), written out as JSON for `tests/text_generation_opencode_flow.rs`. The first
// is the original's own live recording (`OpenCode2TextGeneration.fixture.ts`, 2.0.18, `opencode/big-pickle`); the others
// are the entries those tests build from its first four. Written to a `.jsonl` file so the formatter leaves it alone.
import { writeFileSync } from "node:fs";
import { OPENCODE2_TITLE_GENERATION } from "../../apps/server/src/textGeneration/OpenCode2TextGeneration.fixture.ts";

const [subscribe, connected, create, created] = OPENCODE2_TITLE_GENERATION;
const sessionId = "ses_f0ec85cf6ffebfalpvV2E9nocu";
const promptRequest = {
  type: "expect_outbound",
  frame: { type: "session.prompt", input: { sessionID: sessionId, text: "<any>" } },
};
const promptResponse = {
  type: "emit_inbound",
  frame: {
    type: "sdk.response",
    operation: "session.prompt",
    data: {
      data: {
        id: "msg_1",
        sessionID: sessionId,
        time: { created: 1 },
        type: "user",
        payload: { text: "t" },
        delivery: "steer",
      },
    },
  },
};
const removal = [
  { type: "expect_outbound", frame: { type: "session.remove", input: { sessionID: sessionId } } },
  {
    type: "emit_inbound",
    frame: { type: "sdk.response", operation: "session.remove", data: null },
  },
];
const exit = { type: "runtime_exit", status: "success" };
const refusal = {
  type: "provider.auth",
  message:
    "Error from provider (Console): OpenCode's free tier can only be used from within OpenCode",
  status: 403,
};
const execution = (type, data) => ({
  type: "emit_inbound",
  frame: {
    type: "sdk.event",
    event: {
      id: "evt_0eb7f9ae0001VCNDHVLnqTfoPG",
      created: 1790657403616,
      type,
      data: { sessionID: sessionId, ...data },
      durable: { aggregateID: sessionId, seq: 7, version: 1 },
    },
  },
});
const scenarios = {
  // The live recording: a title on the free tier, the session removed afterwards.
  title_generation: [...OPENCODE2_TITLE_GENERATION, exit],
  // The server goes away before the reply; the temporary session is still removed.
  stream_lost: [
    subscribe,
    connected,
    create,
    created,
    promptRequest,
    promptResponse,
    exit,
    ...removal,
  ],
  // A deny-all session is refused this way; the provider's message stays out of the detail.
  refused: [
    subscribe,
    connected,
    create,
    created,
    promptRequest,
    promptResponse,
    execution("session.execution.failed", { error: refusal }),
    ...removal,
    exit,
  ],
  // A reason added after 2.0.18: the full schema rejects the frame.
  unreadable_end: [
    subscribe,
    connected,
    create,
    created,
    promptRequest,
    promptResponse,
    execution("session.execution.interrupted", { reason: "budget" }),
    ...removal,
    exit,
  ],
  // An interrupted execution that is readable.
  interrupted: [
    subscribe,
    connected,
    create,
    created,
    promptRequest,
    promptResponse,
    execution("session.execution.interrupted", {}),
    ...removal,
    exit,
  ],
};
writeFileSync(
  new URL("../crates/server/tests/fixtures/text-generation-opencode2.jsonl", import.meta.url),
  Object.entries(scenarios)
    .map(([name, entries]) => JSON.stringify({ name, entries }))
    .join("\n") + "\n",
);
console.log(Object.keys(scenarios).join(", "));
