// Runs the committed sidebar drag script against a minimal fake DOM, without a browser: the message it
// reports for a drop, and that the lifted row stays under the pointer when showing the drop areas moves
// the layout. Laid out like one project: Pinned (empty, shown only while dragging), Active with three
// rows of 32px, and an empty Settled shelf (also shown only while dragging).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const code = readFileSync(
  new URL("../crates/ui/assets/sidebar_drag_abi.js", import.meta.url),
  "utf8",
);
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const run = new AsyncFunction(
  "dioxus",
  "window",
  "document",
  "Element",
  "setTimeout",
  "clearTimeout",
  code,
);

const camel = (name) => name.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
class El {
  constructor(classes = [], data = {}, rect = null) {
    this.classes = new Set(classes);
    this.dataset = data;
    this.children = [];
    this.parent = null;
    this.rect = rect;
    this.style = {};
    this.scrollTop = 0;
    this.captured = null;
    this.classList = {
      add: (name) => this.classes.add(name),
      remove: (name) => this.classes.delete(name),
      contains: (name) => this.classes.has(name),
    };
  }
  add(child) {
    child.parent = this;
    this.children.push(child);
    return child;
  }
  matches(selector) {
    return selector.split(",").some((part) => {
      const m = part.trim().match(/^([a-z]*)((?:\.[\w-]+)*)(?:\[([\w-]+)\])?$/);
      if (!m) throw new Error(`fake DOM cannot match ${part}`);
      const [, tag, classes, attribute] = m;
      if (tag && tag !== (this.tag ?? "div")) return false;
      for (const name of classes.split(".").filter(Boolean))
        if (!this.classes.has(name)) return false;
      return !attribute || camel(attribute.replace(/^data-/, "")) in this.dataset;
    });
  }
  closest(selector) {
    for (let el = this; el; el = el.parent) if (el.matches(selector)) return el;
    return null;
  }
  contains(other) {
    for (let el = other; el; el = el.parent) if (el === this) return true;
    return false;
  }
  all() {
    return this.children.flatMap((child) => [child, ...child.all()]);
  }
  querySelectorAll(selector) {
    return this.all().filter((el) => el.matches(selector));
  }
  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }
  setPointerCapture(id) {
    this.captured = id;
  }
  getBoundingClientRect() {
    const { top, height, hidden } = this.rect();
    const bottom = top + (hidden ? 0 : height);
    return {
      left: 0,
      right: 260,
      top: hidden ? 0 : top,
      bottom: hidden ? 0 : bottom,
      height: hidden ? 0 : height,
    };
  }
}

