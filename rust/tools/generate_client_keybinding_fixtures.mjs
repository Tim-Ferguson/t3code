// Runs the original shortcut matcher (`apps/web/src/keybindings.ts`: precedence, `when` context,
// `mod` per platform, effective shortcuts and labels) and records what it decides for many events,
// contexts and rule sets. The Rust tests in t3-client replay the rows and require the same
// command, the same label and the same effective shortcut set. The web module cannot be imported
// as is (it uses extensionless imports and the browser's `navigator`), so its source is loaded
// with its imports replaced by the shared package's; the code itself is unchanged.
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { pathToFileURL } from "node:url";
import * as Shared from "../../packages/shared/src/keybindings.ts";

const source = readFileSync(new URL("../../apps/web/src/keybindings.ts", import.meta.url), "utf8");
const body = source
  .replace(/^import[\s\S]*?from "[^"]+";\n/gm, "")
  .replace(/^export type \{[^}]*\} from "[^"]+";\n/gm, "")
  .replace(/^export \{[^}]*\} from "[^"]+";\n/gm, "");
const root = new URL("../../", import.meta.url).pathname;
const header = `
import { isMacPlatform, matchesKeybindingShortcut, matchesKeybindingShortcutModifiers, normalizeEventKey, resolveEventKeys } from "${root}packages/shared/src/keybindings.ts";
import { MODEL_PICKER_JUMP_KEYBINDING_COMMANDS, THREAD_JUMP_KEYBINDING_COMMANDS } from "${root}packages/contracts/src/keybindings.ts";
const isElectron = false;
const navigator = { platform: "Linux" };
const projectScriptIdFromCommand = (command) => (command.startsWith("script.") ? command : null);
`;
const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-keybinding-client-"));
const file = NodePath.join(scratch, "web-keybindings.ts");
writeFileSync(file, header + body);
const Web = await import(pathToFileURL(file).href);
rmSync(scratch, { recursive: true, force: true });

const json = (value) => JSON.parse(JSON.stringify(value));
const rule = (key, command, when) => {
  const compiled = Shared.compileResolvedKeybindingRule({
    key,
    command,
    ...(when === undefined ? {} : { when }),
  });
  if (!compiled) throw new Error(`rule does not compile: ${key} ${when}`);
  return compiled;
};
const defaults = Shared.DEFAULT_RESOLVED_KEYBINDINGS;
const custom = [
  ...defaults,
  rule("mod+d", "terminal.toggle"),
  rule("mod+shift+k", "diff.toggle", "terminalOpen && !terminalFocus"),
  rule("alt+x", "chat.new", "composerFocus || isWeb"),
  rule("c", "thread.stop", "!editableFocus"),
  rule("mod+k", "sidebar.toggle", "dialogOpen"),
];
const ruleSets = {
  defaults,
  custom,
  merged: Shared.mergeWithDefaultKeybindings(custom.slice(defaults.length)),
};

const flags = [
  "terminalFocus",
  "terminalOpen",
  "editableFocus",
  "composerFocus",
  "turnRunning",
  "dialogOpen",
  "isDesktop",
];
const contexts = [];
for (let bits = 0; bits < 1 << flags.length; bits += 11) {
  const context = {};
  flags.forEach((flag, index) => {
    context[flag] = ((bits >> index) & 1) === 1;
  });
  context.isWeb = !context.isDesktop;
  contexts.push(context);
}
const platforms = ["MacIntel", "Linux x86_64"];
const events = [];
const seen = new Set();
const press = (key, mods = {}, code = "") => {
  const event = {
    key,
    code,
    metaKey: false,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    ...mods,
  };
  const id = JSON.stringify(event);
  if (!seen.has(id)) {
    seen.add(id);
    events.push(event);
  }
};
for (const rules of Object.values(ruleSets))
  for (const entry of rules) {
    const s = entry.shortcut;
    for (const [metaKey, ctrlKey] of [
      [s.metaKey || s.modKey, s.ctrlKey],
      [s.metaKey, s.ctrlKey || s.modKey],
    ])
      press(s.key, { metaKey, ctrlKey, shiftKey: s.shiftKey, altKey: s.altKey });
  }
for (const key of [
  "k",
  "K",
  "j",
  "d",
  "n",
  "w",
  "1",
  "9",
  "Enter",
  "Escape",
  "Esc",
  "ArrowUp",
  "[",
  "]",
  "{",
  "}",
  "a",
  "c",
  "x",
  "Dead",
  "Process",
  " ",
  "F5",
]) {
  for (const mods of [
    {},
    { metaKey: true },
    { ctrlKey: true },
    { ctrlKey: true, shiftKey: true },
    { metaKey: true, shiftKey: true },
    { altKey: true },
    { ctrlKey: true, altKey: true },
    { metaKey: true, altKey: true, shiftKey: true },
    { shiftKey: true },
  ])
    press(
      key,
      mods,
      /^[a-z]$/i.test(key) ? `Key${key.toUpperCase()}` : /^\d$/.test(key) ? `Digit${key}` : "",
    );
}
press("о", { ctrlKey: true }, "KeyJ");
press("о", { metaKey: true }, "KeyJ");
press("{", { ctrlKey: true, shiftKey: true }, "BracketLeft");
press("}", { metaKey: true, shiftKey: true }, "BracketRight");
press("+", { ctrlKey: true }, "Equal");
press("=", { metaKey: true }, "Equal");
press("-", { ctrlKey: true }, "Minus");
press("0", { ctrlKey: true }, "Digit0");
press("¡", { altKey: true }, "Digit1");
press("@", { ctrlKey: true, altKey: true });
press("€", { ctrlKey: true, altKey: true }, "KeyE");

const rows = [
  { type: "events", events },
  { type: "contexts", contexts },
  { type: "platforms", platforms },
];
const commandLabels = [
  ...new Set([
    ...defaults.map((entry) => entry.command),
    "terminal.toggle",
    "diff.toggle",
    "chat.new",
    "sidebar.toggle",
    "thread.stop",
  ]),
];
let skipped = 0;
for (const [name, rules] of Object.entries(ruleSets)) {
  rows.push({ type: "rules", name, rules: json(rules) });
  platforms.forEach((platform, p) => {
    contexts.forEach((context, c) => {
      events.forEach((event, e) => {
        const command = Web.resolveShortcutCommand(event, rules, { platform, context });
        // Most events match nothing; a seventh of those is enough to pin that they stay unmatched.
        if (command === null && (e + c) % 7 !== 0) {
          skipped++;
          return;
        }
        rows.push([name, p, c, e, command]);
      });
      if (c % 4 === 0)
        for (const command of commandLabels) {
          rows.push({
            type: "label",
            rules: name,
            platform: p,
            context: c,
            command,
            label: Web.shortcutLabelForCommand(rules, command, { platform, context }),
            effective: json(
              Web.effectiveShortcutsForCommand(rules, command, { platform, context }),
            ),
          });
        }
    });
  });
}
writeFileSync(
  new URL("../crates/client/tests/fixtures/keybinding-matcher.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`wrote ${rows.length} rows (${skipped} unmatched events not recorded)`);
