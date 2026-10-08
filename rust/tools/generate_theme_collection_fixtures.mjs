// Executes original collection-card helpers. Locale witnesses use actual Intl casing.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/web/src/components/settings/ThemeSettings.tsx", import.meta.url),
  "utf8",
);
function slice(begin, end) {
  const a = source.indexOf(begin),
    b = source.indexOf(end, a + begin.length);
  if (a < 0 || b < 0) throw Error(`Missing original collection boundary: ${begin}`);
  return source.slice(a, b);
}
const labels = new Function(
  stripTypeScriptTypes(slice("function collectionVariantLabels", "\nfunction downloadThemeFile")) +
    "\nreturn collectionVariantLabels;",
)();
const groupSource = slice("customThemes\n      .reduce", "\n  ];");
const group = new Function(
  "customThemes",
  stripTypeScriptTypes(`const result = [...${groupSource}];`) + "\nreturn result;",
);
const defaults = new Function(
  "themes",
  "getThemeModes",
  stripTypeScriptTypes(slice("  const defaultLightTheme =", "  const selectCollectionDefaults =")) +
    "\nreturn [defaultLightTheme?.id ?? null, defaultDarkTheme?.id ?? null];",
);
const initial = new Function(
  "themes",
  "activeModesFor",
  slice("    const activeIndex = themes.findIndex", "\n  });"),
);
const sets = [
  [],
  ["One"],
  ["Pack Dark", "Pack Light"],
  ["Pack", "Pack Light"],
  ["Pack Dark", "Pack Dark"],
  ["Pack  Dark", "pack\tSoft"],
  ["", "\ufeff"],
  ["\u0085Pack Light", "Pack Dark"],
  ["İSTANBUL Night", "istanbul Day"],
  ["Istanbul Night", "ıstanbul Day"],
  ["I\u0301 Pack Night", "i\u0307\u0301 Pack Day"],
  ["ΣΟΣ Night", "σος Day"],
  ["Pack 😀 Dark", "PACK 😀 Light"],
  ["Pack\u2028Light", "pack\ufeffDark"],
  ["Same Label", "same label", "SAME LABEL"],
];
const rows = [],
  lower = String.prototype.toLocaleLowerCase;
for (const locale of ["en", "tr", "lt"])
  for (const input of sets) {
    let expected;
    try {
      String.prototype.toLocaleLowerCase = function () {
        return lower.call(this, locale);
      };
      expected = labels(input.map((label) => ({ label })));
    } finally {
      String.prototype.toLocaleLowerCase = lower;
    }
    rows.push({
      kind: "labels",
      locale,
      input,
      folded: input.map((label) =>
        label
          .trim()
          .split(/\s+/)
          .map((word) => lower.call(word, locale)),
      ),
      expected,
    });
  }
const theme = (id, appearance, paired, collection) => ({
  id,
  label: `Pack ${id}`,
  appearance,
  colors: {},
  ...(paired ? { variants: { [appearance === "light" ? "dark" : "light"]: {} } } : {}),
  ...(collection ? { collection: { id: collection, label: collection } } : {}),
});
const states = [
  [],
  [theme("a", "light", false)],
  [theme("a", "dark", true)],
  [
    theme("a", "dark", false, "pack"),
    theme("b", "light", false, "pack"),
    theme("c", "dark", true, "other"),
    theme("d", "light", true),
    theme("e", "dark", false, "pack"),
  ],
  [theme("a", "light", false, "x"), theme("b", "dark", false, "y"), theme("c", "light", true, "x")],
];
for (const themes of states) {
  const groups = group(themes).map(([id, members]) => [id, members.map((t) => t.id)]);
  const modes = (t) =>
    ["light", "dark"].filter((mode) => mode === t.appearance || t.variants?.[mode]);
  for (const active of [[], ["a"], ["b"], ["c"], ["removed"], ["a", "b"]])
    rows.push({
      kind: "group",
      themes,
      active,
      groups,
      defaults: defaults(themes, modes),
      initial: initial(themes, (id) => (active.includes(id) ? ["dark"] : [])),
    });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-collections.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original collection witnesses`);