function page({ shift = 0 } = {}) {
  const body = new El();
  const dragging = () => body.classes.has("sidebar-dragging");
  // Showing the areas can move the layout by `shift`: the drop areas themselves, and what follows.
  const moved = () => (dragging() ? shift : 0);
  const root = body.add(new El(["app"]));
  const pane = root.add(new El(["project-list"], {}, () => ({ top: 100, height: 700 })));
  const group = pane.add(
    new El(["project-group"], {}, () => ({ top: 200, height: 160 + moved() })),
  );
  const section = (name, rect) => group.add(new El(["thread-section"], { section: name }, rect));
  const pinned = section("pinned", () => ({ top: 202, height: 30, hidden: !dragging() }));
  const active = section("active", () => ({ top: 232 + moved(), height: 96 }));
  const row = (id, index) =>
    active.add(
      new El(["thread-row-wrap"], { threadId: id, drops: "pinned active settled" }, () => ({
        top: 232 + moved() + index * 32,
        height: 32,
      })),
    );
  const rows = { a: row("a", 0), b: row("b", 1), c: row("c", 2) };
  const settled = group.add(
    new El(["thread-shelf"], { section: "settled" }, () => ({
      top: 340 + moved(),
      height: 30,
      hidden: !dragging(),
    })),
  );
  const listeners = new Map();
  const document = {
    body,
    hidden: false,
    addEventListener: (type, handler) =>
      listeners.set(type, [...(listeners.get(type) ?? []), handler]),
    removeEventListener: (type, handler) =>
      listeners.set(
        type,
        (listeners.get(type) ?? []).filter((h) => h !== handler),
      ),
    querySelector: (selector) => body.querySelector(selector),
    querySelectorAll: (selector) => body.querySelectorAll(selector),
    getSelection: () => ({ removeAllRanges() {} }),
  };
  const windowListeners = new Map();
  const window = {
    addEventListener: (type, handler) => windowListeners.set(type, handler),
    removeEventListener: (type) => windowListeners.delete(type),
  };
  const sent = [];
  const waiters = [];
  const dioxus = {
    send: (message) => sent.push(message),
    recv: () => new Promise((resolve) => waiters.push(resolve)),
  };
  const timers = [];
  const setTimeoutFake = (fn, ms) => {
    timers.push({ fn, ms, live: true });
    return timers.length;
  };
  const clearTimeoutFake = (id) => {
    if (timers[id - 1]) timers[id - 1].live = false;
  };
  const task = run(dioxus, window, document, El, setTimeoutFake, clearTimeoutFake);
  const fire = (type, init = {}) => {
    const event = {
      type,
      pointerId: 1,
      isPrimary: true,
      pointerType: "mouse",
      button: 0,
      cancelable: true,
      target: rows.a,
      preventDefault() {},
      stopPropagation() {},
      ...init,
    };
    for (const handler of [...(listeners.get(type) ?? [])]) handler(event);
  };
  const drag = {
    // Press `target` and move in 6px steps (the browser pass: twelve moves, 40ms apart) down or up to `toY`.
    mouse(target, fromY, toY, { release = true } = {}) {
      fire("pointerdown", { target, clientX: 20, clientY: fromY });
      const step = toY >= fromY ? 6 : -6;
      let y = fromY;
      while ((step > 0 && y + step <= toY) || (step < 0 && y + step >= toY)) {
        y += step;
        fire("pointermove", { clientX: 20, clientY: y });
      }
      if (y !== toY) fire("pointermove", { clientX: 20, clientY: toY });
      if (release) fire("pointerup", { clientX: 20, clientY: toY });
    },
  };
  const message = async (value) => {
    waiters.shift()?.(value);
    await Promise.resolve();
    await Promise.resolve();
  };
  const stop = async () => {
    waiters.shift()?.({ type: "stop" });
    await task;
  };
  const marked = (name) =>
    body.querySelectorAll(`.${name}`).map((el) => el.dataset.threadId ?? el.dataset.section);
  return {
    body,
    rows,
    pinned,
    settled,
    fire,
    drag,
    sent,
    timers,
    stop,
    message,
    marked,
    document,
    window,
  };
}

