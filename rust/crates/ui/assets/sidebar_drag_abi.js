// Dragging a sidebar thread row. The lifted row follows the pointer with a transform and the place
// it would land is marked with a class on one row or section; nothing is sent to Rust until the
// pointer is released, when it reports one `drop` (the row, the section, the row it lands before).
// Rows opt in with `data-drops` (the sections they may land in); sections are the
// `.thread-section` / `.thread-shelf` containers of one `.project-group`. Escape, a lost pointer or
// a hidden page cancels without a report. A mouse starts the drag after a few pixels, a finger
// after a press-and-hold so that a swipe still scrolls the list. The list scrolls only as the
// pointer moves near its edge; nothing here runs on a timer once the drag is over.
window.__t3SidebarDrag?.();
const DISTANCE = 6;
const HOLD_MS = 350;
const EDGE = 28;
let busy = false;
let gesture = null;
const list = () => document.querySelector(".project-list");

const classes = ["drop-before", "drop-end", "drop-target"];
const unmark = () => {
  for (const name of classes)
    for (const el of document.querySelectorAll(`.${name}`)) el.classList.remove(name);
};
const mark = (target) => {
  const key = target ? `${target.section}|${target.before?.dataset.threadId ?? ""}` : "";
  if (gesture.marked === key) return;
  gesture.marked = key;
  unmark();
  if (!target) return;
  target.container.classList.add("drop-target");
  if (target.before) target.before.classList.add("drop-before");
  else if (target.section !== "settled") target.container.classList.add("drop-end");
};

// Where the pointer would drop the row: the section it is over (or nearest), and the row it
// lands in front of. The pointer must be over the project the row belongs to.
const locate = (x, y) => {
  const { group, drops, row } = gesture;
  const pane = list()?.getBoundingClientRect();
  const box = group.getBoundingClientRect();
  if (!pane || x < pane.left || x > pane.right || y < pane.top || y > pane.bottom) return null;
  if (y < box.top - 24 || y > box.bottom + 24) return null;
  let best = null;
  for (const container of group.querySelectorAll(".thread-section, .thread-shelf")) {
    const section = container.dataset.section;
    if (!drops.includes(section)) continue;
    const rect = container.getBoundingClientRect();
    const distance = y < rect.top ? rect.top - y : y > rect.bottom ? y - rect.bottom : 0;
    if (best === null || distance < best.distance) best = { container, section, distance };
  }
  if (!best) return null;
  let before = null;
  if (best.section !== "settled") {
    for (const candidate of best.container.querySelectorAll(".thread-row-wrap")) {
      if (candidate === row) continue;
      const rect = candidate.getBoundingClientRect();
      if (rect.top + rect.height / 2 > y) {
        before = candidate;
        break;
      }
    }
  }
  return { container: best.container, section: best.section, before };
};

const suppressClick = (event) => {
  event.stopPropagation();
  event.preventDefault();
};
const stop = (event) => event.preventDefault();

