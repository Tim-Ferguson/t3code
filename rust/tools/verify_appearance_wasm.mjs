// Executes the actual Rust WASM DOM utilities against the original font policy.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire, stripTypeScriptTypes } from "node:module";
const require = createRequire(import.meta.url),
  path = new URL("../target/terminal-node/t3_terminal.js", import.meta.url).pathname;
let scenario = {};
function width(font, text) {
  if (
    scenario.throwCanvas &&
    (scenario.throwCanvas === "all" || font.startsWith(scenario.throwCanvas))
  )
    throw Error("unmeasurable");
  if (scenario.zero) return 0;
  if (text.length === 1) return font.includes("Proportional") ? (text === "i" ? 4 : 8) : 8;
  return font.includes("Present") || font.includes("Proportional") || font.includes("Menlo")
    ? 110
    : 100;
}
class Context {
  font = "";
  measureText(text) {
    return { width: width(this.font, text) };
  }
}
class Canvas {
  getContext() {
    return scenario.noCanvas ? null : new Context();
  }
}
class Style {
  values = {};
  setProperty(key, value) {
    this.values[key] = value;
  }
  removeProperty(key) {
    const old = this.values[key] ?? "";
    delete this.values[key];
    return old;
  }
  set fontFamily(value) {
    this.values["font-family"] = value;
  }
  get fontFamily() {
    return this.values["font-family"];
  }
  set fontSize(value) {
    this.values["font-size"] = value;
  }
  get fontSize() {
    return this.values["font-size"];
  }
}
class Element {
  style = new Style();
  getBoundingClientRect() {
    return {
      width: scenario.domZero
        ? 0
        : this.style.fontFamily?.includes("Menlo")
          ? 108
          : this.style.fontFamily?.includes("monospace")
            ? 108
            : 100,
    };
  }
  remove() {}
  appendChild() {}
}
class Window {
  static [Symbol.hasInstance](value) {
    return value === globalThis;
  }
}
Object.assign(globalThis, {
  Window,
  HTMLCanvasElement: Canvas,
  CanvasRenderingContext2D: Context,
  HTMLElement: Element,
  document: {
    createElement(name) {
      return name === "canvas" ? new Canvas() : new Element();
    },
    body: new Element(),
    documentElement: new Element(),
  },
});
Object.defineProperty(globalThis, "navigator", {
  value: { platform: "MacIntel", permissions: { query: async () => ({ state: "prompt" }) } },
  configurable: true,
});
globalThis.window = globalThis;
const source = readFileSync(
  new URL("../../apps/web/src/appearanceFonts.ts", import.meta.url),
  "utf8",
)
  .replace(/import\s+[\s\S]*?from\s+"[^"\n]+";/g, "")
  .replace(/^export /gm, "");
const compile = () =>
  new Function(
    "DEFAULT_INTERFACE_FONT_SIZE",
    "DEFAULT_PROMPT_FONT_SIZE",
    "DEFAULT_CODE_FONT_SIZE",
    "MIN_INTERFACE_FONT_SIZE",
    "MAX_INTERFACE_FONT_SIZE",
    "MIN_PROMPT_FONT_SIZE",
    "MAX_PROMPT_FONT_SIZE",
    "MIN_CODE_FONT_SIZE",
    "MAX_CODE_FONT_SIZE",
    stripTypeScriptTypes(source) +
      ";return {isFontFamilyAvailable,isMonospaceFamily,resolveDefaultFamilyLabel,applyAppearanceFontVariables,queryInstalledFontFamilies};",
  )(16, 14, 13, 12, 20, 12, 20, 10, 18);
const fresh = () => {
  delete require.cache[require.resolve(path)];
  return require(path);
};
let rust = fresh(),
  count = 0;
for (scenario of [
  {},
  { noCanvas: true },
  { zero: true },
  { throwCanvas: "all" },
  { throwCanvas: "normal 400" },
  { throwCanvas: "italic 700" },
  { domZero: true },
]) {
  const original = compile();
  for (const family of [
    "",
    " ",
    "\ufeffPresent\ufeff",
    "\u0085Present",
    "monospace",
    "MONOSPACE",
    "sans-serif",
    "Absent",
    "Present",
    "Proportional",
    "Menlo",
    "Proportional, Present",
    '"Present Family"',
  ]) {
    assert.deepEqual(
      JSON.parse(rust.appearance_probe_font(family)),
      {
        available: original.isFontFamilyAvailable(family),
        monospace: original.isMonospaceFamily(family),
      },
      `${JSON.stringify(scenario)} ${family}`,
    );
    count++;
  }
  const actual = JSON.parse(rust.appearance_default_fonts());
  assert.equal(
    actual.sans,
    original.resolveDefaultFamilyLabel(
      '-apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif',
    ),
  );
  assert.equal(
    actual.code,
    original.resolveDefaultFamilyLabel(
      '"SF Mono", "SFMono-Regular", Menlo, Consolas, "Liberation Mono", monospace',
    ),
  );
  count += 2;
}
scenario = {};
for (const family of ["", "Present", 'Present, "Other Face"', "\ufeffProportional\ufeff"])
  for (const size of [1, 12, 14.5, 20, 99])
    for (const smoothing of [false, true]) {
      const original = compile(),
        root = new Element();
      original.applyAppearanceFontVariables(root, {
        sans: family,
        code: family,
        composer: family,
        sizeInterface: size,
        sizePrompt: size,
        sizeCode: size,
        smoothing,
      });
      globalThis.document.documentElement = new Element();
      rust.appearance_apply_fonts(
        JSON.stringify({
          fontFamilySans: family,
          fontFamilyCode: family,
          fontFamilyComposer: family,
          fontSizeInterface: size,
          fontSizePrompt: size,
          fontSizeCode: size,
          fontSmoothing: smoothing,
        }),
      );
      assert.deepEqual(globalThis.document.documentElement.style.values, root.style.values);
      count++;
    }
// Unsupported and successful enumeration cache; denial/empty results can retry.
for (const result of [
  undefined,
  [],
  [{ family: ".Private" }, { family: "Menlo" }, { family: "Arial" }, { family: "Menlo" }],
  new Error("denied"),
]) {
  let calls = 0;
  globalThis.queryLocalFonts =
    result === undefined
      ? undefined
      : async () => {
          calls++;
          if (result instanceof Error) throw result;
          return result;
        };
  rust = fresh();
  const original = compile();
  const expected = await original.queryInstalledFontFamilies();
  calls = 0;
  assert.deepEqual(JSON.parse(await rust.appearance_query_fonts()), expected);
  const first = calls;
  await rust.appearance_query_fonts();
  assert.equal(
    calls,
    expected.status === "granted" || expected.status === "unsupported" ? first : first + 1,
  );
  count++;
}
console.log(JSON.stringify({ comparisons: count, actualRustWasm: true }));
