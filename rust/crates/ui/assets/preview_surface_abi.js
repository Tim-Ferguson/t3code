// The live view of a server browser tab (`ServerBrowserSurface.tsx`, `serverBrowserStream.ts`): the socket, the frames painted on a
// canvas, and the person's input on the page. It decides nothing about the tab. Rust mints the address, says when to connect
// again and what each message means; this does what the browser alone can (hold the socket, decode a JPEG, read a pointer, a key or
// a paste) and reports it.
//
// Rust -> here: {type:"init", surface}, {type:"connect", url}, {type:"send", message}, {type:"stop"}.
// Here -> Rust: {t:"open"}, {t:"text", data}, {t:"frame"}, {t:"close", code, reason, opened}, {t:"size", width, height, ratio}.
//
// Input goes to the page only while this viewer has control (`control` is read from the stream's own messages), moves and
// wheel deltas are merged to one message each per animation frame, and Shift+Escape returns the keyboard to the app.
const ACK = JSON.stringify({ type: "ack" });
const SENTINEL = "​";
const RESIZE_DEBOUNCE_MS = 150;
const MULTI_CLICK_MS = 500;
const MULTI_CLICK_SLOP_PX = 4;
const TAP_SLOP_PX = 8;
const WHEEL_LINE_PX = 16;
const BUTTON = ["left", "middle", "right"];

const init = await dioxus.recv();
const root = document.getElementById(init.surface);
const canvas = root?.querySelector("canvas");
const input = root?.querySelector("textarea");
const frameBox = canvas?.parentElement;
if (!root || !canvas || !input) {
  dioxus.send({ t: "close", code: 1011, reason: "no surface", opened: false });
  throw new Error("preview surface is not in the page");
}
const context = canvas.getContext("2d");
let socket = null;
let generation = 0;
let control = null;
let viewport = null;
let hasFrame = false;
let stopped = false;
let size = null;

const modifiers = (event) =>
  (event.altKey ? 1 : 0) |
  (event.ctrlKey ? 2 : 0) |
  (event.metaKey ? 4 : 0) |
  (event.shiftKey ? 8 : 0);

// ---- the socket
const send = (message) => {
  if (!socket || socket.readyState !== WebSocket.OPEN || !control?.canOperate) return false;
  if (message.type !== "takeControl" && control.controller !== "you") return false;
  socket.send(JSON.stringify(message));
  return true;
};
const closeSocket = () => {
  const old = socket;
  socket = null;
  control = null;
  if (old) old.close();
  clearInput();
};
const connect = (url) => {
  closeSocket();
  if (stopped) return;
  const mine = ++generation;
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  socket = ws;
  let opened = false;
  ws.addEventListener("open", () => {
    if (socket !== ws) return;
    opened = true;
    dioxus.send({ t: "open" });
  });
  ws.addEventListener("message", (event) => {
    if (socket !== ws) return;
    if (event.data instanceof ArrayBuffer) {
      ws.send(ACK);
      paint(event.data);
      return;
    }
    if (typeof event.data !== "string") return;
    try {
      const message = JSON.parse(event.data);
      if (message?.type === "control") {
        const previous = control;
        control = message;
        if (message.controller !== "you") clearInput();
        else if (previous?.controller !== "you" && size)
          send({ type: "resize", width: size.width, height: size.height });
      } else if (message?.type === "viewport") {
        viewport = { width: message.width, height: message.height };
      } else if (message?.type === "pointer") {
        placeCursor(message);
      }
    } catch {
      // Rust reads the same text and ignores what is not a message.
    }
    dioxus.send({ t: "text", data: event.data });
  });
  ws.addEventListener("close", (event) => {
    if (socket !== ws || generation !== mine) return;
    socket = null;
    control = null;
    clearInput();
    dioxus.send({ t: "close", code: event.code, reason: event.reason, opened });
  });
  ws.addEventListener("error", () => ws.close());
};

// ---- frames: one decode at a time, keeping only the newest frame that is waiting
let decoding = false;
let waiting = null;
const paint = (jpeg) => {
  if (decoding) {
    waiting = jpeg;
    return;
  }
  decoding = true;
  createImageBitmap(new Blob([jpeg], { type: "image/jpeg" }))
    .then((bitmap) => {
      if (stopped || !context) return bitmap.close();
      if (canvas.width !== bitmap.width || canvas.height !== bitmap.height) {
        canvas.width = bitmap.width;
        canvas.height = bitmap.height;
      }
      context.drawImage(bitmap, 0, 0);
      bitmap.close();
      if (!hasFrame) {
        hasFrame = true;
        dioxus.send({ t: "frame" });
      }
    })
    // A frame that does not decode is skipped; the next one repaints.
    .catch(() => undefined)
    .finally(() => {
      decoding = false;
      const next = waiting;
      waiting = null;
      if (next && !stopped) paint(next);
    });
};

