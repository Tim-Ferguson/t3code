// Exercise the shipped browser-tab bridge (`crates/ui/assets/preview_surface_abi.js`) without a browser or a server: a fake page, socket and
// canvas stand in, and what the bridge sends to the socket and to Rust is checked. The protocol itself (what a message means) is Rust's and is
// tested against the real server in `preview_tests.rs`; this covers what only the page's script does: where a pointer lands on the letterboxed
// frame, what input is held back while the viewer has no control, how moves and wheel deltas are merged, and how keys and text are sent.
import { readFileSync } from "node:fs";
import { strict as assert } from "node:assert";

const source = readFileSync(
  new URL("../crates/ui/assets/preview_surface_abi.js", import.meta.url),
  "utf8",
);
const AsyncFunction = Object.getPrototypeOf(async () => {}).constructor;

function emitter() {
  const listeners = new Map();
  return {
    addEventListener: (type, fn) => listeners.set(type, [...(listeners.get(type) ?? []), fn]),
    removeEventListener: (type, fn) =>
      listeners.set(
        type,
        (listeners.get(type) ?? []).filter((f) => f !== fn),
      ),
    fire: (type, event = {}) => {
      for (const fn of listeners.get(type) ?? [])
        fn({ type, preventDefault() {}, stopPropagation() {}, ...event });
    },
  };
}

async function bridge() {
  const sent = [];
  const queue = [];
  let wake;
  const frames = [];
  let socket;
  class FakeSocket {
    constructor(url) {
      Object.assign(this, emitter());
      this.url = url;
      this.readyState = 0;
      this.out = [];
      socket = this;
    }
    send(data) {
      this.out.push(data);
    }
    close() {
      this.readyState = 3;
    }
  }
  FakeSocket.OPEN = 1;
  const canvas = Object.assign(emitter(), {
    width: 1280,
    height: 800,
    clientWidth: 640,
    clientHeight: 500,
    parentElement: null,
    getContext: () => ({ drawImage: () => frames.push("drawn") }),
    getBoundingClientRect: () => ({ left: 100, top: 50, width: 640, height: 500 }),
    setPointerCapture() {},
  });
  const input = Object.assign(emitter(), {
    value: "",
    focus() {},
    blur() {},
    setSelectionRange() {},
  });
  const box = Object.assign(emitter(), {
    append() {},
    getBoundingClientRect: () => ({ width: 640, height: 500 }),
  });
  canvas.parentElement = box;
  const root = { querySelector: (selector) => (selector === "canvas" ? canvas : input) };
  const rafs = [];
  const environment = {
    document: {
      getElementById: () => root,
      createElement: () => ({ style: {}, dataset: {}, remove() {} }),
    },
    WebSocket: FakeSocket,
    WheelEvent: { DOM_DELTA_LINE: 1, DOM_DELTA_PAGE: 2 },
    createImageBitmap: async () => ({ width: 1280, height: 800, close() {} }),
    Blob: class {},
    ResizeObserver: class {
      constructor(callback) {
        this.callback = callback;
      }
      observe() {
        this.callback();
      }
      disconnect() {}
    },
    requestAnimationFrame: (fn) => rafs.push(fn),
    cancelAnimationFrame: () => rafs.splice(0),
    setTimeout: (fn) => {
      fn();
      return 0;
    },
    clearTimeout() {},
    window: { devicePixelRatio: 2 },
  };
  const dioxus = {
    send: (message) => sent.push(message),
    recv: () =>
      queue.length
        ? Promise.resolve(queue.shift())
        : new Promise((resolve) => {
            wake = resolve;
          }),
  };
  const push = (message) => {
    if (wake) {
      const resolve = wake;
      wake = undefined;
      resolve(message);
    } else queue.push(message);
  };
  const names = Object.keys(environment);
  const done = new AsyncFunction("dioxus", ...names, source)(
    dioxus,
    ...names.map((name) => environment[name]),
  );
  push({ type: "init", surface: "surface", followSize: true });
  await new Promise((resolve) => setImmediate(resolve));
  return { sent, push, canvas, input, rafs, frames, done, socket: () => socket };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));
const parse = (socket) => socket.out.map((data) => JSON.parse(data));

const page = await bridge();
// The first size is reported in css pixels with the pixel ratio; Rust turns it into the frame cap.
assert.deepEqual(
  page.sent.find((m) => m.t === "size"),
  { t: "size", width: 640, height: 500, ratio: 2 },
);

page.push({ type: "connect", url: "ws://server/api/preview-stream/ws?threadId=t&tabId=a" });
await settle();
const socket = page.socket();
assert.equal(socket.url, "ws://server/api/preview-stream/ws?threadId=t&tabId=a");
socket.readyState = 1;
socket.fire("open");
assert.deepEqual(page.sent.at(-1), { t: "open" });

