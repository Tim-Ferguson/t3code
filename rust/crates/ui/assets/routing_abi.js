// The browser's address bar for the app's views. Rust owns what each address means: this reports the
// address on load and on back and forward, and writes the address Rust asks for. `push` adds a history
// entry, `replace` rewrites the current one, and an address already shown is left alone, so a view
// that was reached from the address does not add an entry for it.
const current = () => window.location.pathname;
const report = () => dioxus.send({ type: "location", path: current() });
window.addEventListener("popstate", report);
report();
try {
  for (;;) {
    const message = await dioxus.recv();
    if (!message || message.type === "stop") break;
    if (message.path === current()) continue;
    if (message.type === "push") window.history.pushState(null, "", message.path);
    else if (message.type === "replace") window.history.replaceState(null, "", message.path);
  }
} finally {
  window.removeEventListener("popstate", report);
}
