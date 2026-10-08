// Source oracle for Open VSX parsing/archives; requires the same Node23/V8 color pin.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes, createRequire } from "node:module";
import { gzipSync } from "node:zlib";
import { createVsCodeOracle } from "./generate_vscode_theme_fixtures.mjs";
const require = createRequire(new URL("../../apps/web/package.json", import.meta.url));
const JSZip = require("jszip"),
  { parse } = require("jsonc-parser"),
  { sha256 } = require("@noble/hashes/sha2");
if (process.version !== "v23.11.0" || process.versions.v8 !== "12.9.202.28-node.14")
  throw Error("Regenerate with pinned Node23.11.0/V8 12.9.202.28-node.14");
const source = stripTypeScriptTypes(
  readFileSync(new URL("../../apps/web/src/openVsxThemes.ts", import.meta.url), "utf8"),
)
  .replace(/^import[\s\S]*?from\s+"[^"\n]+";\s*/gm, "")
  .replace(/^export /gm, "");
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const oracle = (fetch) =>
  new AsyncFunction(
    "deps",
    Object.keys({ ...createVsCodeOracle(), sha256, JSZip, parse, fetch })
      .map((key) => `const ${key}=deps.${key};`)
      .join("\n") +
      "\n" +
      source +
      "\nreturn {extensionFromDetail,normalizePackagePath,parseJsoncObject,sanitizeThemeObject,inspectZipDirectory,openVsxThemeId,openVsxCollectionId,importOpenVsxThemeExtension,searchOpenVsxThemes};",
  )({ ...createVsCodeOracle(), sha256, JSZip, parse, fetch });
const api = await oracle(() => {
    throw Error("unexpected network");
  }),
  rows = [];
const add = (kind, input, run) => {
  let expected = null,
    error = null;
  try {
    expected = run();
  } catch (cause) {
    error = cause.message;
  }
  rows.push({ kind, input, expected, error });
};
const root = "https://open-vsx.org/api/demo/theme/1.0.0/file";
const detail = {
  namespace: "demo",
  name: "theme",
  displayName: "Demo Theme",
  version: "1.0.0",
  license: "MIT",
  description: "A nice theme",
  downloadCount: 123456,
  repository: "https://github.com/demo/theme",
  files: {
    icon: root + "/icon.png",
    manifest: root + "/package.json",
    sha256: root + "/theme.sha256",
    download: root + "/theme.vsix",
  },
};
for (const input of [
  null,
  0,
  [],
  {},
  detail,
  ...[
    "namespace",
    "name",
    "version",
    "displayName",
    "license",
    "downloadCount",
    "repository",
    "homepage",
    "url",
  ].flatMap((key) =>
    [
      null,
      0,
      [],
      {},
      "",
      true,
      " MIT ",
      "mit",
      "MPL-2.0",
      "\ufeffdemo\ufeff",
      "\u0085demo\u0085",
      "https://name:pass@example.com",
      "https://github.com/demo/theme",
      Infinity,
      -42,
    ].map((value) => ({ ...detail, [key]: value })),
  ),
  ...["manifest", "sha256", "download", "icon"].flatMap((key) =>
    [
      null,
      {},
      "https://OPEN-VSX.ORG:443/a",
      "http://open-vsx.org/a",
      "https://user:pass@open-vsx.org/a",
      "https://open-vsx.org.evil/a",
      "https://open-vsx.org:1234/a",
      "https://open-vsx.org/a#fragment",
    ].map((value) => ({ ...detail, files: { ...detail.files, [key]: value } })),
  ),
])
  add("detail", input, () => api.extensionFromDetail(input));
for (const path of [
  "",
  ".",
  "./theme.json",
  "themes/a.json",
  "../theme",
  "../../outside",
  "extension/theme",
  "extension/../theme",
  "\\theme",
  "C:theme",
  "c:\\foo",
  "1:foo",
  "//path",
  "/path",
  "a\0b",
  "themes/./x",
  "themes/../x",
  "themes\\x",
  "😀".repeat(513),
  "a".repeat(1025),
  "\ufefffile",
  "\u0085file",
])
  for (const relative of ["extension/", "extension/themes/a.json", "elsewhere/a.json", "extension"])
    add("path", { path, relative }, () => api.normalizePackagePath(path, relative));
