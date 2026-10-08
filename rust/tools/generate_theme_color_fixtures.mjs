// Actual original themePalette helpers plus pinned Culori4.0.2; no application runtime dependency.
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { pathToFileURL } from "node:url";
const base = new URL("../../", import.meta.url),
  read = (p) => readFileSync(new URL(p, base), "utf8");
const culoriBase = new URL("node_modules/.pnpm/culori@4.0.2/node_modules/culori/", base);
const culori = await import(new URL("src/bootstrap/css.js", culoriBase));
const { default: parse } = await import(new URL("src/parse.js", culoriBase));
const { default: converter } = await import(new URL("src/converter.js", culoriBase));
const { default: named } = await import(new URL("src/colors/named.js", culoriBase));
const shared = stripTypeScriptTypes(read("packages/shared/src/themePalettes.ts")).replace(
  /^export /gm,
  "",
);
const data = new Function(
  shared +
    ";return {BUILT_IN_THEMES,EMBER_THEME,GROVE_THEME,IRIS_THEME,OCEAN_THEME,T3_CHAT_THEME,T3_CODE_LIGHT_THEME_COLORS,T3_CODE_DARK_THEME_COLORS,RESERVED_THEME_IDS,THEME_COLOR_ROLES};",
)();
let source = stripTypeScriptTypes(read("apps/web/src/themePalette.ts"));
source = source
  .replace(/^import[\s\S]*?from\s+"[^"\n]+";\s*/gm, "")
  .replace(/^import\s+"[^"\n]+";\s*/gm, "")
  .replace(/^export\s*\{[^}]*\};\s*/gm, "")
  .replace(/^export /gm, "");
