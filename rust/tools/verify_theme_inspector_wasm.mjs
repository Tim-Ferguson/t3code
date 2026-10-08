// Actual Rust WASM over a deterministic DOM boundary. Live browser proof is separate.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire, stripTypeScriptTypes } from "node:module";
const rust = createRequire(import.meta.url)(
  new URL("../target/terminal-node/t3_terminal.js", import.meta.url).pathname,
);
class EventTarget {
  listeners = new Map();
  addEventListener(name, listener) {
    if (this.failListenerName === name) throw Error("injected listener registration failure");
    const list = this.listeners.get(name) ?? [];
    list.push(listener);
    this.listeners.set(name, list);
  }
  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter((value) => value !== listener),
    );
  }
  dispatch(name, target, key) {
    const event = {
      target,
      key,
      relatedTarget: null,
      preventDefault() {
        this.prevented = true;
      },
      stopPropagation() {
        this.stopped = true;
      },
    };
    for (const listener of this.listeners.get(name) ?? []) listener(event);
    return event;
  }
  get count() {
    return [...this.listeners.values()].reduce((sum, list) => sum + list.length, 0);
  }
}
class NodeList extends Array {
  item(index) {
    return this[index] ?? null;
  }
}
class Node extends EventTarget {
  static TEXT_NODE = 3;
  constructor() {
    super();
    this.nodeType = 1;
    this.childNodes = new NodeList();
    this.parentElement = null;
    this.connected = true;
    this.textContent = "";
  }
  get isConnected() {
    return this.connected;
  }
  appendChild(child) {
    child.parentElement = this;
    child.connected = true;
    this.childNodes.push(child);
    return child;
  }
  append(...children) {
    children.forEach((child) => this.appendChild(child));
  }
  remove() {
    if (this.parentElement)
      this.parentElement.childNodes = this.parentElement.childNodes.filter((node) => node !== this);
    this.parentElement = null;
    this.connected = false;
  }
  set textContent(text) {
    this._text = text;
    this.childNodes = new NodeList();
  }
  get textContent() {
    return this._text;
  }
  cloneNode() {
    const copy = new this.constructor(this.tagName);
    copy.attributes = new Map(this.attributes);
    copy.bounds = { ...this.bounds };
    return copy;
  }
  replaceChildren(...children) {
    this.childNodes = new NodeList();
    this.append(...children);
  }
}
class Style {
  values = new Map();
  priorities = new Map();
  getPropertyValue(name) {
    return this.values.get(name) ?? "";
  }
  getPropertyPriority(name) {
    return this.priorities.get(name) ?? "";
  }
  setProperty(name, value, priority = "") {
    this.values.set(name, value);
    this.priorities.set(name, priority);
  }
  removeProperty(name) {
    const previous = this.getPropertyValue(name);
    this.values.delete(name);
    this.priorities.delete(name);
    return previous;
  }
}
class DomTokenList extends Array {
  item(index) {
    return this[index] ?? null;
  }
}
class Element extends Node {
  constructor(tag = "div") {
    super();
    this.tagName = tag;
    this.attributes = new Map();
    this.classList = new DomTokenList();
    this.bounds = { left: 10, top: 20, width: 100, height: 50, right: 110, bottom: 70 };
    this.style = new Style();
    this.paint = {};
    this.dataset = {};
  }
  setAttribute(name, value) {
    this.attributes.set(name, value);
    if (name === "id") this.id = value;
  }
  getAttribute(name) {
    return this.attributes.get(name) ?? null;
  }
  hasAttribute(name) {
    return this.attributes.has(name);
  }
  removeAttribute(name) {
    this.attributes.delete(name);
  }
  matches(selector) {
    return selector.split(",").some((value) => {
      value = value.trim();
      if (value.startsWith("[")) return this.hasAttribute(value.slice(1, -1));
      if (value.startsWith("#")) return this.id === value.slice(1);
      return value === this.tagName;
    });
  }
  closest(selector) {
    for (let node = this; node; node = node.parentElement) {
      if (node.matches(selector)) return node;
    }
    return null;
  }
  all() {
    return this.childNodes.flatMap((node) =>
      node instanceof Element ? [node, ...node.all()] : [],
    );
  }
  querySelectorAll(selector) {
    return new NodeList(...this.all().filter((node) => selector === "*" || node.matches(selector)));
  }
  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }
  getBoundingClientRect() {
    return this.bounds;
  }
  scrollIntoView() {
    this.scrolled = true;
  }
}
class HtmlElement extends Element {}
class SVGElement extends Element {}
class Document extends EventTarget {
  constructor() {
    super();
    this.documentElement = new HtmlElement("html");
    this.body = new HtmlElement("body");
    this.documentElement.append(this.body);
  }
  createElement(tag) {
    return new HtmlElement(tag);
  }
  createElementNS(_, tag) {
    return new SVGElement(tag);
  }
  getElementById(id) {
    return (
      [this.documentElement, ...this.documentElement.all()].find((node) => node.id === id) ?? null
    );
  }
  querySelectorAll(selector) {
    if (selector === "body *") return this.body.querySelectorAll("*");
    return this.documentElement.querySelectorAll(selector);
  }
  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }
}
class MutationObserver {
  static instances = [];
  constructor(callback) {
    this.callback = callback;
    this.disconnected = false;
    MutationObserver.instances.push(this);
  }
  observe() {}
  disconnect() {
    this.disconnected = true;
  }
}
class Window extends EventTarget {
  innerWidth = 800;
  innerHeight = 600;
  performance = { now: () => now };
  getComputedStyle(element) {
    if (
      this.failPaint &&
      ["#01fea7", "#fe01a7"].includes(
        this.document.documentElement.style.getPropertyValue("--canvas"),
      )
    )
      throw Error("injected computed style failure");
    const fallback = {
      display: "block",
      visibility: "visible",
      opacity: "1",
      "background-color": "hardcoded",
      "background-image": "none",
      "border-image-source": "none",
      "box-shadow": "none",
      color: "hardcoded",
      "text-decoration-line": "none",
      "text-shadow": "none",
      "outline-style": "none",
      "outline-width": "0px",
      "border-top-left-radius": "2px",
    };
    for (const side of ["top", "right", "bottom", "left"]) {
      fallback[`border-${side}-style`] = "none";
      fallback[`border-${side}-width`] = "0px";
    }
    const style = new Style();
    for (const [key, value] of Object.entries({ ...fallback, ...element.paint })) {
      style.values.set(
        key,
        value.startsWith("var(")
          ? this.document.documentElement.style.getPropertyValue(value.slice(4, -1))
          : value,
      );
    }
    return new Proxy(style, {
      get(target, key) {
        if (key in target)
          return typeof target[key] === "function" ? target[key].bind(target) : target[key];
        return target.getPropertyValue(
          String(key).replace(/[A-Z]/g, (letter) => "-" + letter.toLowerCase()),
        );
      },
    });
  }
  setTimeout(callback, delay) {
    const id = ++next;
    timers.set(id, { callback, due: now + delay });
    return id;
  }
  clearTimeout(id) {
    timers.delete(id);
  }
  requestAnimationFrame(callback) {
    const id = ++next;
    frames.set(id, callback);
    return id;
  }
  cancelAnimationFrame(id) {
    frames.delete(id);
  }
}
let now = 1000,
  next = 0,
  timers = new Map(),
  frames = new Map();
