// The panel's tab bar when its tabs do not fit. Rust draws the two scroll buttons; this reports whether the
// bar overflows and which way it can still scroll, and does what the buttons and the selection ask. A report
// is sent when that answer changes (the bar or one of its tabs resized, a tab came or went, the bar scrolled),
// never on a timer. The wheel scrolls the bar sideways, as in the original.
window.__t3TabScroll?.();
const TOLERANCE = 1;
const bar = document.querySelector(".right-panel-tabs");
let last = "";
const report = () => {
  if (!bar) return;
  const has = bar.scrollWidth - bar.clientWidth > TOLERANCE;
  const left = has && bar.scrollLeft > TOLERANCE;
  const right = has && bar.scrollLeft + bar.clientWidth < bar.scrollWidth - TOLERANCE;
  const key = `${has}|${left}|${right}`;
  if (key === last) return;
  last = key;
  dioxus.send({ has, left, right });
};
const resize = bar ? new ResizeObserver(report) : null;
const watch = () => {
  if (!bar) return;
  resize.disconnect();
  resize.observe(bar);
  for (const child of bar.children) resize.observe(child);
  report();
};
const tabs = bar ? new MutationObserver(watch) : null;
const wheel = (event) => {
  if (event.ctrlKey) return;
  let delta = Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : event.deltaY;
  if (event.deltaMode === WheelEvent.DOM_DELTA_LINE) delta *= 16;
  if (event.deltaMode === WheelEvent.DOM_DELTA_PAGE) delta *= bar.clientWidth;
  if (delta === 0) return;
  const before = bar.scrollLeft;
  bar.scrollLeft += delta;
  if (bar.scrollLeft === before) return;
  event.preventDefault();
  report();
};
if (bar) {
  tabs.observe(bar, { childList: true });
  bar.addEventListener("scroll", report, { passive: true });
  bar.addEventListener("wheel", wheel, { passive: false });
  watch();
}
const remove = () => {
  resize?.disconnect();
  tabs?.disconnect();
  bar?.removeEventListener("scroll", report);
  bar?.removeEventListener("wheel", wheel);
};
window.__t3TabScroll = remove;
try {
  for (;;) {
    const message = await dioxus.recv();
    if (!message || message.type === "stop") break;
    if (!bar) continue;
    if (message.type === "scroll") {
      const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      bar.scrollBy({
        left: message.direction * Math.max(120, bar.clientWidth * 0.75),
        behavior: reduceMotion ? "auto" : "smooth",
      });
    } else if (message.type === "reveal") {
      // Only a bar that overflows has anything to bring into view.
      if (bar.scrollWidth - bar.clientWidth > TOLERANCE)
        bar
          .querySelector(".right-panel-tab.selected")
          ?.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
  }
} finally {
  remove();
  if (window.__t3TabScroll === remove) window.__t3TabScroll = undefined;
}
