#!/usr/bin/env node
// Compares what the original web and desktop clients do with what the Rust UI (`rust/crates/ui`, `client`, `terminal`,
// `desktop`) does, derived from source on both sides, the way `check_rpc_surface.mjs` does for the server:
//   rpc       every RPC method the original web client calls (`WS_METHODS.x` / `ORCHESTRATION_V2_WS_METHODS.x` in
//             `apps/web/src` and `packages/client-runtime/src`) against the methods the Rust UI and client send;
//   http      every HTTP endpoint the original client uses against the paths the Rust UI and client request;
//   routes    every file route of `apps/web/src/routes` against the Rust router (`client/src/route.rs`);
//   commands  every keybinding command (`STATIC_KEYBINDING_COMMANDS` and the script pattern) and every command palette
//             action of the original against the Rust key runner (`keys.rs` `execute`) and palette
//             (`command_palette.rs`), and the commands the Rust side lists without carrying them out;
//   settings  every entry of the original's settings catalog (`settingsSearch.ts`) against the Rust settings pages and
//             whether the control it names exists and is acted on;
//   desktop   every member of the original's `desktopBridge` (`apps/desktop/src/preload.ts`) against the Rust desktop shell;
//   surface   every turn-item type, timeline row kind, work-group kind, message context kind and right panel tab of the
//             original, and the files that make up its composer, against the Rust timeline and composer;
//   caps      every environment capability flag the original UI reads against the ones the Rust UI reads.
// Usage: node rust/tools/check_ui_surface.mjs [--section rpc|http|routes|commands|settings|desktop|surface|caps] [--json] [--all]
// Statuses: handled | partial (present but incomplete or unable to work; reason in the allowlist) | pending (known gap, to
// be ported) | omitted (deliberate, reason in the allowlist) | MISSING (a gap the allowlist does not know: exits 1).
// The allowlist is `check_ui_surface_allowlist.json` beside this file; every entry carries a reason. An entry the script
// cannot derive as handled is declared `handled` there with a `probe` ("file::regex") that must match in the Rust source,
// so a feature that is removed turns into MISSING again.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../..");
const read = (file) => fs.readFileSync(path.join(root, file), "utf8");
const exists = (file) => fs.existsSync(path.join(root, file));
const allow = JSON.parse(
  fs.readFileSync(path.join(here, "check_ui_surface_allowlist.json"), "utf8"),
);
const args = process.argv.slice(2);
const only = args.includes("--section") ? args[args.indexOf("--section") + 1] : null;
const asJson = args.includes("--json");
const showAll = args.includes("--all");

const walk = (dir, keep, out = []) => {
  for (const entry of fs.readdirSync(path.join(root, dir), { withFileTypes: true })) {
    const p = `${dir}/${entry.name}`;
    if (entry.isDirectory()) {
      if (entry.name !== "node_modules") walk(p, keep, out);
    } else if (keep(p)) out.push(p);
  }
  return out;
};
const isTsTest = (p) =>
  /\.(test|bench|testkit|browser\.test)\.tsx?$/.test(p) || /\/(test|__tests__)\//.test(p);
// Without `#[cfg(test)] mod ... { }` blocks: a method only a test names is not handled.
const stripTests = (text) => {
  let out = "";
  let rest = text;
  for (;;) {
    const m = /#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))? )?mod \w+ \{/.exec(rest);
    if (!m) return out + rest;
    out += rest.slice(0, m.index);
    let depth = 1;
    let i = m.index + m[0].length;
    for (; i < rest.length && depth > 0; i += 1)
      depth += rest[i] === "{" ? 1 : rest[i] === "}" ? -1 : 0;
    rest = rest.slice(i);
  }
};
const snake = (k) => k.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);

// ---- the original, as text
const webFiles = [
  ...walk("apps/web/src", (p) => /\.tsx?$/.test(p) && !isTsTest(p)),
  ...walk("packages/client-runtime/src", (p) => /\.tsx?$/.test(p) && !isTsTest(p)),
];
const webText = Object.fromEntries(webFiles.map((f) => [f, read(f)]));
const webWhere = (re) => webFiles.filter((f) => re.test(webText[f]));

