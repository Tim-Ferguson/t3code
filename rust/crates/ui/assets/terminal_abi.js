// ABI/lifecycle routing only. Rendering, input, geometry and emulator state are Rust.
const registry = (window.__t3RustTerminals ??= new Map());
const slot = { disposed: false, surface: null };
registry.set(args.id, slot);
try {
  const module = await import(args.base + "/t3_terminal.js");
  await module.default({ module_or_path: args.wasm });
  if (slot.disposed) return;
  const host = document.getElementById(args.id);
  if (!host) return;
  const surface = await module.mount_terminal(host, JSON.stringify(args.options), (event) => {
    if (!slot.disposed) dioxus.send(JSON.parse(event));
  });
  slot.surface = surface;
  if (slot.disposed) return;
  dioxus.send({ type: "ready" });
  while (!slot.disposed) {
    const command = await dioxus.recv();
    if (!command || command.type === "dispose") break;
    switch (command.type) {
      case "append":
        surface.write(command.data);
        break;
      case "reset":
        surface.reset_and_write(command.data);
        break;
      case "visible":
        surface.set_visible(command.visible);
        break;
      case "readonly":
        surface.set_read_only(command.readOnly);
        break;
      case "focus":
        surface.focus();
        break;
      case "fit":
        surface.fit();
        break;
      case "font":
        surface.set_font(command.family, command.size);
        break;
      case "size":
        surface.resend_size();
        break;
      default:
        throw new Error("Unknown Rust terminal ABI command");
    }
    if (command.receipt) dioxus.send({ type: "applied", receipt: command.receipt });
  }
} catch (error) {
  if (!slot.disposed) dioxus.send({ type: "error", message: String(error) });
} finally {
  slot.disposed = true;
  if (slot.surface) {
    slot.surface.dispose();
    slot.surface.free();
  }
  if (registry.get(args.id) === slot) registry.delete(args.id);
}
