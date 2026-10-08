// Development oracle: original app code executes only here, never in the Rust UI.
// Build t3-terminal WASM and run wasm-bindgen --target nodejs first.
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes, createRequire } from "node:module";
const root = new URL("../../", import.meta.url);
const read = (name) =>
  readFileSync(new URL(`apps/web/src/terminal/ghostty/${name}.ts`, root), "utf8");
function compile(text, names, inject = {}) {
  const code = text.replace(/import\s+[\s\S]*?from\s+"[^"\n]+";/g, "").replace(/^export /gm, "");
  return new Function(
    ...Object.keys(inject),
    stripTypeScriptTypes(code) + `\nreturn {${names.join(",")}};`,
  )(...Object.values(inject));
}
const runtime = compile(read("runtime"), ["GhosttyRuntime", "loadGhosttyRuntime"], {
  ghosttyWasmUrl: "ghostty-vt.wasm",
  ghosttyWritePtyWasmUrl: "ghostty-write-pty.wasm",
  fetch: async (file) =>
    new Response(readFileSync(new URL(`rust/crates/terminal/vendor/${file}`, root))),
});
const keyboard = compile(read("keyCodes"), [
  "ghosttyConsumedMods",
  "ghosttyKeyForCode",
  "ghosttyUnshiftedCodepoint",
  "loadGhosttyKeyboardLayoutMap",
]);
const original = compile(
  read("core"),
  ["GhosttyTerminalCore", "GHOSTTY_CELL_WIDE", "ghosttyColorsEqual"],
  { ...runtime, ...keyboard },
);
const renderer = compile(
  read("renderer"),
  ["renderGhosttySnapshot", "measureGhosttyCell", "terminalGridSize"],
  original,
);
const selectionActions = compile(
  readFileSync(new URL("apps/web/src/lib/selectionActions.ts", root), "utf8"),
  ["SELECTION_MULTI_CLICK_INTERVAL_MS"],
);
const surface = compile(
  read("surface"),
  [
    "resolveTerminalMouseData",
    "resolveTerminalMouseTrackingState",
    "terminalWheelDeltaRows",
    "terminalWheelArrowData",
    "advanceTerminalSelectionClickSequence",
  ],
  selectionActions,
);
const surfaceRows = [];
for (const previous of [
  null,
  { count: 1, time: 100, x: 10, y: 20 },
  { count: 2, time: 100, x: 10, y: 20 },
  { count: 3, time: 100, x: 10, y: 20 },
])
  for (const time of [99, 100, 599, 600, 601])
    for (const [x, y] of [
      [10, 20],
      [14, 20],
      [14.01, 20],
      [13, 23],
    ]) {
      surfaceRows.push({
        kind: "click",
        previous,
        time,
        x,
        y,
        expected: surface.advanceTerminalSelectionClickSequence(previous, {
          timeStamp: time,
          clientX: x,
          clientY: y,
        }),
      });
    }
for (const action of ["press", "release", "motion"])
  for (const data of ["", "report", "other"])
    for (const previous of ["", "report"])
      surfaceRows.push({
        kind: "mouse",
        action,
        data,
        previous,
        expected: surface.resolveTerminalMouseData(action, data, previous),
      });
for (const previous of [false, true])
  for (const tracking of [false, true])
    for (const data of ["", "report"])
      surfaceRows.push({
        kind: "tracking",
        previous,
        tracking,
        data,
        expected: surface.resolveTerminalMouseTrackingState(previous, tracking, data),
      });
for (const mode of [0, 1, 2])
  for (const delta of [-17, -1, -0.2, 0, 0.2, 1, 17])
    for (const remainder of [-0.7, 0, 0.7]) {
      const expected = surface.terminalWheelDeltaRows(
        { deltaY: delta, deltaMode: mode },
        16,
        24,
        remainder,
      );
      surfaceRows.push({
        kind: "wheel",
        delta,
        mode,
        height: 16,
        viewportRows: 24,
        remainder,
        expected,
      });
    }
for (const rows of [-3, -1, 0, 1, 3])
  for (const application of [false, true])
    surfaceRows.push({
      kind: "arrows",
      rows,
      application,
      expected: surface.terminalWheelArrowData(rows, application),
    });
