// Runs the original attachment path, id, and image-mime functions over adversarial inputs.
// Nothing here writes outside the fixture file; the filesystem-bound parts are covered by
// Rust tests that port attachmentStore.test.ts and AttachmentClaims.test.ts.
import { writeFileSync, renameSync } from "node:fs";
import * as Mime from "../../apps/server/node_modules/effect/dist/http/Mime.js";
import {
  normalizeAttachmentRelativePath,
  resolveAttachmentRelativePath,
} from "../../apps/server/src/attachmentPaths.ts";
import {
  attachmentFileExtension,
  attachmentRelativePath,
  createDeterministicAttachmentId,
  createPendingAttachmentId,
  parseAttachmentFileExtension,
  parseAttachmentUuid,
  parseThreadSegmentFromAttachmentId,
  toSafeThreadAttachmentSegment,
} from "../../apps/server/src/attachmentStore.ts";
import { inferImageExtension, parseBase64DataUrl } from "../../apps/server/src/imageMime.ts";
import { audioMimeTypeFromExtension } from "../../packages/shared/src/filePreview.ts";

const rows = [];
const add = (fn, input, expected) => rows.push({ fn, input, expected });
const dir = "/srv/state/attachments";
const uuid = "00000000-0000-4000-8000-0000000000aa";

const paths = [
  "a.png",
  "./a.png",
  "a/../a.png",
  "a/b/../../c.png",
  "../a.png",
  "a/../../a.png",
  "/a.png",
  "//a.png",
  "\\a.png",
  "\\..\\a.png",
  "/../a.png",
  "..",
  "...",
  "..a",
  ".",
  "",
  "/",
  "a/",
  "a//b.png",
  "a\\b.png",
  "a\u0000b.png",
  "x/./y",
  "./",
  "../",
  "a/..",
  "a/../b",
  "%2e%2e/a",
  "..\\a",
  "é/ü.png",
  "a b/c d.png",
];
for (const path of paths) {
  add("normalizeAttachmentRelativePath", { path }, normalizeAttachmentRelativePath(path));
  add(
    "resolveAttachmentRelativePath",
    { path },
    resolveAttachmentRelativePath({ attachmentsDir: dir, relativePath: path }),
  );
}

const threads = [
  "thread-1",
  "Thread 1",
  "  spaced  ",
  "pending",
  "PENDING",
  "pending-",
  "_pending",
  "a_b-c",
  "--x--",
  "__x__",
  "x".repeat(100),
  `${"x".repeat(79)}-y`,
  `${"x".repeat(78)}--y`,
  "日本語",
  "a/b\\c",
  "İstanbul",
  "Kelvin",
  "ǅ",
  "thread:with:colons",
  "",
  "   ",
  "---",
  "a---b",
  "Ünïcode-Thread",
  "😀",
  "a😀b",
  "﻿trim﻿",
  "t t",
];
for (const threadId of threads) {
  add("toSafeThreadAttachmentSegment", { threadId }, toSafeThreadAttachmentSegment(threadId));
  for (const key of ["message-1:0", "message-1:1", ""]) {
    add(
      "createDeterministicAttachmentId",
      { threadId, key },
      createDeterministicAttachmentId(threadId, key),
    );
  }
}
add(
  "createDeterministicAttachmentId",
  { threadId: "t", key: 'quo"te\\ \n\u0001\u007f é😀' },
  createDeterministicAttachmentId("t", 'quo"te\\ \n\u0001\u007f é😀'),
);

const names = [
  "report.PDF",
  "a.png",
  "archive.part",
  "a.PART",
  ".bashrc",
  "..",
  "a.",
  "file",
  "x.tar.gz",
  "x.verylongextension",
  "x.toolongext12",
  "x.ex-t",
  "x.é",
  "dir.d/file",
  "dir.d/",
  "a/b.TXT",
  ".hidden.md",
  "a.b.",
  ".a.",
  "   ",
  "a." + "z".repeat(10),
  "a." + "z".repeat(11),
  "a. png",
];
for (const name of names) add("attachmentFileExtension", { name }, attachmentFileExtension(name));

