// Executes original themePalette.ts recovery, import/export, and library mutations.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { createThemeOracle } from "./generate_theme_color_fixtures.mjs";
const api = createThemeOracle(undefined),
  rows = [];
const stored = {
  id: "custom",
  label: " Custom ",
  appearance: "light",
  colors: { canvas: "#abc", futureRole: "pink" },
};
const file = { version: 1, name: " Custom ", appearance: "light", colors: { canvas: "#abc" } };
for (const input of [
  null,
  0,
  [],
  {},
  stored,
  ...["id", "label", "appearance", "colors", "variants", "collection", "managed"].flatMap((key) =>
    [null, 0, true, [], {}, "", "dark", "\ufeff Wrapped \ufeff", "\u0085wrapped\u0085"].map(
      (value) => ({ ...stored, [key]: value }),
    ),
  ),
  ...["light", "dark", "future"].flatMap((key) =>
    [null, {}, [], { canvas: "#fff", futureRole: "red" }].map((value) => ({
      ...stored,
      variants: { [key]: value },
    })),
  ),
  ...[24, 25, 48, 49].map((n) => ({ ...stored, label: "😀".repeat(n) })),
  { ...stored, colors: { canvas: "bogus", futureRole: "red" } },
  { ...stored, collection: { id: "C.1:a-b", label: " Group " } },
]) {
  rows.push({ kind: "stored", input, expected: api.parseStoredTheme(input) });
}
for (const input of [
  null,
  0,
  [],
  {},
  file,
  ...["version", "id", "name", "appearance", "colors", "variants", "collection", "managed"].flatMap(
    (key) =>
      [null, 0, true, [], {}, "", "dark", "\ufeff Wrapped \ufeff", "\u0085wrapped\u0085"].map(
        (value) => ({ ...file, [key]: value }),
      ),
  ),
  ...["light", "dark", "future"].flatMap((key) =>
    [null, {}, [], { canvas: "#fff", futureRole: "red" }].map((value) => ({
      ...file,
      variants: { [key]: value },
    })),
  ),
  ...["SYSTEM", "t3-chat", "valid-name", "-bad", "a".repeat(48), "a".repeat(49)].map((id) => ({
    ...file,
    id,
  })),
  ...["red", "rgb(1\u00a02\u00a03)", "var(--x)", null, 1].map((color) => ({
    ...file,
    colors: { canvas: color },
  })),
  { ...file, colors: { futureRole: "red" } },
  {
    ...file,
    colors: { canvas: "red" },
    variants: { dark: { text: "white" } },
    collection: { id: "C.1", label: " Group " },
    managed: true,
  },
]) {
  try {
    const expected = api.parseThemeFile(input);
    rows.push({ kind: "import", input, expected, serialized: api.serializeThemeFile(expected) });
  } catch (cause) {
    rows.push({ kind: "import", input, error: cause.message });
  }
}
for (const input of [
  [],
  [stored],
  [stored, { ...stored, label: "Duplicate" }],
  [null, "future", 7, stored, { id: "future", colors: null, unknown: { retain: true } }],
  [{ ...stored, variants: { light: null, dark: { canvas: "blue" } } }],
  [{ ...stored, id: "system" }],
]) {
  rows.push({ kind: "many", input, expected: api.parseStoredThemes(input) });
}
for (const raw of [
  null,
  "",
  "{}",
  "null",
  "true",
  "[",
  "[]",
  JSON.stringify([stored, { id: "future", opaque: [1, 2] }]),
]) {
  const oracle = createThemeOracle({ localStorage: { getItem: () => raw } });
  rows.push({ kind: "read", raw, expected: oracle.readCustomThemeLibrarySnapshot() });
}
const canonical = api.parseThemeFile({ ...file, id: "added" });
const grouped = api.parseThemeFile({
  ...file,
  id: "member",
  collection: { id: "group", label: "Group" },
});
for (const initial of [
  [],
  [stored],
  [stored, { ...stored, label: "Duplicate" }, null, { id: "future", opaque: { keep: true } }],
  [{ id: "added", invalid: true }],
  [
    grouped,
    { ...grouped, id: "second" },
    stored,
    { id: "group-broken", collection: { id: "group" }, opaque: true },
  ],
  null,
  {},
])
  for (const operation of ["install", "update", "remove", "replace"])
    for (const failWrite of [false, true]) {
      let raw = JSON.stringify(initial),
        writes = 0;
      const oracle = createThemeOracle({
        localStorage: {
          getItem: () => raw,
          setItem: (key, value) => {
            writes++;
            if (failWrite) throw Error("write denied");
            raw = value;
          },
        },
      });
      const input =
        operation === "install"
          ? canonical
          : operation === "update"
            ? { ...canonical, id: "custom", label: "Changed" }
            : operation === "remove"
              ? ["custom"]
              : [grouped];
      let error = null;
      try {
        if (operation === "install") oracle.installCustomTheme(input);
        if (operation === "update") oracle.updateCustomTheme(input);
        if (operation === "remove") oracle.removeCustomThemes(input);
        if (operation === "replace") oracle.replaceCustomThemeCollection("group", input);
      } catch (cause) {
        error = cause.message;
      }
      rows.push({
        kind: "mutation",
        initial,
        operation,
        input,
        failWrite,
        error,
        writes,
        stored: JSON.parse(raw),
      });
    }