writeFileSync(
  new URL("../crates/terminal/tests/fixtures/surface.jsonl", import.meta.url),
  surfaceRows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
const rust = createRequire(import.meta.url)(
  new URL("../target/terminal-node/t3_terminal.js", import.meta.url).pathname,
);
const theme = {
  foreground: { r: 255, g: 255, b: 255 },
  background: { r: 0, g: 0, b: 0 },
  cursor: { r: 255, g: 255, b: 255 },
};
const frames = [];
let comparisons = 0;
function snapshot(core) {
  const raw = core.snapshot();
  return JSON.parse(JSON.stringify({ ...raw, dirtyRows: [...raw.dirtyRows] }));
}
function compare(left, right, label) {
  assert.deepEqual(right, left, label);
  comparisons++;
}
function record(options) {
  const ops = [];
  const context = { canvas: { width: 320, height: 160 } };
  for (const method of [
    "save",
    "restore",
    "resetTransform",
    "beginPath",
    "clip",
    "fillRect",
    "strokeRect",
    "rect",
    "fillText",
  ]) {
    context[method] = (...args) => ops.push(args.length ? { op: method, args } : { op: method });
  }
  for (const name of ["fillStyle", "strokeStyle", "textBaseline", "font"]) {
    Object.defineProperty(context, name, {
      set(value) {
        ops.push({ op: name, args: value });
      },
    });
  }
  renderer.renderGhosttySnapshot({
    ...options,
    context,
    snapshot: { ...options.snapshot, dirtyRows: new Set(options.snapshot.dirtyRows) },
  });
  return ops;
}
const cases = [
  ["plain", ["hello\r\nworld", "!", "\x1b[2J\x1b[Hclear"]],
  ["unicode", ["汉🙂a\u0301\r\n", "x".repeat(15) + "🙂", "\ufeffend"]],
  [
    "style",
    [
      "\x1b[31;1;3;4mred\x1b[0m normal",
      "\r\n\x1b[2;7mFaintInverse\x1b[0m",
      "\x1b[8;9;53mhide\x1b[0m",
    ],
  ],
  [
    "alternate",
    ["primary\r\n", "\x1b[?1049halt\r\nsecond", "\x1b[?1049l", "\x1b[2 q", "\x1b[5 q", "\x1b[0 q"],
  ],
  ["scrollback", [Array.from({ length: 20 }, (_, i) => `row${i}\r\n`).join(""), "next\r\n"]],
  ["reset", ["\x1b[?25lhide", "\x1bcreset"]],
];
for (const [label, writes] of cases) {
  const responses = [],
    rs = [];
  const source = await original.GhosttyTerminalCore.create(12, 3, 8, 16, theme, (data) =>
    responses.push(data),
  );
  const port = await rust.create_terminal(12, 3, JSON.stringify(theme));
  port.resize(12, 3, 8, 16);
  port.set_writer((data) => rs.push(data));
  compare(snapshot(source), JSON.parse(port.snapshot_json()), label + " initial");
  let previous = null;
  for (const data of writes) {
    source.write(data);
    port.write(data);
    const expected = snapshot(source);
    compare(expected, JSON.parse(port.snapshot_json()), label + " write");
    for (const forceFull of [false, true])
      for (const focused of [false, true]) {
        const options = {
          metrics: { width: 8, height: 16, baseline: 12 },
          fontSize: 12,
          fontFamily: "monospace",
          padding: 4,
          forceFull,
          cursorOn: true,
          previousCursorY: previous,
          focused,
        };
        frames.push({
          snapshot: expected,
          ...options,
          canvas: [320, 160],
          expected: record({ ...options, snapshot: expected }),
        });
      }
    previous = expected.cursorY;
  }
  source.write("\x1b[6n");
  port.write("\x1b[6n");
  compare(responses, rs, label + " PTY");
  const count = rs.length;
  source.resetAndWrite("replayed\x1b[6n\r\n");
  port.reset_and_write("replayed\x1b[6n\r\n");
  compare(snapshot(source), JSON.parse(port.snapshot_json()), label + " replay");
  assert.equal(rs.length, count, "historical replay must not send PTY responses");
  source.write("\x1b[6n");
  port.write("\x1b[6n");
  compare(responses, rs, label + " writer restored");
  source.dispose();
  port.free();
}
// Encoding must use the engine's active legacy/Kitty/mouse/paste modes.
const keySource = await original.GhosttyTerminalCore.create(12, 3, 8, 16, theme, () => {});
const keyRust = await rust.create_terminal(12, 3, JSON.stringify(theme));
const keyFixtures = [];
for (const mode of ["", "\x1b[?1h", "\x1b[>1u", "\x1b[>31u"]) {
  keySource.resetAndWrite(mode);
  keyRust.reset_and_write(mode);
  for (const [code, key] of [
    ["KeyA", "a"],
    ["KeyA", "A"],
    ["KeyC", "c"],
    ["Digit1", "!"],
    ["IntlRo", "é"],
    ["KeyI", "İ"],
    ["KeyX", "🙂"],
    ["Space", " "],
    ["Enter", "Enter"],
    ["Tab", "Tab"],
    ["Backspace", "Backspace"],
    ["Escape", "Escape"],
    ["ArrowUp", "ArrowUp"],
    ["Delete", "Delete"],
    ["F1", "F1"],
    ["Numpad1", "1"],
  ])
    for (let mods = 0; mods < 16; mods++)
      for (const release of [false, true]) {
        const input = {
          code,
          key,
          shiftKey: !!(mods & 1),
          ctrlKey: !!(mods & 2),
          altKey: !!(mods & 4),
          metaKey: !!(mods & 8),
          repeat: false,
          isComposing: false,
          capsLock: false,
          numLock: false,
          release,
        };
        const event = {
          ...input,
          getModifierState: (name) => (name === "CapsLock" ? input.capsLock : input.numLock),
        };
        compare(
          keySource.encodeKey(event, release ? "release" : "press"),
          keyRust.encode_key(JSON.stringify(input)),
          `key ${mode} ${code} ${mods} ${release}`,
        );
        if (!mode && !release)
          keyFixtures.push({
            input,
            key: keyboard.ghosttyKeyForCode(code),
            consumed: keyboard.ghosttyConsumedMods(event),
            unshifted: keyboard.ghosttyUnshiftedCodepoint(event),
          });
      }
}
for (const mode of ["", "\x1b[?2004h"]) {
  keySource.resetAndWrite(mode);
  keyRust.reset_and_write(mode);
  for (const text of ["", "hello", "é🙂\r\n", "\ufeffBOM", "\x1b[201~unsafe", "\n\r\t", "\0"])
    compare(keySource.encodePaste(text), keyRust.encode_paste(text), "paste " + mode);
}
for (const mode of ["", "\x1b[?1000h", "\x1b[?1002h\x1b[?1006h", "\x1b[?1003h\x1b[?1016h"]) {
  keySource.resetAndWrite(mode);
  keyRust.reset_and_write(mode);
  for (const action of ["press", "release", "motion"])
    for (const button of [null, 0, 1, 2, 4, 5])
      for (const mods of [0, 1, 2, 4, 8]) {
        const input = {
          action,
          button,
          mods,
          x: 23.2,
          y: 17.9,
          screenWidth: 160,
          screenHeight: 80,
          cellWidth: 8,
          cellHeight: 16,
          paddingLeft: 4,
          paddingRight: 4,
          paddingTop: 4,
          paddingBottom: 4,
          anyButtonPressed: action === "motion",
        };
        compare(
          keySource.encodeMouse(input),
          keyRust.encode_mouse(JSON.stringify(input)),
          "mouse " + mode + action + button + mods,
        );
      }
}
keySource.resetAndWrite(
  "one two\r\n汉🙂three\r\n\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
);
keyRust.reset_and_write(
  "one two\r\n汉🙂three\r\n\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
);
for (let row = 0; row < 3; row++)
  for (let col = 0; col < 12; col++) {
    compare(
      keySource.hyperlinkAt(col, row) ?? null,
      keyRust.hyperlink_at(col, row) ?? null,
      "hyperlink",
    );
    compare(
      keySource.selectWord(col, row),
      JSON.parse(keyRust.select_word_json(col, row)),
      "word selection",
    );
    compare(keySource.selectionText(), keyRust.selection_text(), "word text");
    compare(snapshot(keySource), JSON.parse(keyRust.snapshot_json()), "selected cells");
    compare(
      keySource.selectLine(col, row),
      JSON.parse(keyRust.select_line_json(col, row)),
      "line selection",
    );
    compare(keySource.selectionText(), keyRust.selection_text(), "line text");
  }
keySource.setSelection({ x: 0, y: 0 }, { x: 5, y: 1 });
keyRust.set_selection_json('{"x":0,"y":0}', '{"x":5,"y":1}');
compare(keySource.selectionText(), keyRust.selection_text(), "range selection");
keySource.selectAll();
keyRust.select_all();
compare(keySource.selectionText(), keyRust.selection_text(), "all selection");
keySource.clearSelection();
keyRust.clear_selection();
compare(keySource.selectionText(), keyRust.selection_text(), "clear selection");
keySource.write(Array.from({ length: 20 }, (_, i) => `row${i}\r\n`).join(""));
keyRust.write(Array.from({ length: 20 }, (_, i) => `row${i}\r\n`).join(""));
for (const delta of [-3, -2, 1, 99]) {
  keySource.scroll(delta);
  keyRust.scroll(delta);
  compare(keySource.scrollbarState(), JSON.parse(keyRust.scrollbar_json()), "scrollbar");
  compare(keySource.isViewportActive(), keyRust.is_viewport_active(), "viewport active");
  compare(snapshot(keySource), JSON.parse(keyRust.snapshot_json()), "scroll cells");
  for (let row = 0; row < 3; row++)
    compare(
      keySource.viewportPointToScreen(0, row),
      JSON.parse(keyRust.point_json(0, row, 1, 2)),
      "coordinate conversion",
    );
}
keySource.dispose();
keyRust.free();
writeFileSync(
  new URL("../crates/terminal/tests/fixtures/keyboard.jsonl", import.meta.url),
  keyFixtures.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
// These calls reenter the shared immutable WASM callback while terminal A's
// writer remains active; a borrowed registry or FnMut trampoline panics here.
const a = await rust.create_terminal(8, 3, JSON.stringify(theme));
const b = await rust.create_terminal(8, 3, JSON.stringify(theme));
const seen = [];
a.set_writer((data) => {
  seen.push(["a", data]);
  b.write("\x1b[6n");
});
b.set_writer((data) => seen.push(["b", data]));
a.write("\x1b[6n");
assert.deepEqual(seen, [
  ["a", "\x1b[1;1R"],
  ["b", "\x1b[1;1R"],
]);
// Removing another writer from inside a callback must not retain a map borrow.
a.set_writer(() => b.set_writer((data) => seen.push(["replacement", data])));
a.write("\x1b[6n");
b.write("\x1b[6n");
assert.equal(seen.at(-1)[0], "replacement");
a.free();
b.free();
// Add renderer-only witnesses for selected runs, wide spacer tails, decorations
// and previous/current cursor rows that otherwise depend on mouse selection.
const base = structuredClone(
  frames.find((f) => f.snapshot.rowData.some((r) => r.cells.some((c) => c.text.includes("汉")))),
);
base.snapshot.rowData[0].cells[0].selected = true;
base.snapshot.rowData[0].cells[1].foreground = { r: 1, g: 2, b: 3 };
base.snapshot.rowData[0].cells[2].underline = true;
base.snapshot.rowData[0].cells[3].overline = true;
base.snapshot.rowData[0].cells[4].strikethrough = true;
for (const cursorStyle of [0, 1, 2, 3])
  for (const cursorOn of [false, true]) {
    const fixture = {
      ...base,
      forceFull: false,
      cursorOn,
      previousCursorY: 2,
      originY: 9,
      selectionBackground: "rgba(1,2,3,0.4)",
      hoveredLinkRange: { start: { x: 0, y: 0 }, end: { x: 4, y: 1 } },
      snapshot: {
        ...base.snapshot,
        cursorX: 0,
        cursorY: 0,
        cursorVisible: true,
        cursorStyle,
        dirtyRows: [1],
      },
    };
    fixture.expected = record(fixture);
    frames.push(fixture);
  }
writeFileSync(
  new URL("../crates/terminal/tests/fixtures/renderer.jsonl", import.meta.url),
  frames.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(
  JSON.stringify({
    engineComparisons: comparisons,
    nestedCallback: true,
    replayWriterSilenced: true,
    rendererFixtures: frames.length,
    surfacePolicyFixtures: surfaceRows.length,
  }),
);