Object.assign(globalThis, {
  Window,
  Document,
  Element,
  HTMLElement: HtmlElement,
  SVGElement,
  Node,
  NodeList,
  CSSStyleDeclaration: Style,
  EventTarget,
  MutationObserver,
  PointerEvent: Object,
  KeyboardEvent: Object,
});
const roles = ["canvas", "text", "accent"],
  variables = Object.fromEntries(roles.map((role) => [role, `--${role}`]));
// js_sys::global caches the browsing global. A real WebView keeps its Window
// identity, so every witness refreshes the document instead of replacing that realm.
const sharedWindow = new Window();
function setup() {
  now = 1000;
  timers.clear();
  frames.clear();
  MutationObserver.instances = [];
  const document = new Document(),
    window = sharedWindow;
  window.document = document;
  window.failPaint = false;
  window.failListenerName = null;
  window.listeners = new Map();
  Object.assign(globalThis, { window, document });
  for (const [name, value] of [
    ["--canvas", "#ffffff"],
    ["--text", "#ffffff"],
    ["--accent", "#01fea7"],
  ])
    document.documentElement.style.setProperty(name, value, "important");
  const hard = document.createElement("div");
  hard.id = "hardcoded";
  hard.paint = { "background-color": "#ffffff" };
  const canvas = document.createElement("div");
  canvas.id = "canvas";
  canvas.paint = { "background-color": "var(--canvas)" };
  const child = document.createElement("span");
  child.id = "text";
  child.paint = { color: "var(--text)" };
  const text = new Node();
  text.nodeType = 3;
  text.textContent = "visible";
  child.appendChild(text);
  canvas.append(child);
  const accent = document.createElement("div");
  accent.id = "accent";
  accent.paint = { "background-color": "var(--accent)" };
  const editor = document.createElement("aside");
  editor.id = "editor";
  editor.setAttribute("data-theme-editor-panel", "");
  editor.paint = { "background-color": "var(--canvas)" };
  document.body.append(hard, canvas, accent, editor);
  return { document, window, hard, canvas, child, accent, editor };
}
function original() {
  const source = readFileSync(
    new URL("../../apps/web/src/components/settings/themeInspector.ts", import.meta.url),
    "utf8",
  );
  return new Function(
    "THEME_COLOR_ROLES",
    "getThemeColorVariable",
    "document",
    "window",
    stripTypeScriptTypes(
      source
        .replace(/import\s+[\s\S]*?from\s+"[^"\n]+";/g, "")
        .replace(/^export /gm, "")
        .replaceAll("import.meta.env.DEV", "false"),
    ) +
      "\nreturn {highlightThemeRoleUsage,inspectThemeRoleAtElement,clearThemeInspectorHighlights};",
  )(roles, (role) => variables[role], document, window);
}
const config = JSON.stringify({ roles, variables });
let comparisons = 0;
for (const requested of [
  ["canvas"],
  ["text"],
  ["accent"],
  ["canvas", "text"],
  ["canvas", "canvas"],
  [],
]) {
  setup();
  const expected = original().highlightThemeRoleUsage(requested);
  const matches = [...document.querySelectorAll("[data-theme-inspector-match]")].map(
    (element) => element.id,
  );
  setup();
  const events = [],
    surface = new rust.ThemeInspector(config, (event) => events.push(JSON.parse(event)));
  const before = [...document.documentElement.style.values];
  const priorities = [...document.documentElement.style.priorities];
  surface.selection(JSON.stringify(requested), false);
  assert.equal(events.at(-1)?.count ?? 0, expected);
  assert.deepEqual(
    [...document.querySelectorAll("[data-theme-inspector-match]")].map((element) => element.id),
    matches,
  );
  assert.deepEqual([...document.documentElement.style.values], before);
  assert.deepEqual([...document.documentElement.style.priorities], priorities);
  assert.equal(document.documentElement.hasAttribute("data-theme-token-probe"), false);
  surface.selection(JSON.stringify(requested), false);
  assert.equal(document.querySelectorAll("#theme-inspector-spotlight").length, expected ? 1 : 0);
  surface.dispose();
  surface.free();
  assert.equal(document.count, 0);
  assert.equal(window.count, 0);
  assert.equal(document.getElementById("theme-inspector-spotlight"), null);
  assert.equal(timers.size, 0);
  assert.equal(frames.size, 0);
  assert.ok(MutationObserver.instances.every((observer) => observer.disconnected));
  comparisons++;
}
for (const target of ["hard", "canvas", "child", "accent", "editor"]) {
  let dom = setup();
  const expected = original().inspectThemeRoleAtElement(dom[target])?.role ?? null;
  dom = setup();
  const events = [],
    surface = new rust.ThemeInspector(config, (event) => events.push(JSON.parse(event)));
  surface.selection("[]", true);
  const before = [...document.documentElement.style.values];
  const event = document.dispatch("pointerdown", dom[target]);
  assert.equal(events.find((event) => event.type === "role")?.role ?? null, expected);
  assert.equal(!!event.prevented, target !== "editor");
  assert.deepEqual([...document.documentElement.style.values], before);
  surface.dispose();
  surface.free();
  comparisons++;
}
{
  const dom = setup(),
    events = [],
    surface = new rust.ThemeInspector(config, (event) => events.push(JSON.parse(event)));
  surface.selection("[]", true);
  document.dispatch("pointerover", dom.child);
  assert.equal(timers.size, 1);
  now += 140;
  for (const [id, timer] of [...timers]) {
    timers.delete(id);
    timer.callback();
  }
  assert.equal(
    document
      .getElementById("theme-inspector-hover")
      ?.querySelector("[data-theme-inspector-hover-label]")?.textContent,
    "Text",
  );
  document.dispatch("scroll", dom.child); // actual listener is on window
  window.dispatch("scroll", dom.child);
  assert.equal(document.getElementById("theme-inspector-hover"), null);
  document.dispatch("pointerover", dom.child);
  surface.dispose();
  surface.free();
  assert.equal(timers.size, 0);
  assert.equal(document.count, 0);
  assert.equal(window.count, 0);
}
{
  setup();
  const events = [],
    surface = new rust.ThemeInspector(config, (event) => events.push(JSON.parse(event)));
  const before = [...document.documentElement.style.values],
    priority = [...document.documentElement.style.priorities];
  window.failPaint = true;
  surface.selection('["canvas","canvas"]', false);
  assert.equal(events.at(-1)?.type, "error");
  assert.deepEqual([...document.documentElement.style.values], before);
  assert.deepEqual([...document.documentElement.style.priorities], priority);
  assert.equal(document.documentElement.hasAttribute("data-theme-token-probe"), false);
  surface.dispose();
  surface.free();
}
{
  setup();
  window.failListenerName = "resize";
  assert.throws(
    () => new rust.ThemeInspector(config, () => {}),
    /injected listener registration failure/,
  );
  assert.equal(document.count, 0);
  assert.equal(window.count, 0);
  assert.ok(MutationObserver.instances.every((observer) => observer.disconnected));
}
{
  setup();
  const events = [],
    surface = new rust.ThemeInspector(config, (event) => events.push(JSON.parse(event)));
  surface.selection('["canvas"]', false);
  const scans = events.length;
  const observer = MutationObserver.instances.at(-1);
  observer.callback([{ target: document.getElementById("editor") }]);
  assert.equal(frames.size, 0);
  assert.equal(timers.size, 0);
  now += 100;
  observer.callback([{ target: document.body }]);
  observer.callback([{ target: document.body }]);
  assert.equal(timers.size, 1);
  now += 400;
  for (const [id, timer] of [...timers]) {
    timers.delete(id);
    timer.callback();
  }
  assert.equal(events.length, scans + 1);
  now += 600;
  observer.callback([{ target: document.body }]);
  assert.equal(frames.size, 1);
  surface.dispose();
  surface.free();
  assert.equal(frames.size, 0);
  assert.equal(timers.size, 0);
}
console.log(
  JSON.stringify({
    domBoundaryComparisons: comparisons,
    hoverAndDispose: true,
    failedProbeRestoration: true,
    failedMountCleanup: true,
    mutationThrottle: true,
  }),
);
