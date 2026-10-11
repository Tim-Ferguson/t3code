#!/usr/bin/env node
// Writes `rust/crates/client/src/settings_search_items.json` from the original's settings search catalog
// (`apps/web/src/components/settings/settingsSearch.ts`), so the Rust port searches the same entries with the
// same titles, terms and gates. Run it again when the original's catalog changes.
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const source = readFileSync(
  join(root, "apps/web/src/components/settings/settingsSearch.ts"),
  "utf8",
);
const start = source.indexOf("export const SETTINGS_SEARCH_ITEMS = [");
const end = source.indexOf("] as const satisfies ReadonlyArray<SettingsSearchItem>;");
if (start < 0 || end < 0) throw new Error("settings search catalog not found");
const literal = source.slice(start + "export const SETTINGS_SEARCH_ITEMS = ".length, end + 1);
// The keybinding entries are spread in from the command list; the Rust client builds those from its own.
const items = new Function(`return ${literal.replace("...KEYBINDING_SEARCH_ITEMS,", "")};`)();
const out = items.map((item) => ({
  id: item.id,
  title: item.title,
  to: item.to.replace("/settings/", ""),
  ...(item.targetId ? { targetId: item.targetId } : {}),
  ...(item.searchTerms ? { searchTerms: item.searchTerms } : {}),
  ...(item.scope ? { scope: item.scope } : {}),
  ...Object.fromEntries(
    [
      "desktopOnly",
      "macOnly",
      "windowsOnly",
      "cloudOnly",
      "environmentOnly",
      "providerSettingsOnly",
      "macProviderSettingsOnly",
      "localBackendManagementOnly",
      "localEnvironmentOnly",
      "wslAvailableOnly",
      "managedTunnelOnly",
      "secondary",
      "requiresThreadAutoSettlement",
    ]
      .filter((flag) => item[flag] === true)
      .map((flag) => [flag, true]),
  ),
}));
writeFileSync(
  join(root, "rust/crates/client/src/settings_search_items.json"),
  `${JSON.stringify(out, null, 2)}\n`,
);
console.log(`${out.length} entries`);
