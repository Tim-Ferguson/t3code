// Development-only oracle from original provider sign-in components; Node24.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const root = new URL("../../", import.meta.url);
const source = readFileSync(
  new URL("apps/web/src/components/settings/ProviderAuthenticationSection.tsx", root),
  "utf8",
);
const begin = source.indexOf("  const auth = query.data;");
const end = source.indexOf("  async function run(", begin);
if (begin < 0 || end < 0) throw Error("Missing original account policy boundary");
const resolve = new Function(
  "provider",
  "query",
  "readOnly",
  "pending",
  "methodId",
  "environmentLabel",
  "draft",
  stripTypeScriptTypes(source.slice(begin, end)) +
    `
  return {active,signedIn,isDiscovering,needsExternalSetup,accountDescription,statusMessage,disabled,draftId,url,
    callback:Boolean(url&&(interaction?.type==="browser"?interaction.acceptsCallback:!interaction)),
    methodPicker:!active&&(auth?.methods?.length??0)>1,
    selectedMethod:auth?.methods?.some(method=>method.id===methodId)?methodId:"",
    startDisabled:disabled||!provider.enabled||!provider.installed||!auth,
    startLabel:signedIn?"Change account":auth?.phase==="failed"||auth?.phase==="cancelled"?"Retry sign-in":"Sign in",
    canLogout:Boolean(!active&&signedIn&&(provider.auth.canLogout??provider.setup?.canAuthenticate))};`,
);
const maskSource = readFileSync(
  new URL("apps/web/src/components/settings/RedactedSensitiveText.tsx", root),
  "utf8",
);
const maskEnd = maskSource.indexOf("export function RedactedSensitiveText");
const maskBegin = maskSource.indexOf("const REDACTED_TEXT_ALPHABET");
if (maskEnd < 0 || maskBegin < 0) throw Error("Missing original redaction boundary");
const redact = new Function(
  stripTypeScriptTypes(
    maskSource.slice(maskBegin, maskEnd).replace("export function", "function"),
  ) + "\nreturn redactedPlaceholder;",
)();
const rows = [];
for (const driver of ["codex", "acpRegistry"])
  for (const status of ["authenticated", "unauthenticated", "unknown"])
    for (const phase of [
      undefined,
      "idle",
      "starting",
      "waiting",
      "verifying",
      "succeeded",
      "failed",
      "cancelled",
    ])
      for (const methods of [
        undefined,
        [],
        [
          { id: "agent", name: "Browser", type: "agent" },
          { id: "credentials", name: "Token", type: "credentials" },
        ],
      ])
        for (const gate of [
          {},
          { error: "Unavailable" },
          { readOnly: true },
          { pending: true },
          { canAuthenticate: false },
        ]) {
          const provider = {
            driver,
            enabled: true,
            installed: true,
            auth: { status },
            setup: { canAuthenticate: gate.canAuthenticate ?? true },
          };
          const auth =
            phase === undefined
              ? undefined
              : {
                  phase,
                  flowId: "flow",
                  authorizationUrl: null,
                  message: "Fixture failure",
                  ...(methods !== undefined ? { methods } : {}),
                };
          const input = {
            provider,
            auth: auth ?? null,
            queryError: gate.error ?? null,
            readOnly: gate.readOnly ?? false,
            pending: gate.pending ?? false,
            method: "agent",
            environment: "Fixture environment",
          };
          const raw = resolve(
            provider,
            { data: auth, error: input.queryError },
            input.readOnly,
            input.pending,
            input.method,
            input.environment,
            { id: "", values: {} },
          );
          const { isDiscovering, needsExternalSetup, accountDescription, ...rest } = raw;
          rows.push({
            kind: "account",
            ...input,
            expected: {
              ...rest,
              discovering: isDiscovering,
              needsExternalSetup,
              description: accountDescription,
              statusMessage: raw.statusMessage ?? null,
              url: raw.url ?? null,
            },
          });
        }
for (const interaction of [
  null,
  {
    type: "browser",
    id: "browser",
    url: "https://example.test",
    requiresConsent: true,
    acceptsCallback: true,
  },
  { type: "deviceCode", id: "code", url: "https://example.test", userCode: "CODE" },
  { type: "credentials", id: "credentials", fields: [] },
  { type: "terminal", id: "terminal", output: "$ " },
]) {
  const input = {
    provider: {
      driver: "acpRegistry",
      enabled: true,
      installed: true,
      auth: { status: "unknown" },
    },
    auth: {
      phase: "waiting",
      flowId: "new-flow",
      authorizationUrl: "https://example.test/fallback",
      interaction,
    },
    queryError: null,
    readOnly: false,
    pending: false,
    method: "",
    environment: "Remote",
  };
  const raw = resolve(
    input.provider,
    { data: input.auth, error: null },
    false,
    false,
    "",
    "Remote",
    { id: "", values: {} },
  );
  const { isDiscovering, needsExternalSetup, accountDescription, ...rest } = raw;
  rows.push({
    kind: "account",
    ...input,
    expected: {
      ...rest,
      discovering: isDiscovering,
      needsExternalSetup,
      description: accountDescription,
      statusMessage: raw.statusMessage ?? null,
      url: raw.url ?? null,
    },
  });
}
for (const value of [
  "",
  "a@example.test",
  "someone-long_name@host.test",
  "😀@🌟.test",
  "a\ufeffb\u0085c",
  "e\u0301@é.test",
  "İ@example.test",
  "𝒜𝒷_😀-foo.bar",
  "中文@测试.test",
])
  rows.push({ kind: "redact", value, expected: redact(value) });
for (const output of ["", "abc", "a😀b", "😀😀", "\ufeffhello", "汉字\r\n"])
  for (const written of [0, 1, 2, 4, 8, 100])
    for (const offset of [undefined, 0, 1, 2, 4, 8, 100]) {
      const latest = offset ?? output.length,
        delta = latest - written;
      const paint =
        delta > 0 && delta <= output.length
          ? { type: "append", data: output.slice(-delta) }
          : delta !== 0
            ? { type: "reset", data: output }
            : { type: "none" };
      // The original JS result may contain isolated UTF-16 surrogates. Keep these
      // witnesses explicitly outside Rust String JSON parity, never silently skip.
      const gap = paint.data !== undefined && !paint.data.isWellFormed();
      rows.push({
        kind: "paint",
        written,
        output,
        ...(offset !== undefined ? { offset } : {}),
        next: latest,
        expected: gap
          ? {
              type: paint.type,
              utf16: Array.from({ length: paint.data.length }, (_, index) =>
                paint.data.charCodeAt(index),
              ),
            }
          : paint,
        ...(gap ? { isolatedSurrogate: true } : {}),
      });
    }
writeFileSync(
  new URL("rust/crates/client/tests/fixtures/provider-auth.jsonl", root),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
process.stdout.write(
  `${rows.length} original provider auth witnesses; ${rows.filter((row) => row.isolatedSurrogate).length} separately tagged isolated-surrogate cases\n`,
);