const ids = [
  `pending-${uuid}`,
  `pending-${uuid}-pdf`,
  `PENDING-${uuid.toUpperCase()}-PDF`,
  `thread-1-${uuid}`,
  `thread-1-${uuid}-`,
  `thread-1-${uuid}-toolongextension`,
  `a-b_c-${uuid}-bin`,
  `${uuid}`,
  `-${uuid}`,
  `x-${uuid}.png`,
  `x/${uuid}`,
  `./pending-${uuid}`,
  `x/../pending-${uuid}`,
  `pending-${uuid}/`,
  `pending-${uuid.slice(0, -1)}`,
  `pending-${uuid}-a-b`,
  "",
  "pending",
  "a\u0000b",
  `K-${uuid}`,
  `k-${uuid}-ſ`,
  `T-${uuid}`,
  `thread--1-${uuid}`,
  `_a-${uuid}`,
];
for (const id of ids)
  add(
    "parseAttachmentId",
    { id },
    {
      uuid: parseAttachmentUuid(id),
      extension: parseAttachmentFileExtension(id),
      segment: parseThreadSegmentFromAttachmentId(id),
    },
  );

for (const [type, name, mimeType] of [
  ["image", "a.png", "image/png"],
  ["image", "a.jpeg", "image/jpeg"],
  ["image", "photo", "image/webp"],
  ["image", "a.PNG", "IMAGE/PNG"],
  ["image", "a.svg", "image/x-unknown"],
  ["image", "a.txt", "image/x-unknown"],
  ["file", "r.PDF", "application/pdf"],
  ["file", "a.part", "text/plain"],
  ["file", "noext", "text/plain"],
  ["video", "v.mp4", "video/mp4"],
  ["audio", "a.wav", "audio/wav"],
  ["future", "x.y", "a/b"],
])
  add(
    "attachmentRelativePath",
    { type, id: `thread-1-${uuid}`, name, mimeType },
    attachmentRelativePath({ type, id: `thread-1-${uuid}`, name, mimeType }),
  );

const mimes = new Set([
  "image/png",
  "IMAGE/PNG",
  "image/jpeg",
  "image/jpg",
  "image/gif",
  "image/webp",
  "image/avif",
  "image/bmp",
  "image/heic",
  "image/heif",
  "image/svg+xml",
  "image/tiff",
  "image/x-icon",
  "image/vnd.microsoft.icon",
  "image/x-ms-bmp",
  "image/apng",
  "application/pdf",
  "text/plain",
  "",
  "not a mime",
  "image/png; charset=binary",
  " image/png ",
  "image/png;",
  "image/jpeg ; q=1",
  "IMAGE/SVG+XML;x",
  "image/jpg;x",
  "image/tiff;x",
  "image/gif\u00a0",
  "toString",
  "__proto__",
  "constructor",
]);
const { default: standardTypes } =
  await import("../../.repos/effect-smol/packages/effect/src/http/internal/mimeTypes.ts");
for (const type of Object.keys(standardTypes)) if (type.startsWith("image/")) mimes.add(type);
for (const mimeType of mimes)
  for (const fileName of [
    undefined,
    "a.png",
    "a.JPEG",
    "a.ico",
    "a.txt",
    "a.toolongexten",
    "a",
    " b.gif ",
    "a.İco",
  ])
    add(
      "inferImageExtension",
      { mimeType, ...(fileName === undefined ? {} : { fileName }) },
      inferImageExtension({ mimeType, ...(fileName === undefined ? {} : { fileName }) }),
    );

