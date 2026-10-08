// Rust owns probing, hover, scheduling, listener lifetime and spotlight rendering.
const registry = (window.__t3RustThemeInspectors ??= new Map());
const slot = { disposed: false, surface: null };
registry.set(args.id, slot);
try {
  const module = await import(args.base + "/t3_terminal.js");
  await module.default({ module_or_path: args.wasm });
  if (slot.disposed) return;
  slot.surface = new module.ThemeInspector(JSON.stringify(args.config), (event) => {
    if (!slot.disposed) dioxus.send(JSON.parse(event));
  });
  dioxus.send({ type: "ready" });
  while (!slot.disposed) {
    const command = await dioxus.recv();
    if (!command || command.type === "dispose") break;
    if (command.type === "selection")
      slot.surface.selection(JSON.stringify(command.roles), command.armed);
    else if (command.type === "reveal") slot.surface.reveal(command.role);
  }
} catch (error) {
  if (!slot.disposed) dioxus.send({ type: "error", message: String(error) });
} finally {
  slot.disposed = true;
  slot.surface?.dispose();
  slot.surface?.free();
  slot.surface = null;
  if (registry.get(args.id) === slot) registry.delete(args.id);
}
