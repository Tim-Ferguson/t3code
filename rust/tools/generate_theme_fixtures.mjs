// Original storage/resolution policy and builtin palette data oracle.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const read = (path) => readFileSync(new URL("../../" + path, import.meta.url), "utf8");
const shared = read("packages/shared/src/themePalettes.ts").replace(/^export /gm, "");
const data = new Function(
  stripTypeScriptTypes(shared) +
    ";return {builtin:BUILT_IN_THEMES,standard:{light:T3_CODE_LIGHT_THEME_COLORS,dark:T3_CODE_DARK_THEME_COLORS},roles:THEME_COLOR_ROLES,reserved:[...RESERVED_THEME_IDS]};",
)();
const palette = read("apps/web/src/themePalette.ts"),
  hook = read("apps/web/src/hooks/useTheme.ts");
function fn(text, name) {
  const start = text.indexOf("function " + name + "(");
  if (start < 0) throw Error("Missing original function " + name);
  const end = text.indexOf("\n}", start) + 2;
  return stripTypeScriptTypes(text.slice(start, end));
}
const vars = palette.slice(
  palette.indexOf("const APP_THEME_VARIABLES:"),
  palette.indexOf("\nexport function getThemeColorVariable"),
);
data.variables = new Function(stripTypeScriptTypes(vars) + ";return APP_THEME_VARIABLES;")();
writeFileSync(
  new URL("../crates/client/src/themes/builtin.json", import.meta.url),
  JSON.stringify(data, null, 2) + "\n",
);
const helpers = [
  "normalizeThemeId",
  "themeIdFromPreference",
  "canonicalThemePreference",
  "legacyThemeMode",
  "getThemeDefinition",
  "getThemeColorsForMode",
  "getThemeModes",
  "getThemePreferenceMode",
  "resolveThemeAppearance",
  "resolveDesktopTheme",
  "isKnownThemePreference",
  "parseThemeHalves",
  "resolveThemeHalf",
]
  .map((name) => fn(palette, name))
  .join("\n");
const reads = [
  "readStoredFollowSystem",
  "isThemePreferenceMode",
  "readAppearanceModePreference",
  "readThemePreference",
]
  .map((name) => fn(hook, name))
  .join("\n");
