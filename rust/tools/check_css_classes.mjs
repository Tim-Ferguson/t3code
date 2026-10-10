#!/usr/bin/env node
// Fails when a class name used in the UI's markup (`class: "..."` in rsx!, and class strings in
// the page scripts) has no rule in the stylesheet and is not listed as marker-only.
//
//   node rust/tools/check_css_classes.mjs          check, exit 1 on any unstyled class
//   node rust/tools/check_css_classes.mjs --list   print every class with no rule (to triage)
//
// `rust/tools/css_class_markers.json` maps a class that is deliberately unstyled to the reason:
// a test hook, an ARIA-less marker, a state flag read by another selector. A stale entry (the
// class got a rule, or left the markup) fails too, so the list cannot rot.
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "crates", "ui");
const allowlistPath = join(dirname(fileURLToPath(import.meta.url)), "css_class_markers.json");

/// The class strings in an attribute expression. In `if cond { "a" } else { "b" }` only the
/// branches name classes, not the strings the condition compares with; in a `match` only what
/// follows `=>` does. Anything else (a plain literal, `.then_some("a")`) is read whole.
function literals(span) {
  const found = [];
  const re = /"((?:[^"\\]|\\.)*)"/g;
  const trimmed = span.trim();
  const conditional = /^if[\s{(]/.test(trimmed) || /^if\b/.test(trimmed);
  const matching = /^match\b/.test(trimmed);
  let match;
  while ((match = re.exec(span))) {
    const before = span.slice(0, match.index);
    if (conditional) {
      const depth = (before.match(/{/g) || []).length - (before.match(/}/g) || []).length;
      if (depth < 1) continue;
    } else if (matching && !/=>\s*(?:\{\s*)?$/.test(before)) continue;
    found.push(match[1]);
  }
  return found;
}

/// The expression after `class:` up to the comma or closing brace that ends the attribute.
function expressionAt(text, start) {
  let depth = 0;
  let i = start;
  while (i < text.length) {
    const c = text[i];
    if (c === '"') {
      i++;
      while (i < text.length && text[i] !== '"') i += text[i] === "\\" ? 2 : 1;
    } else if (c === "(" || c === "[" || c === "{") depth++;
    else if (c === ")" || c === "]" || c === "}") {
      if (depth === 0) break;
      depth--;
    } else if (c === "," && depth === 0) break;
    i++;
  }
  return text.slice(start, i);
}

function markupClasses() {
  const used = new Map();
  const note = (name, file) => {
    if (!/^-?[A-Za-z_][\w-]*$/.test(name)) return;
    if (!used.has(name)) used.set(name, new Set());
    used.get(name).add(file);
  };
  const dir = join(root, "src");
  for (const file of readdirSync(dir)) {
    if (!file.endsWith(".rs") || file.endsWith("_tests.rs")) continue;
    const text = readFileSync(join(dir, file), "utf8");
    // `class: ...` and `"class": ...` attributes. An interpolated token (`{x}`) names nothing fixed.
    const attribute = /(?<![\w-])"?class"?\s*:\s*/g;
    let match;
    while ((match = attribute.exec(text))) {
      for (const literal of literals(expressionAt(text, match.index + match[0].length))) {
        for (const token of literal.split(/\s+/)) if (!/[{}]/.test(token)) note(token, file);
      }
    }
  }
  const assets = join(root, "assets");
  for (const file of readdirSync(assets)) {
    if (!file.endsWith(".js") || file.includes("terminal-surface")) continue;
    const text = readFileSync(join(assets, file), "utf8");
    for (const match of text.matchAll(
      /(?:className\s*=|classList\.(?:add|toggle|remove)\()\s*["']([^"']*)["']/g,
    )) {
      for (const token of match[1].split(/\s+/)) note(token, file);
    }
  }
  return used;
}

function styledClasses() {
  const styled = new Set();
  for (const file of ["app.css", "theme-tokens.css"]) {
    let text = readFileSync(join(root, "assets", file), "utf8");
    text = text
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/g, '""');
    // Only selectors matter: the text before each `{`, so declarations are not read as classes.
    for (const block of text.split("{")) {
      const selector = block.slice(block.lastIndexOf("}") + 1).replace(/\[[^\]]*\]/g, "");
      for (const match of selector.matchAll(/\.(-?[A-Za-z_][\w-]*)/g)) styled.add(match[1]);
    }
  }
  return styled;
}

const used = markupClasses();
const styled = styledClasses();
const allow = JSON.parse(readFileSync(allowlistPath, "utf8"));
const unstyled = [...used.keys()].filter((name) => !styled.has(name)).sort();

if (process.argv.includes("--list")) {
  for (const name of unstyled) console.log(`${name}\t${[...used.get(name)].join(",")}`);
  console.log(`${unstyled.length} classes of ${used.size} have no rule`);
  process.exit(0);
}

const missing = unstyled.filter((name) => !(name in allow));
const stale = Object.keys(allow).filter((name) => !unstyled.includes(name));
for (const name of missing)
  console.error(`no rule and not allowlisted: .${name} (${[...used.get(name)].join(", ")})`);
for (const name of stale) console.error(`stale allowlist entry (styled or unused): .${name}`);
for (const [name, reason] of Object.entries(allow))
  if (!String(reason).trim()) console.error(`allowlist entry without a reason: .${name}`);
if (missing.length || stale.length) process.exit(1);
console.log(
  `ok: ${used.size} classes in markup, ${used.size - unstyled.length} styled, ${unstyled.length} marker-only`,
);
