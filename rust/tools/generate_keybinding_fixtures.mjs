// Runs the original keybindings runtime (parsing, resolution, startup sync, upsert/remove and the
// error envelopes) and records what it produces. The Rust tests replay the rows and require the
// same values, issue texts, persisted bytes and wire errors. Also writes the original's default
// rule list (key and `when` as written) beside the server source, because the startup sync
// persists those exact strings.
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Logger from "../../apps/server/node_modules/effect/dist/Logger.js";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as ServerConfig from "../../apps/server/src/config.ts";
import * as Keybindings from "../../apps/server/src/keybindings.ts";
import * as Shared from "../../packages/shared/src/keybindings.ts";
import * as KeybindingContracts from "../../packages/contracts/src/keybindings.ts";
import * as ServerContracts from "../../packages/contracts/src/server.ts";

const rows = [];
const json = (value) => JSON.parse(JSON.stringify(value));

// ---- constants -------------------------------------------------------------------------------
rows.push({
  type: "constants",
  commands: [...KeybindingContracts.STATIC_KEYBINDING_COMMANDS],
  max: KeybindingContracts.MAX_KEYBINDINGS_COUNT,
  maxWhenDepth: KeybindingContracts.MAX_WHEN_EXPRESSION_DEPTH,
  defaults: json(Shared.DEFAULT_KEYBINDINGS),
});

// ---- shortcut parsing ------------------------------------------------------------------------
const shortcuts = [
  "mod+j",
  "mod++",
  "mod+shift+d+o",
  "+",
  "a+",
  "++",
  "+a",
  "mod+",
  "Cmd+K",
  "CTRL+Alt+X",
  "esc",
  "ESC",
  "space",
  "mod+space",
  "mod+ esc",
  "  mod + j ",
  "mod+shift",
  "shift",
  "",
  " ",
  "mod+plus",
  "a++",
  "mod+++",
  "mod+shift++",
  "ctrl+control+j",
  "meta+cmd+j",
  "option+x",
  "mod+[",
  "mod+ ",
  " mod+j ",
  "MOD+ARROWUP",
  "mod+é",
  "İ",
  "ǅ",
  "ΑΣ",
  "mod+alt+shift+a",
  "mod+﻿j",
  "﻿",
  "a+b",
  "+ +",
  "mod+\t+j",
  "mod+=",
  "mod+-",
  "mod+0",
  "mod+shift+1",
  "alt+arrowup",
  "mod+enter",
  "mod+Enter ",
  "1",
  "f12",
  "mod+shift+/",
  "mod+`",
  "mod+,",
  "mod+.",
  "mod+shift+\\",
  "mod+😀",
  "😀",
  "mod+a b",
  "ctrl+",
  "ctrl+ctrl",
  "mod+mod",
  "mod+shift+shift+k",
  "+++",
  "+ + +",
  "mod+ + ",
  "mod+j+",
  "meta+",
  "esc+",
  "space+space",
];
for (const input of shortcuts)
  rows.push({ type: "shortcut", input, shortcut: json(Shared.parseKeybindingShortcut(input)) });

// ---- when expressions ------------------------------------------------------------------------
const nested = (depth) => `${"(".repeat(depth)}a${")".repeat(depth)}`;
const whens = [
  "terminalFocus",
  "!terminalFocus",
  "!!terminalFocus",
  "a && b",
  "a || b",
  "a && b || c",
  "a || b && c",
  "(a || b) && c",
  "!(a || b)",
  "!a && !b",
  "a && (b || (c && d))",
  "a&&b",
  "a||b",
  " a ",
  "",
  " ",
  "a &&",
  "&& a",
  "a b",
  "a & b",
  "a | b",
  "(a",
  "a)",
  "()",
  "(",
  ")",
  "!",
  "a.b",
  "a-b",
  "a_b",
  "A1",
  "1a",
  "a1.b-c_d",
  "-a",
  ".a",
  "_a",
  "a.",
  "a-",
  "a !b",
  "a!",
  "!(!a)",
  "a && !b || !(c && d)",
  "terminalFocus && (",
  "isDesktop",
  "modelPickerOpen && isDesktop",
  "a\n&&\nb",
  "a\t||\tb",
  "a && b",
  "é",
  "a && é",
  "a == b",
  "a = b",
  "true",
  "a && true",
  "'a'",
  '"a"',
  "a && b && c && d && e",
  "a || b || c",
  "((a))",
  "(a) && (b)",
  "!(a) || !(b)",
  "!".repeat(64) + "a",
  "!".repeat(65) + "a",
  "!".repeat(300) + "a",
  nested(63),
  nested(64),
  nested(65),
  nested(100),
  `${"!(".repeat(30)}a${")".repeat(30)}`,
  Array.from({ length: 40 }, (_, i) => `v${i}`).join(" && "),
  Array.from({ length: 40 }, (_, i) => `v${i}`).join(" || "),
];
for (const input of whens)
  rows.push({ type: "when", input, ast: json(Shared.parseKeybindingWhenExpression(input)) });

