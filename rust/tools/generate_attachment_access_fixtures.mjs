// Runs the original upload-URL signer, asset-URL issuer/resolver, response-header builder
// and image-header reader against a throwaway state directory, and records what they do.
// The Rust tests replay these rows with the same secret and files and require identical
// URLs, resolved assets, headers, and dimensions. The state directory is deleted afterwards.
import { mkdirSync, rmSync, writeFileSync, renameSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import { AssetAccessError } from "../../packages/contracts/src/index.ts";
import * as ServerSecretStore from "../../apps/server/src/auth/ServerSecretStore.ts";
import * as ServerConfig from "../../apps/server/src/config.ts";
import * as WorkspacePaths from "../../apps/server/src/workspace/WorkspacePaths.ts";
import * as Upload from "../../apps/server/src/assets/AttachmentUpload.ts";
import * as Access from "../../apps/server/src/assets/AssetAccess.ts";
import { assetResponseHeaders, downloadContentDisposition } from "../../apps/server/src/http.ts";
import { readImageDimensions } from "../../packages/shared/src/imageDimensions.ts";

// The original refuses to read image headers through a symlinked path (macOS /var -> /private/var).
process.env.TMPDIR = realpathSync(tmpdir());
const hex = (bytes) => Buffer.from(bytes).toString("hex");
const u16 = (n, little = false) => {
  const b = Buffer.alloc(2);
  little ? b.writeUInt16LE(n) : b.writeUInt16BE(n);
  return [...b];
};
const u32 = (n, little = false) => {
  const b = Buffer.alloc(4);
  little ? b.writeUInt32LE(n) : b.writeUInt32BE(n);
  return [...b];
};
const png = (w, h, length = 33) => {
  const b = [
    0x89,
    0x50,
    0x4e,
    0x47,
    0x0d,
    0x0a,
    0x1a,
    0x0a,
    ...u32(13),
    0x49,
    0x48,
    0x44,
    0x52,
    ...u32(w),
    ...u32(h),
  ];
  while (b.length < length) b.push(8);
  return b.slice(0, length);
};
const gif = (w, h) => [0x47, 0x49, 0x46, 0x38, 0x39, 0x61, ...u16(w, true), ...u16(h, true), 0, 0];
const riff = (chunk, body) => [
  0x52,
  0x49,
  0x46,
  0x46,
  ...u32(body.length + 4 + 4, true),
  0x57,
  0x45,
  0x42,
  0x50,
  ...[...chunk].map((c) => c.charCodeAt(0)),
  ...body,
];
const vp8 = (w, h) =>
  riff("VP8 ", [
    ...u32(10, true),
    0,
    0,
    0,
    0x9d,
    0x01,
    0x2a,
    ...u16(w, true),
    ...u16(h, true),
    0,
    0,
  ]);
const vp8l = (w, h) =>
  riff("VP8L", [...u32(10, true), 0x2f, ...u32((w - 1) | ((h - 1) << 14), true), 0, 0, 0, 0]);
const vp8x = (w, h) =>
  riff("VP8X", [
    ...u32(10, true),
    0,
    0,
    0,
    0,
    (w - 1) & 255,
    ((w - 1) >> 8) & 255,
    ((w - 1) >> 16) & 255,
    (h - 1) & 255,
    ((h - 1) >> 8) & 255,
    ((h - 1) >> 16) & 255,
  ]);
const sof = (w, h, marker = 0xc0) => [
  0xff,
  marker,
  ...u16(17),
  8,
  ...u16(h),
  ...u16(w),
  3,
  1,
  0x22,
  0,
  2,
  0x11,
  1,
  3,
  0x11,
  1,
];
const app0 = [0xff, 0xe0, ...u16(16), 0x4a, 0x46, 0x49, 0x46, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0];
const exif = (orientation, little, extraEntries = 0) => {
  const w = little ? (n) => u16(n, true) : u16,
    w32 = little ? (n) => u32(n, true) : u32;
  const entries = [];
  for (let i = 0; i < extraEntries; i++)
    entries.push(...w(0x0100 + i), ...w(3), ...w32(1), ...w(1), 0, 0);
  entries.push(...w(0x0112), ...w(3), ...w32(1), ...w(orientation), 0, 0);
  const tiff = [
    ...(little ? [0x49, 0x49, 0x2a, 0] : [0x4d, 0x4d, 0, 0x2a]),
    ...w32(8),
    ...w(extraEntries + 1),
    ...entries,
    ...w32(0),
  ];
  const payload = [0x45, 0x78, 0x69, 0x66, 0, 0, ...tiff];
  return [0xff, 0xe1, ...u16(payload.length + 2), ...payload];
};
const samples = [
  ["png 3x5", png(3, 5)],
  ["png 4096x2160", png(4096, 2160)],
  ["png zero width", png(0, 5)],
  ["png too short", png(3, 5, 23)],
  [
    "png bad ihdr",
    (() => {
      const b = png(3, 5);
      b[12] = 0x58;
      return b;
    })(),
  ],
  ["gif 7x9", gif(7, 9)],
  ["gif zero", gif(0, 9)],
  ["gif short", gif(7, 9).slice(0, 9)],
  ["webp vp8 320x200", vp8(320, 200)],
  ["webp vp8 14-bit mask", vp8(0xc140, 0xc0c8)],
  ["webp vp8l 100x50", vp8l(100, 50)],
  ["webp vp8x 1920x1080", vp8x(1920, 1080)],
  ["webp vp8x huge", vp8x(16000000, 3)],
  ["webp short", vp8(320, 200).slice(0, 29)],
  ["webp unknown chunk", riff("ABCD", [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])],
  ["jpeg sof0", [0xff, 0xd8, ...app0, ...sof(640, 480)]],
  ["jpeg sof2", [0xff, 0xd8, ...app0, ...sof(800, 600, 0xc2)]],
  ["jpeg skips dht", [0xff, 0xd8, ...app0, 0xff, 0xc4, ...u16(4), 0, 0, ...sof(33, 44)]],
  [
    "jpeg skips c8 and cc",
    [0xff, 0xd8, 0xff, 0xc8, ...u16(2), 0xff, 0xcc, ...u16(2), ...sof(21, 22)],
  ],
  ["jpeg padding bytes", [0xff, 0xd8, 0xff, 0xff, 0xff, 0xff, ...app0, ...sof(10, 20)]],
  ["jpeg restart markers", [0xff, 0xd8, 0xff, 0xd0, 0xff, 0x01, ...sof(11, 12)]],
  ["jpeg exif 6 little", [0xff, 0xd8, ...exif(6, true), ...sof(640, 480)]],
  ["jpeg exif 8 big", [0xff, 0xd8, ...exif(8, false), ...sof(640, 480)]],
  ["jpeg exif 1", [0xff, 0xd8, ...exif(1, true), ...sof(640, 480)]],
  ["jpeg exif 5 after others", [0xff, 0xd8, ...exif(5, true, 3), ...sof(300, 200)]],
  ["jpeg exif 9", [0xff, 0xd8, ...exif(9, false), ...sof(640, 480)]],
  [
    "jpeg two exif only first counts",
    [0xff, 0xd8, ...exif(1, true), ...exif(6, true), ...sof(640, 480)],
  ],
  [
    "jpeg exif truncated ifd",
    (() => {
      const e = exif(6, true);
      return [0xff, 0xd8, ...e.slice(0, e.length - 8), ...sof(640, 480)];
    })(),
  ],
  ["jpeg stops at sos", [0xff, 0xd8, ...app0, 0xff, 0xda, ...u16(4), 0, 0, ...sof(5, 5)]],
  ["jpeg stops at eoi", [0xff, 0xd8, 0xff, 0xd9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]],
  ["jpeg not marker", [0xff, 0xd8, 0x00, 0x10, 0, 0, 0, 0, 0, 0, 0, 0]],
  ["jpeg too short", [0xff, 0xd8, 0xff]],
  ["jpeg zero dims", [0xff, 0xd8, ...sof(0, 480)]],
  ["text", [...Buffer.from("not an image at all, just text")]],
  ["empty", []],
];
const dimensionRows = [];
for (const [label, bytes] of samples) {
  let dimensions;
  try {
    dimensions = readImageDimensions(Uint8Array.from(bytes));
  } catch (error) {
    console.error("skipping throwing sample", label, String(error));
    continue;
  }
  dimensionRows.push({ label, hex: hex(bytes), dimensions });
}

const layer = ServerSecretStore.layer.pipe(
  Layer.provideMerge(WorkspacePaths.layer),
  Layer.provideMerge(
    ServerConfig.layerTest(process.cwd(), { prefix: "t3-attachment-access-fixture-" }),
  ),
  Layer.provideMerge(NodeServices.layer),
);
const uuid = (n) => `00000000-0000-4000-8000-0000000000${n.toString(16).padStart(2, "0")}`;
const stage = [
  { id: `thread-1-${uuid(1)}`, ext: ".png", bytes: png(3, 5) },
  { id: `thread-1-${uuid(2)}-mp4`, ext: ".mp4", bytes: [1, 2, 3] },
  { id: `thread-1-${uuid(3)}-pdf`, ext: ".pdf", bytes: [1, 2, 3] },
  { id: `thread-1-${uuid(4)}-wav`, ext: ".wav", bytes: [1, 2, 3] },
  { id: `thread-1-${uuid(5)}-zip`, ext: ".zip", bytes: [1, 2, 3] },
  { id: `thread-1-${uuid(6)}-html`, ext: ".html", bytes: [...Buffer.from("<b>hi</b>")] },
  { id: `thread-1-${uuid(7)}-txt`, ext: ".txt", bytes: [...Buffer.from("plain")] },
  { id: `thread-1-${uuid(8)}`, ext: ".jpg", bytes: [0xff, 0xd8, ...app0, ...sof(640, 480)] },
  { id: `thread-1-${uuid(9)}`, ext: ".bin", bytes: [9, 9, 9] },
  { id: `pending-${uuid(10)}`, ext: ".png", bytes: png(1, 1) },
];
const att = (n, extra = {}) => ({ _tag: "attachment", attachmentId: stage[n].id, ...extra });
const resources = [
  att(0),
  att(0, { fileName: "sc reen.png", mimeType: "image/png" }),
  att(0, { fileName: 'we"ird\\nåme😀 (1)*.png', mimeType: "image/png" }),
  att(0, { disposition: "inline" }),
  att(1, { fileName: "clip.mp4", mimeType: 'video/mp4; codecs="avc1"' }),
  att(1),
  att(1, { mimeType: "video/" }),
  att(2, { fileName: "Report é.pdf", mimeType: "application/pdf" }),
  att(2, { fileName: "r.pdf", mimeType: "application/pdf", disposition: "inline" }),
  att(2, { disposition: "attachment" }),
  att(2),
  att(3, { fileName: "a.wav", mimeType: "audio/wav", disposition: "inline" }),
  att(3, { fileName: "a.wav", mimeType: "audio/wav", disposition: "attachment" }),
  att(4, { fileName: "a.zip", mimeType: "application/zip", disposition: "inline" }),
  att(5, { fileName: "p.html", mimeType: "text/html", disposition: "inline" }),
  att(5, { fileName: "p.html", mimeType: "text/html" }),
  att(6),
  att(6, { mimeType: "text/plain", fileName: "n.txt" }),
  att(7, { fileName: "photo.jpg", mimeType: "image/jpeg" }),
  att(8, { fileName: "blob", mimeType: "application/octet-stream" }),
  att(9),
  { _tag: "attachment", attachmentId: `thread-1-${uuid(99)}` },
  { _tag: "attachment", attachmentId: "../../etc/passwd" },
  { _tag: "attachment", attachmentId: `x/../${stage[0].id}` },
  { _tag: "attachment", attachmentId: "not-an-id" },
];
const encodeError = Schema.encodeSync(AssetAccessError);
const result = await Effect.runPromise(
  Effect.gen(function* () {
    const config = yield* ServerConfig.ServerConfig;
    const secretStore = yield* ServerSecretStore.ServerSecretStore;
    mkdirSync(config.attachmentsDir, { recursive: true });
    for (const item of stage)
      writeFileSync(join(config.attachmentsDir, item.id + item.ext), Uint8Array.from(item.bytes));
    const secret = yield* secretStore.getOrCreateRandom("asset-access-signing-key", 32);
    const uploads = [];
    for (const input of [
      { name: "screenshot.png", mimeType: "image/png", sizeBytes: 6 },
      { type: "image", name: "a b.PNG", mimeType: "image/jpeg", sizeBytes: 1048576 },
      { type: "file", name: "report.PDF", mimeType: "application/pdf", sizeBytes: 6 },
      { type: "file", name: "archive.part", mimeType: "application/octet-stream", sizeBytes: 1 },
      { type: "file", name: "noext", mimeType: "text/plain", sizeBytes: 52428800 },
      { type: "file", name: 'we"ird é😀.tar.gz', mimeType: "application/gzip", sizeBytes: 99 },
    ]) {
      const issued = yield* Upload.issueAttachmentUploadUrl(input);
      const token = issued.relativeUrl.slice(`${Upload.ATTACHMENT_UPLOAD_ROUTE_PREFIX}/`.length);
      const claims = yield* Upload.validateAttachmentUploadToken(token);
      uploads.push({
        input,
        issued,
        claims,
        payload: Buffer.from(token.split(".")[0], "base64url").toString("utf8"),
      });
    }
    const assets = [];
    for (const resource of resources) {
      const issued = yield* Effect.exit(Access.issueAssetUrl({ resource }));
      if (issued._tag !== "Success") {
        const error =
          issued.cause?.reasons?.[0]?.error ??
          issued.cause?.failures?.[0]?.error ??
          issued.cause?.error;
        assets.push({ resource, error: JSON.parse(JSON.stringify(encodeError(error))) });
        continue;
      }
      const url = issued.value;
      const suffix = url.relativeUrl.slice(`${Access.ASSET_ROUTE_PREFIX}/`.length);
      const slash = suffix.indexOf("/");
      const resolved = yield* Access.resolveAsset(suffix.slice(0, slash), suffix.slice(slash + 1));
      assets.push({
        resource,
        issued: url,
        resolved:
          resolved === null
            ? null
            : {
                kind: resolved.kind,
                file: resolved.path.slice(config.attachmentsDir.length + 1),
                ...(resolved.download ? { download: true } : {}),
                ...(resolved.fileName !== undefined ? { fileName: resolved.fileName } : {}),
                ...(resolved.mimeType !== undefined ? { mimeType: resolved.mimeType } : {}),
              },
      });
    }
    return {
      config: { state: dirname(config.attachmentsDir) },
      secret: Buffer.from(secret).toString("hex"),
      uploads,
      assets,
    };
  }).pipe(Effect.provide(layer)),
);
rmSync(result.config.state, { recursive: true, force: true });

const names = [
  "report.pdf",
  "Report é.pdf",
  'we"ird\\name.pdf',
  "a\u0000b\u007f\u0085c.txt",
  "😀.png",
  "o'neil (1)*.txt",
  "",
  "a/b.txt",
  "lone\ufffd.txt",
  " spaced name .txt",
  "üñí.pdf",
];
const dispositions = names.map((fileName) => ({
  fileName,
  value: downloadContentDisposition(fileName),
}));
dispositions.push({ fileName: null, value: downloadContentDisposition(undefined) });
const headerCases = [];
for (const path of [
  "/x/a.png",
  "/x/a.svg",
  "/x/a.SVG",
  "/x/a.html",
  "/x/a.HTM",
  "/x/a.pdf",
  "/x/a.bin",
])
  for (const options of [
    undefined,
    {},
    { download: true },
    { download: true, fileName: "n é.pdf" },
    { download: true, fileName: "n.pdf", mimeType: "application/pdf" },
    { download: true, mimeType: "text/html" },
    { download: true, mimeType: "image/svg+xml" },
    { download: true, mimeType: "text/xml" },
    { download: true, mimeType: "application/xhtml+xml" },
    { download: true, mimeType: "not a mime" },
    { download: true, mimeType: "application/vnd.ms-excel" },
    { mimeType: "video/mp4" },
    { mimeType: 'video/mp4; codecs="avc1"' },
    { mimeType: "audio/wav" },
    { mimeType: "audio/wav;x=y" },
    { mimeType: "text/html" },
    { mimeType: "TEXT/HTML" },
    { mimeType: "application/pdf" },
    { mimeType: "application/PDF" },
    { mimeType: "image/png" },
    { mimeType: "video/" },
    { mimeType: "video/a b" },
    { mimeType: "text/plain" },
    { fileName: "x" },
  ])
    headerCases.push({
      path,
      options: options ?? null,
      headers: assetResponseHeaders(path, options),
    });

const target = new URL("../crates/server/tests/fixtures/attachment-access.json", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(
  temporary,
  JSON.stringify(
    {
      secret: result.secret,
      stage: stage.map(({ id, ext, bytes }) => ({ file: id + ext, hex: hex(bytes) })),
      uploads: result.uploads,
      assets: result.assets,
      dispositions,
      headerCases,
    },
    null,
    1,
  ),
);
renameSync(temporary, target);
const dimTarget = new URL(
    "../crates/server/tests/fixtures/image-dimensions.jsonl",
    import.meta.url,
  ),
  dimTemp = new URL(dimTarget.href + ".tmp");
writeFileSync(dimTemp, dimensionRows.map(JSON.stringify).join("\n") + "\n");
renameSync(dimTemp, dimTarget);
console.log(
  JSON.stringify({
    dimensions: dimensionRows.length,
    uploads: result.uploads.length,
    assets: result.assets.length,
    headers: headerCases.length,
  }),
);