export const createThemeOracle = (window) =>
  new Function(
    "deps",
    "converter",
    "parse",
    "Equal",
    "window",
    Object.keys(data)
      .map((k) => `const ${k}=deps.${k};`)
      .join("\n") +
      "\nconst Schema={String:null,Literals:()=>null,optional:()=>null,Defect:()=>null,TaggedError:()=>()=>class extends Error {constructor(fields){super();Object.assign(this,fields)}}};\n" +
      source +
      ";return {toCanonicalThemeColor,themeColorToHex,parseThemeFile,serializeThemeFile,createVividThemeColors,getDefaultThemeColors,lenientThemeColorOverrides,isReservedThemeId,parseStoredTheme,parseStoredThemes,readCustomThemeLibrarySnapshot,installCustomTheme,updateCustomTheme,removeCustomThemes,replaceCustomThemeCollection,updateThemeColorFamily,themeIdFromName,getThemeModes,getCustomThemes,removeCustomTheme};",
  )(data, converter, parse, { equals: (a, b) => JSON.stringify(a) === JSON.stringify(b) }, window);
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  writeFileSync(
    new URL("../crates/client/src/themes/named-colors.json", import.meta.url),
    JSON.stringify(named, null, 2) + "\n",
  );
  mkdirSync(new URL("../vendor/culori", import.meta.url), { recursive: true });
  writeFileSync(
    new URL("../vendor/culori/LICENSE", import.meta.url),
    readFileSync(new URL("LICENSE", culoriBase)),
  );
  const api = createThemeOracle(undefined);
  let rows = [];
  const add = (input) => {
    let canonical, hex;
    try {
      canonical = api.toCanonicalThemeColor(input);
      hex = typeof input === "string" ? api.themeColorToHex(input) : null;
    } catch (error) {
      rows.push({ input, originalError: String(error) });
      return;
    }
    rows.push({ input, canonical, hex });
  };
  for (const input of [
    null,
    0,
    {},
    [],
    true,
    "",
    "var(--foreground)",
    "currentColor",
    "transparent",
    "TRANSPARENT",
    " transparent ",
    ...Object.keys(named),
    ...Object.keys(named).map((k) => k.toUpperCase()),
    "#123",
    "#1234",
    "#abcdef",
    "#ABCDEF80",
    "112233",
    "12345",
    "rgb()",
    "rgb(1 2 3",
    "rgb(1 2 3)",
    "rgb(1-2+3)",
    "rgb(none none none)",
    "rgb(10% 0 50%)",
    "RGB(1 2 3)",
    "hsl(0 50 50)",
    "rgba(1, 2, 3, .5)",
    "rgb(50%, 0%, 100%)",
    "rgb(1%, 0, 100%)",
    "hsl(100grad,20%,50%)",
    "hsl(1rad 20% 50%)",
    "hwb(10 20 30)",
    "hwb(10 -20% 130%)",
    "oklch(none none none / none)",
    "oklch(.5 .2 none / none",
    "oklch(.5 .2 1turn / none)",
    "oklch(.5 1deg 2)",
    "lab(50% 20% -30%)",
    "lch(50% 20% 2rad)",
    "\ufeff#ABC\ufeff",
    "\u0085#ABC\u0085",
    "rgb(1\r2\r3)",
    "rgb(1,\r2,\r3)",
  ])
    add(input);
  for (const fn of ["rgb", "rgba", "hsl", "hsla", "hwb", "lab", "lch", "oklab", "oklch"])
    for (const x of [
      "none",
      "-2",
      "0",
      ".0078125",
      "0.5",
      "1",
      "50%",
      "100%",
      "1e2",
      "1e-20",
      "1e20",
      "1e100",
      "1e309",
      "1turn",
    ])
      for (const y of ["none", "0", ".2", "20%", "1e2"])
        for (const alpha of ["", " / .3", " / 50%", " / none"]) add(`${fn}(${x} ${y} 30${alpha})`);
  for (const profile of [
    "srgb",
    "srgb-linear",
    "display-p3",
    "a98-rgb",
    "prophoto-rgb",
    "rec2020",
    "xyz",
    "xyz-d50",
    "xyz-d65",
    "--hsv",
    "--lab-d65",
    "--lch-d65",
    "unknown",
  ])
    for (const x of ["none", "-1", "0", ".123456", "1", "20%", "1e2"])
      for (const alpha of ["", " / .5", " / none"]) add(`color(${profile} ${x} .2 .7${alpha})`);
  for (const fn of ["rgb", "rgba", "hsl", "hsla", "hwb", "lab", "lch", "oklab", "oklch"])
    for (const body of ["1 2 3", "none none none", "0% 50% 100%", "1e-309 .5 1e-20"])
      for (const suffix of [
        "",
        ")",
        ") ",
        ")junk",
        "))",
        " / none)",
        " / .5",
        " / 1deg)",
        " / .5 / .2)",
        ", .5)",
        ")/*x*/",
      ])
        add(`${fn}(${body}${suffix}`);
  for (const space of ["\r", "\v", "\f", "\u00a0", "\ufeff", "\u0085", "\u2003", "\u2028"])
    for (const fn of ["rgb", "hsl", "oklch"]) {
      add(`${fn}(1${space}2${space}3)`);
      add(`${fn}(1,${space}2,${space}3)`);
      add(`${space}${fn}(1 2 3)${space}`);
    }
  for (const profile of [
    "srgb",
    "srgb-linear",
    "display-p3",
    "a98-rgb",
    "prophoto-rgb",
    "rec2020",
    "xyz",
    "xyz-d50",
    "xyz-d65",
    "--hsv",
    "--lab-d65",
    "--lch-d65",
  ])
    for (const x of [
      "1e-309",
      "1e-100",
      "1e-20",
      "1e10",
      "-1e10",
      "1e20",
      "1e100",
      "-1e100",
      "1e309",
    ])
      add(`color(${profile} ${x} ${x} ${x})`);
  let seed = 173;
  const random = () => {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed / 2 ** 32;
  };
  for (let i = 0; i < 2500; i++) {
    const fn = ["rgb", "hsl", "oklch", "oklab", "lab", "lch"][i % 6];
    add(`${fn}(${random() * 100} ${random() * 100} ${random() * 360} / ${random()})`);
  }
  writeFileSync(
    new URL("../crates/client/tests/fixtures/theme-colors.jsonl", import.meta.url),
    rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
  );
  console.log(
    JSON.stringify({ colors: rows.length, errors: rows.filter((r) => r.originalError).length }),
  );
}