// ---- rule compilation, and encoding back to a key string ------------------------------------
const encodeResolved = Schema.encodeSync(Keybindings.ResolvedKeybindingFromConfig);
const compactResolved = (rules) =>
  rules.map((rule) => {
    const encoded = encodeResolved(rule);
    return [encoded.command, encoded.key, encoded.when ?? null];
  });
const rules = [
  { key: "mod+j", command: "terminal.toggle" },
  { key: "mod+d", command: "terminal.split", when: "terminalOpen && !terminalFocus" },
  { key: "mod+shift+d+o", command: "terminal.new" },
  { key: "mod+d", command: "terminal.split", when: "terminalFocus && (" },
  { key: "mod+d", command: "terminal.split", when: `${"!".repeat(300)}terminalFocus` },
  { key: "mod++", command: "terminal.toggle" },
  { key: "mod+space", command: "chat.new", when: "a || b && !c" },
  { key: "Cmd+Shift+K", command: "chat.new", when: "(a)" },
  { key: "esc", command: "chat.new" },
  { key: "mod+x", command: "script.run-tests.run" },
];
for (const rule of rules) {
  const resolved = Shared.compileResolvedKeybindingRule(rule);
  rows.push({
    type: "compile",
    rule,
    resolved: json(resolved),
    encoded: resolved === null ? null : json(encodeResolved(resolved)),
  });
}
rows.push({ type: "defaults", resolved: json(Shared.DEFAULT_RESOLVED_KEYBINDINGS) });

// ---- the runtime over real files --------------------------------------------------------------
const quiet = Logger.layer([], { mergeWithExisting: false });
const withService = async (setup, body) => {
  const home = mkdtempSync(NodePath.join(tmpdir(), "t3-keybindings-fixture-"));
  const layer = Keybindings.layer.pipe(
    Layer.provideMerge(ServerConfig.layerTest(home, home)),
    Layer.provideMerge(NodeServices.layer),
    Layer.provideMerge(quiet),
  );
  const config = await Effect.runPromise(
    ServerConfig.ServerConfig.pipe(
      Effect.provide(ServerConfig.layerTest(home, home)),
      Effect.provide(NodeServices.layer),
    ),
  );
  const configPath = config.keybindingsConfigPath;
  mkdirSync(NodePath.dirname(configPath), { recursive: true });
  const migrations = NodePath.join(NodePath.dirname(configPath), "keybindings-migrations");
  setup({ configPath, migrations });
  try {
    return await body({
      configPath,
      migrations,
      run: (effect) => Effect.runPromise(Effect.exit(effect).pipe(Effect.provide(layer))),
    });
  } finally {
    try {
      chmodSync(NodePath.dirname(configPath), 0o700);
    } catch {}
    rmSync(home, { recursive: true, force: true });
  }
};
const scrub = (value, configPath) =>
  JSON.parse(JSON.stringify(value).replaceAll(JSON.stringify(configPath).slice(1, -1), "<CONFIG>"));
const failureOf = (exit, configPath) => {
  const error = exit.cause.reasons[0].error;
  return scrub(
    Schema.encodeSync(Schema.toCodecJson(KeybindingContracts.KeybindingsConfigError))(error),
    configPath,
  );
};
const readIf = (path) => (existsSync(path) ? readFileSync(path, "utf8") : null);

