// ABI/lifecycle routing only; font probing, enumeration and CSS policy are Rust.
const registry = (window.__t3RustAppearances ??= new Map());
const slot = { disposed: false };
registry.set(args.id, slot);
try {
  const module = await import(args.base + "/t3_terminal.js");
  await module.default({ module_or_path: args.wasm });
  if (slot.disposed) return;
  dioxus.send({ type: "ready", defaults: JSON.parse(module.appearance_default_fonts()) });
  const reply = async (command) => {
    try {
      let value;
      switch (command.type) {
        case "probe":
          value = JSON.parse(module.appearance_probe_font(command.family));
          break;
        case "collection-labels":
          value = JSON.parse(module.appearance_collection_labels(JSON.stringify(command.labels)));
          break;
        case "permission":
          value = await module.appearance_font_permission();
          break;
        case "enumerate":
          value = JSON.parse(await module.appearance_query_fonts());
          break;
        default:
          throw new Error("Unknown Rust appearance ABI request");
      }
      if (!slot.disposed) dioxus.send({ type: "response", id: command.id, value });
    } catch (error) {
      if (!slot.disposed) dioxus.send({ type: "response", id: command.id, error: String(error) });
    }
  };
  while (!slot.disposed) {
    const command = await dioxus.recv();
    if (!command || command.type === "dispose") break;
    if (command.type === "apply") module.appearance_apply_fonts(JSON.stringify(command.settings));
    else void reply(command);
  }
} catch (error) {
  if (!slot.disposed) dioxus.send({ type: "error", message: String(error) });
} finally {
  slot.disposed = true;
  if (registry.get(args.id) === slot) registry.delete(args.id);
}
