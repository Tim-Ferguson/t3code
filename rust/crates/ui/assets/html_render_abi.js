// The page side of an agent's HTML render frame (`HtmlRenderDocument` in BrowserDocumentFrame.tsx). The frame is a sandboxed
// iframe with an opaque origin; this script is the app's end of the MCP Apps message bridge it speaks. It forwards to Rust what
// the page posts (its content height, a link it wants opened) together with the two facts the app checks before opening a link
// (whether the frame has focus, whether the reader has just used the app), reports the width of the frame's box so Rust can pick
// the frame's height, and posts into the frame what Rust sends (the theme). Rust decides everything; nothing is timed or polled.
// `args` is {id}, the frame's element id.
let frame = document.getElementById(args.id);
// The script can run a moment before the frame is in the document; it waits for the next frame once.
if (!frame) {
  await new Promise((resolve) => requestAnimationFrame(resolve));
  frame = document.getElementById(args.id);
}
const box = frame?.parentElement;
if (!frame || !box) {
  dioxus.send({ type: "missing" });
} else {
  const post = (message) => frame.contentWindow?.postMessage(message, "*");
  const measure = () => dioxus.send({ type: "width", width: box.clientWidth });
  const resize = new ResizeObserver(([entry]) => {
    if (entry) dioxus.send({ type: "width", width: entry.contentRect.width });
  });
  resize.observe(box);
  // Read before first paint in the original; the first report lands as soon as the script runs.
  measure();
  const onMessage = (event) => {
    if (event.source !== frame.contentWindow) return;
    dioxus.send({
      type: "message",
      data: event.data ?? null,
      focused: document.activeElement === frame,
      active: navigator.userActivation ? navigator.userActivation.isActive : null,
    });
  };
  window.addEventListener("message", onMessage);
  const onLoad = () => dioxus.send({ type: "load" });
  frame.addEventListener("load", onLoad);
  try {
    for (;;) {
      const command = await dioxus.recv();
      if (!command || command.type === "stop") break;
      // The theme, and the empty result of a request the app carried out (the page cannot open windows itself).
      if (command.type === "post") post(command.message);
    }
  } finally {
    resize.disconnect();
    window.removeEventListener("message", onMessage);
    frame.removeEventListener("load", onLoad);
  }
}