// State of a file as the runtime loads it (issues and resolved bindings).
const fileContents = [
  "{ not-json",
  "",
  "   ",
  "null",
  "{}",
  '{"key":"mod+j","command":"terminal.toggle"}',
  "[]",
  "[ ]",
  "[1]",
  '["x"]',
  "[null]",
  "[[]]",
  "[true]",
  '[{"key":"mod+j","command":"terminal.toggle"}]',
  '[{"key":"mod+j","command":"terminal.toggle"},{"key":"mod+shift+d+o","command":"terminal.new"},{"key":"mod+x","command":"invalid.command"}]',
  '[{"key":"mod+j","command":"terminal.toggle"},] ',
  '// comment\n[\n  /* block */ {"key":"mod+k","command":"chat.new"}, // trailing\n]',
  '[{"key":" mod+j ","command":"terminal.toggle","when":" a && b "}]',
  '[{"key":"","command":"chat.new"}]',
  '[{"key":"   ","command":"chat.new"}]',
  '[{"command":"chat.new"}]',
  '[{"key":"mod+j"}]',
  '[{"key":5,"command":"chat.new"}]',
  '[{"key":"mod+j","command":5}]',
  '[{"key":"mod+j","command":null}]',
  '[{"key":"mod+j","command":"chat.new","when":5}]',
  '[{"key":"mod+j","command":"chat.new","when":null}]',
  '[{"key":"mod+j","command":"chat.new","when":""}]',
  '[{"key":"mod+j","command":"chat.new","when":"a &&"}]',
  '[{"key":"mod+j","command":"chat.new","when":"   "}]',
  `[{"key":"${"k".repeat(65)}","command":"chat.new"}]`,
  `[{"key":"${"k".repeat(64)}","command":"chat.new"}]`,
  `[{"key":"mod+j","command":"chat.new","when":"${"w".repeat(257)}"}]`,
  `[{"key":"mod+j","command":"chat.new","when":"${"w".repeat(256)}"}]`,
  '[{"key":"mod+j","command":"script.run-tests.run"},{"key":"mod+k","command":"script.Bad.run"},{"key":"mod+l","command":"script..run"},{"key":"mod+m","command":"script.-x.run"},{"key":"mod+n","command":"script.' +
    "a".repeat(25) +
    '.run"}]',
  '[{"key":"mod+j","command":"chat.new","extra":1}]',
  '[{"key":"mod+j","command":"chat.new"},{"key":"mod+j","command":"chat.new"}]',
  '[{"key":"mod+shift+d+o","command":"terminal.new"},{"key":"mod+x","command":"chat.new","when":"("}]',
  '[{"key":"mod+a","command":"chat.new"},{"key":"mod+b","command":"chat.newLocal"}]',
  '[{"key":null,"command":"chat.new"}]',
  '[{"key":true,"command":"chat.new"}]',
  '[{"key":[],"command":"chat.new"}]',
  '[{"key":{},"command":"chat.new"}]',
  '[{"key":5.5,"command":"chat.new"}]',
  '[{"key":"mod+j","command":true}]',
  '[{"key":"mod+j","command":[]}]',
  '[{"key":"mod+j","command":{}}]',
  '[{"key":"mod+j","command":"chat.new","when":true}]',
  '[{"key":"mod+j","command":"chat.new","when":[]}]',
  '[{"key":"mod+j","command":"chat.new","when":{}}]',
  '[{"key":"","command":"bad","when":5}]',
  '[{"command":"bad","when":5}]',
  '[{"key":"mod+j"}]',
  "[{}]",
  '[{"key":"mod+j","command":"script.a.run"},{"key":"mod+k","command":"script.0.run"},{"key":"mod+l","command":"script.a_b.run"}]',
  `[{"key":"${"😀".repeat(33)}","command":"chat.new"}]`,
  `[{"key":"${"😀".repeat(32)}","command":"chat.new"}]`,
  '[{"key":"\\u00a0 mod+j \\u00a0","command":"chat.new"}]',
  '[{"key":"\\ufeff","command":"chat.new"}]',
  '[{"key":"mod+j","command":"chat.new","when":"\\u00a0\\u00a0"}]',
  '[{"key":"mod+j","command":"chat.new","when":"a\\u00a0&&\\u00a0b"}]',
  '[{"key":"mod+j","command":"chat.new","when":"é"}]',
  '[{"key":"mod+j","command":"chat.new"}, 5, {"key":"x+y+z","command":"chat.new"}, null]',
  "\ufeff[]",
  "[]\n// trailing",
  "[]]",
  "[,]",
  "[1,]",
  '[{"key":"mod+j","command":"chat.new"}',
  "not json at all",
  '"a string"',
  "123",
  "true",
  JSON.stringify(
    Array.from({ length: 256 }, (_, i) => ({
      key: "mod+alt+f1",
      command: `script.filler-${i}.run`,
    })),
  ),
  JSON.stringify(
    Array.from({ length: 257 }, (_, i) => ({
      key: "mod+alt+f1",
      command: `script.filler-${i}.run`,
    })),
  ),
  JSON.stringify(
    Array.from({ length: 200 }, (_, i) => ({
      key: "mod+alt+f1",
      command: `script.filler-${i}.run`,
    })),
  ),
];
const fullStates = new Set([fileContents[13], fileContents[17], fileContents[16]]);
for (const content of fileContents) {
  const result = await withService(
    ({ configPath }) => writeFileSync(configPath, content),
    async ({ run }) => {
      const exit = await run(
        Effect.gen(function* () {
          return yield* (yield* Keybindings.Keybindings).loadConfigState;
        }),
      );
      if (exit._tag !== "Success") return { error: "unexpected" };
      // Compact (command, key, when) rows keep the file small; three states also keep the full structure.
      return {
        issues: json(exit.value.issues),
        keybindings: compactResolved(exit.value.keybindings),
        full: fullStates.has(content) ? json(exit.value.keybindings) : undefined,
      };
    },
  );
  rows.push({ type: "state", content, ...result });
}
// A directory where the file should be: the read itself fails, and a later good file recovers.
rows.push(
  await withService(
    ({ configPath }) => mkdirSync(configPath),
    async ({ configPath, run }) => {
      const read = await run(
        Effect.gen(function* () {
          return yield* (yield* Keybindings.Keybindings).loadConfigState;
        }),
      );
      return {
        type: "unreadable",
        loadDetail: failureOf(read, configPath).detail,
        loadCause: failureOf(read, configPath).cause?.name,
      };
    },
  ),
);