const payload = "aGVsbG8gd29ybGQ=";
const urls = [
  `data:image/png;base64,${payload}`,
  ` DATA:IMAGE/PNG;BASE64,${payload}\n`,
  `data:image/png;charset=utf-8;base64,${payload}`,
  `data:image/png;base64,${payload.slice(0, 8)}\r\n${payload.slice(8)}`,
  `data:image/png;base64,${payload.replace("=", "")}`,
  `data:image/png;base64,aGVsbG8gd29ybGQ==`,
  "data:image/png;base64,aG=sbG8=",
  "data:image/png;base64,=GVs",
  "data:image/png;base64,",
  "data:image/png;base64",
  "data:;base64,AAAA",
  "data:base64,AAAA",
  "data:image/png,AAAA",
  "data:image/png;base64;,AAAA",
  "data:image/png; base64 ,AAAA",
  "data:image/png;base64,AA\tAA",
  "data:image/png;base64,AAA",
  "data:image/png;base64,AAAA====",
  "data:image/png;base64,é===",
  "image/png;base64,AAAA",
  "data:;;image/png;base64,AAAA",
  "data: ;base64,AAAA",
  `data:image/png;base64,${"AAAA".repeat(100)}`,
  "data:IMAGE/PNG ; Base64 ,AAAA",
];
for (const dataUrl of urls) add("parseBase64DataUrl", { dataUrl }, parseBase64DataUrl(dataUrl));

const files = [
  "a.png",
  "a.JPG",
  "a.jpeg",
  "a.gif",
  "a.webp",
  "a.avif",
  "a.bmp",
  "a.heic",
  "a.heif",
  "a.tiff",
  "a.tif",
  "a.svg",
  "a.ico",
  "a.pdf",
  "a.html",
  "a.htm",
  "a.txt",
  "a.log",
  "a.text",
  "a.md",
  "a.markdown",
  "a.css",
  "a.csv",
  "a.js",
  "a.mjs",
  "a.json",
  "a.xml",
  "a.yaml",
  "a.yml",
  "a.zip",
  "a.gz",
  "a.mp4",
  "a.m4v",
  "a.mov",
  "a.webm",
  "a.mkv",
  "a.avi",
  "a.mpeg",
  "a.mpg",
  "a.mp3",
  "a.wav",
  "a.ogg",
  "a.oga",
  "a.opus",
  "a.flac",
  "a.aac",
  "a.m4a",
  "a.aiff",
  "a.bin",
  "a.unknown",
  "noext",
  "thread-1-x.PNG",
];
for (const path of files) {
  const t = Mime.getType(path);
  add("mimeTypeForPath", { path }, t._tag === "Some" ? t.value : "application/octet-stream");
}
for (const extension of [
  ".mp3",
  ".WAV",
  ".ogg",
  ".oga",
  ".flac",
  ".aac",
  ".m4a",
  ".opus",
  ".aiff",
  ".pdf",
  "mp3",
  "..mp3",
  ".mp-3",
  ".",
  "",
  ".é",
])
  add("audioMimeType", { extension }, audioMimeTypeFromExtension(extension));

for (const extension of [
  undefined,
  ".pdf",
  "PDF",
  ".PDF",
  ".toolongextension1",
  ".a-b",
  "",
  ".bin",
]) {
  const id = createPendingAttachmentId(extension);
  const suffix =
    extension === undefined || extension === ""
      ? ""
      : /^[a-z0-9]{1,10}$/.test(extension.replace(/^\./, "").toLowerCase())
        ? "-" + extension.replace(/^\./, "").toLowerCase()
        : "-bin";
  const shape = `^pending-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}${suffix}$`;
  if (!new RegExp(shape).test(id)) throw new Error(`pending id shape ${id}`);
  add(
    "createPendingAttachmentId",
    { ...(extension === undefined ? {} : { extension }), shape },
    id.replace(/[0-9a-f]{8}-[0-9a-f-]{27}/, "<uuid>"),
  );
}

const target = new URL("../crates/server/tests/fixtures/attachment-store.jsonl", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map(JSON.stringify).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