// ---- where a pointer is on the page
const pagePoint = (clientX, clientY, clamp) => {
  if (!viewport || !hasFrame || control?.controller !== "you") return null;
  const rect = canvas.getBoundingClientRect();
  // `object-fit: contain` letterboxes the frame inside the canvas box.
  const fit = Math.min(rect.width / canvas.width, rect.height / canvas.height);
  const width = canvas.width * fit;
  const height = canvas.height * fit;
  if (!(width > 0 && height > 0)) return null;
  const scale = viewport.width / width;
  const x = (clientX - rect.left - (rect.width - width) / 2) * scale;
  const y = (clientY - rect.top - (rect.height - height) / 2) * (viewport.height / height);
  const inside = x >= 0 && y >= 0 && x <= viewport.width && y <= viewport.height;
  if (!inside && !clamp) return null;
  return {
    x: Math.min(Math.max(x, 0), viewport.width),
    y: Math.min(Math.max(y, 0), viewport.height),
    scale,
  };
};
// The agent's cursor, over the letterboxed frame, while the agent drives the page.
const cursor = document.createElement("div");
cursor.className = "preview-agent-cursor";
cursor.hidden = true;
frameBox.append(cursor);
const placeCursor = (pointer) => {
  if (control?.controller === "you" || !viewport || canvas.width < 1 || canvas.height < 1) return;
  const fit = Math.min(canvas.clientWidth / canvas.width, canvas.clientHeight / canvas.height);
  const width = canvas.width * fit;
  const height = canvas.height * fit;
  cursor.style.left = `${(canvas.clientWidth - width) / 2 + (pointer.x * width) / viewport.width}px`;
  cursor.style.top = `${(canvas.clientHeight - height) / 2 + (pointer.y * height) / viewport.height}px`;
  cursor.dataset.phase = pointer.phase;
  cursor.hidden = false;
};

// ---- input, merged per animation frame
let frame = null;
let pendingMove = null;
let pendingWheel = null;
let buttons = 0;
let clicks = { left: 1, middle: 1, right: 1, none: 1 };
let lastClick = null;
let touch = null;
let keySent = false;
const flush = () => {
  if (frame !== null) cancelAnimationFrame(frame);
  frame = null;
  const move = pendingMove;
  const wheel = pendingWheel;
  pendingMove = pendingWheel = null;
  if (move) send(move);
  if (wheel) send(wheel);
};
const schedule = () => {
  frame ??= requestAnimationFrame(flush);
};
function clearInput() {
  if (frame !== null) cancelAnimationFrame(frame);
  frame = null;
  pendingMove = pendingWheel = null;
  buttons = 0;
  clicks = { left: 1, middle: 1, right: 1, none: 1 };
  lastClick = null;
  touch = null;
  keySent = false;
  input.value = SENTINEL;
  input.blur();
}
const queueWheel = (point, deltaX, deltaY, mods) => {
  pendingWheel = {
    type: "wheel",
    x: point.x,
    y: point.y,
    deltaX: (pendingWheel?.deltaX ?? 0) + deltaX,
    deltaY: (pendingWheel?.deltaY ?? 0) + deltaY,
    modifiers: mods,
  };
  schedule();
};
const countClick = (button, x, y, time) => {
  const last = lastClick;
  const count =
    last &&
    last.button === button &&
    time - last.time < MULTI_CLICK_MS &&
    Math.hypot(x - last.x, y - last.y) < MULTI_CLICK_SLOP_PX
      ? last.count + 1
      : 1;
  lastClick = { button, time, x, y, count };
  return count;
};
const buttonOf = (button) => BUTTON[button] ?? "none";
const pressedOf = (mask) => (mask & 1 ? "left" : mask & 2 ? "right" : mask & 4 ? "middle" : "none");

