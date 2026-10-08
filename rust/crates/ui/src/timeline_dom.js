// Renderer adapter: DOM geometry, event target hit-testing and scroll writes.
// Follow/anchor/reading decisions are made by Rust, not this bridge.
const node = document.getElementById(timelineId);
if (!node) {
  dioxus.send({ kind: "error", message: "Mounted conversation element is unavailable" });
  return;
}
let stopped = false,
  frame = null,
  pendingKind = null,
  intentGeneration = 0,
  writeGeneration = 0;
const removers = [];
let geometryDirty = true;
const listen = (target, type, fn, options) => {
  target.addEventListener(type, fn, options);
  removers.push(() => target.removeEventListener(type, fn, options));
};
const measure = (kind, extra = {}) => {
  if (stopped || !node.isConnected) return;
  let geometry = { hasGeometry: false };
  if (geometryDirty || kind === "layout" || kind === "measure") {
    geometryDirty = false;
    const top = node.getBoundingClientRect().top + node.clientTop;
    const rows = Array.from(node.querySelectorAll(":scope > [data-timeline-row]"));
    geometry = {
      hasGeometry: true,
      rowIds: rows.map((row) => row.dataset.timelineRow),
      positions: rows.map((row) => row.getBoundingClientRect().top - top + node.scrollTop),
      sizes: rows.map((row) => row.getBoundingClientRect().height),
      messages: rows.map((row) => row.dataset.messageId || null),
    };
  }
  dioxus.send({
    kind,
    intentGeneration,
    ...geometry,
    scroll: node.scrollTop,
    viewportHeight: node.clientHeight,
    contentLength: node.scrollHeight,
    ...extra,
  });
};
const schedule = (kind) => {
  if (kind === "layout" || kind === "measure") geometryDirty = true;
  // Geometry-dirty work must survive an earlier scroll notification in the
  // same frame, including the final streamed chunk with no later events.
  if (pendingKind === null || kind === "layout" || kind === "measure") pendingKind = kind;
  if (frame !== null) return;
  frame = requestAnimationFrame(() => {
    frame = null;
    const kind = pendingKind;
    pendingKind = null;
    measure(kind);
  });
};
const nestedConsumes = (target, direction) => {
  for (
    let current = target instanceof Element ? target : target?.parentElement;
    current && current !== node;
    current = current.parentElement
  ) {
    const style = getComputedStyle(current);
    if (
      !["auto", "scroll"].includes(style.overflowY) ||
      current.scrollHeight <= current.clientHeight
    )
      continue;
    if (style.overscrollBehaviorY === "contain" || style.overscrollBehaviorY === "none")
      return true;
    if (
      direction < 0
        ? current.scrollTop > 0
        : current.scrollTop + current.clientHeight < current.scrollHeight - 1
    )
      return true;
  }
  return false;
};
const intent = (kind, extra) => {
  // Invalidates a frame queued before a gesture, even before Rust receives it.
  intentGeneration++;
  writeGeneration++;
  measure(kind, extra);
};
listen(node, "scroll", () => schedule("scroll"), { passive: true });
listen(
  node,
  "wheel",
  (event) =>
    intent("wheel", {
      deltaY: event.deltaY,
      ctrlKey: event.ctrlKey,
      timelineTarget: !nestedConsumes(event.target, event.deltaY),
    }),
  { passive: true },
);
listen(node, "touchmove", () => intent("touch", {}), { passive: true });
listen(node, "pointerdown", (event) => intent("pointer", { scrollbar: event.target === node }), {
  passive: true,
});
listen(document, "keydown", (event) => {
  if (
    event.defaultPrevented ||
    event.isComposing ||
    event.altKey ||
    event.ctrlKey ||
    event.metaKey ||
    event.shiftKey
  )
    return;
  if (
    event.target !== document.body &&
    event.target !== document.documentElement &&
    !node.contains(event.target)
  )
    return;
  if (
    event.target instanceof Element &&
    event.target.closest('input,textarea,select,[contenteditable="true"],[role="textbox"]')
  )
    return;
  if (document.querySelector('[role="dialog"],[data-mcp-app-fullscreen]')) return;
  const direction = ["PageUp", "Home", "ArrowUp"].includes(event.key)
    ? -1
    : ["PageDown", "End", "ArrowDown"].includes(event.key)
      ? 1
      : 0;
  if (direction)
    intent("key", { direction, timelineTarget: !nestedConsumes(event.target, direction) });
});
const resize = new ResizeObserver(() => schedule("layout"));
resize.observe(node);
const observeRows = () => {
  resize.disconnect();
  resize.observe(node);
  for (const row of node.querySelectorAll(":scope > [data-timeline-row]")) resize.observe(row);
};
const mutation = new MutationObserver((records) => {
  if (records.some((record) => record.type === "childList")) observeRows();
  if (
    records.some(
      (record) =>
        !(record.target instanceof Element) || !record.target.hasAttribute("data-timeline-spacer"),
    )
  )
    schedule("layout");
});
mutation.observe(node, {
  subtree: true,
  childList: true,
  characterData: true,
  attributes: true,
  attributeFilter: ["open"],
});
observeRows();
schedule("layout");
try {
  while (!stopped) {
    const command = await dioxus.recv();
    if (command.type === "stop") break;
    if (command.type === "measure") {
      schedule("measure");
      continue;
    }
    if (command.intentGeneration !== intentGeneration) continue;
    const generation = ++writeGeneration;
    if (command.type === "cancel") {
      const spacer = node.querySelector(":scope > [data-timeline-spacer]");
      if (spacer) spacer.style.height = "0px";
      node.scrollTop = node.scrollTop;
      continue;
    }
    if (command.type !== "scroll" || !Number.isFinite(command.offset)) continue;
    // Layout writes wait two frames; a user gesture or newer owned request
    // supersedes them while the renderer mounts/updates the row tree.
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        if (
          stopped ||
          !node.isConnected ||
          generation !== writeGeneration ||
          command.intentGeneration !== intentGeneration
        )
          return;
        const spacer = node.querySelector(":scope > [data-timeline-spacer]");
        if (spacer) spacer.style.height = `${command.spacer}px`;
        if (Math.abs(node.scrollTop - command.offset) > 1) node.scrollTop = command.offset;
        // Also reports completion when the scroll offset already matched.
        schedule("scroll");
      }),
    );
  }
} finally {
  stopped = true;
  if (frame !== null) cancelAnimationFrame(frame);
  mutation.disconnect();
  resize.disconnect();
  for (const remove of removers) remove();
}