// Pointer y (row centres are 248, 280, 312 with the areas hidden and the layout unmoved).
// A move down past every other row lands last in Active.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 326);
  assert.deepEqual(p.sent, [{ type: "drop", id: "a", section: "active", before: null }]);
  await p.stop();
}
// A move down that stops between the second and third rows lands before the third.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 296);
  assert.deepEqual(p.sent, [{ type: "drop", id: "a", section: "active", before: "c" }]);
  await p.stop();
}
// The browser pass: the first row dragged twelve 6px steps down, then released at the bottom edge of
// the third row without another move: the row lands where the pointer is released.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 248 + 72, { release: false });
  assert.deepEqual(p.marked("lifted"), ["a"]);
  assert.deepEqual(p.marked("drop-target"), ["active"]);
  assert.deepEqual(p.marked("drop-end"), ["active"]);
  p.fire("pointerup", { clientX: 20, clientY: 327 });
  assert.deepEqual(p.sent, [{ type: "drop", id: "a", section: "active", before: null }]);
  assert.deepEqual(p.marked("lifted"), []);
  assert.deepEqual(p.marked("drop-target"), []);
  assert.equal(p.body.classes.has("sidebar-dragging"), false);
  assert.equal(p.rows.a.style.transform, "");
  await p.stop();
}
// A move up lands before the first row; the line is drawn on that row.
{
  const p = page();
  p.drag.mouse(p.rows.c, 312, 240, { release: false });
  assert.deepEqual(p.marked("drop-before"), ["a"]);
  p.fire("pointerup", { clientX: 20, clientY: 240 });
  assert.deepEqual(p.sent, [{ type: "drop", id: "c", section: "active", before: "a" }]);
  await p.stop();
}
// A drop within the row's own slot reports where it already is: the page leaves the decision to Rust.
{
  const p = page();
  p.drag.mouse(p.rows.b, 280, 300);
  assert.deepEqual(p.sent, [{ type: "drop", id: "b", section: "active", before: "c" }]);
  await p.stop();
}
// The empty Pinned area is shown by the lift, and dropping on it pins.
{
  const p = page();
  p.drag.mouse(p.rows.b, 280, 215, { release: false });
  assert.deepEqual(p.marked("drop-target"), ["pinned"]);
  p.fire("pointerup", { clientX: 20, clientY: 215 });
  assert.deepEqual(p.sent, [{ type: "drop", id: "b", section: "pinned", before: null }]);
  await p.stop();
}
// The Settled shelf takes a row dropped on it, with no row to land before.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 355);
  assert.deepEqual(p.sent, [{ type: "drop", id: "a", section: "settled", before: null }]);
  await p.stop();
}
// A press that never moves, or moves less than a few pixels, is a click and reports nothing.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 252);
  assert.deepEqual(p.sent, []);
  assert.equal(p.rows.a.classes.has("lifted"), false);
  await p.stop();
}
// Escape cancels: nothing is reported, and everything the lift changed is put back.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 300, { release: false });
  assert.deepEqual(p.marked("lifted"), ["a"]);
  p.fire("keydown", { key: "Escape" });
  assert.deepEqual(p.marked("lifted"), []);
  assert.deepEqual(p.marked("drop-before").concat(p.marked("drop-target")), []);
  assert.equal(p.body.classes.has("sidebar-dragging"), false);
  assert.equal(p.rows.a.style.transform, "");
  p.fire("pointerup", { clientX: 20, clientY: 300 });
  assert.deepEqual(p.sent, []);
  await p.stop();
}
// A pointer cancel (a lost pointer) cancels the same way.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 300, { release: false });
  p.fire("pointercancel");
  assert.deepEqual(p.sent, []);
  assert.deepEqual(p.marked("lifted"), []);
  await p.stop();
}
// Outside the list there is no target: releasing there reports nothing.
{
  const p = page();
  p.drag.mouse(p.rows.a, 248, 300, { release: false });
  p.fire("pointermove", { clientX: 900, clientY: 300 });
  p.fire("pointerup", { clientX: 900, clientY: 300 });
  assert.deepEqual(p.sent, []);
  await p.stop();
}
// A write in flight holds the next lift back; the bridge says so through the channel `recv` reads.
{
  const p = page();
  await p.message({ type: "busy", busy: true });
  p.drag.mouse(p.rows.a, 248, 300);
  assert.deepEqual(p.sent, []);
  assert.deepEqual(p.marked("lifted"), []);
  await p.message({ type: "busy", busy: false });
  p.drag.mouse(p.rows.a, 248, 300);
  assert.equal(p.sent.length, 1);
  await p.stop();
}
// A finger: a press-and-hold of 350ms lifts the row, then it moves with the finger.
{
  const p = page();
  p.fire("pointerdown", { target: p.rows.a, clientX: 20, clientY: 248, pointerType: "touch" });
  assert.equal(p.timers.length, 1);
  assert.equal(p.timers[0].ms, 350);
  // Moving only a little before the hold does not lift it.
  p.fire("pointermove", { clientX: 20, clientY: 251, pointerType: "touch" });
  assert.deepEqual(p.marked("lifted"), []);
  p.timers[0].fn();
  assert.deepEqual(p.marked("lifted"), ["a"]);
  for (let y = 254; y <= 326; y += 6)
    p.fire("pointermove", { clientX: 20, clientY: y, pointerType: "touch" });
  p.fire("pointerup", { clientX: 20, clientY: 326, pointerType: "touch" });
  assert.deepEqual(p.sent, [{ type: "drop", id: "a", section: "active", before: null }]);
  await p.stop();
}
// A swipe (a finger that moves before the hold) scrolls the list instead and never lifts.
{
  const p = page();
  p.fire("pointerdown", { target: p.rows.a, clientX: 20, clientY: 248, pointerType: "touch" });
  p.fire("pointermove", { clientX: 20, clientY: 270, pointerType: "touch" });
  assert.equal(p.timers[0].live, false);
  p.fire("pointerup", { clientX: 20, clientY: 270, pointerType: "touch" });
  assert.deepEqual(p.sent, []);
  assert.deepEqual(p.marked("lifted"), []);
  await p.stop();
}
// The lifted row stays under the pointer when showing the drop areas moves the layout by 55px: its
// transform is the pointer's distance from where the row now sits, not from where the press began.
{
  const p = page({ shift: 55 });
  p.fire("pointerdown", { target: p.rows.a, clientX: 20, clientY: 248 });
  p.fire("pointermove", { clientX: 20, clientY: 260 });
  // The row's top is now 232 + 55; the pointer grabbed it 16px below its original top of 232.
  assert.equal(p.rows.a.style.transform, `translateY(${260 - 16 - (232 + 55)}px)`);
  p.fire("pointermove", { clientX: 20, clientY: 320 });
  assert.equal(p.rows.a.style.transform, `translateY(${320 - 16 - (232 + 55)}px)`);
  p.fire("keydown", { key: "Escape" });
  await p.stop();
}

console.log(JSON.stringify({ sidebarDragScenarios: 15 }));
