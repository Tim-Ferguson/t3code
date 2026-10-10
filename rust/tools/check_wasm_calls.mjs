#!/usr/bin/env node
// Fails when code compiled for `wasm32-unknown-unknown` calls something the target cannot do.
// `std::time::Instant::now()`, `SystemTime::now()`, `std::thread::spawn`, `std::fs`, `block_on` and
// the like compile for wasm and then abort the module at run time ("RuntimeError: unreachable"),
// which kills the whole web application with no message.
//
//   node rust/tools/check_wasm_calls.mjs          check, exit 1 on a call that is neither gated nor allowed
//   node rust/tools/check_wasm_calls.mjs --list   print every hit with its verdict
//
// Scanned: the crates that the web build compiles in (`ui`, `client`, `contracts`, `highlight`,
// `terminal`). A hit is fine when
//   * it sits inside an item or statement marked `#[cfg(not(target_arch = "wasm32"))]`,
//     `#[cfg(feature = "desktop")]` (the desktop and web features are never built together), or
//     `#[cfg(test)]` / `#[test]`, or in a file whose `mod` declaration is marked so, or
//   * `rust/tools/wasm_calls_allowlist.json` has an entry for it, with the reason it is safe.
//     An entry is `"<path>::<rule id>": "<reason>"`; a stale entry (no such hit any more) fails too.
//
// It also checks that `chrono` keeps its `wasmbind` feature, which is what makes `Utc::now()` and
// `Local` read the browser's clock instead of `SystemTime`.
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const tools = dirname(fileURLToPath(import.meta.url));
const rustRoot = join(tools, "..");
const crates = ["ui", "client", "contracts", "highlight", "terminal"];
const allowlistPath = join(tools, "wasm_calls_allowlist.json");