for (const text of [
  "{}",
  "[]",
  "null",
  "1",
  "true",
  '{"a":1,}',
  '{"a":[1,2,],}',
  "// comment\n{}",
  "/* comment */{}",
  '{"x":"/* hi */"}',
  '{"a":1 // trailing\n}',
  '{/*comment*/"a":1}',
  "{/*unterminated",
  '{"a":1,,}',
  '{"a":1} extra',
  '{"x":01}',
  '{"x":1e400}',
  '{"x":-1e400}',
  '{"x":1e+}',
  '{"x":NaN}',
  '{"x":Infinity}',
  "\ufeff{}",
  '{\u00a0"a":1}',
  '{\u0085"a":1}',
  '{"x":"\\u0000"}',
  '{"__proto__":{"yes":true}}',
])
  add("jsonc", text, () => api.parseJsoncObject(text, "Witness"));
for (const text of [
  '{"nested":{"__proto__":{"yes":true},"constructor":3,"prototype":4}}',
  '{"array":[{"__proto__":{"yes":true},"x":1}]}',
  '{"__proto__":null,"__proto__":{"yes":true},"constructor":1,"prototype":2}',
  '{"__proto__":{"__proto__":null},"__proto__":{"yes":true}}',
  '{"__proto__":42,"__proto__":{"yes":true}}',
  '{"__proto__":{"__proto__":null,"__proto__":5},"__proto__":{"yes":true}}',
  '{"__proto__":[1,2],"__proto__":{"yes":true}}',
  '{"__proto__":1e400,"constructor":{"prototype":{"x":true}}}',
  '{"x":{"__proto__":null,"__proto__":"own"},"__proto__":{"x":3}}',
])
  add("jsonc", text, () => api.parseJsoncObject(text, "Witness"));
for (const key of [
  "editor.background",
  "input.background",
  "input.border",
  "terminal.foreground",
  "unused",
])
  for (const value of [
    "#123",
    "#" + "a".repeat(127),
    "x".repeat(129),
    "😀".repeat(64),
    "😀".repeat(65),
    null,
    42,
  ])
    add(
      "sanitize",
      { colors: { [key]: value }, include: "./base.json", tokenColors: [{ scope: "any" }] },
      () =>
        api.sanitizeThemeObject({
          colors: { [key]: value },
          include: "./base.json",
          tokenColors: [{ scope: "any" }],
        }),
    );
for (const id of [
  "demo.theme",
  "DEMO.THEME",
  "-invalid",
  "éxtension.name",
  "A".repeat(150),
  "ΣΟΣ.theme",
  "😀.theme",
]) {
  add("collectionId", id, () => api.openVsxCollectionId(id));
  for (const path of ["extension/theme.json", "theme\0path", "Renamed Light"])
    add("themeId", { id, path }, () => api.openVsxThemeId(id, path));
}
const baselineManifest = {
  publisher: "demo",
  name: "theme",
  version: "1.0.0",
  license: "MIT",
  contributes: {
    themes: [
      { label: "Demo Dark", uiTheme: "vs-dark", path: "./themes/dark.json" },
      { label: "Demo Light", uiTheme: "vs", path: "./themes/light.json" },
      { label: "Demo", uiTheme: "vs-dark", path: "./themes/solo.json" },
    ],
  },
};
const baseFiles = {
  "extension/themes/base.jsonc":
    '{/* inherited */"colors":{"editor.foreground":"#eeeeee","focusBorder":"#8b5cf6",},}',
  "extension/themes/dark.json":
    '{"include":"./base.jsonc","colors":{"editor.background":"#111111"}}',
  "extension/themes/light.json":
    '{"colors":{"editor.background":"#fafafa","editor.foreground":"#222222","focusBorder":"#8b5cf6"}}',
  "extension/themes/solo.json":
    '{"colors":{"editor.background":"#181818","editor.foreground":"#eeeeee"}}',
};
const pack = async (
  manifest,
  files = baseFiles,
  compression = "DEFLATE",
  comment = "",
  junk = 0,
) => {
  const zip = new JSZip();
  for (const [path, text] of Object.entries(files))
    zip.file(path, text, { date: new Date("2020-01-01T00:00:00Z") });
  for (let i = 0; i < junk; i++)
    zip.file(`extension/node_modules/package-${i}.js`, "", {
      date: new Date("2020-01-01T00:00:00Z"),
    });
  zip.file(
    "extension/package.json",
    typeof manifest === "string" ? manifest : JSON.stringify(manifest),
    { date: new Date("2020-01-01T00:00:00Z") },
  );
  return zip.generateAsync({ type: "uint8array", compression, comment });
};
const extension = api.extensionFromDetail(detail),
  publicManifest = JSON.stringify({ contributes: { themes: [{ path: "./public-only.json" }] } });
