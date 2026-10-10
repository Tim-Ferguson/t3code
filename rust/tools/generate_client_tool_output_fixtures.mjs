// Runs the original's tool-output image reading over many outputs, for
// `crates/client/tests/tool_output_parity.rs`: how many images a tool returned inline, which is how
// many `tool-output-image` assets its row asks for.
//   readToolOutputImage, toolOutputImages   packages/shared/src/toolOutput.ts
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const source = readFileSync(
  new URL("../../packages/shared/src/toolOutput.ts", import.meta.url),
  "utf8",
);
const from = source.indexOf("/** An image a tool returned inline.");
const to = source.indexOf("/**\n * Replaces each image's bytes", from);
if (from < 0 || to < 0) throw new Error("cannot find the image readers");
const header = `
const Predicate = { isObject: (value) => (typeof value === "object" && value !== null) || typeof value === "function" };
const isProviderSendTurnSupportedImageMimeType = (mime) => ["image/gif", "image/jpeg", "image/png", "image/webp"].includes(mime.toLowerCase());
const PROVIDER_SEND_TURN_MAX_IMAGE_BYTES = 10 * 1024 * 1024;
`;
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-tool-output-"));
const file = NodePath.join(scratch, "tool-output.mjs");
writeFileSync(
  file,
  `${header}${stripTypeScriptTypes(source.slice(from, to).replace(/\bexport /g, ""))}\nexport { toolOutputImages };\n`,
);
const original = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const mcp = (mimeType, data) => ({
  type: "image",
  mimeType,
  ...(data === undefined ? {} : { data }),
});
const anthropic = (media_type, data) => ({
  type: "image",
  source: { type: "base64", media_type, data },
});
const text = { type: "text", text: "hello" };
const outputs = [
  null,
  "plain text",
  42,
  [],
  [text],
  [mcp("image/png", "AAAA")],
  [mcp("image/PNG", "AAAA")],
  [mcp("image/png")],
  [mcp("image/svg+xml", "AAAA")],
  [mcp("image/avif", "AAAA")],
  [mcp(5, "AAAA")],
  [text, mcp("image/jpeg", "AAAA"), anthropic("image/webp", "BBBB"), text],
  [anthropic("image/gif", "AAAA")],
  [{ type: "image", source: { type: "url", url: "https://x" } }],
  [{ type: "image", source: "nope" }],
  mcp("image/png", "AAAA"),
  { content: [text, mcp("image/png", "AAAA"), mcp("image/jpeg", "CCCC")] },
  { content: [mcp("image/png", "AAAA")], isError: false },
  { content: "not a list" },
  { type: "text", text: "x" },
  Array.from({ length: 12 }, () => mcp("image/png", "AAAA")),
  Array.from({ length: 12 }, (_, n) => (n % 2 ? mcp("image/png", "AAAA") : text)),
  [[mcp("image/png", "AAAA")]],
  { content: [{ type: "image", mimeType: "image/png", data: 12 }] },
];
const rows = outputs.map((output) => ({
  output,
  images: original.toolOutputImages(output).length,
}));
writeFileSync(
  new URL("../crates/client/tests/fixtures/tool-output.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`wrote ${rows.length} rows`);
