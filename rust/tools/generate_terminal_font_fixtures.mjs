import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const surface = readFileSync(
  new URL("../../apps/web/src/terminal/ghostty/surface.ts", import.meta.url),
  "utf8",
);
const appearance = readFileSync(
  new URL("../../apps/web/src/appearanceFonts.ts", import.meta.url),
  "utf8",
);
function fn(source, name) {
  const start = source.indexOf(`function ${name}(`);
  if (start < 0) throw Error(name + " helper missing");
  const body = source.indexOf("{", start);
  let depth = 1,
    index = body + 1;
  while (depth && index < source.length) {
    if (source[index] === "{") depth++;
    else if (source[index] === "}") depth--;
    index++;
  }
  return stripTypeScriptTypes(source.slice(start, index));
}
const constants = surface
  .slice(
    surface.indexOf("export const DEFAULT_TERMINAL_FONT_SIZE"),
    surface.indexOf("const CONTENT_PADDING"),
  )
  .replaceAll("export ", "");
const helpers = new Function(
  constants +
    fn(surface, "quoteTerminalFontFamilies") +
    fn(surface, "uncheckedTerminalFontFamily") +
    fn(surface, "terminalFontSize") +
    fn(appearance, "areFontAdvancesMonospace") +
    "const MONOSPACE_ADVANCE_TOLERANCE=.01;return {quoteTerminalFontFamilies,uncheckedTerminalFontFamily,terminalFontSize,areFontAdvancesMonospace};",
)();
const rows = [];
for (const family of [
  "",
  ",",
  "Menlo",
  "SF Mono",
  "3270 Nerd Font",
  "M+ 1m",
  '"already quoted"',
  "'Single Quoted'",
  " a,b ,, ",
  "\ufeffMono\ufeff",
  "\u0085Mono\u0085",
  '"bad\nquote"',
  'two"quotes',
  "中文",
  "Font_Name",
  "-start",
  "0first",
]) {
  rows.push({ kind: "family", family, expected: helpers.quoteTerminalFontFamilies(family) });
  rows.push({ kind: "stack", family, expected: helpers.uncheckedTerminalFontFamily(family) });
}
for (const size of [-100, 0, 5.49, 5.5, 6, 11.49, 11.5, 12, 20, 31.49, 31.5, 32, 999])
  rows.push({ kind: "size", size, expected: helpers.terminalFontSize(size) });
for (const advances of [
  [],
  [0],
  [-1],
  [1, 1, 1],
  [1, 1.009],
  [1, 1.01],
  [1, 1.011],
  [20, 20, 20],
  [20, 19.999],
  [30, 40],
])
  rows.push({ kind: "advances", advances, expected: helpers.areFontAdvancesMonospace(advances) });
writeFileSync(
  new URL("../crates/terminal/tests/fixtures/fonts.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(rows.length + " original font witnesses");