const hex = (bytes) => Buffer.from(bytes).toString("hex");
const directory = (bytes) =>
  add("directory", hex(bytes), () => api.inspectZipDirectory(bytes).length);
const packageCase = async (manifest, files = baseFiles, options = {}) => {
  let bytes = await pack(
    manifest,
    files,
    options.compression ?? "DEFLATE",
    options.comment ?? "",
    options.junk ?? 0,
  );
  if (options.mutate) bytes = options.mutate(bytes);
  const checksum = options.checksum ?? hex(sha256(bytes));
  const input = {
    extension: options.extension ?? extension,
    manifest: options.publicManifest ?? publicManifest,
    bytes: hex(bytes),
    checksum,
  };
  const test = await oracle((url) => {
    if (String(url) === input.extension.manifestUrl)
      return Promise.resolve(new Response(input.manifest));
    if (String(url) === input.extension.sha256Url) return Promise.resolve(new Response(checksum));
    return Promise.resolve(new Response(bytes));
  });
  let expected = null,
    error = null;
  try {
    expected = await test.importOpenVsxThemeExtension(input.extension);
  } catch (cause) {
    error = cause.message;
  }
  rows.push({ kind: "package", input, expected, error });
  return bytes;
};
const zip = await packageCase(baselineManifest);
directory(zip);
await packageCase(baselineManifest, baseFiles, {
  compression: "STORE",
  comment: "PK\x05\x06" + "x".repeat(26),
  junk: 3000,
});
for (const field of ["publisher", "name", "version", "license"])
  for (const value of [null, 42, "other", "DEMO", " mit "])
    await packageCase({ ...baselineManifest, [field]: value });
for (const themes of [
  [],
  [null, 42],
  Array.from({ length: 41 }, () => baselineManifest.contributes.themes[0]),
  [{ label: "Missing" }],
  [{ path: "./themes/missing.json" }],
  [{ path: "../../outside.json" }],
  [{ path: "C:outside.json" }],
  [{ path: "./themes/dark.json", uiTheme: "unknown", label: "Renamed" }],
  [
    { path: "./themes/dark.json", uiTheme: "vs-dark", label: "Renamed Dark" },
    { path: "./themes/light.json", uiTheme: "vs", label: "Renamed Light" },
  ],
  [
    ...baselineManifest.contributes.themes,
    { path: "./themes/solo.json", uiTheme: "vs-dark", label: "Duplicate Solo" },
  ],
])
  await packageCase({ ...baselineManifest, contributes: { themes } });
for (const value of [
  '{"include":"./dark.json","colors":{"editor.background":"#111"}}',
  '{"include":"../themes/dark.json","colors":{"editor.background":"#111"}}',
  '{"colors":{"input.background":"#111"}}',
  "{/*unterminated",
  '{"colors":{"editor.background":"#111"},"tokenColors":[{"scope":"x"}]}',
])
  await packageCase(
    { ...baselineManifest, contributes: { themes: [baselineManifest.contributes.themes[0]] } },
    { ...baseFiles, "extension/themes/dark.json": value },
  );
await packageCase(
  JSON.stringify({ __proto__: null, ...baselineManifest }).replace(
    '"publisher":"demo"',
    '"__proto__":{"publisher":"demo"}',
  ),
);
await packageCase('{"__proto__":' + JSON.stringify(baselineManifest) + "}");
await packageCase(
  { ...baselineManifest, contributes: { themes: [{ path: "./themes/dark.json" }] } },
  {
    ...baseFiles,
    "extension/themes/dark.json":
      '{"__proto__":{"include":"./base.jsonc","colors":{"editor.background":"#123456"}}}',
  },
);
await packageCase(
  { ...baselineManifest, contributes: { themes: [{ path: "./themes/dark.json" }] } },
  {
    ...baseFiles,
    "extension/themes/dark.json":
      '{"colors":{"__proto__":{"editor.background":"#123456"},"constructor":"#123456","prototype":"#fff"}}',
  },
);
for (const local of [0xfffffff0, 0xffffffff])
  await packageCase(baselineManifest, baseFiles, {
    mutate: (bytes) => {
      const copy = bytes.slice(),
        view = new DataView(copy.buffer),
        end = copy.length - 22,
        offset = view.getUint32(end + 16, true);
      view.setUint32(offset + 42, local, true);
      return copy;
    },
  });
