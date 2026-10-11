// Runs the unchanged `makeAssistantStreamingFilter` and `splitBufferedAssistantText` of the original
// (`apps/server/src/orchestration-v2/assistantStreaming.ts`) over scripted markdown and event sequences. The Rust
// port replays every row (`assistant_streaming.rs`). Output: rust/crates/server/tests/fixtures/assistant-streaming.jsonl
import { writeFileSync } from "node:fs";

const root = new URL("../../", import.meta.url);
const { makeAssistantStreamingFilter, splitBufferedAssistantText } = await import(
  new URL("apps/server/src/orchestration-v2/assistantStreaming.ts", root).href
);

// A small deterministic generator, so the fixture does not change between runs.
let seed = 0x2f6e2b1;
const random = () => {
  seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
  return seed / 0x100000000;
};
const pick = (items) => items[Math.floor(random() * items.length)];

const lines = [
  "A plain sentence that goes on.",
  "",
  "",
  "# Heading",
  "## Sub heading",
  "**Bold title**",
  "**Bold title:**",
  "- first item",
  "  - nested item",
  "* star item",
  "1. numbered item",
  "2) paren item",
  "```js",
  "const x = 1;",
  "```",
  "~~~",
  "    indented code",
  "text with trailing spaces   ",
  "tab\tseparated",
  "unicode: héllo wörld 日本語 🙂",
  " ",
  "> quote",
  "-not a list",
  "1.not a list",
  "####### seven hashes",
];
// Never cut between the halves of a surrogate pair: JSON would carry a lone surrogate the Rust side cannot read.
const cut = (text, length) => {
  const end = Math.max(0, Math.min(text.length, length));
  const code = text.charCodeAt(end - 1);
  return end > 0 && end < text.length && code >= 0xd800 && code <= 0xdbff
    ? text.slice(0, end + 1)
    : text.slice(0, end);
};
const documents = [];
for (let index = 0; index < 60; index++) {
  const count = 1 + Math.floor(random() * 12);
  let text = "";
  for (let line = 0; line < count; line++) text += pick(lines) + (random() < 0.9 ? "\n" : "\r\n");
  if (random() < 0.5) text = cut(text, text.length - 1 - Math.floor(random() * 3));
  documents.push(text);
}
documents.push(
  "",
  "\n",
  "\n\n",
  "a",
  "a\n",
  "a\n\nb",
  "```\nopen fence\n\nstill open",
  "```\nx\n```\nafter",
  "- a\n- b\n- c",
);

const rows = [];
for (const text of documents) {
  rows.push({ type: "split", text, result: splitBufferedAssistantText(text) });
}

const message = (id, text, streaming) => ({
  id,
  role: "assistant",
  text,
  streaming,
});
const item = (id, type, text, streaming) => ({ id, type, text, streaming });
const toOriginal = (step) => {
  switch (step.kind) {
    case "message.updated":
      return { type: "message.updated", message: step.payload };
    case "turn-item.updated":
      return { type: "turn_item.updated", turnItem: step.payload };
    case "node.updated":
      return { type: "node.updated", node: step.payload };
    default:
      return { type: step.kind };
  }
};
const fromOriginal = (event, step) => {
  if (event === null) return null;
  switch (step.kind) {
    case "message.updated":
      return event.message;
    case "turn-item.updated":
      return event.turnItem;
    default:
      return step.payload;
  }
};

const sequence = (mode, steps) => {
  const filter = makeAssistantStreamingFilter(mode);
  const delivered = steps.map((step) => fromOriginal(filter(toOriginal(step), step.now), step));
  rows.push({ type: "sequence", mode, steps, delivered });
};

// Messages and reasoning that grow chunk by chunk at irregular times, then complete.
for (let index = 0; index < 24; index++) {
  const mode = index % 3 === 2 ? "turn" : "paragraph";
  const body = documents[index * 2];
  const steps = [];
  let now = 1_000_000;
  const kind = index % 2 === 0 ? "message.updated" : "turn-item.updated";
  const make = (text, streaming) =>
    kind === "message.updated"
      ? message("m1", text, streaming)
      : item("m1", index % 4 === 1 ? "assistant_message" : "reasoning", text, streaming);
  steps.push({
    kind: "node.updated",
    payload: { kind: "assistant_message", status: "running" },
    now,
  });
  let shown = 0;
  while (shown < body.length) {
    shown = Math.min(body.length, shown + 1 + Math.floor(random() * 25));
    now += Math.floor(random() * 700);
    steps.push({ kind, payload: make(cut(body, shown), true), now });
    if (random() < 0.15) {
      steps.push({
        kind: "message.updated",
        payload: { id: "u1", role: "user", text: "q", streaming: true },
        now,
      });
    }
  }
  steps.push({ kind, payload: make(body, false), now: now + 5 });
  steps.push({ kind, payload: make(body + " more", true), now: now + 6 });
  sequence(mode, steps);
}
// Two interleaved messages share the filter, a repeat of the same ready text is withheld, and the interval is 400 ms.
sequence("paragraph", [
  { kind: "message.updated", payload: message("a", "one\n\ntwo", true), now: 0 },
  { kind: "message.updated", payload: message("a", "one\n\ntwo three", true), now: 399 },
  { kind: "message.updated", payload: message("a", "one\n\ntwo three", true), now: 400 },
  { kind: "message.updated", payload: message("a", "one\n\ntwo three\n\n", true), now: 800 },
  { kind: "message.updated", payload: message("b", "other\n\npartial", true), now: 800 },
  {
    kind: "turn-item.updated",
    payload: item("a", "assistant_message", "same id\n\nx", true),
    now: 800,
  },
  { kind: "message.updated", payload: message("a", "one\n\ntwo three\n\nfour", false), now: 801 },
]);

writeFileSync(
  new URL("rust/crates/server/tests/fixtures/assistant-streaming.jsonl", root),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} rows`);
