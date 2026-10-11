// Development oracle for the environment theme store and `t3 theme`: runs the unchanged original
// (`apps/server/src/environmentTheme.ts` `readPublishedThemes`, and `t3 theme set|clear|show` as a subprocess of
// `apps/server/src/bin.ts` with an isolated HOME and base directory) over scenarios of every shape the directory and the
// command must survive, and records what each produced. `tests/environment_theme_flow.rs` replays them against the Rust
// service and command. Written to a `.jsonl` file so the formatter leaves it alone.
// Run: node rust/tools/generate_environment_theme_fixtures.mjs
import { spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { readPublishedThemes } from "../../apps/server/src/environmentTheme.ts";

const scratch = realpathSync(
  mkdtempSync(join(realpathSync(tmpdir()), "t3code-rust-port-test-themes-")),
);
const rows = [];

// A part of a file's text: a string, or {"repeat": [unit, count]} for a long run of one.
const text = (parts) =>
  parts
    .map((part) => (typeof part === "string" ? part : part.repeat[0].repeat(part.repeat[1])))
    .join("");
const seeded = (extra = {}) =>
  JSON.stringify({
    name: "Nightfall",
    appearance: "dark",
    canvas: "#101820",
    accent: "#ffb000",
    ...extra,
  });
const exported = {
  version: 1,
  name: "Exported",
  appearance: "light",
  colors: { canvas: "oklch(0.98 0 0)", surface: "#fff" },
  variants: { dark: { canvas: "oklch(0.2 0 0)" } },
};

const writeFiles = (directory, files) => {
  for (const file of files) {
    const path = join(directory, file.name);
    if (file.symlink !== undefined) symlinkSync(file.symlink, path);
    else if (file.dir) mkdirSync(path);
    else if (file.base64 !== undefined) writeFileSync(path, Buffer.from(file.base64, "base64"));
    else writeFileSync(path, text(file.parts ?? [file.text]));
  }
};

const readScenario = async (name, files, extra = {}) => {
  const directory = join(scratch, `read-${rows.length}`);
  mkdirSync(directory, { recursive: true });
  const themesDir = join(directory, "themes");
  mkdirSync(themesDir);
  writeFiles(themesDir, files);
  if (extra.symlinkedDirectory) {
    // The themes directory itself is a symlink: still usable.
    const real = join(directory, "real-themes");
    mkdirSync(real);
    writeFiles(real, files);
    rmSync(themesDir, { recursive: true });
    symlinkSync(real, themesDir);
  }
  const themes = await Effect.runPromise(
    readPublishedThemes(themesDir).pipe(Effect.provide(NodeServices.layer)),
  );
  rows.push({
    kind: "read",
    name,
    files,
    symlinkedDirectory: extra.symlinkedDirectory === true,
    themes: JSON.parse(JSON.stringify(themes)),
  });
};

const json = (value) => JSON.stringify(value);
await readScenario("empty directory", []);
await readScenario("seeded and exported", [
  { name: "nightfall.json", text: seeded() },
  { name: "exported.json", text: json(exported) },
]);
await readScenario("sorted by filename, not id", [
  { name: "a.json", text: seeded() },
  { name: "a-b.json", text: seeded({ name: "A-B" }) },
  { name: "a1.json", text: seeded({ name: "A1" }) },
  { name: "0lead.json", text: seeded({ name: "Zero" }) },
]);
await readScenario("an embedded id is ignored, unknown keys dropped", [
  {
    name: "real.json",
    text: json({
      id: "other",
      extra: [1, 2],
      name: "Real",
      appearance: "dark",
      canvas: "#111",
      accent: "#abcdef",
    }),
  },
]);
await readScenario("a repeated key keeps the last value", [
  {
    name: "dup.json",
    text: '{"name":"First","name":"Second","appearance":"dark","canvas":"#111","accent":"#222"}',
  },
]);
await readScenario("null for an optional key", [
  {
    name: "null-canvas.json",
    text: json({
      name: "N",
      appearance: "dark",
      canvas: null,
      accent: "#222",
      colors: { canvas: "#111" },
    }),
  },
  {
    name: "null-colors.json",
    text: json({ name: "N2", appearance: "dark", canvas: "#111", accent: "#222", colors: null }),
  },
  {
    name: "null-version.json",
    text: json({ version: null, name: "N3", appearance: "dark", canvas: "#111", accent: "#222" }),
  },
  {
    name: "null-variants.json",
    text: json({ name: "N4", appearance: "dark", canvas: "#111", accent: "#222", variants: null }),
  },
]);
await readScenario("version other than 1", [
  { name: "v2.json", text: seeded({ version: 2 }) },
  { name: "v1.json", text: seeded({ version: 1 }) },
]);
await readScenario("names and appearances", [
  { name: "long-name.json", text: seeded({ name: "x".repeat(48) }) },
  { name: "too-long-name.json", text: seeded({ name: "x".repeat(49) }) },
  { name: "padded-name.json", text: seeded({ name: "  Padded  " }) },
  { name: "blank-name.json", text: seeded({ name: "   " }) },
  { name: "bad-appearance.json", text: seeded({ appearance: "sepia" }) },
  { name: "no-appearance.json", text: json({ name: "N", canvas: "#111", accent: "#222" }) },
]);
await readScenario("colors", [
  { name: "short-hex.json", text: seeded({ canvas: "#abc", accent: "#ABCDEF" }) },
  { name: "bad-hex.json", text: seeded({ canvas: "#abcd" }) },
  { name: "bad-hex-2.json", text: seeded({ accent: "red" }) },
  { name: "canvas-only.json", text: json({ name: "C", appearance: "dark", canvas: "#111" }) },
  {
    name: "accent-only-with-colors.json",
    text: json({ name: "A", appearance: "dark", accent: "#111", colors: { canvas: "#000" } }),
  },
  {
    name: "empty-colors.json",
    text: json({ name: "E", appearance: "dark", canvas: "#111", colors: {} }),
  },
  { name: "empty-colors-only.json", text: json({ name: "E2", appearance: "dark", colors: {} }) },
  {
    name: "role-keys.json",
    text: json({ name: "R", appearance: "dark", colors: { canvas: "#000", "1bad": "#000" } }),
  },
  {
    name: "role-key-ok.json",
    text: json({ name: "R2", appearance: "dark", colors: { canvas: "#000", newRole2: "#111" } }),
  },
  {
    name: "role-key-long.json",
    text: json({ name: "R3", appearance: "dark", colors: { [`a${"b".repeat(63)}`]: "#000" } }),
  },
  {
    name: "role-key-too-long.json",
    text: json({ name: "R4", appearance: "dark", colors: { [`a${"b".repeat(64)}`]: "#000" } }),
  },
  {
    name: "value-blank.json",
    text: json({ name: "V", appearance: "dark", colors: { canvas: "  " } }),
  },
  {
    name: "value-long.json",
    text: json({ name: "V2", appearance: "dark", colors: { canvas: "x".repeat(64) } }),
  },
  {
    name: "value-too-long.json",
    text: json({ name: "V3", appearance: "dark", colors: { canvas: "x".repeat(65) } }),
  },
  {
    name: "value-padded.json",
    text: json({ name: "V4", appearance: "dark", colors: { canvas: "  #123  " } }),
  },
  {
    name: "value-number.json",
    text: json({ name: "V5", appearance: "dark", colors: { canvas: 1 } }),
  },
  {
    name: "variants.json",
    text: json({
      name: "Va",
      appearance: "dark",
      colors: { canvas: "#000" },
      variants: { light: { canvas: "#fff" }, dark: {}, other: { canvas: "#111" } },
    }),
  },
  {
    name: "variants-bad.json",
    text: json({
      name: "Vb",
      appearance: "dark",
      colors: { canvas: "#000" },
      variants: { light: { canvas: 5 } },
    }),
  },
]);
await readScenario("ids", [
  { name: "dark.json", text: seeded() },
  { name: "light.json", text: seeded() },
  { name: "system.json", text: seeded() },
  { name: "ocean.json", text: seeded() },
  { name: "t3-chat.json", text: seeded() },
  { name: "t3-code.json", text: seeded() },
  { name: "t3-ocean.json", text: seeded() },
  { name: "t3-chat-dark.json", text: seeded() },
  { name: "Upper.json", text: seeded() },
  { name: "with_underscore.json", text: seeded() },
  { name: "-leading.json", text: seeded() },
  { name: "trailing-.json", text: seeded({ name: "Trailing" }) },
  { name: `${"a".repeat(48)}.json`, text: seeded({ name: "Max" }) },
  { name: `${"a".repeat(49)}.json`, text: seeded({ name: "Over" }) },
  { name: "dark-mode.json", text: seeded({ name: "Dark Mode" }) },
  { name: "café.json", text: seeded() },
  { name: "noext", text: seeded() },
  { name: "theme.JSON", text: seeded() },
  { name: "theme.json.bak", text: seeded() },
  { name: ".json", text: seeded() },
]);
await readScenario("content", [
  { name: "empty.json", text: "" },
  { name: "blank.json", text: " \n\t " },
  { name: "bom-blank.json", text: "﻿" },
  { name: "bom-json.json", text: `﻿${seeded()}` },
  { name: "not-json.json", text: "{nope" },
  { name: "array.json", text: "[]" },
  { name: "null.json", text: "null" },
  { name: "string.json", text: '"x"' },
  {
    name: "trailing-comma.json",
    text: '{"name":"T","appearance":"dark","canvas":"#111","accent":"#222",}',
  },
  { name: "comment.json", text: `// c\n${seeded()}` },
  {
    name: "invalid-utf8.json",
    base64: Buffer.concat([
      Buffer.from('{"name":"Bad '),
      Buffer.from([0xff, 0xfe]),
      Buffer.from('","appearance":"dark","canvas":"#111","accent":"#222"}'),
    ]).toString("base64"),
  },
  { name: "ok.json", text: seeded({ name: "Ok" }) },
]);
await readScenario("file types", [
  { name: "dir.json", dir: true },
  { name: "link.json", symlink: "target.json" },
  { name: "target-file.json", text: seeded({ name: "Target" }) },
  { name: "dangling.json", symlink: "missing.json" },
  { name: "plain.json", text: seeded({ name: "Plain" }) },
]);
await readScenario(
  "a symlinked themes directory",
  [{ name: "inside.json", text: seeded({ name: "Inside" }) }],
  { symlinkedDirectory: true },
);
await readScenario("size of a file", [
  {
    name: "at-limit.json",
    parts: [
      `{"name":"At","appearance":"dark","canvas":"#111","accent":"#222","pad":"`,
      { repeat: ["x", 32 * 1024 - 80] },
      '"}',
    ],
  },
  {
    name: "over-limit.json",
    parts: [
      `{"name":"Over","appearance":"dark","canvas":"#111","accent":"#222","pad":"`,
      { repeat: ["x", 32 * 1024] },
      '"}',
    ],
  },
  { name: "small.json", text: seeded({ name: "Small" }) },
]);
// Each file just under the limit: the total of 192 KiB admits some and stops at the first that would pass it.
await readScenario(
  "the total size",
  Array.from({ length: 10 }, (_, index) => ({
    name: `big-${String(index).padStart(2, "0")}.json`,
    parts: [
      `{"name":"Big ${index}","appearance":"dark","canvas":"#111","accent":"#222","pad":"`,
      { repeat: ["y", 30 * 1024] },
      '"}',
    ],
  })),
);
// A skipped file does not eat the budget: invalid files sorted first are not counted.
await readScenario("the total size ignores skipped files", [
  ...Array.from({ length: 5 }, (_, index) => ({
    name: `a-bad-${index}.json`,
    parts: ['{"nope":"', { repeat: ["z", 31 * 1024] }, '"}'],
  })),
  ...Array.from({ length: 6 }, (_, index) => ({
    name: `b-ok-${index}.json`,
    parts: [
      `{"name":"Ok ${index}","appearance":"dark","canvas":"#111","accent":"#222","pad":"`,
      { repeat: ["y", 30 * 1024] },
      '"}',
    ],
  })),
]);
await readScenario(
  "the count of files examined",
  Array.from({ length: 40 }, (_, index) => ({
    name: `t${String(index).padStart(2, "0")}.json`,
    text: index % 2 === 0 ? seeded({ name: `T${index}` }) : "{bad",
  })),
);
await readScenario(
  "the count of files examined, all valid",
  Array.from({ length: 40 }, (_, index) => ({
    name: `v${String(index).padStart(2, "0")}.json`,
    text: seeded({ name: `V${index}` }),
  })),
);
await readScenario("reserved ids are not examined", [
  ...["dark", "light", "system", "ocean", "grove", "ember", "iris", "t3-chat", "t3-code"].map(
    (id) => ({ name: `${id}.json`, text: seeded() }),
  ),
  ...Array.from({ length: 32 }, (_, index) => ({
    name: `z${String(index).padStart(2, "0")}.json`,
    text: seeded({ name: `Z${index}` }),
  })),
]);

// ---- The command.
const bin = new URL("../../apps/server/src/bin.ts", import.meta.url).pathname;
const normalize = (value, root) => value.split(root).join("<ROOT>");
const normalizeSettings = (value) =>
  value.replace(/"defaultThemeSetAt": "[^"]+"/g, '"defaultThemeSetAt": "<NOW>"');
const listThemes = (themesDir, root) => {
  let names;
  try {
    names = readdirSync(themesDir).sort();
  } catch {
    return null;
  }
  return Object.fromEntries(
    names.map((name) => [name, normalize(readFileSync(join(themesDir, name), "utf8"), root)]),
  );
};
const cliScenario = (name, setup, steps) => {
  const root = join(scratch, `cli-${rows.length}`);
  mkdirSync(join(root, "home", "userdata", "themes"), { recursive: true });
  mkdirSync(join(root, "src"), { recursive: true });
  const userdata = join(root, "home", "userdata");
  const settings = join(userdata, "settings.json");
  if (setup.noThemesDir) rmSync(join(userdata, "themes"), { recursive: true });
  if (setup.settings !== undefined) writeFileSync(settings, setup.settings);
  if (setup.settingsIsDirectory) mkdirSync(settings);
  writeFiles(join(userdata, "themes"), setup.themes ?? []);
  writeFiles(join(root, "src"), setup.sources ?? []);
  const recorded = [];
  for (const step of steps) {
    const args = step.args.map((arg) => arg.replaceAll("<BASE>", join(root, "home")));
    const run = spawnSync(process.execPath, ["--no-warnings", bin, "theme", ...args], {
      cwd: root,
      env: { PATH: process.env.PATH, HOME: root, NODE_NO_WARNINGS: "1" },
      encoding: "utf8",
    });
    // The original logs a failure as a record on standard output.
    const error = /ERROR \(#\d+\): (\w+): ([\s\S]*?)\n\s+at /.exec(run.stdout + run.stderr);
    let after = null;
    try {
      after = normalizeSettings(normalize(readFileSync(settings, "utf8"), root));
    } catch {}
    recorded.push({
      args: step.args,
      stdout: error ? "" : normalize(run.stdout, root),
      error: error ? { tag: error[1], message: normalize(error[2], root) } : null,
      stderrWithoutError: error ? null : normalize(run.stderr, root),
      exit: run.status,
      settings: after,
      themes: listThemes(join(userdata, "themes"), root),
    });
  }
  rows.push({ kind: "cli", name, setup, steps: recorded });
};

const base = ["--base-dir", "<BASE>"];
cliScenario("show with nothing", {}, [{ args: ["show", ...base] }]);
cliScenario("set a built-in and show it", {}, [
  { args: ["set", ...base, "ocean"] },
  { args: ["show", ...base] },
  { args: ["set", ...base, "ocean"] },
  { args: ["set", ...base, "iris"] },
  { args: ["clear", ...base] },
  { args: ["show", ...base] },
  { args: ["clear", ...base] },
]);
cliScenario("ids that are refused", {}, [
  { args: ["set", ...base, "nosuch"] },
  { args: ["set", ...base, "Bad_Id"] },
  { args: ["set", ...base, "system"] },
  { args: ["set", ...base, "dark"] },
  { args: ["set", ...base, "t3-code"] },
  { args: ["set", ...base, " "] },
  { args: ["set", ...base, "a".repeat(49)] },
  { args: ["show", ...base] },
]);
cliScenario(
  "publish a file",
  {
    sources: [
      { name: "nightfall.json", text: seeded() },
      { name: "colorless.json", text: json({ name: "C", appearance: "dark" }) },
      { name: "broken.json", text: "{" },
      { name: "dark.json", text: seeded() },
      { name: "Bad Name.json", text: seeded() },
      { name: "exported.json", text: json(exported) },
    ],
  },
  [
    { args: ["set", ...base, "./src/nightfall.json"] },
    { args: ["show", ...base] },
    { args: ["set", ...base, "nightfall"] },
    { args: ["set", ...base, "--id", "other", "src/nightfall.json"] },
    { args: ["show", ...base] },
    { args: ["set", ...base, "src/colorless.json"] },
    { args: ["set", ...base, "src/broken.json"] },
    { args: ["set", ...base, "src/dark.json"] },
    { args: ["set", ...base, "--id", "fine", "src/dark.json"] },
    { args: ["set", ...base, "--id", "Bad", "src/exported.json"] },
    { args: ["set", ...base, "src/Bad Name.json"] },
    { args: ["set", ...base, "--id", "bad-name", "src/Bad Name.json"] },
    { args: ["set", ...base, "src/exported.json"] },
    { args: ["set", ...base, "src/missing.json"] },
    { args: ["set", ...base, "src"] },
    { args: ["set", ...base, "./nightfall"] },
    { args: ["show", ...base] },
  ],
);
cliScenario(
  "publish over an existing theme",
  {
    themes: [{ name: "nightfall.json", text: seeded({ name: "Old" }) }],
    sources: [{ name: "nightfall.json", text: seeded({ name: "New" }) }],
  },
  [{ args: ["set", ...base, "src/nightfall.json"] }, { args: ["show", ...base] }],
);
cliScenario("a file named like a built-in does not shadow it", { sources: [] }, [
  { args: ["set", ...base, "ocean"] },
]);
cliScenario(
  "an oversized file",
  {
    sources: [
      {
        name: "big.json",
        parts: [
          `{"name":"Big","appearance":"dark","canvas":"#111","accent":"#222","pad":"`,
          { repeat: ["x", 40 * 1024] },
          '"}',
        ],
      },
      {
        name: "limit.json",
        parts: [
          `{"name":"At","appearance":"dark","canvas":"#111","accent":"#222","pad":"`,
          { repeat: ["x", 32 * 1024 - 80] },
          '"}',
        ],
      },
    ],
  },
  [{ args: ["set", ...base, "src/big.json"] }, { args: ["set", ...base, "src/limit.json"] }],
);
cliScenario(
  "a symlinked source",
  {
    sources: [
      { name: "real.json", text: seeded() },
      { name: "link.json", symlink: "real.json" },
    ],
  },
  [{ args: ["set", ...base, "src/link.json"] }, { args: ["show", ...base] }],
);
cliScenario(
  "settings keep their other keys and order",
  {
    settings:
      '{\n  // kept by the parser, not by the writer\n  "enableProviderUpdateChecks": false,\n  "unknownFutureKey": {"a": [1, 2, {"b": null}]},\n  "defaultTheme": "grove",\n  "last": 1,\n}\n',
  },
  [{ args: ["show", ...base] }, { args: ["set", ...base, "ember"] }, { args: ["clear", ...base] }],
);
cliScenario("settings that are not an object", { settings: "[1, 2]" }, [
  { args: ["show", ...base] },
  { args: ["set", ...base, "ocean"] },
  { args: ["clear", ...base] },
]);
cliScenario("settings that are not JSON", { settings: "{not json" }, [
  { args: ["show", ...base] },
  { args: ["set", ...base, "ocean"] },
]);
cliScenario("settings that are null, a string, blank", { settings: "null" }, [
  { args: ["show", ...base] },
]);
cliScenario("blank settings", { settings: "  \n" }, [
  { args: ["show", ...base] },
  { args: ["set", ...base, "grove"] },
]);
cliScenario("settings that cannot be read", { settingsIsDirectory: true }, [
  { args: ["show", ...base] },
  { args: ["set", ...base, "ocean"] },
  { args: ["clear", ...base] },
]);
cliScenario("a default theme that is not a string", { settings: '{"defaultTheme": 5}' }, [
  { args: ["show", ...base] },
]);
cliScenario("an empty default theme", { settings: '{"defaultTheme": ""}' }, [
  { args: ["show", ...base] },
]);
cliScenario(
  "show lists what is published",
  {
    themes: [
      { name: "b.json", text: seeded() },
      { name: "a.json", text: seeded() },
      { name: "bad.json", text: "{" },
    ],
  },
  [{ args: ["show", ...base] }, { args: ["set", ...base, "a"] }, { args: ["set", ...base, "bad"] }],
);
cliScenario("no themes directory yet", { noThemesDir: true }, [
  { args: ["show", ...base] },
  { args: ["set", ...base, "ocean"] },
]);

rmSync(scratch, { recursive: true, force: true });
const out = new URL("../crates/server/tests/fixtures/environment-themes.jsonl", import.meta.url);
writeFileSync(out, `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`);
console.log(
  `${rows.length} scenarios, ${rows.reduce((sum, row) => sum + (row.steps?.length ?? 0), 0)} command runs`,
);
