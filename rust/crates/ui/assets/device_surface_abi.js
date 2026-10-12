// The input socket of an iOS simulator shown in the device panel (`stream.ts`, iOS half). The picture is an <img> Rust gives the hub's MJPEG
// address; this holds the helper's socket that carries what the person does to it, as serve-sim reads it: a packet is a tag byte and JSON.
// Rust says when to connect and what the buttons do; this turns a pointer on the picture into touches and reports the socket.
//
// Rust -> here: {type:"init", surface}, {type:"connect", url}, {type:"send", message:{tag, body}}, {type:"stop"}.
// Here -> Rust: {t:"open"}, {t:"close", code, reason, opened}.
const TOUCH = 0x03;
const HARDWARE_KEYBOARD = 0x0d;
const encoder = new TextEncoder();
const init = await dioxus.recv();
const root = document.getElementById(init.surface);
const picture = root?.querySelector("img");
if (!root || !picture) {
  dioxus.send({ t: "close", code: 1011, reason: "no surface", opened: false });
  throw new Error("device surface is not in the page");
}
let socket = null;
let stopped = false;
let touching = null;

const packet = (tag, body) => {
  const json = encoder.encode(JSON.stringify(body));
  const out = new Uint8Array(1 + json.length);
  out[0] = tag;
  out.set(json, 1);
  return out;
};
const send = (tag, body) => {
  if (!socket || socket.readyState !== WebSocket.OPEN) return false;
  socket.send(packet(tag, body));
  return true;
};
const connect = (url) => {
  if (socket) socket.close();
  if (stopped) return;
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  socket = ws;
  let opened = false;
  ws.addEventListener("open", () => {
    if (socket !== ws) return;
    opened = true;
    // The simulator's own keyboard stays out of the way of the one the person types on.
    send(HARDWARE_KEYBOARD, { enabled: false });
    dioxus.send({ t: "open" });
  });
  ws.addEventListener("close", (event) => {
    if (socket !== ws) return;
    socket = null;
    dioxus.send({ t: "close", code: event.code, reason: event.reason, opened });
  });
  ws.addEventListener("error", () => ws.close());
};

// A point on the picture, 0..1 on both axes, or null outside it. The picture is letterboxed in its box (`object-fit: contain`).
const pointOf = (event, clamp) => {
  const rect = picture.getBoundingClientRect();
  const naturalWidth = picture.naturalWidth || rect.width;
  const naturalHeight = picture.naturalHeight || rect.height;
  const fit = Math.min(rect.width / naturalWidth, rect.height / naturalHeight);
  const width = naturalWidth * fit;
  const height = naturalHeight * fit;
  if (!(width > 0 && height > 0)) return null;
  const x = (event.clientX - rect.left - (rect.width - width) / 2) / width;
  const y = (event.clientY - rect.top - (rect.height - height) / 2) / height;
  const inside = x >= 0 && y >= 0 && x <= 1 && y <= 1;
  if (!inside && !clamp) return null;
  return { x: Math.min(Math.max(x, 0), 1), y: Math.min(Math.max(y, 0), 1) };
};
const onPointer = (event) => {
  if (event.type === "pointerdown") {
    if (event.button !== 0) return;
    const point = pointOf(event, false);
    if (!point) return;
    picture.setPointerCapture(event.pointerId);
    touching = event.pointerId;
    send(TOUCH, { type: "begin", ...point });
  } else if (touching === event.pointerId) {
    const point = pointOf(event, true);
    if (!point) return;
    if (event.type === "pointermove") send(TOUCH, { type: "move", ...point });
    else {
      touching = null;
      send(TOUCH, { type: "end", ...point });
    }
  }
};
for (const name of ["pointerdown", "pointermove", "pointerup", "pointercancel"])
  picture.addEventListener(name, onPointer);
const noDefault = (event) => event.preventDefault();
picture.addEventListener("dragstart", noDefault);
picture.addEventListener("contextmenu", noDefault);

try {
  for (;;) {
    const message = await dioxus.recv();
    if (!message || message.type === "stop") break;
    if (message.type === "connect") connect(message.url);
    else if (message.type === "send") send(message.message.tag, message.message.body);
  }
} finally {
  stopped = true;
  if (socket) socket.close();
  for (const name of ["pointerdown", "pointermove", "pointerup", "pointercancel"])
    picture.removeEventListener(name, onPointer);
  picture.removeEventListener("dragstart", noDefault);
  picture.removeEventListener("contextmenu", noDefault);
}
