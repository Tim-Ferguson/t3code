// Runs the original helpers behind the provider management words and rules and records what they
// answer: how a version reads (`getProviderVersionLabel`), what the version popover says and offers
// (`getProviderVersionAdvisoryPresentation`), which providers offer a one-click update
// (`isProviderSettingsUpdateCandidate`, `isProviderUpdateActive`), and how a new instance's id is
// derived and checked. The Rust tests in t3-client replay the rows and require identical output.
// `providerStatus.ts` only imports types, so node runs it as it stands; the other two modules need
// React or the contracts, so their functions are cut out of the source and run as they stand.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const read = (path) => readFileSync(join(root, path), "utf8");
const cut = (source, name) => {
  const start = source.search(new RegExp(`(export )?function ${name}\\b`));
  if (start < 0) throw new Error(`no ${name}`);
  const next = source.indexOf("\n}\n", start);
  return source.slice(start, next + 3).replace(/^export /, "");
};
const directory = mkdtempSync(join(tmpdir(), "provider-manage-"));
const status = await import(
  pathToFileURL(join(root, "apps/web/src/components/settings/providerStatus.ts"))
);

const launch = read("apps/web/src/components/ProviderUpdateLaunchNotification.logic.ts");
writeFileSync(
  join(directory, "candidate.ts"),
  `${cut(launch, "isProviderUpdateActive")}\n${cut(launch, "isProviderSettingsUpdateCandidate")}\nexport { isProviderUpdateActive, isProviderSettingsUpdateCandidate };\n`,
);
const candidate = await import(pathToFileURL(join(directory, "candidate.ts")));

const dialog = read("apps/web/src/components/settings/AddProviderInstanceDialog.tsx");
const logic = read("apps/web/src/components/settings/AddProviderInstanceDialog.logic.ts");
writeFileSync(
  join(directory, "identity.ts"),
  `const INSTANCE_ID_PATTERN = /^[a-zA-Z][a-zA-Z0-9_-]*$/;\n${cut(dialog, "slugifyLabel")}\n${cut(dialog, "deriveInstanceId")}\n${cut(dialog, "validateInstanceId")}\n${cut(logic, "deriveAvailableInstanceId")}\nexport { deriveInstanceId, validateInstanceId, deriveAvailableInstanceId };\n`,
);
const identity = await import(pathToFileURL(join(directory, "identity.ts")));

const rows = [];
for (const version of [
  null,
  undefined,
  "",
  "1.2.3",
  "v1.2.3",
  "0.156.1",
  "nightly",
  "agy_acp_server_20260818_01_RC01",
  "agy_acp_server_20260818_01",
  "agy_acp_server_2026081_01",
  "agy_acp_server_20260818_xx_RC01",
  "agy_acp_server_20260818_01_RC-01",
]) {
  rows.push({
    type: "label",
    version: version ?? null,
    label: status.getProviderVersionLabel(version),
  });
}

const versions = [
  undefined,
  {
    status: "current",
    currentVersion: "1.0.0",
    latestVersion: "1.0.0",
    updateCommand: null,
    canUpdate: false,
    checkedAt: null,
    message: null,
  },
  {
    status: "unknown",
    currentVersion: "1.0.0",
    latestVersion: null,
    updateCommand: null,
    canUpdate: false,
    checkedAt: null,
    message: null,
  },
  {
    status: "behind_latest",
    currentVersion: "1.0.0",
    latestVersion: "1.2.0",
    updateCommand: "npm install -g @openai/codex@latest",
    canUpdate: true,
    checkedAt: null,
    message: null,
  },
  {
    status: "behind_latest",
    currentVersion: "1.0.0",
    latestVersion: "1.2.0",
    updateCommand: "npm install -g @openai/codex@latest",
    canUpdate: false,
    canInstallVersion: true,
    checkedAt: null,
    message: "Codex 1.2.0 is available.",
  },
  {
    status: "behind_latest",
    currentVersion: "1.0.0",
    latestVersion: null,
    updateCommand: null,
    canUpdate: false,
    checkedAt: null,
    message: null,
  },
];
const compatibilities = [
  undefined,
  { status: "supported" },
  { status: "unknown" },
  { status: "graceful", message: "Some features are limited." },
  { status: "graceful", recommendedVersion: "1.1.0" },
  { status: "unsupported", recommendedRange: ">=1.1 <2" },
  {
    status: "unsupported",
    message: "Too old.",
    recommendedVersion: "1.1.0",
    latestVersionStatus: "supported",
  },
  { status: "broken" },
  { status: "broken", latestVersionStatus: "broken", message: "Broken." },
  { status: "supported", latestVersionStatus: "unsupported" },
];
for (const versionAdvisory of versions) {
  for (const compatibilityAdvisory of compatibilities) {
    for (const enabled of [true, false]) {
      const provider = {
        enabled,
        ...(versionAdvisory ? { versionAdvisory } : {}),
        ...(compatibilityAdvisory ? { compatibilityAdvisory } : {}),
      };
      const advisory = status.getProviderVersionAdvisoryPresentation(
        versionAdvisory,
        compatibilityAdvisory,
        enabled,
      );
      rows.push({
        type: "advisory",
        provider,
        advisory,
        candidate: candidate.isProviderSettingsUpdateCandidate(provider),
      });
    }
  }
}
for (const state of [undefined, "queued", "running", "succeeded", "failed", "unchanged"]) {
  const provider = state ? { updateState: { status: state, message: null, output: null } } : {};
  rows.push({ type: "active", provider, active: candidate.isProviderUpdateActive(provider) });
}

const existing = new Set(["codex_work", "codex_work_2", "claudeAgent_a"]);
for (const [driver, label] of [
  ["codex", "Work"],
  ["codex", " Work "],
  ["codex", "My  Work!! Laptop"],
  ["codex", "---"],
  ["codex", ""],
  ["codex", "A".repeat(80)],
  ["claudeAgent", "a"],
  ["cursor", "ÄÖ résumé 9"],
  ["opencode", "x".repeat(47) + "-y"],
]) {
  const id = identity.deriveInstanceId(driver, label);
  rows.push({
    type: "derive",
    driver,
    label,
    id,
    available: identity.deriveAvailableInstanceId(
      (text) => identity.deriveInstanceId(driver, text),
      label,
      existing,
    ),
  });
}
for (const id of [
  "",
  "codex_work",
  "fresh",
  "9lives",
  "a b",
  "a-b_c",
  "a".repeat(64),
  "a".repeat(65),
  "Ünï",
  "codex_work_2",
  "-x",
  "_x",
]) {
  rows.push({ type: "validate", id, error: identity.validateInstanceId(id, existing) });
}

writeFileSync(
  join(root, "rust/crates/client/tests/fixtures/provider_manage.json"),
  `${JSON.stringify(rows, null, 1)}\n`,
);
console.log(`${rows.length} rows`);
