// Executes the original panel's actual submit callback, including its merge and rollback branches.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { createThemeOracle } from "./generate_theme_color_fixtures.mjs";
const source = readFileSync(
  new URL("../../apps/web/src/components/settings/ThemeEditorPanel.tsx", import.meta.url),
  "utf8",
);
const begin = source.indexOf("const handleSubmit = () => {");
const end = source.indexOf("\n  const ", begin + 30);
if (begin < 0 || end < 0) throw Error("original submit callback boundary missing");
const body = stripTypeScriptTypes(source.slice(begin, end))
  .replace(/^const handleSubmit = /, "")
  .trim()
  .replace(/;$/, "");
const base = createThemeOracle(undefined),
  rows = [];
const light = base.createVividThemeColors("light", "#f0e0d0", "#aa33aa"),
  dark = base.createVividThemeColors("dark", "#182838", "#aa33aa");
const theme = (id, name, appearance, paired = false) =>
  base.parseThemeFile({
    version: 1,
    id,
    name,
    appearance,
    colors: appearance === "light" ? light : dark,
    ...(paired
      ? {
          variants: {
            [appearance === "light" ? "dark" : "light"]: appearance === "light" ? dark : light,
          },
        }
      : {}),
    managed: true,
    collection: { id: "pack", label: "Pack" },
  });
const states = [
  [],
  [theme("existing", "Existing", "light")],
  [theme("existing", "Existing", "dark")],
  [theme("existing", "Existing", "light", true)],
  [theme("existing", "Existing", "light"), theme("renamed-id", "Target", "dark")],
  [theme("existing", "Existing", "dark"), theme("renamed-id", "Target", "light")],
  [theme("renamed-id", "  ΣΟΣ  ", "dark")],
  [theme("renamed-id", "İstanbul", "dark")],
];
for (const initial of states)
  for (const editingId of [null, "existing", "removed"])
    for (const name of [
      "New",
      "Existing",
      "Target",
      " target ",
      "\ufeffΣΟΣ\ufeff",
      "İSTANBUL",
      "\u0085Existing\u0085",
      "A".repeat(48),
      "😀".repeat(25),
      "",
    ])
      for (const appearance of ["light", "dark"])
        for (const advanced of [false, true]) {
          let raw = JSON.stringify(initial),
            error = null,
            saved = null,
            context = null,
            closed = false;
          const api = createThemeOracle({
            localStorage: {
              getItem: () => raw,
              setItem: (_, value) => {
                raw = value;
              },
            },
          });
          const editingTheme = api.getCustomThemes().find((t) => t.id === editingId) ?? null;
          const normalizedName = name.trim().toLowerCase(),
            nameTargetId = api.themeIdFromName(name);
          const mergeTarget =
            normalizedName === ""
              ? null
              : (api
                  .getCustomThemes()
                  .find(
                    (t) =>
                      t.id !== editingTheme?.id &&
                      (t.id === nameTargetId || t.label.trim().toLowerCase() === normalizedName),
                  ) ?? null);
          const values = {
            name,
            editingTheme,
            mergeTarget,
            takenAppearances: mergeTarget ? api.getThemeModes(mergeTarget) : [],
            activeAppearance: appearance,
            isAdvanced: advanced,
            isEditing: editingTheme !== null,
            colorsByAppearance: { light, dark },
            simpleColorsDirtyByAppearance: { light: false, dark: false },
            THEME_FILE_VERSION: 1,
            setError: (cause) => {
              error = cause;
            },
            onSaved: (theme, meta) => {
              saved = theme;
              context = meta;
              return true;
            },
            onOpenChange: (open) => {
              closed = !open;
            },
            getManagedEditorColors: (mode, colors) => colors,
            ...api,
          };
          const execute = new Function(...Object.keys(values), `return (${body})()`);
          execute(...Object.values(values));
          rows.push({
            initial,
            draft: { editingId, name, appearance, advanced, colors: { light, dark } },
            target: mergeTarget?.id ?? null,
            error,
            saved,
            context,
            closed,
          });
        }
const dictionary = [],
  interned = new Map();
const intern = (value) => {
  const bytes = JSON.stringify(value);
  if (!interned.has(bytes)) {
    interned.set(bytes, dictionary.length);
    dictionary.push(value);
  }
  return interned.get(bytes);
};
for (const row of rows) {
  row.initial = intern(row.initial);
  row.draft.colors = intern(row.draft.colors);
  row.saved = intern(row.saved);
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-editor.jsonl", import.meta.url),
  [JSON.stringify({ dictionary }), ...rows.map((row) => JSON.stringify(row))].join("\n") + "\n",
);
console.log(JSON.stringify({ editor: rows.length }));