// Nothing is sent before the server says this viewer may operate and holds control.
page.push({ type: "send", message: { type: "navigate", url: "http://x/" } });
await settle();
assert.deepEqual(socket.out, []);
socket.fire("message", {
  data: JSON.stringify({
    type: "control",
    canOperate: true,
    controller: "unclaimed",
    generation: 1,
    dialog: null,
  }),
});
socket.fire("message", { data: JSON.stringify({ type: "viewport", width: 1280, height: 800 }) });
page.push({ type: "send", message: { type: "navigate", url: "http://x/" } });
await settle();
assert.deepEqual(socket.out, [], "the page is not driven by a viewer that does not have control");
page.push({ type: "send", message: { type: "takeControl" } });
await settle();
assert.deepEqual(
  parse(socket),
  [{ type: "takeControl" }],
  "taking control is allowed to a viewer that may operate",
);
socket.fire("message", {
  data: JSON.stringify({
    type: "control",
    canOperate: true,
    controller: "you",
    generation: 2,
    dialog: null,
  }),
});
assert.ok(
  page.sent.some((m) => m.t === "text" && JSON.parse(m.data).controller === "you"),
  "Rust reads the same words",
);
assert.deepEqual(
  parse(socket).at(-1),
  { type: "resize", width: 640, height: 500 },
  "taking control sizes the page to the box",
);

// A binary frame is acked at once and painted; the first painted frame is reported once.
socket.fire("message", { data: new ArrayBuffer(4) });
assert.equal(socket.out.at(-1), '{"type":"ack"}');
await settle();
assert.equal(page.sent.filter((m) => m.t === "frame").length, 1);

// A pointer is mapped through the letterboxed frame: the frame (1280x800) fits a 640x500 box at 640x400, 50px below the top.
socket.out.length = 0;
const pointer = (type, extra) =>
  page.canvas.fire(type, {
    pointerId: 1,
    pointerType: "mouse",
    timeStamp: 1,
    altKey: false,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    buttons: 1,
    button: 0,
    ...extra,
  });
pointer("pointerdown", { clientX: 100 + 320, clientY: 50 + 50 + 200 });
assert.deepEqual(parse(socket).at(-1), {
  type: "mouse",
  action: "down",
  x: 640,
  y: 400,
  modifiers: 0,
  button: "left",
  buttons: 1,
  clickCount: 1,
});
// Above the frame (in the letterbox) a press lands nowhere.
socket.out.length = 0;
pointer("pointerup", { clientX: 100 + 320, clientY: 50 + 10, buttons: 0 });
assert.deepEqual(
  parse(socket).at(-1),
  {
    type: "mouse",
    action: "up",
    x: 640,
    y: 0,
    modifiers: 0,
    button: "left",
    buttons: 0,
    clickCount: 1,
  },
  "a release is clamped to the page so a drag ends",
);
// Moves are merged to the last one per frame.
socket.out.length = 0;
pointer("pointermove", { clientX: 200, clientY: 200, buttons: 0 });
pointer("pointermove", { clientX: 300, clientY: 300, buttons: 0 });
assert.deepEqual(socket.out, []);
page.rafs.splice(0).forEach((fn) => fn());
assert.equal(parse(socket).length, 1);
assert.equal(parse(socket)[0].action, "move");

// Wheel deltas add up, in page pixels.
socket.out.length = 0;
const wheel = (deltaY) =>
  page.canvas.fire("wheel", {
    clientX: 420,
    clientY: 300,
    deltaX: 0,
    deltaY,
    deltaMode: 0,
    altKey: false,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
  });
wheel(10);
wheel(15);
page.rafs.splice(0).forEach((fn) => fn());
assert.deepEqual(parse(socket), [
  { type: "wheel", x: 640, y: 400, deltaX: 0, deltaY: 25, modifiers: 0 },
]);

// Keys: a printable key carries its text, Enter is a carriage return, a shortcut carries none; paste sends the text; Shift+Escape leaves the page.
socket.out.length = 0;
const key = (action, event) =>
  page.input.fire(action === "down" ? "keydown" : "keyup", {
    isComposing: false,
    keyCode: 65,
    code: "KeyA",
    altKey: false,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    ...event,
  });
key("down", { key: "a" });
key("down", { key: "Enter", keyCode: 13, code: "Enter" });
key("down", { key: "c", ctrlKey: true });
page.input.fire("paste", { clipboardData: { getData: () => "pasted" } });
key("down", { key: "Escape", shiftKey: true, keyCode: 27, code: "Escape" });
const out = parse(socket);
assert.equal(out[0].text, "a");
assert.equal(out[1].text, "\r");
assert.equal(out[2].text, undefined);
assert.deepEqual(out[3], { type: "text", text: "pasted" });
assert.equal(out.length, 4, "Shift+Escape is the app's, not the page's");

// Losing control drops what was held and refuses more input.
socket.fire("message", {
  data: JSON.stringify({
    type: "control",
    canOperate: true,
    controller: "agent",
    generation: 3,
    dialog: null,
  }),
});
socket.out.length = 0;
pointer("pointerdown", { clientX: 420, clientY: 300 });
assert.deepEqual(socket.out, []);

// The close is reported with its code and reason, once, for Rust to decide on.
socket.readyState = 3;
socket.fire("close", { code: 4503, reason: '{"need":"sandbox","command":"x"}' });
assert.deepEqual(page.sent.at(-1), {
  t: "close",
  code: 4503,
  reason: '{"need":"sandbox","command":"x"}',
  opened: true,
});

page.push({ type: "stop" });
await page.done;
console.log("preview surface bridge: ok");