// Startup sync.
const lateRule = {
  key: "mod+alt+enter",
  command: "composer.sendBackground",
  when: "composerFocus && draftThreadRoute",
};
const filler = (count) =>
  Array.from({ length: count }, (_, i) => ({
    key: "mod+alt+f1",
    command: `script.filler-${i}.run`,
  }));
const syncScenarios = [
  { name: "missing file", file: null },
  {
    name: "missing file with migrations already applied",
    file: null,
    migrations: "composer.sendBackground:mod+enter\n",
  },
  { name: "empty array", file: "[]" },
  {
    name: "custom terminal.toggle and a script",
    file: JSON.stringify([
      { key: "mod+shift+y", command: "terminal.toggle" },
      { key: "mod+shift+r", command: "script.run-tests.run" },
    ]),
  },
  {
    name: "late default joins an untouched earlier default",
    file: JSON.stringify([lateRule]),
    runs: 2,
    rewriteBetween: JSON.stringify([lateRule]),
  },
  {
    name: "customized command is left alone",
    file: JSON.stringify([
      {
        key: "alt+b",
        command: "composer.sendBackground",
        when: "composerFocus && draftThreadRoute",
      },
    ]),
  },
  {
    name: "late default stays pending while the config is full",
    file: JSON.stringify([lateRule, ...filler(255)]),
    runs: 2,
    rewriteBetween: JSON.stringify([lateRule]),
  },
  {
    name: "conflicting default is skipped",
    file: JSON.stringify([{ key: "mod+j", command: "script.custom-action.run" }]),
  },
  { name: "malformed file is not touched", file: "{ not-json" },
  {
    name: "invalid entries skip the sync",
    file: JSON.stringify([
      { key: "mod+j", command: "terminal.toggle" },
      { key: "x", command: "nope" },
    ]),
  },
  { name: "already complete", file: JSON.stringify(Shared.DEFAULT_KEYBINDINGS) },
  {
    name: "comments and trailing commas are rewritten",
    file: '[\n // mine\n {"key":"mod+j","command":"terminal.toggle"},\n]',
  },
  { name: "full config evicts nothing", file: JSON.stringify(filler(256)) },
  { name: "nearly full config takes what fits", file: JSON.stringify(filler(250)) },
  { name: "unreadable migrations file", file: JSON.stringify([lateRule]), migrations: "" },
];
for (const scenario of syncScenarios) {
  rows.push(
    await withService(
      ({ configPath, migrations }) => {
        if (scenario.file !== null) writeFileSync(configPath, scenario.file);
        if (scenario.migrations !== undefined) writeFileSync(migrations, scenario.migrations);
      },
      async ({ configPath, migrations, run }) => {
        const steps = [];
        for (let i = 0; i < (scenario.runs ?? 1); i++) {
          if (i > 0 && scenario.rewriteBetween !== undefined)
            writeFileSync(configPath, scenario.rewriteBetween);
          const exit = await run(
            Effect.gen(function* () {
              yield* (yield* Keybindings.Keybindings).syncDefaultKeybindingsOnStartup;
            }),
          );
          steps.push({
            ok: exit._tag === "Success",
            file: readIf(configPath),
            migrations: readIf(migrations),
          });
        }
        return {
          type: "sync",
          name: scenario.name,
          file: scenario.file,
          migrations: scenario.migrations ?? null,
          rewriteBetween: scenario.rewriteBetween ?? null,
          steps,
        };
      },
    ),
  );
}

