// Original utility/probe paint priority, editor families and spotlight geometry.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const root = new URL("../../", import.meta.url);
const read = (path) => readFileSync(new URL(path, root), "utf8");
const source = read("apps/web/src/components/settings/themeInspector.ts");
const panel = read("apps/web/src/components/settings/ThemeEditorPanel.tsx");
function between(text, start, end) {
  const a = text.indexOf(start),
    b = text.indexOf(end, a + start.length);
  if (a < 0 || b < 0) throw Error(`Missing original inspector boundary ${start}`);
  return text.slice(a, b);
}
const roles = JSON.parse(read("rust/crates/client/src/themes/builtin.json")).roles;
const window = { innerWidth: 800, innerHeight: 600, getComputedStyle: (element) => element.style };
const api = new Function(
  "THEME_COLOR_ROLES",
  "getThemeColorVariable",
  "window",
  stripTypeScriptTypes(
    source
      .replace(/import\s+[\s\S]*?from\s+"[^"\n]+";/g, "")
      .replace(/^export /gm, "")
      .replaceAll("import.meta.env.DEV", "false"),
  ) +
    "\nreturn {themeRoleFromUtilityClass,changedThemePaintKinds,spotlightRect,THEME_UTILITY_ROLES};",
)(roles, (role) => role, window);
const families = new Function(
  stripTypeScriptTypes(
    between(panel, "const THEME_EDITOR_ROLE_GROUPS:", "\ntype ThemeEditorColors ="),
  ) + "\nreturn {groups:THEME_EDITOR_ROLE_GROUPS,getThemeEditorColorFamily};",
)();
const labelSource = between(
  read("apps/web/src/components/settings/ThemeColorPicker.tsx"),
  "export function getThemeRoleLabel",
  "\n/**",
).replace(/^export /, "");
const label = new Function(stripTypeScriptTypes(labelSource) + "\nreturn getThemeRoleLabel;")();
const data = {
  utilities: api.THEME_UTILITY_ROLES,
  groups: families.groups,
  labels: Object.fromEntries(roles.map((role) => [role, label(role)])),
};
writeFileSync(
  new URL("../crates/client/src/themes/inspector-data.json", import.meta.url),
  JSON.stringify(data) + "\n",
);
const rows = [];
for (const name of [
  ...Object.keys(data.utilities),
  "white",
  "black",
  "red-500",
  "",
  "foreground:active",
  "Primary",
  "[var(--foreground)]",
])
  for (const prefix of [
    "bg-",
    "border-",
    "outline-",
    "ring-",
    "text-",
    "caret-",
    "fill-",
    "stroke-",
    "",
  ])
    for (const suffix of ["", "/90", ":hover", "/50/90"])
      for (const kind of ["background", "border", "foreground"]) {
        const className = prefix + name + suffix;
        rows.push({
          kind: "utility",
          className,
          paint: kind,
          expected: api.themeRoleFromUtilityClass(className, kind),
        });
      }
const before = { background: "white\nnone", border: "white", foreground: "white" };
for (let mask = 0; mask < 8; mask++) {
  const after = { ...before };
  ["background", "border", "foreground"].forEach((kind, i) => {
    if (mask & (1 << i)) after[kind] = "probe";
  });
  rows.push({ kind: "paint", before, after, expected: api.changedThemePaintKinds(before, after) });
}
for (const role of [...roles, "unknown", "terminalCursor"]) {
  rows.push({ kind: "family", role, expected: families.getThemeEditorColorFamily(role) });
}
for (const bounds of [
  { left: 10, top: 20, width: 30, height: 40 },
  { left: -20, top: -10, width: 30, height: 40 },
  { left: 800, top: 600, width: 1, height: 1 },
  { left: 801, top: 601, width: 1, height: 1 },
  { left: -50, top: 5, width: 20, height: 20 },
  { left: 0, top: 0, width: 0, height: 20 },
])
  for (const radius of ["0px", "2px", "15px", "100%", "bad"]) {
    const expanded = {
      ...bounds,
      right: bounds.left + bounds.width,
      bottom: bounds.top + bounds.height,
    };
    rows.push({
      kind: "rectangle",
      bounds: expanded,
      radius: parseFloat(radius) || 0,
      viewport: [800, 600],
      expected: api.spotlightRect({
        getBoundingClientRect: () => expanded,
        style: { borderTopLeftRadius: radius },
      }),
    });
  }
const filtered = new Function(
  "roleQuery",
  "THEME_EDITOR_ROLE_GROUPS",
  "getThemeRoleLabel",
  stripTypeScriptTypes(
    between(panel, "    const query = roleQuery.trim()", "    return isAdvanced ?"),
  ) + "\nreturn groups;",
);
for (const query of [
  "",
  "canvas",
  "Canvas",
  "  text  ",
  "terminal",
  "toolbar foreground",
  "raised",
  "warning",
  "brand",
  "\ufeffAccent\ufeff",
  "\u0085Accent\u0085",
  "not-there",
])
  rows.push({ kind: "filter", query, expected: filtered(query, families.groups, label) });
const highlight = new Function(
  "selectedRole",
  "isAdvanced",
  "colorsByAppearance",
  "activeAppearance",
  "THEME_COLOR_ROLES",
  "getThemeEditorColorFamily",
  "THEME_EDITOR_SIMPLE_ROLES",
  stripTypeScriptTypes(
    between(panel, "  const selectedHighlightRoles =", "  const selectedHighlightRolesKey ="),
  ) + "\nreturn selectedHighlightRoles;",
);
const builtin = JSON.parse(read("rust/crates/client/src/themes/builtin.json"));
const palettes = [
  builtin.standard.light,
  builtin.standard.dark,
  { ...builtin.standard.light, canvas: "  RED\ufeff", accent: "\ufeffred  ", surface: "red" },
];
for (let i = 0; i < palettes.length; i++)
  for (const selected of [null, ...roles, "unknown"])
    for (const advanced of [false, true])
      rows.push({
        kind: "highlight",
        palette: i,
        selected,
        advanced,
        expected: highlight(
          selected,
          advanced,
          { light: palettes[i] },
          "light",
          roles,
          families.getThemeEditorColorFamily,
          ["canvas", "accent"],
        ),
      });
rows.unshift({ kind: "dictionary", palettes, roles });
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-inspector.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original inspector witnesses`);
