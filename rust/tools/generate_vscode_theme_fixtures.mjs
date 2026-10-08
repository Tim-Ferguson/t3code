// Original VS Code importer oracle. Color math uses the pinned Node23.11.0 runtime.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { pathToFileURL } from "node:url";
import { createThemeOracle } from "./generate_theme_color_fixtures.mjs";
const theme = createThemeOracle(undefined);
const source = stripTypeScriptTypes(
  readFileSync(new URL("../../apps/web/src/vscodeThemeImport.ts", import.meta.url), "utf8"),
)
  .replace(/^import[\s\S]*?from\s+"[^"\n]+";\s*/gm, "")
  .replace(/^export /gm, "");
export const createVsCodeOracle = () =>
  new Function(
    "deps",
    Object.keys(theme)
      .map((k) => `const ${k}=deps.${k};`)
      .join("\n") +
      "\nconst THEME_FILE_VERSION=1;\n" +
      source +
      ";return {isVsCodeThemeFile,parseVsCodeThemeFile,humanizeThemeName,pairVsCodeThemes,resolveThemeLabelCollisions,parseVsCodeColor};",
  )(theme);
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.version !== "v23.11.0") throw Error("Regeneration requires pinned Node23.11.0");
  const api = createVsCodeOracle(),
    rows = [];
  const tests = readFileSync(
    new URL("../../apps/web/src/vscodeThemeImport.test.ts", import.meta.url),
    "utf8",
  );
  const start = tests.indexOf("const VSCODE_DARK ="),
    end = tests.indexOf("\ndescribe(", start);
  if (start < 0 || end < 0) throw Error("original fixture not found");
  const dark = new Function(tests.slice(start, end) + ";return VSCODE_DARK;")();
  const add = (input) => {
    let expected,
      error = null;
    try {
      expected = api.parseVsCodeThemeFile(input);
    } catch (cause) {
      error = cause.message;
    }
    rows.push({
      kind: "import",
      input,
      isFile: api.isVsCodeThemeFile(input),
      expected: expected ?? null,
      error,
    });
  };
  for (const input of [
    null,
    0,
    "text",
    [],
    {},
    dark,
    { tokenColors: [] },
    { version: 1, colors: { "editor.background": "#abc" } },
    ...["type", "colors", "name", "displayName"].flatMap((key) =>
      [null, 0, [], {}, "", "---", "dark", "light", "hc-black", "hc-light", "unknown"].map(
        (value) => ({ ...dark, [key]: value }),
      ),
    ),
  ])
    add(input);
  const literals = [
    "#123",
    "#1234",
    "#abcdef",
    "#ABCDEF80",
    "abc",
    "12345",
    "red",
    "rgb(1 2 3)",
    "color(srgb 1 .2 .3)",
    "color(display-p3 1 .5 0 / .3)",
    "color(DISPLAY-P3 50% 20% 30% / 30%)",
    "Color(srgb 1 .2 .3)",
    "color(srgb 1x .2junk 30%)",
    "color(srgb 1٢ .2 .3)",
    "color(srgb 1٢% .2 .3)",
    "color(srgb ١ .2 .3)",
    "color(srgb Infinity 0 0)",
    "color(srgb +Infinity 0 0)",
    "color(srgb 1e+ 0 0)",
    "color(srgb .25% .2 .3 / .5junk)",
    "color(srgb 1 .2 .3 / .5 / .2)",
    "color(srgb 1 .2 .3 /)",
    "color(srgb 1\u00a0.2\ufeff.3)",
    "color(srgb 1\u0085.2 .3)",
    null,
    0,
  ];
  const keys = [
    "editor.background",
    "editorPane.background",
    "editor.foreground",
    "foreground",
    "editorWidget.background",
    "dropdown.background",
    "focusBorder",
    "button.background",
    "textLink.foreground",
    "activityBarBadge.background",
    "progressBar.background",
    "badge.background",
    "input.background",
    "input.border",
    "button.foreground",
    "sideBar.background",
    "sideBar.foreground",
    "sideBar.border",
    "terminal.background",
    "terminal.foreground",
    "terminalCursor.foreground",
    "terminal.selectionBackground",
    "scrollbarSlider.background",
    "list.hoverBackground",
    "list.activeSelectionBackground",
    "list.inactiveSelectionBackground",
    "input.placeholderForeground",
    "descriptionForeground",
    "editorError.foreground",
  ];
  for (const type of ["light", "dark"])
    for (const key of keys)
      for (const literal of literals)
        add({ ...dark, type, colors: { ...dark.colors, [key]: literal } });
  for (const name of [
    "pierre-dark-soft",
    "a__b..c",
    "---",
    "dark",
    "Grove",
    "\ufeffΣΟΣ\ufeff",
    "\u0085wrapped\u0085",
    "İSTANBUL",
    "a".repeat(49),
    "😀".repeat(25),
  ]) {
    rows.push({ kind: "humanize", input: name, expected: api.humanizeThemeName(name) });
    add({ ...dark, name, displayName: name });
  }
  const definition = (name, appearance, paired = false) =>
    theme.parseThemeFile({
      version: 1,
      ...(theme.isReservedThemeId(theme.themeIdFromName(name))
        ? { id: theme.themeIdFromName(name) + "-source" }
        : {}),
      name,
      appearance,
      colors: { canvas: appearance === "light" ? "#fff" : "#111" },
      ...(paired
        ? {
            variants: {
              [appearance === "light" ? "dark" : "light"]: {
                canvas: appearance === "light" ? "#111" : "#fff",
              },
            },
          }
        : {}),
      managed: true,
    });
  for (const labels of [
    ["Aurora Light", "Aurora Dark"],
    ["Dark+", "Light+"],
    ["Grove Dark", "Grove Light"],
    ["FOO DARK", "Foo Light"],
    ["Light éFoo", "Dark éFoo"],
    ["Light\u0085Foo", "Dark\u0085Foo"],
    ["Light\ufeffFoo", "Dark\ufeffFoo"],
    ["lightest", "darkest"],
    ["Foo Dark", "Foo Light", "Foo Dark"],
  ])
    for (const paired of [false, true]) {
      const input = labels.map((name, i) =>
        definition(name, i % 2 === 0 ? "dark" : "light", paired),
      );
      rows.push({ kind: "pair", input, expected: api.pairVsCodeThemes(input) });
    }
  for (const names of [
    ["Dracula", "Dracula"],
    ["Aurora", "Aurora", "Aurora"],
    ["Grove", "Grove"],
    ["Dracula", "Dracula", "Dracula"],
  ])
    for (const sources of [
      [],
      ["dracula.json", "dracula-soft.json"],
      [".json", "---.json"],
      ["grove.json", "grove.json"],
      ["a-b.JSON", "a-b.json"],
    ]) {
      const input = names.map((name, i) => ({
        theme: definition(name === "Grove" ? "Grove copy" : name, i % 2 === 0 ? "dark" : "light"),
        ...(sources[i] ? { sourceName: sources[i] } : {}),
      }));
      rows.push({ kind: "collisions", input, expected: api.resolveThemeLabelCollisions(input) });
    }
  const dialog = readFileSync(
    new URL("../../apps/web/src/components/settings/ThemeImportDialog.tsx", import.meta.url),
    "utf8",
  );
  const copyStart = dialog.indexOf("  const versionedCopy ="),
    copyEnd = dialog.indexOf("  const resolveConflicts", copyStart);
  if (copyStart < 0 || copyEnd < 0) throw Error("original copy function not found");
  const copySource = stripTypeScriptTypes(dialog.slice(copyStart, copyEnd));
  for (const name of ["Dracula", "A".repeat(48), "ΣΟΣ", "İSTANBUL", "😀".repeat(4)])
    for (const preferred of [
      null,
      "Dracula",
      "DRACULA",
      "Dracula Soft",
      "dark",
      "---",
      "a".repeat(60),
    ])
      for (const count of [0, 2, 99]) {
        const input = definition(name, "dark", true),
          existing = [];
        for (let i = 1; i <= count; i++) {
          let suffix = ` (${i})`;
          existing.push(
            theme.parseThemeFile({
              version: 1,
              name: name.slice(0, 48 - suffix.length) + suffix,
              appearance: "dark",
              colors: input.colors,
            }),
          );
        }
        const copy = new Function(
          "parseThemeFile",
          "getCustomThemes",
          "THEME_FILE_VERSION",
          copySource + ";return versionedCopy;",
        )(theme.parseThemeFile, () => existing, 1);
        let expected = null,
          error = null;
        try {
          expected = copy(input, preferred);
        } catch (cause) {
          error = cause.message;
        }
        rows.push({ kind: "copy", input: { theme: input, existing, preferred }, expected, error });
      }
  const dictionary = [],
    indices = new Map(),
    intern = (value) => {
      const key = JSON.stringify(value);
      if (!indices.has(key)) {
        indices.set(key, dictionary.length);
        dictionary.push(value);
      }
      return indices.get(key);
    };
  for (const row of rows) {
    row.input = intern(row.input);
    row.expected = intern(row.expected);
  }
  writeFileSync(
    new URL("../crates/client/tests/fixtures/vscode-themes.jsonl", import.meta.url),
    [JSON.stringify({ dictionary }), ...rows.map((row) => JSON.stringify(row))].join("\n") + "\n",
  );
  console.log(JSON.stringify({ cases: rows.length }));
}