for (const initial of [
  [],
  [grouped, stored, { id: "member-broken", collection: { id: "group" }, opaque: true }],
  [stored, { id: "member", opaque: true }],
])
  for (const input of [
    [grouped],
    [grouped, grouped],
    [],
    [{ ...grouped, collection: { id: "other", label: "Other" } }],
    [{ ...grouped, colors: null }],
  ])
    for (const expected of [
      undefined,
      [],
      [grouped],
      [{ ...grouped, label: "Changed concurrently" }],
    ]) {
      let raw = JSON.stringify(initial),
        writes = 0;
      const oracle = createThemeOracle({
        localStorage: {
          getItem: () => raw,
          setItem: (_key, value) => {
            writes++;
            raw = value;
          },
        },
      });
      let error = null;
      try {
        oracle.replaceCustomThemeCollection("group", input, { expectedCollection: expected });
      } catch (cause) {
        error = cause.message;
      }
      rows.push({
        kind: "mutation",
        initial,
        operation: "replace",
        input,
        ...(expected ? { expected } : {}),
        failWrite: false,
        error,
        writes,
        stored: JSON.parse(raw),
      });
    }
const familyRows = [];
for (const appearance of ["light", "dark"])
  for (const role of Object.keys(api.createVividThemeColors(appearance, "black", "red")))
    for (const color of ["#abc", "rgba(0, 0, 255, .3)", "oklch(.8 .2 40)", "invalid"]) {
      const colors = api.createVividThemeColors(
        appearance,
        appearance === "dark" ? "#17111a" : "#faeff7",
        "#a84370",
      );
      familyRows.push({
        appearance,
        role,
        color,
        colors,
        expected: api.updateThemeColorFamily(appearance, colors, role, color),
      });
    }
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-families.jsonl", import.meta.url),
  familyRows.map((v) => JSON.stringify(v)).join("\n") + "\n",
);
let environmentSource = stripTypeScriptTypes(
  readFileSync(new URL("../../apps/web/src/hooks/useEnvironmentTheme.ts", import.meta.url), "utf8"),
)
  .replace(/^import[\s\S]*?from\s+"[^"\n]+";\s*/gm, "")
  .replace(/^export /gm, "");
const environmentApi = new Function(
  "deps",
  [
    "createVividThemeColors",
    "getDefaultThemeColors",
    "lenientThemeColorOverrides",
    "isReservedThemeId",
  ]
    .map((k) => `const ${k}=deps.${k};`)
    .join("\n") +
    "\n" +
    environmentSource +
    ";return {publishedThemeDefinitions};",
)(api);
const environmentRows = [];
for (const appearance of ["light", "dark"])
  for (const id of ["published", "t3-chat", "dark", "t3-iris"])
    for (const shape of [
      {},
      { canvas: "#000", accent: "#f00" },
      { canvas: "#fff", accent: "#00f", colors: {} },
      { colors: { canvas: "red", futureRole: "pink" } },
      { colors: { canvas: "invalid", futureRole: "pink" } },
      { variants: { light: { canvas: "white" }, dark: { canvas: "black" } } },
      { variants: { [appearance]: { canvas: "red" } } },
      {
        canvas: "#000",
        accent: "#f00",
        variants: { light: { text: "black" }, dark: { text: "white" } },
      },
      { canvas: "#000", accent: "#f00", colors: { surface: "rgb(12 34 56)" } },
    ]) {
      const input = [{ id, name: "Published", appearance, ...shape }];
      environmentRows.push({ input, expected: environmentApi.publishedThemeDefinitions(input) });
    }
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-environment.jsonl", import.meta.url),
  environmentRows.map((v) => JSON.stringify(v)).join("\n") + "\n",
);
const vividRows = [];
for (const appearance of ["light", "dark"])
  for (const background of [
    "black",
    "white",
    "#faeff7",
    "#17111a",
    "#567890",
    "rgb(127 127 127)",
    "rgb(129 129 129)",
    "oklch(.5 .4 200)",
    "transparent",
    "invalid",
  ])
    for (const accent of ["#a84370", "gray", "white", "blue", "oklch(.8 .5 40)", "invalid"])
      vividRows.push({
        appearance,
        background,
        accent,
        expected: api.createVividThemeColors(appearance, background, accent),
      });
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-vivid.jsonl", import.meta.url),
  vividRows.map((v) => JSON.stringify(v)).join("\n") + "\n",
);
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-library.jsonl", import.meta.url),
  rows.map((v) => JSON.stringify(v)).join("\n") + "\n",
);
console.log(
  JSON.stringify({
    library: rows.length,
    vivid: vividRows.length,
    families: familyRows.length,
    environment: environmentRows.length,
  }),
);