// ---- the Rust UI, as text (tests, the fixture servers and the benchmarks do not count as the UI)
const uiRoots = [
  "rust/crates/ui/src",
  "rust/crates/client/src",
  "rust/crates/terminal/src",
  "rust/crates/desktop/src",
];
const uiFiles = uiRoots.flatMap((dir) =>
  walk(
    dir,
    (p) =>
      /\.(rs|js)$/.test(p) &&
      !/_tests?\.rs$/.test(p) &&
      !/\/(fixture_server|fake_github)\.rs$/.test(p),
  ),
);
const uiText = Object.fromEntries(
  uiFiles.map((f) => [f, f.endsWith(".rs") ? stripTests(read(f)) : read(f)]),
);
const uiWhere = (re, files = uiFiles) => files.filter((f) => re.test(uiText[f]));
const uiOf = (name) => uiFiles.find((f) => f.endsWith(`/${name}`));

const result = {};
let unknown = 0;
const rules = (section) => allow[section] ?? {};
const base = (f) => f.replace(/^rust\/crates\//, "").replace(/\/src\//, "/");
const short = (files, n = 3) =>
  files.slice(0, n).map(base).join(", ") + (files.length > n ? ", ..." : "");

// A declared-handled entry must prove itself: `probe` is "file-suffix::regex" (several joined with " && ").
const probeHolds = (probe) =>
  probe.split(" && ").every((part) => {
    const [file, re] = part.split("::");
    const files = file === "*" ? uiFiles : uiFiles.filter((f) => f.endsWith(file));
    return files.length > 0 && files.some((f) => new RegExp(re, "s").test(uiText[f]));
  });

// derived: whether the script found the entry handled; where: evidence text
const classify = (section, name, derived, where, context = "") => {
  const r = rules(section);
  let status = derived ? "handled" : "MISSING";
  let reason = where ?? "";
  const declared = r.handled?.[name];
  if (!derived && declared) {
    const probe = typeof declared === "string" ? null : declared.probe;
    if (probe && !probeHolds(probe))
      reason = `declared handled, but probe ${probe} no longer matches`;
    else [status, reason] = ["handled", typeof declared === "string" ? declared : declared.reason];
  }
  if (r.partial?.[name] && (status === "handled" || derived))
    [status, reason] = ["partial", r.partial[name]];
  if (status === "MISSING" && r.partial?.[name]) [status, reason] = ["partial", r.partial[name]];
  if (status === "MISSING" && r.pending?.[name]) [status, reason] = ["pending", r.pending[name]];
  if (status === "MISSING" && r.omitted?.[name]) [status, reason] = ["omitted", r.omitted[name]];
  if (status === "MISSING") unknown += 1;
  (result[section] ??= []).push({ name, status, reason: reason + context });
};

// ================================================================ rpc
const rpcSection = () => {
  const block = (source, name) => {
    const m = source.match(new RegExp(`export const ${name} = \\{([\\s\\S]*?)\\} as const`));
    return Object.fromEntries(
      [...m[1].matchAll(/^\s*(\w+):\s*"([^"]+)"/gm)].map((x) => [x[1], x[2]]),
    );
  };
  const methods = {
    ...block(read("packages/contracts/src/rpc.ts"), "WS_METHODS"),
    ...block(read("packages/contracts/src/orchestrationV2.ts"), "ORCHESTRATION_V2_WS_METHODS"),
  };
  // What the server handles (the same reading as check_rpc_surface.mjs).
  const serverDir = "rust/crates/server/src";
  const serverFiles = fs
    .readdirSync(path.join(root, serverDir))
    .filter((f) => f.endsWith(".rs"))
    .filter(
      (f) =>
        !/(_tests?\.rs$|^rpc_handler_decoders|^rpc_response|^rpc_admission|^test_support|^fork_parity|^reducer_bench|^fixture\.rs)/.test(
          f,
        ),
    );
  const serverText = serverFiles.map((f) => stripTests(read(`${serverDir}/${f}`)));
  const serverHandles = (m) => serverText.some((t) => t.includes(`"${m}"`));
  // A Rust call names the method as a string literal near a request, or as the value of a `&str` constant.
  const callWindow =
    /request|ask\(|subscribe|stream|transport|method|: &str =|RequestKind|dispatch|call\(/i;
  const rustCalls = (m) =>
    uiFiles.filter((f) => {
      if (!f.endsWith(".rs")) return false;
      const lines = uiText[f].split("\n");
      return lines.some((line, i) => {
        if (!line.includes(`"${m}"`)) return false;
        const window = lines.slice(Math.max(0, i - 10), i + 2).join("\n");
        return (
          callWindow.test(window) &&
          !/(keys|command_palette|keybinding_settings|keybindings|palette)\.rs$/.test(f)
        );
      });
    });
  for (const [key, method] of Object.entries(methods).sort((a, b) => a[1].localeCompare(b[1]))) {
    const tagRe = new RegExp(`\\b(?:WS_METHODS|ORCHESTRATION_V2_WS_METHODS)\\.${key}\\b`);
    let orig = webWhere(tagRe);
    // A tag named only where an atom command is defined (`clear: createEnvironmentRpcCommand(... tag: X)`) counts
    // when something outside the definitions uses that member.
    const defined = orig.filter((f) =>
      new RegExp(
        `(\\w+):\\s*createEnvironmentRpc\\w+\\([^]*?tag:\\s*(?:WS_METHODS|ORCHESTRATION_V2_WS_METHODS)\\.${key}\\b`,
      ).test(webText[f]),
    );
    if (defined.length > 0 && defined.length === orig.length) {
      const props = new Set();
      for (const f of defined) {
        for (const m of webText[f].matchAll(
          new RegExp(
            `(\\w+):\\s*createEnvironmentRpc\\w+\\((?:(?!createEnvironmentRpc)[^])*?tag:\\s*(?:WS_METHODS|ORCHESTRATION_V2_WS_METHODS)\\.${key}\\b`,
            "g",
          ),
        ))
          props.add(m[1]);
      }
      const used = webFiles.filter(
        (f) =>
          !defined.includes(f) &&
          [...props].some((prop) => new RegExp(`[eE]nvironment\\.${prop}\\b`).test(webText[f])),
      );
      const inState = defined.filter((f) =>
        [...props].some((prop) =>
          new RegExp(`(?:\\.|\\b)${prop}\\b[^:]`).test(
            webText[f].replace(new RegExp(`${[...props][0]}:\\s*createEnvironmentRpc`, "g"), ""),
          ),
        ),
      );
      orig = used.length ? used : inState.length ? inState : [];
    }
    if (orig.length === 0) continue; // the original web client never calls it
    const calls = rustCalls(method);
    const served = serverHandles(method) ? "server handles it" : "server does not handle it either";
    // A call to a method the server does not handle cannot work, however much UI sits on it.
    if (calls.length > 0 && !serverHandles(method)) {
      classify(
        "rpc",
        method,
        false,
        `the Rust UI calls it (${short(calls)}) but the server does not handle it`,
      );
      continue;
    }
    classify(
      "rpc",
      method,
      calls.length > 0,
      calls.length ? short(calls) : "",
      calls.length ? "" : `; ${served}; original: ${short(orig, 2)}`,
    );
  }
};

// ================================================================ http
const httpSection = () => {
  const contract = read("packages/contracts/src/environmentHttp.ts");
  const webhook = contract.match(/WEBHOOK_PATH = "([^"]+)"/)[1];
  const endpoints = [];
  for (const m of contract.matchAll(
    /HttpApiEndpoint\.(get|post|put|patch|delete)\(\s*"(\w+)",\s*(WEBHOOK_PATH|"[^"]+")/g,
  ))
    endpoints.push([
      m[1].toUpperCase(),
      m[3] === "WEBHOOK_PATH" ? webhook : m[3].slice(1, -1),
      m[2],
    ]);
  for (const m of contract.matchAll(
    /\.add\(HttpApiEndpoint\.(get|post)\("(\w+)",\s*"([^"]+)"\)\)/g,
  ))
    endpoints.push([m[1].toUpperCase(), m[3], m[2]]);
  // Routes the original client builds by hand (asset, attachment and device-hub URLs).
  for (const [p, name] of [
    ["/api/assets/:environmentId/:token", "assets"],
    ["/api/attachments/upload/:attachmentId", "attachmentUpload"],
    ["/api/device-hub", "deviceHub"],
    ["/api/preview-stream", "previewStream"],
  ])
    endpoints.push(["GET", p, name]);
  const prefix = (p) => p.split("/:")[0].replace(/^\//, "");
  const seen = new Set();
  for (const [method, p, name] of endpoints) {
    const key = `${method} ${p}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const pre = prefix(p);
    // The original calls an endpoint through a generated client by its name, or builds the path.
    const escaped = pre.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    // A generated-client call names the endpoint (`client.auth.session(`, or a chained `.linkProof(`); an MCP OAuth
    // endpoint shares its name with a plain one (`token`), so it needs its group.
    const call = p.startsWith("/oauth/mcp")
      ? `mcpOAuth\\.${name}\\(`
      : `(?:client\\.(?:\\w+\\.)?|^\\s*\\.)${name}\\(`;
    const origFiles = webFiles.filter((f) =>
      new RegExp(`${escaped}|${call}`, "m").test(webText[f]),
    );
    if (origFiles.length === 0 || /^(webhook(Post|Put|Patch|Get))$/.test(name)) continue;
    const rustFiles = uiWhere(new RegExp(escaped));
    classify(
      "http",
      key,
      rustFiles.length > 0,
      short(rustFiles),
      rustFiles.length ? "" : `; original: ${short(origFiles, 2)}`,
    );
  }
};

// ================================================================ routes
const routesSection = () => {
  const routeFiles = fs
    .readdirSync(path.join(root, "apps/web/src/routes"))
    .filter((f) => f.endsWith(".tsx"));
  const routeRs = uiText[uiOf("route.rs")] ?? "";
  const rustPaths = new Set(["/"]);
  for (const m of routeRs.matchAll(/Self::\w+ => "([a-z-]+)",/g))
    rustPaths.add(`/settings/${m[1]}`);
  for (const m of routeRs.matchAll(/\["([a-z-]+)"\] =>/g)) rustPaths.add(`/${m[1]}`);
  if (/Route::Thread/.test(routeRs)) rustPaths.add("/$environmentId/$threadId");
  // A route with one parameter after a fixed segment (`["projects", key] => Some(Route::Project(..))`).
  for (const m of routeRs.matchAll(/\["([a-z-]+)", \w+\] (?:if [^=]+)?=> Some\(Route::\w+\(/g))
    rustPaths.add(`/${m[1]}/*`);
  for (const file of routeFiles.sort()) {
    const name = file.replace(/\.tsx$/, "");
    if (["__root", "_chat", "settings"].includes(name)) continue; // layouts, not pages
    const p =
      "/" +
      name
        .replace(/^_chat\./, "")
        .replace(/^_chat$/, "")
        .replace(/^index$/, "")
        .split(".")
        .map((s) => (s === "index" ? "" : s))
        .filter(Boolean)
        .join("/");
    const key = p === "/" ? "/" : p.replace(/\/$/, "");
    classify(
      "routes",
      key,
      rustPaths.has(key) ||
        (key === "/" && rustPaths.has("/")) ||
        (/^\/[a-z-]+\/\$\w+$/.test(key) && rustPaths.has(key.replace(/\/\$\w+$/, "/*"))),
      "client/route.rs",
      `; original: routes/${file}`,
    );
  }
};

// ================================================================ commands
const commandsSection = () => {
  // the original's keybinding commands
  const kb = read("packages/contracts/src/keybindings.ts");
  const arrayOf = (name) => {
    const m = kb.match(new RegExp(`(?:export )?const ${name} = \\[([\\s\\S]*?)\\] as const`));
    return [...m[1].matchAll(/"([^"]+)"/g)].map((x) => x[1]);
  };
  const commands = [...new Set(arrayOf("STATIC_KEYBINDING_COMMANDS"))];
  const modelJump = arrayOf("MODEL_PICKER_JUMP_KEYBINDING_COMMANDS");
  const threadJump = arrayOf("THREAD_JUMP_KEYBINDING_COMMANDS");
  const all = [
    ...new Set([
      ...commands,
      ...modelJump,
      ...threadJump,
      ...arrayOf("THREAD_KEYBINDING_COMMANDS"),
      ...arrayOf("MODEL_PICKER_KEYBINDING_COMMANDS"),
    ]),
    "script.<id>.run",
  ];
  const keys = uiText[uiOf("keys.rs")];
  const exec = keys.slice(keys.indexOf("pub fn execute"));
  const execBody = exec.slice(0, exec.indexOf("/// Puts the cursor in the message box"));
  const arms = new Set();
  for (const m of execBody.matchAll(/^\s{12}((?:"[\w.]+"\s*\|?\s*)+)(?:if [^=]*)?=>/gm))
    for (const c of m[1].matchAll(/"([\w.]+)"/g)) arms.add(c[1]);
  if (/jump if jump\.starts_with\("modelPicker\.jump\."\)/.test(execBody))
    for (let i = 1; i <= 9; i += 1) arms.add(`modelPicker.jump.${i}`);
  if (/thread_nav::jump_index/.test(execBody))
    for (let i = 1; i <= 9; i += 1) arms.add(`thread.jump.${i}`);
  const wired = new Set(
    [...uiText[uiOf("keybinding_settings.rs")].matchAll(/^\s{4}"([\w.]+)",$/gm)].map((m) => m[1]),
  );
  for (const c of all) {
    if (c === "script.<id>.run") {
      const ran = uiWhere(/script\.\{?[^"]*\.run|"script\./);
      classify("commands", c, ran.length > 0, short(ran), "; project scripts (Run actions)");
      continue;
    }
    const carried = arms.has(c);
    const listed = wired.has(c);
    const label = carried
      ? listed
        ? "key runner arm; listed on the Keybindings page"
        : "key runner arm; not listed on the Keybindings page"
      : listed
        ? "LISTED BUT NOTHING RUNS IT"
        : "";
    // A command the page lists and the runner has no arm for is the worst kind of gap: the shortcut does nothing.
    if (!carried && listed && !allow.commands?.pending?.[c] && !allow.commands?.omitted?.[c])
      unknown += 1;
    classify("commands", c, carried && listed, label);
  }
  // the original's palette actions
  const palette = read("apps/web/src/components/CommandPalette.tsx");
  const actionIds = new Set();
  for (const m of palette.matchAll(/value: [`"]action:([a-z-]+)(?::[^`"]*)?[`"]/g))
    actionIds.add(`action:${m[1]}`);
  const rustPalette = uiText[uiOf("command_palette.rs")];
  const rustIds = new Set(
    [...rustPalette.matchAll(/"(action:[a-z-]+)(?::[^"]*)?"/g)].map((m) => m[1]),
  );
  for (const id of [...actionIds].sort()) {
    const listedIn = rustIds.has(id);
    classify("commands", `palette ${id}`, listedIn, listedIn ? "command_palette.rs" : "");
  }
  // Rust palette actions nothing handles: an id that is built but named once only (never matched in `run`) does nothing.
  for (const m of rustPalette.matchAll(/Item::action\("(action:[a-z-]+)"/g)) {
    const named = rustPalette.split(`"${m[1]}"`).length - 1;
    if (named < 2)
      classify(
        "commands",
        `palette ${m[1]} runs`,
        false,
        "listed by the Rust palette but nothing handles it",
      );
  }
  // buttons drawn with no handler at all (not a submit, no click, keyboard or press handler)
  for (const f of uiFiles.filter((p) => p.endsWith(".rs") && p.startsWith("rust/crates/ui/"))) {
    const text = uiText[f];
    for (const m of text.matchAll(/\bbutton\s*\{/g)) {
      let depth = 1;
      let i = m.index + m[0].length;
      for (; i < text.length && depth > 0; i += 1)
        depth += text[i] === "{" ? 1 : text[i] === "}" ? -1 : 0;
      const body = text.slice(m.index, i);
      if (
        /onclick|onsubmit|"submit"|onmousedown|onkeydown|onpointerdown|\.\.props|\.\.attrs/.test(
          body,
        )
      )
        continue;
      const label =
        body.match(/"aria-label": "([^"]+)"/)?.[1] ?? body.replace(/\s+/g, " ").slice(0, 40);
      classify(
        "commands",
        `button without handler ${base(f)} "${label}"`,
        false,
        "a button that does nothing when pressed",
      );
    }
  }
  // palette entries that name a command the runner has no arm for
  const entryCommands = [
    ...rustPalette.matchAll(/\("action:[a-z-]+", "[^"]+", &\[[^\]]*\], "([\w.]+)"\)/g),
  ].map((m) => m[1]);
  const shortcutCommands = [...rustPalette.matchAll(/Some\("([\w.]+)"\)/g)].map((m) => m[1]);
  for (const c of new Set([...entryCommands, ...shortcutCommands]))
    if (!arms.has(c) && !/^(theme\.select|appearance\.cycle)$/.test(c))
      classify(
        "commands",
        `palette entry runs ${c}`,
        false,
        "the palette lists an action whose command the key runner does not carry out",
      );
};

// ================================================================ settings
const settingsSection = () => {
  const items = JSON.parse(read("rust/crates/client/src/settings_search_items.json"));
  const catalog = read("apps/web/src/components/settings/settingsSearch.ts");
  const originalCount = (
    catalog.slice(catalog.indexOf("SETTINGS_SEARCH_ITEMS = [")).match(/^\s{4}id: "/gm) ?? []
  ).length;
  if (originalCount !== items.length) {
    classify(
      "settings",
      "catalog copy",
      false,
      `the Rust copy has ${items.length} entries, the original ${originalCount}: run generate_settings_search_items.mjs`,
    );
  }
  const placement = uiText[uiOf("settings_search.rs")];
  const place = new Map();
  for (const m of placement.matchAll(/"([\w-]+)" => Placement::(Here|Missing)\("([^"]*)"\)/g))
    place.set(m[1], [m[2], m[3]]);
  // The rows of settings_rows.rs: title -> key.
  const rowsText = uiText[uiOf("settings_rows.rs")];
  const rowOfTitle = new Map();
  for (const m of rowsText.matchAll(/Row::(?:switch|choice|slider)\("(\w+)", "([^"]+)"/g))
    rowOfTitle.set(m[2], m[1]);
  const noEffectFiles = (k) =>
    uiFiles.filter(
      (f) =>
        !/settings_rows\.rs$|client_settings\.rs$|general_page\.rs$|\/settings_search/.test(f) &&
        f.endsWith(".rs"),
    );
  const serverDir = "rust/crates/server/src";
  const serverText = fs
    .readdirSync(path.join(root, serverDir))
    .filter((f) => f.endsWith(".rs") && !/(_tests?\.rs$|^server_settings|^settings)/.test(f))
    .map((f) => stripTests(read(`${serverDir}/${f}`)));
  const uiPageText = uiFiles
    .filter((f) => f.endsWith(".rs"))
    .map((f) => uiText[f])
    .join("\n");
  const originalPages = new Set();
  for (const item of items) originalPages.add(item.to);
  for (const item of items) {
    const entry = place.get(item.id);
    const gate = [
      "desktopOnly",
      "macOnly",
      "windowsOnly",
      "cloudOnly",
      "providerSettingsOnly",
      "localEnvironmentOnly",
      "wslAvailableOnly",
      "managedTunnelOnly",
      "localBackendManagementOnly",
    ].filter((g) => item[g]);
    const ctx = ` [${item.to}${gate.length ? "; " + gate.join(",") : ""}] ${item.title}`;
    if (!entry) {
      classify("settings", item.id, false, "no placement in settings_search.rs", ctx);
      continue;
    }
    const [kind, where] = entry;
    if (kind === "Missing") {
      classify("settings", item.id, false, where, ctx);
      continue;
    }
    // Here: the control is anchored by its title (aria-label) or id, and something acts on its value.
    const anchor =
      uiPageText.includes(`"${item.title}"`) ||
      uiPageText.includes(`"${item.targetId ?? item.id}"`) ||
      [...rowOfTitle.keys()].some((t) => t === item.title);
    const key = rowOfTitle.get(item.title);
    let acts = true;
    if (key) {
      const field = snake(key);
      const client = noEffectFiles(key).some((f) => new RegExp(`\\b${field}\\b`).test(uiText[f]));
      const server = serverText.some((t) => t.includes(`.${field}`) || t.includes(`"${key}"`));
      acts = client || server;
    }
    const derived = anchor && acts;
    const note = !anchor
      ? "the page names the entry but no control with its title or id exists"
      : !acts
        ? `control for \`${key}\` present, nothing reads the value`
        : "control present";
    classify("settings", item.id, derived, `${where}: ${note}`, ctx);
  }
};

// ================================================================ desktop IPC
const desktopSection = () => {
  const preload = read("apps/desktop/src/preload.ts");
  const bridge = preload.slice(preload.indexOf('exposeInMainWorld("desktopBridge", {'));
  const members = [];
  let group = null;
  for (const line of bridge.split("\n").slice(1)) {
    let m = /^  (\w+): \{$/.exec(line);
    if (m) {
      group = m[1];
      continue;
    }
    if (/^  \},?$/.test(line)) {
      group = null;
      continue;
    }
    m = /^  (\w+):/.exec(line);
    if (m) {
      members.push(m[1]);
      continue;
    }
    m = /^    (\w+):/.exec(line);
    if (m && group) members.push(`${group}.${m[1]}`);
    m = /^      (\w+): \{$/.exec(line);
    if (m && group) members.push(`${group}.${m[1]}`);
    m = /^      (open|close|onInput|startScreencast|stopScreencast|save|onFrame):/.exec(line);
    if (m && group === "preview") members.push(`preview.*.${m[1]}`);
  }
  // Callers in the original web app.
  for (const member of [...new Set(members)]) {
    const leaf = member.split(".").pop();
    const callers = webWhere(new RegExp(`\\b${leaf}\\b`)).filter((f) =>
      f.startsWith("apps/web/src"),
    );
    classify(
      "desktop",
      member,
      false,
      "",
      callers.length ? `; web callers: ${short(callers, 2)}` : "; no web caller",
    );
  }
};

// ================================================================ timeline, composer, panels
const surfaceSection = () => {
  // turn-item types
  const orch = read("packages/contracts/src/orchestrationV2.ts");
  const from = orch.indexOf("export const OrchestrationV2TurnItem = Schema.Union([");
  const to = orch.indexOf("export const OrchestrationV2ProjectedTurnItem");
  const itemTypes = [
    ...new Set(
      [...orch.slice(from, to).matchAll(/^\s{4}type: Schema\.Literal\("(\w+)"\)/gm)].map(
        (m) => m[1],
      ),
    ),
  ];
  const timelineFiles = ["timeline.rs", "item_cards.rs", "work_rows.rs", "timeline_rows.rs"]
    .map((f) => uiOf(f))
    .filter(Boolean);
  for (const type of itemTypes) {
    const found = timelineFiles.filter((f) => new RegExp(`"${type}"`).test(uiText[f]));
    classify("surface", `turn item ${type}`, found.length > 0, short(found));
  }
  // timeline row kinds
  const logic = read("apps/web/src/components/chat/MessagesTimeline.logic.ts");
  const rowFrom = logic.indexOf("export type MessagesTimelineRow =");
  const rowKinds = [
    ...new Set(
      [...logic.slice(rowFrom, rowFrom + 6000).matchAll(/^\s{6}kind: "([\w-]+)";/gm)].map(
        (m) => m[1],
      ),
    ),
  ];
  for (const kind of rowKinds) classify("surface", `timeline row ${kind}`, false, "");
  // work group kinds: the variants of `GroupAction` and `SummaryKind` in the Rust timeline derivation
  const tl = read("apps/web/src/components/chat/MessagesTimeline.tsx");
  const groupFrom = tl.indexOf('case "pull-request":');
  const groupKinds = [
    ...new Set(
      [...tl.slice(groupFrom, groupFrom + 1400).matchAll(/case "([\w-]+)":/g)].map((m) => m[1]),
    ),
  ];
  const rows = uiText[uiOf("timeline_rows.rs")];
  const variants = new Set(
    [
      ...(rows.match(/pub enum GroupAction \{([^}]*)\}/)?.[1] ?? "").matchAll(/^\s*(\w+),/gm),
      ...(rows.match(/pub enum SummaryKind \{([^}]*)\}/)?.[1] ?? "").matchAll(/^\s*(\w+)[,(]/gm),
    ].map((m) => m[1]),
  );
  const pascal = (kind) =>
    kind
      .split("-")
      .map((w) => w[0].toUpperCase() + w.slice(1))
      .join("");
  for (const kind of groupKinds)
    classify(
      "surface",
      `work group ${kind}`,
      variants.has(pascal(kind)),
      `client/timeline_rows.rs ${pascal(kind)}`,
    );
  // user message context kinds
  const contextKinds = [
    ...new Set(
      [
        ...tl.matchAll(
          /canRender: \(record(?:, context)?\) =>[\s\S]{0,60}?record\.kind === "([\w-]+)"/g,
        ),
      ].map((m) => m[1]),
    ),
  ];
  for (const kind of contextKinds) classify("surface", `message context ${kind}`, false, "");
  // right panel surfaces
  const store = read("apps/web/src/rightPanelStore.ts");
  const union = store.slice(
    store.indexOf("export type RightPanelSurface ="),
    store.indexOf("const RIGHT_PANEL_STORAGE_KEY"),
  );
  for (const kind of [...new Set([...union.matchAll(/kind: "([\w-]+)"/g)].map((m) => m[1]))])
    classify("surface", `right panel surface ${kind}`, false, "");
  // composer: one entry per source file of the original's composer
  const composerFiles = walk(
    "apps/web/src/components/chat",
    (p) =>
      /\/(Composer|use(Composer)|Queued|ComposerStash|composer)[\w]*\.tsx?$/.test(p) &&
      !isTsTest(p),
  )
    .map((p) =>
      p
        .split("/")
        .pop()
        .replace(/\.tsx?$/, ""),
    )
    .concat(
      [
        "composer-logic",
        "composer-rich-text",
        "composer-code-block",
        "composerDraftStore",
        "promptStashStore",
        "pendingUserInput",
      ].filter((n) => exists(`apps/web/src/${n}.ts`)),
    );
  for (const name of [...new Set(composerFiles)].sort())
    classify("surface", `composer ${name}`, false, "");
};

// ================================================================ capabilities
const capsSection = () => {
  const env = read("packages/contracts/src/environment.ts");
  const start = env.indexOf("export const ExecutionEnvironmentCapabilities = Schema.Struct({");
  const body = env.slice(start, env.indexOf("export type ExecutionEnvironmentCapabilities", start));
  const flags = [...body.matchAll(/^  (\w+):/gm)].map((m) => m[1]);
  const rustCaps = uiText[uiFiles.find((f) => f.endsWith("contracts/src/server_config.rs"))] ?? "";
  void rustCaps;
  for (const flag of flags) {
    const readers = webWhere(new RegExp(`\\b${flag}\\b`)).filter((f) => !/\/contracts\//.test(f));
    if (readers.length === 0) continue; // the original UI never reads it
    const field = snake(flag);
    const rust = uiWhere(new RegExp(`\\b${field}\\b|"${flag}"`)).filter((f) => f.endsWith(".rs"));
    classify(
      "caps",
      flag,
      rust.length > 0,
      short(rust),
      rust.length ? "" : `; original reads it in ${short(readers, 2)}`,
    );
  }
};

const sections = {
  rpc: rpcSection,
  http: httpSection,
  routes: routesSection,
  commands: commandsSection,
  settings: settingsSection,
  desktop: desktopSection,
  surface: surfaceSection,
  caps: capsSection,
};
for (const [name, run] of Object.entries(sections)) if (!only || only === name) run();

if (asJson) console.log(JSON.stringify(result, null, 2));
else {
  const order = ["handled", "partial", "pending", "omitted", "MISSING"];
  for (const [section, rows] of Object.entries(result)) {
    const counts = {};
    for (const row of rows) counts[row.status] = (counts[row.status] ?? 0) + 1;
    console.log(`\n== ${section}: ${rows.length} ${JSON.stringify(counts)}`);
    for (const status of order) {
      if (status === "handled" && !showAll) continue;
      for (const row of rows.filter((r) => r.status === status))
        console.log(`  ${status.padEnd(8)} ${row.name}${row.reason ? `  -- ${row.reason}` : ""}`);
    }
  }
}
if (unknown > 0) {
  console.error(
    `\n${unknown} gap(s) the allowlist does not know. Port them, or add them to check_ui_surface_allowlist.json with a reason.`,
  );
  process.exit(1);
}
