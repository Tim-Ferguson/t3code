// The one window-level keydown listener. It decides nothing about commands: Rust sends a table of
// the chords it would act on right now (see `Claims` in t3-client), each with a bit per
// combination of DOM focus. This reads the focus from the DOM at the keypress, looks the chord
// up, and only then claims the event and reports it. Everything else (typing, IME composition,
// chords no rule names, commands the app cannot run) is never touched.
window.__t3Keys?.();
let table = {};
const EDITABLE =
  'input,textarea,select,[contenteditable=""],[contenteditable="true"],[contenteditable="plaintext-only"],[role="textbox"]';
const PHYSICAL = {
  Backquote: "`",
  Backslash: "\\",
  BracketLeft: "[",
  BracketRight: "]",
  Comma: ",",
  Digit0: "0",
  Digit1: "1",
  Digit2: "2",
  Digit3: "3",
  Digit4: "4",
  Digit5: "5",
  Digit6: "6",
  Digit7: "7",
  Digit8: "8",
  Digit9: "9",
  Equal: "=",
  Minus: "-",
  Period: ".",
  Quote: "'",
  Semicolon: ";",
  Slash: "/",
};
const LETTER = /^[a-z]$/;
const names = (event) => {
  let layout = event.key.toLowerCase();
  if (layout === "esc") layout = "escape";
  const out = [layout];
  const letter = /^Key([A-Z])$/.exec(event.code);
  if (letter && !LETTER.test(layout)) out.push(letter[1].toLowerCase());
  const physical = LETTER.test(layout) ? layout : (PHYSICAL[event.code] ?? layout);
  if (!out.includes(physical)) out.push(physical);
  return out;
};
const focusBits = () => {
  const active = document.activeElement;
  const within = (selector) =>
    active && active.closest ? active.closest(selector) !== null : false;
  const terminal = within(".terminal-canvas");
  const editable = within(EDITABLE);
  const composer = within(".composer textarea, .new-thread-prompt textarea");
  const dialog = document.querySelector('[aria-modal="true"]') !== null;
  return (terminal ? 1 : 0) | (editable ? 2 : 0) | (composer ? 4 : 0) | (dialog ? 8 : 0);
};
const onKey = (event) => {
  // Recording a new shortcut: the key belongs to the field that is capturing it.
  if (event.target && event.target.closest && event.target.closest("[data-keybinding-capture]"))
    return;
  if (event.isComposing || event.keyCode === 229) return;
  if (
    !args.mac &&
    event.getModifierState &&
    event.getModifierState("AltGraph") &&
    !/^[a-z0-9]$/i.test(event.key)
  )
    return;
  const prefix = `${+event.metaKey}${+event.ctrlKey}${+event.shiftKey}${+event.altKey}|`;
  const keys = names(event);
  if (!keys.some((key) => table[prefix + key])) return;
  const dom = focusBits();
  if (!keys.some((key) => ((table[prefix + key] ?? 0) >> dom) & 1)) return;
  event.preventDefault();
  event.stopPropagation();
  dioxus.send({
    key: event.key,
    code: event.code,
    meta: event.metaKey,
    ctrl: event.ctrlKey,
    shift: event.shiftKey,
    alt: event.altKey,
    altGraph: !!(event.getModifierState && event.getModifierState("AltGraph")),
    repeat: event.repeat,
    composing: false,
    keyCode: event.keyCode,
    dom,
  });
};
window.addEventListener("keydown", onKey, true);
const remove = () => window.removeEventListener("keydown", onKey, true);
window.__t3Keys = remove;
try {
  for (;;) {
    const message = await dioxus.recv();
    if (!message || message.type === "stop") break;
    if (message.type === "claims") table = message.table;
  }
} finally {
  remove();
  if (window.__t3Keys === remove) window.__t3Keys = undefined;
}