// Upsert and remove.
const R = (key, command, when) => (when === undefined ? { key, command } : { key, command, when });
const mutationScenarios = [
  {
    name: "upsert onto an existing file",
    file: JSON.stringify([R("mod+j", "terminal.toggle")]),
    ops: [["upsert", R("mod+shift+r", "script.run-tests.run")]],
  },
  {
    name: "upsert with no file",
    file: null,
    ops: [["upsert", R("mod+shift+r", "script.run-tests.run")]],
  },
  {
    name: "append a second binding for one command",
    file: JSON.stringify([R("mod+r", "script.run-tests.run")]),
    ops: [["upsert", R("mod+shift+r", "script.run-tests.run")]],
  },
  {
    name: "replace only the target",
    file: JSON.stringify([
      R("mod+r", "script.run-tests.run"),
      R("mod+shift+r", "script.run-tests.run"),
    ]),
    ops: [
      [
        "upsert",
        { ...R("mod+alt+r", "script.run-tests.run"), replace: R("mod+r", "script.run-tests.run") },
      ],
    ],
  },
  {
    name: "replacing with a rule that exists elsewhere does not duplicate it",
    file: JSON.stringify([
      R("mod+r", "script.run-tests.run"),
      R("mod+alt+r", "script.run-tests.run"),
    ]),
    ops: [
      [
        "upsert",
        { ...R("mod+alt+r", "script.run-tests.run"), replace: R("mod+r", "script.run-tests.run") },
      ],
    ],
  },
  {
    name: "replace target that does not exist",
    file: JSON.stringify([R("mod+r", "script.run-tests.run")]),
    ops: [
      [
        "upsert",
        { ...R("mod+alt+r", "script.run-tests.run"), replace: R("mod+q", "script.run-tests.run") },
      ],
    ],
  },
  {
    name: "upserting an identical rule moves it last",
    file: JSON.stringify([R("mod+r", "script.run-tests.run"), R("mod+j", "terminal.toggle")]),
    ops: [["upsert", R("mod+r", "script.run-tests.run")]],
  },
  {
    name: "when differs so the rules differ",
    file: JSON.stringify([R("mod+r", "chat.new", "a")]),
    ops: [
      ["upsert", R("mod+r", "chat.new", "b")],
      ["upsert", R("mod+r", "chat.new")],
      ["upsert", R("mod+r", "chat.new", "a")],
    ],
  },
  {
    name: "remove only the target",
    file: JSON.stringify([
      R("mod+r", "script.run-tests.run"),
      R("mod+shift+r", "script.run-tests.run"),
    ]),
    ops: [["remove", R("mod+r", "script.run-tests.run")]],
  },
  {
    name: "remove a rule that is absent",
    file: JSON.stringify([R("mod+r", "script.run-tests.run")]),
    ops: [
      ["remove", R("mod+q", "script.run-tests.run")],
      ["remove", R("mod+r", "script.run-tests.run", "a")],
    ],
  },
  {
    name: "remove with no file",
    file: null,
    ops: [["remove", R("mod+q", "script.run-tests.run")]],
  },
  {
    name: "remove the last rule leaves defaults",
    file: JSON.stringify([R("mod+r", "terminal.toggle")]),
    ops: [["remove", R("mod+r", "terminal.toggle")]],
  },
  {
    name: "malformed file is refused",
    file: "{ not-json",
    ops: [
      ["upsert", R("mod+shift+r", "script.run-tests.run")],
      ["remove", R("mod+shift+r", "script.run-tests.run")],
    ],
  },
  {
    name: "non-array file is refused",
    file: '{"key":"mod+j","command":"terminal.toggle"}',
    ops: [
      ["upsert", R("mod+shift+r", "script.run-tests.run")],
      ["upsert", R("mod+shift+r", "script.run-tests.run")],
    ],
  },
  {
    name: "empty file is refused",
    file: "",
    ops: [["upsert", R("mod+shift+r", "script.run-tests.run")]],
  },
  {
    name: "invalid entries are dropped on write",
    file: JSON.stringify([
      R("mod+j", "terminal.toggle"),
      R("mod+shift+d+o", "terminal.new"),
      { key: "x", command: "nope" },
      5,
    ]),
    ops: [["upsert", R("mod+shift+r", "script.run-tests.run")]],
  },
  {
    name: "invalid entries are dropped on remove",
    file: JSON.stringify([R("mod+j", "terminal.toggle"), R("mod+shift+d+o", "terminal.new")]),
    ops: [["remove", R("mod+x", "chat.new")]],
  },
  {
    name: "comments are dropped on write",
    file: '[\n // mine\n {"key":"mod+j","command":"terminal.toggle"},\n]',
    ops: [["upsert", R("mod+k", "chat.new")]],
  },
  {
    name: "rule text is stored trimmed",
    file: null,
    ops: [["upsert", R(" mod+k ", "chat.new", " a && b ")]],
  },
  {
    name: "stored text is kept verbatim",
    file: null,
    ops: [["upsert", R("Cmd+K", "chat.new", "a&&b")]],
  },
  {
    name: "a key the shortcut parser rejects is stored but not resolved",
    file: JSON.stringify([R("mod+j", "terminal.toggle")]),
    ops: [
      ["upsert", R("mod+shift+d+o", "terminal.new")],
      ["upsert", R("mod+k", "chat.new", "a &&")],
    ],
  },
  {
    name: "the oldest rule is dropped past the maximum",
    file: JSON.stringify(filler(256)),
    ops: [["upsert", R("mod+k", "chat.new")]],
  },
  {
    name: "a full file whose upsert replaces one stays full",
    file: JSON.stringify(filler(256)),
    ops: [
      ["upsert", { ...R("mod+k", "chat.new"), replace: R("mod+alt+f1", "script.filler-0.run") }],
    ],
  },
  {
    name: "twenty sequential upserts",
    file: "[]",
    ops: Array.from({ length: 20 }, (_, i) => [
      "upsert",
      R(`mod+${String.fromCharCode(97 + i)}`, `script.concurrent-${i}.run`),
    ]),
  },
  {
    name: "plus key",
    file: null,
    ops: [
      ["upsert", R("mod++", "terminal.toggle")],
      ["remove", R("mod++", "terminal.toggle")],
    ],
  },
  {
    name: "upsert then remove restores the defaults",
    file: null,
    ops: [
      ["upsert", R("mod+j", "terminal.toggle")],
      ["remove", R("mod+j", "terminal.toggle")],
    ],
  },
  {
    name: "unreadable file",
    file: "DIRECTORY",
    ops: [
      ["upsert", R("mod+shift+r", "script.run-tests.run")],
      ["remove", R("mod+shift+r", "script.run-tests.run")],
    ],
  },
  {
    name: "unwritable directory",
    file: JSON.stringify([R("mod+j", "terminal.toggle")]),
    readonlyDirectory: true,
    ops: [
      ["upsert", R("mod+shift+r", "script.run-tests.run")],
      ["remove", R("mod+j", "terminal.toggle")],
    ],
  },
];
for (const scenario of mutationScenarios) {
  rows.push(
    await withService(
      ({ configPath }) => {
        if (scenario.file === "DIRECTORY") mkdirSync(configPath);
        else if (scenario.file !== null) writeFileSync(configPath, scenario.file);
        if (scenario.readonlyDirectory) chmodSync(NodePath.dirname(configPath), 0o500);
      },
      async ({ configPath, run }) => {
        const results = [];
        for (const [op, input] of scenario.ops) {
          const effect = Effect.gen(function* () {
            const service = yield* Keybindings.Keybindings;
            return op === "upsert"
              ? yield* service.upsertKeybindingRule(input)
              : yield* service.removeKeybindingRule(input);
          });
          const exit = await run(effect);
          const file = scenario.file === "DIRECTORY" ? null : readIf(configPath);
          results.push(
            exit._tag === "Success"
              ? { ok: compactResolved(exit.value), count: exit.value.length, file }
              : { error: failureOf(exit, configPath), file },
          );
        }
        return {
          type: "mutation",
          name: scenario.name,
          file: scenario.file,
          readonlyDirectory: scenario.readonlyDirectory ?? false,
          ops: scenario.ops,
          results,
        };
      },
    ),
  );
}

