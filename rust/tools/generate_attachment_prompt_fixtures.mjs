// Runs the original provider-input builder (AttachmentPrompt.ts) over attachments with and
// without captured-window sources, element trees that exercise every compaction rule, and
// message lengths at the provider input limit.
import { writeFileSync, renameSync } from "node:fs";
import {
  isProviderNativeImageAttachment,
  providerMessageTextWithAttachmentPaths,
} from "../../apps/server/src/orchestration-v2/AttachmentPrompt.ts";

const rows = [];
const dir = "/srv/state/attachments";
const uuid = "00000000-0000-4000-8000-0000000000aa";
const image = (extra = {}) => ({
  type: "image",
  id: `thread-1-${uuid}`,
  name: "shot.png",
  mimeType: "image/png",
  sizeBytes: 10,
  ...extra,
});
const file = (extra = {}) => ({
  type: "file",
  id: `thread-1-${uuid}-pdf`,
  name: "report.pdf",
  mimeType: "application/pdf",
  sizeBytes: 10,
  ...extra,
});
const unknown = {
  type: "future",
  id: `thread-1-${uuid}`,
  name: "x",
  mimeType: "a/b",
  sizeBytes: 1,
};
const add = (label, text, attachments) =>
  rows.push({
    kind: "text",
    label,
    text,
    attachments,
    expected: providerMessageTextWithAttachmentPaths({ text, attachments, attachmentsDir: dir }),
  });
for (const mimeType of [
  "image/png",
  "IMAGE/JPEG",
  "image/gif",
  "image/webp",
  "image/svg+xml",
  "image/heic",
  "application/pdf",
])
  for (const type of ["image", "file"]) {
    const attachment = { type, id: `thread-1-${uuid}`, name: "a.bin", mimeType, sizeBytes: 1 };
    rows.push({
      kind: "native",
      label: `${type} ${mimeType}`,
      attachments: [attachment],
      expected: isProviderNativeImageAttachment(attachment),
    });
  }