const finish = (report) => {
  const g = gesture;
  if (!g) return;
  gesture = null;
  clearTimeout(g.timer);
  document.removeEventListener("pointermove", onMove, true);
  document.removeEventListener("pointerup", onUp, true);
  document.removeEventListener("pointercancel", onCancel, true);
  document.removeEventListener("keydown", onKey, true);
  document.removeEventListener("touchmove", stop, true);
  document.removeEventListener("contextmenu", stop, true);
  window.removeEventListener("blur", onCancel);
  document.removeEventListener("visibilitychange", onHidden);
  if (g.phase === "dragging") {
    g.row.classList.remove("lifted");
    g.row.style.transform = "";
    document.body.classList.remove("sidebar-dragging");
    unmark();
    // The click that ends a drag is not a click on the row.
    document.addEventListener("click", suppressClick, true);
    setTimeout(() => document.removeEventListener("click", suppressClick, true), 0);
    if (report && g.target)
      dioxus.send({
        type: "drop",
        id: g.id,
        section: g.target.section,
        before: g.target.before?.dataset.threadId ?? null,
      });
  }
};
const onCancel = () => finish(false);
const onHidden = () => {
  if (document.hidden) finish(false);
};
const onKey = (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    event.stopPropagation();
    finish(false);
  }
};
const begin = () => {
  const g = gesture;
  if (!g || g.phase !== "pending") return;
  g.phase = "dragging";
  g.row.classList.add("lifted");
  document.body.classList.add("sidebar-dragging");
  document.getSelection()?.removeAllRanges();
  try {
    g.row.setPointerCapture(g.pointerId);
  } catch {}
  if (g.touch) {
    document.addEventListener("touchmove", stop, { capture: true, passive: false });
    document.addEventListener("contextmenu", stop, true);
  }
};
const update = (event) => {
  const g = gesture;
  const pane = list();
  const lifted = event.clientY - g.startY + (pane ? pane.scrollTop - g.scroll : 0);
  g.row.style.transform = `translateY(${lifted}px)`;
  if (pane) {
    const rect = pane.getBoundingClientRect();
    if (event.clientY < rect.top + EDGE) pane.scrollTop -= 8;
    else if (event.clientY > rect.bottom - EDGE) pane.scrollTop += 8;
  }
  g.target = locate(event.clientX, event.clientY);
  mark(g.target);
};
const onMove = (event) => {
  const g = gesture;
  if (!g || event.pointerId !== g.pointerId) return;
  if (g.phase === "pending") {
    const moved = Math.hypot(event.clientX - g.startX, event.clientY - g.startY);
    // A finger that moves before the hold is a swipe.
    if (g.touch && moved > 8) return finish(false);
    if (!g.touch && moved > DISTANCE) begin();
  }
  if (g.phase === "dragging") {
    if (event.cancelable) event.preventDefault();
    update(event);
  }
};
const onUp = (event) => {
  if (gesture && event.pointerId === gesture.pointerId) finish(true);
};
const onDown = (event) => {
  if (busy || gesture || !event.isPrimary || (event.pointerType === "mouse" && event.button !== 0))
    return;
  const target = event.target;
  if (!(target instanceof Element)) return;
  if (target.closest("input,textarea,select,.thread-woke-dismiss,.context-menu,.menu-backdrop"))
    return;
  const row = target.closest(".thread-row-wrap[data-drops]");
  const pane = list();
  if (!row || !pane || !pane.contains(row) || !row.dataset.drops || row.querySelector(".renaming"))
    return;
  const group = row.closest(".project-group");
  if (!group) return;
  const touch = event.pointerType !== "mouse";
  gesture = {
    id: row.dataset.threadId,
    row,
    group,
    drops: row.dataset.drops.split(" "),
    pointerId: event.pointerId,
    touch,
    startX: event.clientX,
    startY: event.clientY,
    scroll: pane.scrollTop,
    phase: "pending",
    target: null,
    marked: null,
    timer: touch ? setTimeout(begin, HOLD_MS) : 0,
  };
  document.addEventListener("pointermove", onMove, { capture: true, passive: false });
  document.addEventListener("pointerup", onUp, true);
  document.addEventListener("pointercancel", onCancel, true);
  document.addEventListener("keydown", onKey, true);
  window.addEventListener("blur", onCancel);
  document.addEventListener("visibilitychange", onHidden);
};
document.addEventListener("pointerdown", onDown, true);
const remove = () => {
  finish(false);
  document.removeEventListener("pointerdown", onDown, true);
};
window.__t3SidebarDrag = remove;
try {
  for (;;) {
    const message = await dioxus.recv();
    if (!message || message.type === "stop") break;
    if (message.type === "busy") busy = message.busy === true;
  }
} finally {
  remove();
  if (window.__t3SidebarDrag === remove) window.__t3SidebarDrag = undefined;
}
