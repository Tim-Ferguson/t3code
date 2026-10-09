// Development oracle for the workspace-backed `assets.createUrl` kinds (workspace-file,
// draft-workspace-file, media-file, project-favicon, github-media, native-app-icon). Builds a
// throwaway tree (a workspace, a sibling directory outside it, symlinks in and out), runs the
// original `issueAssetUrl` and `resolveAsset` over it, and records the signed claims, the wire
// errors and what each URL resolves to. Absolute paths are written as `{root}` (the workspace)
// and `{outside}` (its sibling), so the Rust test rebuilds the same tree anywhere and compares.
// Run: node rust/tools/generate_asset_workspace_fixtures.mjs
import {
  mkdirSync,
  mkdtempSync,
  realpathSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import { AssetAccessError } from "../../packages/contracts/src/index.ts";
import * as ServerSecretStore from "../../apps/server/src/auth/ServerSecretStore.ts";
import * as ServerConfig from "../../apps/server/src/config.ts";
import * as WorkspacePaths from "../../apps/server/src/workspace/WorkspacePaths.ts";
import * as Access from "../../apps/server/src/assets/AssetAccess.ts";
import * as ProjectFaviconResolver from "../../apps/server/src/project/ProjectFaviconResolver.ts";
import * as T3ProjectFileLoader from "../../apps/server/src/project/T3ProjectFileLoader.ts";
import { githubMediaFetchUrl, githubMediaFileName } from "../../packages/shared/src/githubMedia.ts";
import { hostPreviewMimeTypeFromExtension } from "../../packages/shared/src/filePreview.ts";

// The original reads image headers only through canonical paths (macOS /var -> /private/var).
const base = realpathSync(mkdtempSync(join(realpathSync(tmpdir()), "t3-asset-workspace-")));
process.env.TMPDIR = base;
const root = join(base, "work");
const outside = join(base, "outside");
const favicons = join(base, "icons");
const png = Uint8Array.from([
  0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 3,
  0, 0, 0, 5, 8, 2, 0, 0, 0,
]);
const write = (path, bytes = "x") => {
  mkdirSync(join(path, ".."), { recursive: true });
  writeFileSync(path, bytes);
};
for (const [path, bytes] of [
  [join(root, "index.html"), "<b>hi</b>"],
  [join(root, "docs/guide.html"), "<b>guide</b>"],
  [join(root, "docs/style.css"), "body{}"],
  [join(root, "docs/app.js"), "1"],
  [join(root, "docs/.hidden.css"), "x"],
  [join(root, "docs/notes.txt"), "plain"],
  [join(root, "docs/data.json"), "{}"],
  [join(root, "docs/img/logo.png"), png],
  [join(root, "docs/font.woff2"), "f"],
  [join(root, "shot.png"), png],
  [join(root, "page.pdf"), "%PDF"],
  [join(root, "vector.svg"), "<svg/>"],
  [join(root, "sub/inner.html"), "i"],
  [join(root, "sub/deep/x.css"), "x"],
  [join(root, "..double.html"), "d"],
  [join(outside, "secret.png"), png],
  [join(outside, "page.html"), "<b>out</b>"],
  [join(outside, "clip.mp4"), "mp4"],
  [join(outside, "song.wav"), "wav"],
  [join(outside, "notes.txt"), "no"],
  [join(outside, "doc.pdf"), "%PDF"],
  [join(favicons, "public/favicon.svg"), "<svg/>"],
  [join(favicons, "assets/brand.png"), png],
  [join(favicons, "weird/icon.txt"), "t"],
  [join(favicons, "t3proj/t3.json"), JSON.stringify({ iconPath: "brand/mark.png" })],
  [join(favicons, "t3proj/brand/mark.png"), png],
  [join(favicons, "linked/placeholder"), "p"],
  [join(favicons, "srcfile/index.html"), '<link rel="icon" href="/img/logo.svg">'],
  [join(favicons, "srcfile/public/img/logo.svg"), "<svg/>"],
  [join(favicons, "badsrc/index.html"), '<link rel="icon" href="../../../etc/passwd">'],
  [join(favicons, "external/ext.png"), png],
])
  write(path, bytes);
symlinkSync(join(outside, "secret.png"), join(root, "link-out.png"));
symlinkSync(join(root, "docs/guide.html"), join(root, "link-in.html"));
symlinkSync(outside, join(root, "dir-link"));
symlinkSync(join(outside, "page.html"), join(root, "docs/escape.html"));
symlinkSync(join(outside, "secret.png"), join(favicons, "linked/favicon.png"));
symlinkSync(root, join(base, "root-link"));

const hide = (value) =>
  JSON.parse(
    JSON.stringify(value)
      .split(root)
      .join("{root}")
      .split(outside)
      .join("{outside}")
      .split(favicons)
      .join("{icons}")
      .split(base)
      .join("{base}"),
  );
const encodeError = Schema.encodeSync(AssetAccessError);
const layer = ProjectFaviconResolver.layer.pipe(
  Layer.provideMerge(T3ProjectFileLoader.layer),
  Layer.provideMerge(ServerSecretStore.layer),
  Layer.provideMerge(WorkspacePaths.layer),
  Layer.provideMerge(
    ServerConfig.layerTest(process.cwd(), { prefix: "t3-asset-workspace-fixture-" }),
  ),
  Layer.provideMerge(NodeServices.layer),
);

const sub = (text) =>
  text
    .split("{base}")
    .join(base)
    .split("{root}")
    .join(root)
    .split("{outside}")
    .join(outside)
    .split("{icons}")
    .join(favicons);
const resourceCases = [
  // workspace-file
  ...[
    "index.html",
    "docs/guide.html",
    "page.pdf",
    "shot.png",
    "docs/img/logo.png",
    "vector.svg",
    "sub/inner.html",
    "docs/notes.txt",
    "docs",
    "missing.html",
    "../outside/page.html",
    "./index.html",
    "docs/../index.html",
    "  index.html  ",
    "link-out.png",
    "link-in.html",
    "dir-link/page.html",
    "docs/escape.html",
    "{root}/index.html",
    "{outside}/page.html",
    "..double.html",
  ].map((path) => ({
    resource: { _tag: "workspace-file", threadId: "thread-1", path },
    workspaceRoot: "{root}",
  })),
  {
    resource: { _tag: "workspace-file", threadId: "thread-1", path: "index.html" },
    workspaceRoot: "{base}/missing-root",
  },
  {
    resource: { _tag: "workspace-file", threadId: "thread-1", path: "index.html" },
    workspaceRoot: "{root}/index.html",
  },
  {
    resource: { _tag: "workspace-file", threadId: "thread-1", path: "index.html" },
    workspaceRoot: "{base}/root-link",
  },
  { resource: { _tag: "workspace-file", threadId: "thread-1", path: "index.html" } },
  // draft-workspace-file (relative resolves in its cwd, absolute serves like media)
  ...[
    "index.html",
    "docs/guide.html",
    "shot.png",
    "docs/notes.txt",
    "../outside/page.html",
    "{outside}/secret.png",
    "{outside}/notes.txt",
    "{outside}/missing.png",
    "{root}/page.pdf",
  ].map((path) => ({
    resource: { _tag: "draft-workspace-file", cwd: "{root}", path },
  })),
  { resource: { _tag: "draft-workspace-file", cwd: "{base}/missing-root", path: "index.html" } },
  {
    resource: { _tag: "draft-workspace-file", cwd: "{base}/missing-root", path: "index.html" },
    workspaceRoot: "{root}",
  },
  // media-file
  ...[
    "{outside}/clip.mp4",
    "{outside}/secret.png",
    "{outside}/page.html",
    "{outside}/doc.pdf",
    "{outside}/song.wav",
    "{outside}/notes.txt",
    "{outside}/missing.png",
    "{root}/link-out.png",
    "{root}/shot.png",
    "{root}/dir-link/secret.png",
    "{outside}",
    "{root}/vector.svg",
    "{root}/docs/../shot.png",
  ].map((path) => ({
    resource: { _tag: "media-file", threadId: "thread-1", path },
  })),
  ...["shot.png", "docs/img/logo.png", "../outside/clip.mp4", "docs/notes.txt", "missing.png"].map(
    (path) => ({
      resource: { _tag: "media-file", threadId: "thread-1", path },
      workspaceRoot: "{root}",
    }),
  ),
  { resource: { _tag: "media-file", threadId: "thread-1", path: "shot.png" } },
  {
    resource: { _tag: "media-file", threadId: "thread-1", path: "shot.png" },
    workspaceRoot: "{base}/missing-root",
  },
  // project-favicon
  { resource: { _tag: "project-favicon", cwd: "{icons}/public" }, label: "favicon-in-public-dir" },
  { resource: { _tag: "project-favicon", cwd: "{icons}" } },
  { resource: { _tag: "project-favicon", cwd: "{icons}/t3proj" } },
  { resource: { _tag: "project-favicon", cwd: "{icons}/srcfile" } },
  { resource: { _tag: "project-favicon", cwd: "{icons}/badsrc" } },
  { resource: { _tag: "project-favicon", cwd: "{icons}/linked" } },
  { resource: { _tag: "project-favicon", cwd: "{icons}/weird" }, projectFaviconPath: "icon.txt" },
  { resource: { _tag: "project-favicon", cwd: "{icons}" }, projectFaviconPath: "assets/brand.png" },
  { resource: { _tag: "project-favicon", cwd: "{icons}" }, projectFaviconPath: "missing.png" },
  {
    resource: { _tag: "project-favicon", cwd: "{icons}" },
    projectFaviconPath: "{icons}/external/ext.png",
  },
  {
    resource: { _tag: "project-favicon", cwd: "{icons}" },
    projectFaviconPath: "{icons}/external/./ext.png",
  },
  {
    resource: { _tag: "project-favicon", cwd: "{icons}" },
    projectFaviconPath: "{icons}/external/missing.png",
  },
  { resource: { _tag: "project-favicon", cwd: "{icons}" }, projectCheckoutPending: true },
  { resource: { _tag: "project-favicon", cwd: "{base}/missing-root" } },
  // native-app-icon, github-media
  { resource: { _tag: "native-app-icon", app: { _tag: "app-id", appId: "com.apple.Safari" } } },
  {
    resource: {
      _tag: "native-app-icon",
      app: { _tag: "display-name", displayName: "Visual Studio Code" },
    },
  },
  ...[
    "https://github.com/user-attachments/assets/1a2b-3c4d",
    "https://www.github.com/user-attachments/assets/abc_DEF-1?x=1#frag",
    "https://github.com/acme/web/assets/123/0f0f-aa",
    "https://github.com/acme/web/blob/main/docs/shot%20one.png?raw=true",
    "https://github.com/acme/web/raw/refs/heads/main/a/b.mp4",
    "https://raw.githubusercontent.com/acme/web/main/shot.png",
    "https://RAW.githubusercontent.com:443/acme/web/main/shot.png?token=1",
    "https://media.githubusercontent.com/media/acme/web/main/big.gif",
    "https://raw.githubusercontent.com/acme/web/main/bad%zzname.png",
    "https://raw.githubusercontent.com/acme/web/main/",
    "http://github.com/user-attachments/assets/abc",
    "https://objects.githubusercontent.com/github-production/x",
    "https://github.com/acme/web/issues/1",
    "https://github.com.evil.test/user-attachments/assets/abc",
    "https://user:pw@github.com/user-attachments/assets/abc",
    "not a url",
  ].map((url) => ({ resource: { _tag: "github-media", cwd: "{root}", url } })),
];

const resolveVariants = (fileName) => [
  fileName,
  "other.html",
  "../index.html",
  "..%2Findex.html",
  "docs/style.css",
  "style.css",
  "app.js",
  ".hidden.css",
  "data.json",
  "notes.txt",
  "font.woff2",
  "x%00.css",
  "img/logo.png",
  "%2e%2e/index.html",
  "docs%2Fstyle.css",
  "deep/x.css",
  "inner.html",
  "escape.html",
  "%E0%A4%A",
  "",
  "..double.html",
  "guide.html",
  "a/../style.css",
  "dir-link/page.html",
  "link-out.png",
];

const rows = await Effect.runPromise(
  Effect.gen(function* () {
    const out = [];
    for (const item of resourceCases) {
      const resource = JSON.parse(sub(JSON.stringify(item.resource)));
      const input = { resource };
      if (item.workspaceRoot !== undefined) input.workspaceRoot = sub(item.workspaceRoot);
      if (item.projectFaviconPath !== undefined)
        input.projectFaviconPath = sub(item.projectFaviconPath);
      if (item.projectCheckoutPending) input.projectCheckoutPending = true;
      const now = 1_791_000_000_000;
      const exit = yield* Effect.exit(Access.issueAssetUrl(input));
      const row = {
        resource: item.resource,
        ...(item.workspaceRoot !== undefined ? { workspaceRoot: item.workspaceRoot } : {}),
        ...(item.projectFaviconPath !== undefined
          ? { projectFaviconPath: item.projectFaviconPath }
          : {}),
        ...(item.projectCheckoutPending ? { projectCheckoutPending: true } : {}),
      };
      if (exit._tag !== "Success") {
        const error =
          exit.cause?.reasons?.[0]?.error ?? exit.cause?.failures?.[0]?.error ?? exit.cause?.error;
        row.error = hide(encodeError(error));
        out.push(row);
        continue;
      }
      const issued = exit.value;
      const suffix = issued.relativeUrl.slice(`${Access.ASSET_ROUTE_PREFIX}/`.length);
      const slash = suffix.indexOf("/");
      const token = suffix.slice(0, slash);
      const claims = JSON.parse(Buffer.from(token.split(".")[0], "base64url").toString("utf8"));
      row.issued = {
        claims: hide(claims),
        nameSegment: decodeURIComponent(suffix.slice(slash + 1)),
        sourcePath: issued.sourcePath === undefined ? null : hide(issued.sourcePath),
        imageDimensions: issued.imageDimensions ?? null,
        // Only its distance from now matters: a favicon is bucketed, anything else lasts an hour.
        expiresInMs: claims.expiresAt - Date.now() > 3_000_000 ? "long" : "unknown",
      };
      row.resolved = [];
      // Resolving an app icon runs macOS tools; only its signed claims are recorded.
      if (resource._tag === "native-app-icon") {
        out.push(row);
        continue;
      }
      for (const variant of resolveVariants(decodeURIComponent(suffix.slice(slash + 1)))) {
        const result = yield* Effect.scoped(
          Access.resolveAsset(token, variant).pipe(
            Effect.map((resolved) =>
              resolved === null
                ? null
                : {
                    kind: resolved.kind,
                    path: hide(resolved.path ?? null),
                    mimeType: resolved.mimeType ?? null,
                    hasFile: resolved.file !== undefined,
                  },
            ),
          ),
        );
        row.resolved.push({ variant, result });
      }
      // Resolve under the exact name the URL carries, and with another name appended.
      out.push(row);
    }
    return out;
  }).pipe(Effect.provide(layer)),
);

const urls = [
  "https://github.com/user-attachments/assets/1a2b-3c4d",
  "https://RAW.githubusercontent.com:443/acme/web/main/shot%20one.png?token=1",
  "https://raw.githubusercontent.com/acme/web/main/bad%zzname.png",
  "https://github.com/acme/web/blob/main/x.png?raw=true",
  "https://github.com/acme/web/blob/main/",
  "https://github.com/acme/web/assets/9/abc-1",
  "https://example.com/x.png",
  "https://media.githubusercontent.com/media/a/b/main/\u0001ctl\u007f.gif",
].map((url) => {
  const fetchUrl = githubMediaFetchUrl(url);
  return { url, fetchUrl, fileName: fetchUrl === null ? null : githubMediaFileName(fetchUrl) };
});
const mimes = [
  "",
  ".",
  ".png",
  ".PNG",
  ".mp4",
  ".MKV",
  ".wav",
  ".pdf",
  ".html",
  ".htm",
  ".svg",
  ".txt",
  ".css",
  ".avif",
  ".ico",
  ".m4a",
  ".opus",
  ".a-b",
  ".p ng",
  ".mov",
  ".ogv",
  ".webm",
  ".avi",
  ".m4v",
].map((extension) => ({ extension, mime: hostPreviewMimeTypeFromExtension(extension) }));

const target = new URL("../crates/server/tests/fixtures/asset-workspace.json", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(
  temporary,
  JSON.stringify({ rows, githubMedia: urls, hostPreviewMimes: mimes }, null, 1),
);
renameSync(temporary, target);
rmSync(base, { recursive: true, force: true });
console.log(
  JSON.stringify({
    rows: rows.length,
    errors: rows.filter((row) => row.error).length,
    github: urls.length,
  }),
);