const aliases = {
  "t3-chat-dark": "t3-chat",
  "t3-grove": "grove",
  "t3-ocean": "ocean",
  "t3-ember": "ember",
  "t3-iris": "iris",
};
const custom = [
  { id: "only-dark", label: "Only dark", appearance: "dark", colors: {} },
  { id: "only-light", label: "Only light", appearance: "light", colors: {} },
  { id: "both", label: "Both", appearance: "dark", colors: {}, variants: { light: {} } },
  {
    ...data.builtin[0],
    id: "collision",
    label: "User collision",
    appearance: "light",
    variants: undefined,
  },
];
const environment = [
  { id: "collision", label: "Environment collision", appearance: "dark", colors: {} },
  { id: "published", label: "Published", appearance: "dark", colors: {}, variants: { light: {} } },
];
const make = new Function(
  "window",
  "BUILT_IN_THEME_DEFINITIONS",
  "getCustomThemes",
  "environmentThemeDefinitions",
  "LEGACY_THEME_ID_ALIASES",
  "LEGACY_T3_CHAT_DARK_THEME_ID",
  "isRecord",
  "DEFAULT_THEME_SNAPSHOT",
  "THEME_FOLLOW_SYSTEM_STORAGE_KEY",
  "THEME_APPEARANCE_MODE_STORAGE_KEY",
  "STORAGE_KEY",
  helpers +
    "\n" +
    reads +
    ";return {canonicalThemePreference,getThemePreferenceMode,isKnownThemePreference,resolveThemeAppearance,resolveDesktopTheme,parseThemeHalves,resolveThemeHalf,readAppearanceModePreference,readThemePreference,getThemeDefinition};",
);
const root = { theme: "system" };
let rows = [];
function original(storage) {
  return make(
    { localStorage: { getItem: (key) => storage[key] ?? null } },
    data.builtin,
    () => custom,
    environment,
    aliases,
    "t3-chat-dark",
    (value) => value !== null && typeof value === "object" && !Array.isArray(value),
    root,
    "t3code:theme-follow-system",
    "t3code:theme-appearance-mode",
    "t3code:theme",
  );
}
for (const theme of [
  "light",
  "dark",
  "system",
  ...data.builtin.map((t) => t.id),
  ...Object.keys(aliases),
  "only-dark",
  "only-light",
  "both",
  "collision",
  "published",
  "future",
  "",
  " ocean ",
  "__preview",
]) {
  const api = original({});
  rows.push({
    kind: "preference",
    theme,
    canonical: api.canonicalThemePreference(theme),
    mode: api.getThemePreferenceMode(theme),
    known: api.isKnownThemePreference(theme),
    stored: original({ "t3code:theme": theme }).readThemePreference(),
  });
  for (const mode of [undefined, "light", "dark", "system"])
    for (const follow of [undefined, false, true])
      for (const systemDark of [false, true])
        for (const halves of [
          null,
          { light: "only-light" },
          { dark: "only-dark" },
          { light: "published", dark: "published" },
        ]) {
          rows.push({
            kind: "resolve",
            theme,
            mode: mode ?? null,
            follow: follow ?? null,
            systemDark,
            halves,
            appearance: api.resolveThemeAppearance(theme, systemDark, follow, mode, halves),
            desktop: api.resolveDesktopTheme(theme, follow, mode, halves),
          });
        }
  for (const mode of [null, "light", "dark", "system", "", "garbage"])
    for (const follow of [null, "true", "false", "1", "garbage"]) {
      rows.push({
        kind: "storedMode",
        theme,
        mode,
        follow,
        expected: original({
          "t3code:theme-appearance-mode": mode,
          "t3code:theme-follow-system": follow,
        }).readAppearanceModePreference(theme),
      });
    }
}
for (const value of [
  null,
  [],
  {},
  false,
  "bad",
  { light: "ocean", dark: "t3-chat-dark" },
  { light: "only-dark", dark: "only-light" },
  { light: "only-light", dark: "only-dark" },
  { light: "published", dark: "unpublished" },
  { light: 42, dark: "dark" },
  { light: "system", dark: "ocean" },
  [{ dark: "ocean" }],
  { light: "collision", dark: "collision" },
  { light: "t3-ocean", dark: "t3-iris" },
]) {
  const raw = JSON.stringify(value);
  rows.push({ kind: "halves", raw, expected: original({}).parseThemeHalves(raw) });
}
const normalizeChrome = new Function(
  fn(hook, "normalizeThemeColor") + ";return normalizeThemeColor;",
)();
for (const value of [
  null,
  undefined,
  "",
  " transparent ",
  "TRANSPARENT",
  "rgba(0, 0, 0, 0)",
  "rgba(0 0 0 / 0)",
  "rgba(0,0,0,0)",
  " #ABC ",
  "\ufeff#abc\ufeff",
  "\u0085#abc\u0085",
])
  rows.push({ kind: "chrome", value: value ?? null, expected: normalizeChrome(value) });