/// What cannot run in a browser, by rule id. Each pattern is matched against code with comments and
/// string contents blanked out.
const RULES = {
  instant:
    /\bInstant::now\b|\bstd::time::Instant\b|\btime::\{[^}]*\bInstant\b|\buse\s+std::time::Instant\b/,
  "system-time": /\bSystemTime\b|\bUNIX_EPOCH\b/,
  thread:
    /\bstd::thread\b|\bthread::(?:spawn|sleep|Builder|scope|park|yield_now)\b|\bavailable_parallelism\b/,
  fs: /\bstd::fs\b|\bfs::(?:read|write|File|create_dir|remove|metadata|read_dir|read_to_string)\b|\.canonicalize\(/,
  env: /\bstd::env\b|\benv::(?:var|var_os|args|current_dir|temp_dir|home_dir|vars)\b/,
  process: /\bstd::process\b|\bprocess::(?:Command|exit|abort|id)\b/,
  net: /\bstd::net::(?:Tcp|Udp)/,
  "block-on":
    /\bblock_on\b|\bblocking_(?:recv|send|lock|read|write)\b|\.recv_timeout\(|\bCondvar\b|\bstd::sync::mpsc\b|\breqwest::blocking\b/,
  tokio: /\btokio::/,
  stdin: /\bstdin\(\)/,
  "local-clock": /\bLocal::now\b|\bchrono::Local\b|\bLocal\.(?:timestamp|from_)/,
};

function strip(text) {
  // Keep "wasm32" and "desktop" so a cfg attribute can still be read; blank every other string, char and comment.
  let out = "";
  let i = 0;
  const blank = (chunk) => chunk.replace(/[^\n]/g, " ");
  while (i < text.length) {
    const two = text.slice(i, i + 2);
    if (two === "//") {
      const end = text.indexOf("\n", i);
      const stop = end === -1 ? text.length : end;
      out += blank(text.slice(i, stop));
      i = stop;
    } else if (two === "/*") {
      let depth = 1;
      let j = i + 2;
      while (j < text.length && depth > 0) {
        if (text.startsWith("/*", j)) (depth++, (j += 2));
        else if (text.startsWith("*/", j)) (depth--, (j += 2));
        else j++;
      }
      out += blank(text.slice(i, j));
      i = j;
    } else if (
      text[i] === "r" &&
      /^r#*"/.test(text.slice(i, i + 12)) &&
      !/[A-Za-z0-9_]/.test(text[i - 1] ?? "")
    ) {
      const hashes = text.slice(i + 1).match(/^#*/)[0];
      const close = `"${hashes}`;
      const end = text.indexOf(close, i + 2 + hashes.length);
      const stop = end === -1 ? text.length : end + close.length;
      out += blank(text.slice(i, stop));
      i = stop;
    } else if (text[i] === '"') {
      let j = i + 1;
      while (j < text.length && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
      const literal = text.slice(i, j + 1);
      const keep =
        literal === '"wasm32"'
          ? "WASM32"
          : literal === '"desktop"'
            ? "DESKTOP"
            : /#\s*\[\s*path\s*=\s*$/.test(out)
              ? literal
              : null;
      out += keep ?? blank(literal);
      i = j + 1;
    } else if (text[i] === "'" && /^'(?:\\.[^']*|[^\\'])'/.test(text.slice(i, i + 12))) {
      const literal = text.slice(i).match(/^'(?:\\.[^']*|[^\\'])'/)[0];
      out += blank(literal);
      i += literal.length;
    } else {
      out += text[i];
      i++;
    }
  }
  return out;
}

// A header (attributes plus the start of the item or statement) that keeps the code out of the wasm build.
const GATED = [
  /not\s*\(\s*target_arch\s*=\s*WASM32\s*\)/,
  /\bfeature\s*=\s*DESKTOP\b/,
  /cfg\s*\(\s*test\s*\)/,
  /cfg\s*\(\s*all\s*\(\s*test\b/,
  /#\s*\[\s*(?:\w+::)?test\b/,
];
const gated = (header) => GATED.some((re) => re.test(header));

function walk(dir, files = []) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path, files);
    else if (name.endsWith(".rs")) files.push(path);
  }
  return files;
}

/// Statement starts: for an offset, the text from the previous `;`, `{` or `}` up to it. This holds
/// the attributes of the item or statement that contains the offset.
function headerBefore(code, offset) {
  let j = offset - 1;
  while (j >= 0 && !";{}".includes(code[j])) j--;
  return code.slice(j + 1, offset);
}

/// The headers of every block that encloses `offset`, innermost first, plus the offset's own
/// statement header.
function headersAt(code, offset) {
  const open = [];
  for (let i = 0; i < offset; i++) {
    if (code[i] === "{") open.push(i);
    else if (code[i] === "}") open.pop();
  }
  return [headerBefore(code, offset), ...open.map((at) => headerBefore(code, at))];
}

/// Files that a `mod name;` declaration gates away, as absolute paths.
function gatedModules(file, code) {
  const gatedFiles = new Set();
  const base = /(?:^|\/)(?:lib|main|mod)\.rs$/.test(file)
    ? dirname(file)
    : join(dirname(file), file.split("/").pop().replace(/\.rs$/, ""));
  const declaration = /\bmod\s+(\w+)\s*;/g;
  let match;
  while ((match = declaration.exec(code))) {
    const header = headerBefore(code, match.index);
    if (!gated(header)) continue;
    const path = header.match(/#\s*\[\s*path\s*=\s*"([^"]+)"/);
    if (path) gatedFiles.add(join(dirname(file), path[1]));
    else
      for (const candidate of [join(base, `${match[1]}.rs`), join(base, match[1], "mod.rs")])
        gatedFiles.add(candidate);
  }
  return gatedFiles;
}

const lineOf = (code, offset) => code.slice(0, offset).split("\n").length;

function scan() {
  const hits = [];
  const sources = [];
  for (const name of crates) {
    const src = join(rustRoot, "crates", name, "src");
    if (existsSync(src)) sources.push(...walk(src));
  }
  const stripped = new Map(sources.map((file) => [file, strip(readFileSync(file, "utf8"))]));
  const gatedFiles = new Set();
  for (const [file, code] of stripped)
    for (const path of gatedModules(file, code)) gatedFiles.add(path);
  for (const [file, code] of stripped) {
    const rel = relative(rustRoot, file);
    if (gatedFiles.has(file) || /(?:_tests|tests)\.rs$/.test(file) || /\/tests\//.test(file))
      continue;
    if (/^\s*#!\[cfg\([^\]]*(?:test|not\s*\(\s*target_arch\s*=\s*WASM32)/m.test(code)) continue;
    for (const [rule, pattern] of Object.entries(RULES)) {
      const global = new RegExp(pattern.source, "g");
      let match;
      while ((match = global.exec(code))) {
        const verdict = headersAt(code, match.index).some(gated) ? "gated" : "open";
        hits.push({ file: rel, line: lineOf(code, match.index), rule, text: match[0], verdict });
      }
    }
  }
  return hits;
}

function chronoOk() {
  const manifest = readFileSync(join(rustRoot, "Cargo.toml"), "utf8");
  const line = manifest.split("\n").find((entry) => /^chrono\s*=/.test(entry)) ?? "";
  return !/default-features\s*=\s*false/.test(line) || /wasmbind/.test(line);
}

const hits = scan();
const allowlist = existsSync(allowlistPath) ? JSON.parse(readFileSync(allowlistPath, "utf8")) : {};
const keyOf = (hit) => `${hit.file}::${hit.rule}`;

if (process.argv.includes("--list")) {
  for (const hit of hits)
    console.log(`${hit.verdict.padEnd(5)} ${hit.file}:${hit.line} [${hit.rule}] ${hit.text}`);
}

const problems = [];
const used = new Set();
for (const hit of hits.filter((entry) => entry.verdict === "open")) {
  if (keyOf(hit) in allowlist) used.add(keyOf(hit));
  else
    problems.push(
      `${hit.file}:${hit.line}: \`${hit.text}\` [${hit.rule}] runs in the browser build and aborts it; gate it with #[cfg(not(target_arch = "wasm32"))] and give the web a browser source (js_sys::Date, gloo_timers), or allowlist it with the reason it is safe`,
    );
}
for (const key of Object.keys(allowlist)) {
  if (!used.has(key))
    problems.push(
      `allowlist entry "${key}" matches no ungated hit any more; remove it from rust/tools/wasm_calls_allowlist.json`,
    );
}
if (!chronoOk())
  problems.push(
    "rust/Cargo.toml: chrono must keep default features or enable `wasmbind`, or Utc::now() panics in the browser",
  );

if (problems.length) {
  console.error(problems.join("\n"));
  process.exit(1);
}
console.log(
  `check_wasm_calls: ${hits.length} hits in ${crates.length} crates, ${hits.filter((h) => h.verdict === "gated").length} gated, ${used.size} allowed`,
);