for (const checksum of [
  "0".repeat(64),
  "not-a-hash",
  "a".repeat(257),
  hex(sha256(zip)).toUpperCase() + "  theme.vsix",
])
  await packageCase(baselineManifest, baseFiles, { checksum });
for (const publicManifest of [
  "{}",
  "[]",
  '{/*comment*/"contributes":{"themes":[]}}',
  JSON.stringify({ contributes: { themes: Array.from({ length: 41 }, () => ({})) } }),
])
  await packageCase(baselineManifest, baseFiles, { publicManifest });
const copy = (mutate) => {
  const next = zip.slice();
  mutate(new DataView(next.buffer), next);
  directory(next);
};
const end = zip.length - 22,
  offset = new DataView(zip.buffer).getUint32(end + 16, true);
for (const field of [12, 16]) copy((view) => view.setUint32(end + field, 0xffffffff, true));
for (const [field, value] of [
  [20, 0],
  [20, 1],
  [20, 0xffffffff],
  [24, 0xffffffff],
  [24, 100 * 1024 * 1024 + 1],
  [24, 500000],
])
  copy((view) => view.setUint32(offset + field, value, true));
copy((view) => view.setUint16(offset + 28, 65535, true));
copy((view) => view.setUint32(offset, 0, true));
for (const length of [0, 1, 21, 22, 23]) directory(new Uint8Array(length));
const packageWitness = rows.find((row) => row.kind === "package");
writeFileSync(
  new URL("../crates/ui/tests/fixtures/openvsx-package.json", import.meta.url),
  JSON.stringify(packageWitness.input) + "\n",
);
const networkRows = [];
for (const manifestOk of [true, false])
  for (const packageOk of [true, false])
    for (const packageLength of [null, "20971521", "Infinity", "1٢"])
      for (const body of ["valid", "oversized", "failure"]) {
        const input = { manifestOk, packageOk, packageLength, body };
        let bodyReads = 0;
        const fixture = await oracle(async (url) => {
          if (String(url).includes("/-/search"))
            return new Response(
              JSON.stringify({ extensions: [{ namespace: "demo", name: "theme" }] }),
            );
          if (String(url) === "https://open-vsx.org/api/demo/theme")
            return new Response(JSON.stringify(detail));
          if (String(url) === extension.vsixUrl)
            return {
              ok: packageOk,
              headers: new Headers(
                packageLength === null ? {} : { "content-length": packageLength },
              ),
            };
          if (String(url) === extension.manifestUrl)
            return {
              ok: manifestOk,
              headers: new Headers(body === "oversized" ? { "content-length": "262145" } : {}),
              body: {
                getReader() {
                  bodyReads++;
                  let sent = false;
                  return {
                    async read() {
                      if (body === "failure") throw Error("body failed");
                      if (sent) return { done: true };
                      sent = true;
                      return {
                        done: false,
                        value: new TextEncoder().encode(
                          JSON.stringify({ ...baselineManifest, license: "MIT" }),
                        ),
                      };
                    },
                    releaseLock() {},
                    async cancel() {},
                  };
                },
              },
            };
          throw Error("unexpected request " + url);
        });
        let value = null,
          error = null;
        try {
          value = await fixture.searchOpenVsxThemes("demo");
        } catch (cause) {
          error = cause.message;
        }
        networkRows.push({ input, expected: { value, error, bodyReads } });
      }
writeFileSync(
  new URL("../crates/ui/tests/fixtures/openvsx-network.jsonl", import.meta.url),
  networkRows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
const dictionary = [],
  indices = new Map(),
  intern = (value) => {
    const text = JSON.stringify(value);
    if (!indices.has(text)) {
      indices.set(text, dictionary.length);
      dictionary.push(value);
    }
    return indices.get(text);
  };
for (const row of rows) {
  row.input = intern(row.input);
  row.expected = intern(row.expected);
}
const data =
  [JSON.stringify({ dictionary }), ...rows.map((row) => JSON.stringify(row))].join("\n") + "\n";
writeFileSync(
  new URL("../crates/client/tests/fixtures/openvsx-themes.jsonl.gz", import.meta.url),
  gzipSync(data, { level: 9 }),
);
console.log(
  JSON.stringify({
    networkCases: networkRows.length,
    cases: rows.length,
    kinds: rows.reduce((m, r) => ((m[r.kind] = (m[r.kind] ?? 0) + 1), m), {}),
  }),
);
