// Browser API transport only. Storage policy, migrations and palette resolution are Rust.
const registry = (window.__t3RustThemes ??= new Map());
const slot = { disposed: false };
registry.set(args.id, slot);
const media = window.matchMedia?.("(prefers-color-scheme: dark)");
const changed = (event) => {
  if (!slot.disposed) dioxus.send({ type: "storage", key: event.key });
};
const appearance = () => {
  if (!slot.disposed) dioxus.send({ type: "media", dark: media?.matches ?? false });
};
window.addEventListener("storage", changed);
media?.addEventListener("change", appearance);
let cleaned = false;
slot.dispose = () => {
  if (cleaned) return;
  cleaned = true;
  slot.disposed = true;
  window.removeEventListener("storage", changed);
  media?.removeEventListener("change", appearance);
  if (registry.get(args.id) === slot) registry.delete(args.id);
};
try {
  dioxus.send({ type: "ready", dark: media?.matches ?? false });
  while (!slot.disposed) {
    const command = await dioxus.recv();
    if (!command || command.type === "dispose") break;
    try {
      let value = null;
      switch (command.type) {
        case "get":
          value = window.localStorage.getItem(command.key);
          break;
        case "set":
          window.localStorage.setItem(command.key, command.value);
          break;
        case "remove":
          window.localStorage.removeItem(command.key);
          break;
        case "apply": {
          const root = document.documentElement;
          root.classList.toggle("dark", command.dark);
          root.style.colorScheme = command.dark ? "dark" : "light";
          if (command.paletteId) root.dataset.themeId = command.paletteId;
          else delete root.dataset.themeId;
          for (const [key, value] of Object.entries(command.variables)) {
            if (value === null) root.style.removeProperty(key);
            else root.style.setProperty(key, value);
          }
          const surface =
            document.querySelector("main[data-slot='sidebar-inset']") ??
            document.querySelector("[data-slot='sidebar-inner']") ??
            document.body;
          value = {
            surface: getComputedStyle(surface).backgroundColor,
            body: getComputedStyle(document.body).backgroundColor,
          };
          break;
        }
        case "chrome": {
          document.documentElement.style.backgroundColor = command.color;
          document.body.style.backgroundColor = command.color;
          let metas = [...document.querySelectorAll('meta[name="theme-color"]')];
          if (!metas.length) {
            const meta = document.createElement("meta");
            meta.name = "theme-color";
            meta.setAttribute("data-dynamic-theme-color", "true");
            document.head.append(meta);
            metas = [meta];
          }
          for (const meta of metas) meta.setAttribute("content", command.color);
          break;
        }
        default:
          throw new Error("Unknown Rust theme ABI request");
      }
      dioxus.send({ type: "response", id: command.id, value });
    } catch (error) {
      dioxus.send({ type: "response", id: command.id, error: String(error) });
    }
  }
} catch (error) {
  if (!slot.disposed) dioxus.send({ type: "error", message: String(error) });
} finally {
  slot.dispose();
}
