// Runs the original command palette search (`normalizeSearchText`, and the ranking and filtering in
// `CommandPalette.logic.ts`) over many item sets and queries and records the order it returns. The
// Rust tests in t3-client replay the rows and require the same groups in the same order. The
// functions are cut out of the original source unchanged (the file imports React types and the
// effect library, which a plain script does not need); only the two effect helpers they call are
// stood in for.
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";

const read = (path) => readFileSync(new URL(path, import.meta.url), "utf8");
const cut = (source, start, end) => {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`cannot find ${start} .. ${end}`);
  return source.slice(from, to);
};
const utils = read("../../apps/web/src/lib/utils.ts");
const logic = read("../../apps/web/src/components/CommandPalette.logic.ts");
const body = [
  cut(utils, "export function normalizeSearchText", "export function getLocalFileManagerName"),
  cut(logic, "function rankSearchFieldMatch(", "export function buildBrowseGroups("),
].join("\n");
const header = `
const Result = { failVoid: Symbol("fail"), succeed: (value) => ({ value }) };
const Arr = { filterMap: (items, f) => items.flatMap((item, index) => { const r = f(item, index); return r === Result.failVoid ? [] : [r.value]; }) };
`;
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-palette-client-"));
const file = NodePath.join(scratch, "palette.ts");
writeFileSync(
  file,
  `${header}\n${stripTypeScriptTypes(body.replace(/\bexport /g, ""))}\nexport { normalizeSearchText, filterCommandPaletteGroups };\n`,
);
// Types are stripped from the whole module, so the file is plain JavaScript by now.
writeFileSync(file, readFileSync(file, "utf8"));
const { normalizeSearchText, filterCommandPaletteGroups } = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const item = (value, terms, extra = {}) => ({
  kind: "action",
  value,
  searchTerms: terms,
  ...extra,
});
const actions = {
  value: "actions",
  label: "Actions",
  items: [
    item("a:new", ["new thread", "chat", "create", "draft"]),
    item("a:diff", ["Toggle diff panel", "diff", "changes"]),
    item("a:settings", ["Open settings", "settings", "preferences", "keybindings"]),
    item("a:theme", ["Change theme", "appearance", "colors", "palette"]),
    item("a:later", ["Zeta thing", "secondary thing"], { secondary: true }),
    item("a:cafe", ["Café Noir", "cafe"]),
    item("a:wide", ["ＦＵＬＬ width", "full"]),
  ],
};
const recent = {
  value: "recent-threads",
  label: "Recent Threads",
  items: [item("t:1", ["Fix login", "Project", "t1"], { searchRecency: 5 })],
};
const projects = [
  item("p:1", ["Login service", "/work/login"]),
  item("p:2", ["Billing", "/work/billing"]),
];
const settings = [
  item("s:1", ["Providers", "Settings Providers", "models"], { secondary: false }),
  item("s:2", ["Advanced providers", "providers"], { secondary: true }),
];
const threads = [
  item("t:1", ["Fix login", "Project", "", "t1"], { searchRecency: 5 }),
  item("t:2", ["Login page", "Project", "", "t2", "we should fix the login flow"], {
    searchRecency: 9,
  }),
  item("t:3", ["Billing login", "Other", "", "t3"], { searchRecency: 9 }),
  item("t:4", ["Notes", "Project", "", "t4", "login is broken"], { searchRecency: 1 }),
  item("t:5", ["login", "Project", "", "t5"], { searchRecency: 2 }),
];
const queries = [
  "",
  " ",
  "new",
  "thread",
  "new thread",
  "theme",
  "diff",
  "dif",
  "settings",
  "set",
  "login",
  "Login",
  "LOGIN",
  "log in",
  "fix login",
  "cafe",
  "café",
  "Cafe Noir",
  "full",
  "ＦＵＬＬ",
  "thing",
  "zeta",
  "providers",
  "prov",
  "billing",
  "page",
  "the",
  "t",
  "a",
  "zzz",
  ">",
  ">diff",
  ">settings",
  ">  theme",
  ">zzz",
  "  new   thread  ",
  "1",
  "t3",
  "flow",
];
const rows = [];
for (const isInSubmenu of [false, true]) {
  for (const query of queries) {
    const result = filterCommandPaletteGroups({
      activeGroups: [actions, recent],
      query,
      isInSubmenu,
      projectSearchItems: projects,
      settingsSearchItems: settings,
      threadSearchItems: threads,
    });
    rows.push({
      query,
      isInSubmenu,
      groups: result.map((group) => [
        group.value,
        group.label,
        group.items.map((entry) => entry.value),
      ]),
    });
  }
}
const inputs = { actions, recent, projects, settings, threads };
const normalized = [
  "  Café   NOÏR ",
  "ＦＵＬＬ",
  "ﬁle",
  "Ünïcödé",
  "Ω≈ç",
  "İstanbul",
  "ǅ",
  "x́y",
  "日本語",
  "  ",
  "",
  "a\tb\nc",
  "ÅNGSTRÖM",
  "Ⅻ",
  "①②",
].map((text) => ({ text, normalized: normalizeSearchText(text) }));
writeFileSync(
  new URL("../crates/client/tests/fixtures/palette-search.json", import.meta.url),
  JSON.stringify({ inputs, rows, normalized }),
);
console.log(`wrote ${rows.length} filter rows`);