rows.push({ kind: "catalog", custom, environment });
writeFileSync(
  new URL("../crates/client/tests/fixtures/themes.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(
  `Generated ${rows.length} original theme policy witnesses and ${data.builtin.length} builtin palettes.`,
);
// Exercise actual useTheme callbacks with React/DOM effects replaced, preserving storage code.
const transactionSource =
  helpers +
  "\n" +
  reads +
  "\n" +
  ["writeAppearanceModePreference", "writeThemePreference", "readStoredThemeHalvesRaw", "useTheme"]
    .map((name) => fn(hook, name))
    .join("\n");
const transaction = new Function(
  "window",
  "data",
  "custom",
  "environment",
  "action",
  transactionSource +
    `
const BUILT_IN_THEME_DEFINITIONS=data.builtin, environmentThemeDefinitions=environment;
const getCustomThemes=()=>custom, LEGACY_THEME_ID_ALIASES=${JSON.stringify(aliases)}, LEGACY_T3_CHAT_DARK_THEME_ID='t3-chat-dark';
const isRecord=v=>v!==null&&typeof v==='object'&&!Array.isArray(v);
const DEFAULT_THEME_SNAPSHOT={theme:'system'}, STORAGE_KEY='t3code:theme', THEME_FOLLOW_SYSTEM_STORAGE_KEY='t3code:theme-follow-system', THEME_APPEARANCE_MODE_STORAGE_KEY='t3code:theme-appearance-mode', THEME_HALVES_STORAGE_KEY='t3code:theme-halves:v1';
class ThemeStorageError extends Error{constructor(fields){super('storage');Object.assign(this,fields)}}
const isThemeStorageError=e=>e instanceof ThemeStorageError, safeErrorLogAttributes=()=>({}),console={error(){}};
let themeStorageReadFailure=null,lastAppliedTheme=null;
const getStored=()=>readThemePreference(),applyTheme=()=>{},emitChange=()=>{};
const readStoredThemeHalves=()=>parseThemeHalves(window.localStorage.getItem(THEME_HALVES_STORAGE_KEY));
const getSnapshot=()=>({theme:getStored(),resolvedTheme:'light',appearanceMode:readAppearanceModePreference(getStored()),followSystem:true,themeHalves:readStoredThemeHalves()});
const getServerSnapshot=getSnapshot,useSyncExternalStore=(subscribe,get)=>get(),subscribe=()=>{},useEffect=()=>{},useCallback=f=>f;
const api=useTheme();
switch(action.type){case 'theme':return api.setTheme(action.value);case 'mode':return api.setAppearanceMode(action.value);case 'half':return api.setThemeHalf(action.appearance,action.value);case 'clear':return api.clearThemeHalves();default:throw Error('action');}
`,
);
let transactions = [];
for (const initial of [
  {},
  {
    "t3code:theme": "t3-chat-dark",
    "t3code:theme-halves:v1": '{"light":"unpublished","dark":"ocean","unknown":42}',
  },
  { "t3code:theme": "ocean", "t3code:theme-follow-system": "false" },
  {
    "t3code:theme": "system",
    "t3code:theme-appearance-mode": "dark",
    "t3code:theme-halves:v1": "bad",
  },
])
  for (const action of [
    { type: "theme", value: "grove" },
    { type: "mode", value: "system" },
    { type: "mode", value: "dark" },
    { type: "half", appearance: "light", value: "iris" },
    { type: "half", appearance: "dark", value: null },
    { type: "clear" },
  ])
    for (const failAt of [0, 1, 2, 3, 4, 5]) {
      const saved = { ...initial },
        ops = [];
      let writes = 0;
      const storage = {
        getItem: (k) => saved[k] ?? null,
        setItem(k, v) {
          ops.push({ type: "set", key: k, value: v });
          if (++writes === failAt) throw Error("injected");
          saved[k] = v;
        },
        removeItem(k) {
          ops.push({ type: "remove", key: k });
          if (++writes === failAt) throw Error("injected");
          delete saved[k];
        },
      };
      const success = transaction({ localStorage: storage }, data, custom, environment, action);
      transactions.push({ initial, action, failAt, ops, saved, success });
    }
writeFileSync(
  new URL("../crates/client/tests/fixtures/theme-transactions.jsonl", import.meta.url),
  transactions.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`Generated ${transactions.length} original useTheme transaction witnesses.`);

const css = read("apps/web/src/index.css");
const tokenStart = css.indexOf('html[data-theme-id]:not([data-theme-id=""]) {');
const tokenEnd = css.indexOf("\n}", tokenStart) + 2;
if (tokenStart < 0) throw Error("Missing source theme token aliases");
writeFileSync(
  new URL("../crates/ui/assets/theme-tokens.css", import.meta.url),
  "/* Original source theme role aliases; application owns the palette scope. */\n" +
    css
      .slice(tokenStart, tokenEnd)
      .replace(
        'html[data-theme-id]:not([data-theme-id=""])',
        'html[data-theme-id]:not([data-theme-id=""]) .app',
      ) +
    "\n",
);