const onWheel = (event) => {
  const point = pagePoint(event.clientX, event.clientY, false);
  if (!point) return;
  event.preventDefault();
  const unit =
    event.deltaMode === WheelEvent.DOM_DELTA_LINE
      ? WHEEL_LINE_PX
      : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
        ? (viewport?.height ?? 0)
        : 1;
  queueWheel(point, event.deltaX * unit, event.deltaY * unit, modifiers(event));
};
const onPointer = (event) => {
  if (control?.controller !== "you") return;
  const { type, pointerId, clientX, clientY } = event;
  const cancelled = type === "pointercancel";
  if (event.pointerType === "touch" && !cancelled) {
    const point = pagePoint(clientX, clientY, type === "pointermove");
    if (type === "pointerdown") {
      if (!event.isPrimary) return;
      touch = {
        pointerId,
        startX: clientX,
        startY: clientY,
        lastX: clientX,
        lastY: clientY,
        panning: false,
      };
      return;
    }
    if (!touch || touch.pointerId !== pointerId) return;
    if (type === "pointermove") {
      if (
        !touch.panning &&
        Math.hypot(clientX - touch.startX, clientY - touch.startY) < TAP_SLOP_PX
      )
        return;
      touch.panning = true;
      if (point)
        queueWheel(
          point,
          (touch.lastX - clientX) * point.scale,
          (touch.lastY - clientY) * point.scale,
          0,
        );
      touch.lastX = clientX;
      touch.lastY = clientY;
      return;
    }
    const gesture = touch;
    touch = null;
    if (gesture.panning || !point) return;
    // A tap is a click; a page that takes text gets the keyboard from the tap that follows.
    send({ type: "probe", x: point.x, y: point.y });
    flush();
    const clickCount = countClick("left", clientX, clientY, event.timeStamp);
    const at = { x: point.x, y: point.y, modifiers: 0 };
    send({ type: "mouse", action: "move", ...at, button: "none", buttons: 0, clickCount: 0 });
    send({ type: "mouse", action: "down", ...at, button: "left", buttons: 1, clickCount });
    send({ type: "mouse", action: "up", ...at, button: "left", buttons: 0, clickCount });
    return;
  }
  const previous = buttons;
  if ((cancelled || type === "pointerup") && previous === 0) return;
  if (cancelled || type === "pointerup") buttons = cancelled ? 0 : event.buttons & 7;
  const point = pagePoint(
    clientX,
    clientY,
    type !== "pointerdown" &&
      (cancelled || type === "pointerup" || previous !== 0 || event.buttons !== 0),
  );
  if (!point) return;
  const at = { x: point.x, y: point.y, modifiers: modifiers(event) };
  const mouse = (action, button, mask) => {
    if (action === "down") clicks[button] = countClick(button, clientX, clientY, event.timeStamp);
    send({ type: "mouse", action, ...at, button, buttons: mask, clickCount: clicks[button] });
  };
  if (type === "pointerdown") {
    input.focus({ preventScroll: true });
    canvas.setPointerCapture(pointerId);
    flush();
    buttons = event.buttons & 7;
    mouse("down", buttonOf(event.button), event.buttons);
  } else if (type === "pointerup") {
    flush();
    mouse("up", buttonOf(event.button), event.buttons);
  } else {
    // Chords arrive as pointermove; a cancelled pointer releases every button it held.
    const next = cancelled ? 0 : event.buttons & 7;
    const changed = previous ^ next;
    if (previous !== 0 && changed !== 0) {
      flush();
      let mask = previous;
      for (const bit of [1, 2, 4]) {
        if (!(changed & bit)) continue;
        mask ^= bit;
        mouse(next & bit ? "down" : "up", pressedOf(bit), mask);
      }
      buttons = next;
    }
    if (cancelled) return;
    pendingMove = {
      type: "mouse",
      action: "move",
      ...at,
      button: pressedOf(event.buttons),
      buttons: event.buttons,
      clickCount: 0,
    };
    schedule();
  }
};