add("no attachments", "hello", []);
add("empty text and attachment", "", [image()]);
add("image and file", "look", [image(), file()]);
add("unknown skipped", "x", [unknown, image()]);
add("quotes and unicode in names", "t", [
  image({ name: 'we"ird ✓.png' }),
  file({ name: "ünï cödé.PDF" }),
]);
add("bad ids have no path", "t", [
  image({ id: "../../x" }),
  file({ id: "a/b" }),
  image({ id: "" }),
]);
add("jpeg mime picks .jpg", "t", [image({ mimeType: "image/jpeg", name: "p" })]);
add("part extension is bin", "t", [file({ id: `thread-1-${uuid}-bin`, name: "x.part" })]);
const node = (role, extra = {}) => ({ role, bounds: null, children: [], ...extra });
const b = (x, y, width, height) => ({ x, y, width, height });
const source = (extra = {}) => ({
  kind: "snap-shot",
  capturedAt: "2026-01-01T00:00:00.000Z",
  appName: "Editor",
  windowTitle: "main.rs — repo",
  ...extra,
});
add("flat text accessibility", "see", [
  image({
    source: source({
      accessibility: { format: "flat-text", text: "Hello window", truncated: false },
    }),
  }),
]);
add("flat text truncated", "see", [
  image({
    source: source({
      accessibility: { format: "flat-text", text: "Hello window", truncated: true },
    }),
  }),
]);
add("accessibleText fallback", "see", [
  image({ source: source({ accessibleText: "Legacy text" }) }),
]);
add("no accessibility", "see", [image({ source: source() })]);
add("source on file ignored", "see", [file({ source: { _tag: "pasted-text" } })]);
add("empty window title", "see", [image({ source: source({ windowTitle: "" }) })]);
const size = { width: 800, height: 600 };
const tree = (root, extra = {}) => ({
  format: "element-tree",
  coordinateSpace: "captured-image",
  imageSize: size,
  truncated: false,
  root,
  ...extra,
});
add("tree root full bounds dropped", "x", [
  image({
    source: source({
      accessibility: tree(node("window", { name: "Main", bounds: b(0, 0, 800, 600) })),
    }),
  }),
]);
add("tree root partial bounds kept", "x", [
  image({
    source: source({
      accessibility: tree(node("window", { name: "Main", bounds: b(0, 0, 800, 500) })),
    }),
  }),
]);
add("tree without any bounds", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          name: "Main",
          children: [node("button", { name: "OK", actions: ["press", "focus"] })],
        }),
      ),
    }),
  }),
]);
add("tree with nested bounds", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          name: "Main",
          children: [
            node("group", {
              children: [
                node("button", { name: "OK", bounds: b(10, 10, 40, 20), actions: ["press"] }),
              ],
            }),
          ],
        }),
      ),
    }),
  }),
]);
add("tree truncated", "x", [
  image({
    source: source({ accessibility: tree(node("window", { name: "Main" }), { truncated: true }) }),
  }),
]);
add("redundant window button description", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          name: "Main",
          children: [
            node("button", {
              name: "Close",
              description: "Close the window",
              bounds: b(1, 1, 5, 5),
            }),
            node("button", { name: "Close", description: "Closes it", bounds: b(1, 1, 5, 5) }),
            node("button", {
              name: "Minimize",
              description: "  MINIMIZE  the   window ",
              bounds: b(1, 1, 5, 5),
            }),
          ],
        }),
      ),
    }),
  }),
]);
add("empty groups collapse", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          children: [
            node("group", {
              children: [
                node("group", {
                  children: [node("static_text", { name: "deep", bounds: b(1, 2, 3, 4) })],
                }),
              ],
            }),
            node("group"),
          ],
        }),
      ),
    }),
  }),
]);
add("group with metadata kept", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          children: [
            node("group", {
              name: "Toolbar",
              children: [node("button", { name: "Go", bounds: b(1, 1, 2, 2) })],
            }),
            node("group", { state: { enabled: true } }),
            node("group", { value: "v" }),
            node("group", { description: "d" }),
          ],
        }),
      ),
    }),
  }),
]);
add("separators and tab groups pruned", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          children: [
            node("separator"),
            node("tab_group"),
            node("separator", { name: "S" }),
            node("tab_group", { children: [node("tab", { name: "T", bounds: b(1, 1, 1, 1) })] }),
            node("separator", { actions: ["press"] }),
          ],
        }),
      ),
    }),
  }),
]);
add("static text duplicating parent dropped", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          children: [
            node("button", {
              name: "Save",
              bounds: b(1, 1, 9, 9),
              children: [
                node("static_text", { name: "Save" }),
                node("static_text", { name: "Other" }),
                node("static_text", { name: "Save", value: "val" }),
              ],
            }),
            node("group", { name: "Save", children: [node("static_text", { name: "Save" })] }),
          ],
        }),
      ),
    }),
  }),
]);
add("child name equal to parent dropped except groups", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          name: "W",
          children: [
            node("panel", { name: "W", bounds: b(1, 1, 9, 9) }),
            node("group", {
              name: "W",
              children: [node("panel", { name: "W", bounds: b(1, 1, 9, 9) })],
            }),
          ],
        }),
      ),
    }),
  }),
]);
add("parent name inherited through unnamed nodes", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          name: "W",
          children: [
            node("panel", {
              children: [
                node("static_text", { name: "W" }),
                node("button", { name: "W", bounds: b(1, 1, 2, 2) }),
              ],
            }),
          ],
        }),
      ),
    }),
  }),
]);
add("button press only action dropped", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          children: [
            node("button", { name: "A", actions: ["press"], bounds: b(1, 1, 2, 2) }),
            node("link", { name: "B", actions: ["press", "open"], bounds: b(1, 1, 2, 2) }),
          ],
        }),
      ),
    }),
  }),
]);
add("empty strings omitted", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          name: "W",
          value: "",
          description: "",
          children: [node("button", { name: "", bounds: b(1, 1, 2, 2) })],
        }),
      ),
    }),
  }),
]);
add("state and all fields", "x", [
  image({
    source: source({
      accessibility: tree(
        node("window", {
          name: "W",
          value: "v",
          description: "d",
          bounds: b(0, 0, 10, 10),
          state: { focused: true, checked: "mixed" },
          actions: ["raise"],
          children: [
            node("checkbox", { name: "C", state: { checked: "on" }, bounds: b(2, 2, 3, 3) }),
          ],
        }),
      ),
    }),
  }),
]);
add("two images with sources", "x", [
  image({ source: source({ accessibleText: "one" }) }),
  image({
    id: `thread-1-${uuid.replace(/aa$/, "bb")}`,
    source: source({
      appName: "Other",
      windowTitle: "two",
      accessibility: { format: "flat-text", text: "two", truncated: false },
    }),
  }),
]);
add("unicode and escapes in window data", "x", [
  image({
    source: source({
      appName: 'App "Q" \\',
      windowTitle: "tab\n\t\u0001é😀\u2028",
      accessibleText: "x",
    }),
  }),
]);
const limit = 120000;
add("text exactly at limit minus note", "a".repeat(limit - 200), [image()]);
add("note would overflow so it is dropped", "a".repeat(limit - 10), [image(), file()]);
add("text at limit exactly", "a".repeat(limit), [image()]);
add("multibyte length counted in utf16 units", "😀".repeat(limit / 2 - 40), [image(), file()]);
add("later notes still fit after an earlier one is dropped", "b".repeat(limit - 150), [
  image({ name: "n".repeat(200) }),
  image({ id: `thread-1-${uuid}`, name: "s.png" }),
]);
add("big accessibility dropped but path note kept", "a", [
  image({ source: source({ accessibleText: "x".repeat(125000) }) }),
]);
const target = new URL("../crates/server/tests/fixtures/attachment-prompt.jsonl", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map(JSON.stringify).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