// ---- wire payloads ---------------------------------------------------------------------------
const decodeUpsert = Schema.decodeUnknownExit(
  Schema.toCodecJson(ServerContracts.ServerUpsertKeybindingInput),
);
const decodeRemove = Schema.decodeUnknownExit(
  Schema.toCodecJson(ServerContracts.ServerRemoveKeybindingInput),
);
const payloads = [
  { key: "mod+j", command: "terminal.toggle" },
  { key: " mod+j ", command: "terminal.toggle", when: " a " },
  { key: "mod+j", command: "terminal.toggle", when: "" },
  { key: "mod+j", command: "terminal.toggle", when: null },
  { key: "mod+j", command: "terminal.toggle", extra: 1 },
  { key: "", command: "terminal.toggle" },
  { key: "   ", command: "terminal.toggle" },
  { key: "k".repeat(64), command: "terminal.toggle" },
  { key: "k".repeat(65), command: "terminal.toggle" },
  { key: " " + "k".repeat(64) + " ", command: "terminal.toggle" },
  { key: "😀".repeat(32), command: "terminal.toggle" },
  { key: "😀".repeat(33), command: "terminal.toggle" },
  { key: "mod+j", command: "nope" },
  { key: "mod+j", command: "script.a.run" },
  { key: "mod+j", command: "script..run" },
  { key: "mod+j", command: "script.A.run" },
  { key: "mod+j", command: "script.a-b.run" },
  { key: "mod+j", command: "script.-a.run" },
  { key: "mod+j", command: "script." + "a".repeat(24) + ".run" },
  { key: "mod+j", command: "script." + "a".repeat(25) + ".run" },
  { key: "mod+j", command: "script.0.run" },
  { key: "mod+j", command: " terminal.toggle" },
  { key: "mod+j" },
  { command: "terminal.toggle" },
  {},
  null,
  [],
  "x",
  5,
  { key: "mod+j", command: "terminal.toggle", when: "w".repeat(256) },
  { key: "mod+j", command: "terminal.toggle", when: "w".repeat(257) },
  { key: "mod+j", command: "terminal.toggle", when: 5 },
  { key: "mod+j", command: "terminal.toggle", replace: { key: "mod+k", command: "chat.new" } },
  {
    key: "mod+j",
    command: "terminal.toggle",
    replace: { key: " mod+k ", command: "chat.new", when: " a " },
  },
  { key: "mod+j", command: "terminal.toggle", replace: {} },
  { key: "mod+j", command: "terminal.toggle", replace: null },
  { key: "mod+j", command: "terminal.toggle", replace: { key: "mod+k" } },
  {
    key: "mod+j",
    command: "terminal.toggle",
    replace: { key: "mod+k", command: "chat.new", extra: true },
  },
];
for (const payload of payloads) {
  const wire = payload === undefined ? undefined : json(payload);
  const up = decodeUpsert(wire);
  const rem = decodeRemove(wire);
  rows.push({
    type: "payload",
    payload: wire,
    upsert: up._tag === "Success" ? { ok: json(up.value) } : { error: true },
    remove: rem._tag === "Success" ? { ok: json(rem.value) } : { error: true },
  });
}

const target = new URL("../crates/server/tests/fixtures/keybindings.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
const defaults = new URL("../crates/server/src/keybinding-defaults.json", import.meta.url);
writeFileSync(defaults, JSON.stringify(Shared.DEFAULT_KEYBINDINGS, null, 2) + "\n");
console.log(JSON.stringify({ rows: rows.length }));