// ---- the keyboard: a hidden text field holds the focus, so a soft keyboard, an IME and a paste all have somewhere to go
const BACKSPACE = { key: "Backspace", code: "Backspace", keyCode: 8 };
const DELETE = { key: "Delete", code: "Delete", keyCode: 46 };
const resetInput = () => {
  keySent = false;
  input.value = SENTINEL;
  input.setSelectionRange(SENTINEL.length, SENTINEL.length);
};
const pressKey = (key) => {
  send({ type: "key", action: "down", ...key, modifiers: 0 });
  send({ type: "key", action: "up", ...key, modifiers: 0 });
};
const onKey = (action) => (event) => {
  if (control?.controller !== "you") return;
  keySent = false;
  // IME and soft keyboards deliver text through composition and input events.
  if (
    event.isComposing ||
    event.keyCode === 229 ||
    event.key === "Process" ||
    event.key === "Unidentified"
  )
    return;
  // Shift+Escape returns to the app; a plain Escape and Tab belong to the page.
  if (event.key === "Escape" && event.shiftKey) {
    event.preventDefault();
    event.stopPropagation();
    if (action === "down") input.blur();
    return;
  }
  const shortcut = event.ctrlKey || event.metaKey;
  // A paste carries this device's clipboard (the paste event); copy and cut run in the page and its text comes back.
  if (shortcut && event.key.toLowerCase() === "v") return;
  if (event.shiftKey && event.key === "Insert") return;
  // Enter carries "\r" like Puppeteer's key table, so forms submit and textareas break lines.
  const text = shortcut
    ? undefined
    : [...event.key].length === 1
      ? event.key
      : event.key === "Enter"
        ? "\r"
        : undefined;
  send({
    type: "key",
    action,
    key: event.key,
    code: event.code,
    keyCode: event.keyCode,
    ...(action === "down" && text !== undefined ? { text } : {}),
    modifiers: modifiers(event),
  });
  // Some Android keyboards edit the field even when keydown is prevented.
  keySent =
    action === "down" &&
    !shortcut &&
    (text !== undefined || event.key === "Backspace" || event.key === "Delete");
  // Shortcuts stay with the app (copy, keybindings); other keys belong to the page.
  if (shortcut) return;
  event.preventDefault();
  event.stopPropagation();
};
const onInput = (event) => {
  if (event.isComposing) return;
  if (keySent) keySent = false;
  else if (event.inputType === "deleteContentBackward") pressKey(BACKSPACE);
  else if (event.inputType === "deleteContentForward") pressKey(DELETE);
  else {
    const text = input.value.replace(/^​/, "");
    if (text) send({ type: "text", text });
  }
  resetInput();
};
const onCompositionEnd = (event) => {
  const text = event.data.replace(/^​/, "");
  if (text) send({ type: "text", text });
  resetInput();
};
const onPaste = (event) => {
  event.preventDefault();
  const text = event.clipboardData.getData("text/plain");
  if (text) send({ type: "text", text });
};
const noDefault = (event) => event.preventDefault();

// ---- the size of the box the page is shown in
let timer = null;
const measure = () => {
  timer = null;
  const rect = frameBox.getBoundingClientRect();
  if (rect.width < 1 || rect.height < 1) return;
  const next = { width: Math.round(rect.width), height: Math.round(rect.height) };
  if (size && size.width === next.width && size.height === next.height) return;
  const first = size === null;
  size = next;
  if (!first && init.followSize) send({ type: "resize", ...size });
  dioxus.send({ t: "size", ...size, ratio: window.devicePixelRatio || 1 });
};
const observer = new ResizeObserver(() => {
  if (timer !== null) clearTimeout(timer);
  // The first size is reported right away; later ones settle before the page is resized.
  timer = setTimeout(measure, size === null ? 0 : RESIZE_DEBOUNCE_MS);
});
observer.observe(frameBox);

canvas.addEventListener("wheel", onWheel, { passive: false });
for (const name of ["pointerdown", "pointermove", "pointerup", "pointercancel"])
  canvas.addEventListener(name, onPointer);
canvas.addEventListener("mousedown", noDefault);
canvas.addEventListener("contextmenu", noDefault);
input.addEventListener("focus", resetInput);
input.addEventListener("keydown", onKey("down"));
input.addEventListener("keyup", onKey("up"));
input.addEventListener("input", onInput);
input.addEventListener("compositionend", onCompositionEnd);
input.addEventListener("copy", noDefault);
input.addEventListener("cut", noDefault);
input.addEventListener("paste", onPaste);

const stop = () => {
  stopped = true;
  closeSocket();
  waiting = null;
  if (timer !== null) clearTimeout(timer);
  observer.disconnect();
  canvas.removeEventListener("wheel", onWheel);
  for (const name of ["pointerdown", "pointermove", "pointerup", "pointercancel"])
    canvas.removeEventListener(name, onPointer);
  canvas.removeEventListener("mousedown", noDefault);
  canvas.removeEventListener("contextmenu", noDefault);
  input.removeEventListener("focus", resetInput);
  cursor.remove();
};
try {
  for (;;) {
    const message = await dioxus.recv();
    if (!message || message.type === "stop") break;
    if (message.type === "connect") {
      hasFrame = false;
      connect(message.url);
    } else if (message.type === "send") send(message.message);
  }
} finally {
  stop();
}
