// Development-only oracle for the T3 Connect / remote-access port (`rust/crates/remote`).
// Runs the unchanged original pure functions and writes one JSON object per line to
// rust/crates/remote/tests/fixtures/remote.txt (an ignored extension so the pre-commit hook
// leaves the bytes alone). Everything here is local: no network, no real credentials; the keys are
// derived from fixed seeds.
import { createHash, createPrivateKey, createPublicKey } from "node:crypto";
import { mkdirSync, writeFileSync, renameSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = fileURLToPath(new URL("../../", import.meta.url));
const at = (path) => pathToFileURL(root + path);
const Effect = await import(at("packages/contracts/node_modules/effect/dist/Effect.js"));
const ConfigProvider = await import(
  at("packages/contracts/node_modules/effect/dist/ConfigProvider.js")
);
const { HttpClientResponse, HttpClientRequest } = await import(
  at("packages/contracts/node_modules/effect/dist/http/index.js")
);
const { SignJWT, importPKCS8 } = await import(
  at("packages/shared/node_modules/jose/dist/webapi/index.js")
);

const relayJwt = await import(at("packages/shared/src/relayJwt.ts"));
const relayUrl = await import(at("packages/shared/src/relayUrl.ts"));
const relayAuth = await import(at("packages/shared/src/relayAuth.ts"));
const connectAuth = await import(at("packages/shared/src/connectAuth.ts"));
const linkChecks = await import(at("apps/server/src/cloud/linkChecks.ts"));
const relayResponse = await import(at("apps/server/src/cloud/relayResponse.ts"));
const startup = await import(at("apps/server/src/cloud/managedTunnelStartup.ts"));
const endpointRuntime = await import(at("apps/server/src/cloud/ManagedEndpointRuntime.ts"));
const publicConfig = await import(at("apps/server/src/cloud/publicConfig.ts"));
const bootService = await import(at("apps/server/src/cloud/bootService.ts"));
const tailscale = await import(at("packages/tailscale/src/tailscale.ts"));
const sshCommand = await import(at("packages/ssh/src/command.ts"));
const sshConfig = await import(at("packages/ssh/src/config.ts"));

const rows = [];
const add = (kind, row) => rows.push({ kind, ...row });
const run = (effect) => Effect.runPromise(effect);
const settle = async (effect) => {
  const result = await Effect.runPromise(Effect.result(effect));
  return result._tag === "Success"
    ? { ok: true, value: result.success }
    : { ok: false, error: result.failure };
};

// ---- keys from fixed seeds ----------------------------------------------------------------
const PKCS8_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");
const seedOf = (n) => Buffer.alloc(32, n);
const keysFor = (seed) => {
  const privateKey = createPrivateKey({
    key: Buffer.concat([PKCS8_PREFIX, seed]),
    format: "der",
    type: "pkcs8",
  });
  const publicKey = createPublicKey(privateKey);
  return {
    seedHex: seed.toString("hex"),
    privateKey: privateKey.export({ format: "pem", type: "pkcs8" }),
    publicKey: publicKey.export({ format: "pem", type: "spki" }),
  };
};
const A = keysFor(seedOf(7));
const B = keysFor(seedOf(9));
add("keys", { a: A, b: B });

// ---- sign ----------------------------------------------------------------------------------
const signCases = [
  {
    typ: "t3-env-link+jwt",
    payload: {
      iss: "t3-env:env_1",
      aud: "https://relay.example.test",
      sub: "env_1",
      jti: "j-1",
      iat: 1700000000,
      exp: 1700000300,
      scopes: ["a", "b"],
      nested: { z: 1, a: [1, 2, { k: 'é "/\\' }] },
    },
  },
  {
    typ: "t3-env-activity+jwt",
    payload: { state: null, n: 0, big: 1700000000123, f: 1.5, s: "ünïcödé 🚀", t: true },
  },
  { typ: "x", payload: {} },
];
for (const c of signCases) {
  const token = await run(
    relayJwt.signRelayJwt({ privateKey: A.privateKey, typ: c.typ, payload: c.payload }),
  );
  add("sign", { privateKey: A.privateKey, typ: c.typ, payload: c.payload, token });
}
const badSign = await settle(
  relayJwt.signRelayJwt({ privateKey: "not-a-private-key", typ: "t", payload: {} }),
);
add("sign_error", { privateKey: "not-a-private-key", ok: badSign.ok });
// A fixed P-256 key (not Ed25519), so regenerating the fixture is reproducible.
const notEd = createPrivateKey({
  key: Buffer.from(
    "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgKEE7HjEtnHOIDmdnqwBc7/32MuMG+hCKJnK4bmnvBLChRANCAAR5N2qmFMzO0aV7xFbmnEdSk6AuKJL99U+4RBf9OVHlow3wVpf7qVceFLS+NEPiYZjwmMDujSiq/fEoYxQCxFzz",
    "base64",
  ),
  format: "der",
  type: "pkcs8",
}).export({ format: "pem", type: "pkcs8" });
const ecSign = await settle(relayJwt.signRelayJwt({ privateKey: notEd, typ: "t", payload: {} }));
add("sign_error", { privateKey: notEd, ok: ecSign.ok });

// ---- verify --------------------------------------------------------------------------------
const NOW = 1700000000;
const craft = async (keys, header, payload) => {
  const key = await importPKCS8(keys.privateKey, "EdDSA");
  return new SignJWT(payload).setProtectedHeader(header).sign(key);
};
const base = {
  iss: "https://relay.example.test",
  aud: "t3-env:env_1",
  sub: "user",
  jti: "j",
  iat: NOW,
  exp: NOW + 300,
};
const EdTyp = { alg: "EdDSA", typ: "t3-cloud-mint+jwt" };
const verifyInputs = [
  ["valid", A, EdTyp, base, {}],
  ["audience array", A, EdTyp, { ...base, aud: ["x", "t3-env:env_1"] }, {}],
  ["wrong typ", A, { alg: "EdDSA", typ: "t3-cloud-health+jwt" }, base, {}],
  [
    "typ case and application prefix",
    A,
    { alg: "EdDSA", typ: "Application/T3-Cloud-Mint+JWT" },
    base,
    {},
  ],
  ["no typ header", A, { alg: "EdDSA" }, base, {}],
  ["wrong issuer", A, EdTyp, { ...base, iss: "https://evil.example.test" }, {}],
  ["issuer with trailing slash", A, EdTyp, { ...base, iss: "https://relay.example.test/" }, {}],
  ["wrong audience", A, EdTyp, { ...base, aud: "t3-env:other" }, {}],
  ["missing audience", A, EdTyp, (({ aud, ...rest }) => rest)(base), {}],
  ["missing iat", A, EdTyp, (({ iat, ...rest }) => rest)(base), {}],
  ["no exp", A, EdTyp, (({ exp, ...rest }) => rest)(base), {}],
  ["exp just inside tolerance", A, EdTyp, { ...base, exp: NOW - 59 }, { nowOffset: 0 }],
  ["exp at tolerance edge", A, EdTyp, { ...base, exp: NOW - 60 }, {}],
  ["exp past tolerance", A, EdTyp, { ...base, exp: NOW - 61 }, {}],
  ["exp not a number", A, EdTyp, { ...base, exp: "soon" }, {}],
  ["iat too old", A, EdTyp, { ...base, iat: NOW - 361, exp: NOW + 300 }, {}],
  ["iat at max age plus tolerance", A, EdTyp, { ...base, iat: NOW - 360 }, {}],
  ["iat in the near future", A, EdTyp, { ...base, iat: NOW + 59 }, {}],
  ["iat in the far future", A, EdTyp, { ...base, iat: NOW + 61 }, {}],
  ["nbf in the future", A, EdTyp, { ...base, nbf: NOW + 61 }, {}],
  ["nbf within tolerance", A, EdTyp, { ...base, nbf: NOW + 60 }, {}],
  ["long max age ok", A, EdTyp, { ...base, iat: NOW - 20 * 3600 }, { maxTokenAge: 25 * 3600 }],
  [
    "long max age exceeded",
    A,
    EdTyp,
    { ...base, iat: NOW - 26 * 3600 },
    { maxTokenAge: 25 * 3600 },
  ],
  ["signed with another key", B, EdTyp, base, {}],
];
for (const [name, keys, header, payload, extra] of verifyInputs) {
  const token = await craft(keys, header, payload);
  const input = {
    publicKey: A.publicKey,
    token,
    typ: "t3-cloud-mint+jwt",
    issuer: "https://relay.example.test",
    audience: "t3-env:env_1",
    nowEpochSeconds: NOW,
    ...(extra.maxTokenAge === undefined ? {} : { maxTokenAge: extra.maxTokenAge }),
  };
  const result = await settle(relayJwt.verifyRelayJwt(input));
  add("verify", {
    name,
    ...input,
    ok: result.ok,
    claims: result.ok ? result.value : null,
    code: result.ok ? null : relayJwt.RelayJwtError.diagnosticCode(result.error),
  });
}
// Tampered and malformed tokens
const good = await craft(A, EdTyp, base);
const [h, p, s] = good.split(".");
const tamperedPayload = Buffer.from(JSON.stringify({ ...base, sub: "mallory" })).toString(
  "base64url",
);
const noneHeader = Buffer.from(JSON.stringify({ alg: "none", typ: "t3-cloud-mint+jwt" })).toString(
  "base64url",
);
const hsHeader = Buffer.from(JSON.stringify({ alg: "HS256", typ: "t3-cloud-mint+jwt" })).toString(
  "base64url",
);
for (const [name, token] of [
  ["tampered payload", `${h}.${tamperedPayload}.${s}`],
  ["alg none", `${noneHeader}.${p}.`],
  ["alg HS256", `${hsHeader}.${p}.${s}`],
  ["two parts", `${h}.${p}`],
  ["garbage", "not-a-jwt"],
  ["empty", ""],
  ["bad signature encoding", `${h}.${p}.!!!`],
]) {
  const input = {
    publicKey: A.publicKey,
    token,
    typ: "t3-cloud-mint+jwt",
    issuer: "https://relay.example.test",
    audience: "t3-env:env_1",
    nowEpochSeconds: NOW,
  };
  const result = await settle(relayJwt.verifyRelayJwt(input));
  add("verify", {
    name,
    ...input,
    ok: result.ok,
    claims: result.ok ? result.value : null,
    code: result.ok ? null : relayJwt.RelayJwtError.diagnosticCode(result.error),
  });
}
{
  const input = {
    publicKey: notEd.replace("PRIVATE", "PUBLIC"),
    token: good,
    typ: "t3-cloud-mint+jwt",
    issuer: "https://relay.example.test",
    audience: "t3-env:env_1",
    nowEpochSeconds: NOW,
  };
  const result = await settle(relayJwt.verifyRelayJwt(input));
  add("verify", {
    name: "public key is not a key",
    ...input,
    ok: result.ok,
    claims: null,
    code: result.ok ? null : relayJwt.RelayJwtError.diagnosticCode(result.error),
  });
}

// ---- relay url -----------------------------------------------------------------------------
for (const value of [
  " https://relay.example.test/// ",
  "https://relay.example.test:8443/",
  "https://Relay.Example.Test",
  "https://relay.example.test:443",
  "http://relay.example.test",
  "https://user:password@relay.example.test",
  "https://relay.example.test/path",
  "https://relay.example.test?query=value",
  "https://relay.example.test?",
  "https://relay.example.test#fragment",
  "https://relay.example.test#",
  "https://[::1]:8443",
  "not a url",
  "",
]) {
  add("relay_url", {
    value,
    normalized: relayUrl.normalizeSecureRelayUrl(value),
    secure: relayUrl.isSecureRelayUrl(value),
  });
}
for (const value of [" https://relay.example.test/// ", "https://relay.example.test", "", "///"]) {
  add("issuer", { value, normalized: relayJwt.normalizeRelayIssuer(value) });
}

// ---- link checks ---------------------------------------------------------------------------
for (const input of [
  "http://127.0.0.1:80",
  "https://127.0.0.1:443",
  "http://127.0.0.1:3773",
  "http://localhost:3773",
  "http://[::1]:3773",
  "https://example.com",
  "http://127.0.0.1",
  "HTTP://127.0.0.1:3773",
  " http://127.0.0.1:3773",
  "http://127.0.0.1:3773 ",
  "ftp://127.0.0.1:3773",
  "http://user:password@127.0.0.1:3773",
  "http://127.0.0.1:3773/api",
  "http://127.0.0.1:3773/",
  "http://127.0.0.1:3773?mode=test",
  "http://127.0.0.1:3773?",
  "http://127.0.0.1:3773#fragment",
  "http://127.0.0.1:3773#",
  "http://127.0.0.1:99999",
  "not a url",
  "",
  "http://127.0.0.1:0",
  "http://127.0.0.1:443",
  "https://127.0.0.1:80",
]) {
  let result;
  try {
    result = { ok: true, value: linkChecks.parseManagedEndpointLocalOrigin(input) };
  } catch (e) {
    result = { ok: false, message: e.message };
  }
  add("parse_local_origin", { input, ...result });
}
for (const [host, port, requestUrl] of [
  ["127.0.0.1", 3773, "http://127.0.0.1:3773/api/connect/link-proof"],
  ["127.0.0.1", 3773, "http://localhost:3773/x"],
  ["localhost", 3773, "http://127.0.0.1:3773/x"],
  ["LOCALHOST", 3773, "http://127.0.0.1:3773/x"],
  ["[::1]", 3773, "http://[::1]:3773/x"],
  ["::1", 3773, "http://[::1]:3773/x"],
  ["127.0.0.1", 3774, "http://127.0.0.1:3773/x"],
  ["127.0.0.1", 3773, "http://192.168.1.4:3773/x"],
  ["192.168.1.4", 3773, "http://192.168.1.4:3773/x"],
  ["127.0.0.1", 80, "http://127.0.0.1/x"],
  ["127.0.0.1", 443, "https://127.0.0.1/x"],
  ["127.0.0.1", 443, "http://127.0.0.1:443/x"],
  ["127.0.0.2", 3773, "http://127.0.0.2:3773/x"],
]) {
  add("allowed_origin", {
    host,
    port,
    requestUrl,
    allowed: linkChecks.isAllowedEndpointOrigin({
      origin: { localHttpHost: host, localHttpPort: port },
      requestUrl,
    }),
  });
}
for (const [iat, exp, nowSeconds] of [
  [1000, 1300, 1000],
  [1000, 1301, 1000],
  [1000, 1000, 1000],
  [1000, 900, 1000],
  [1060, 1100, 1000],
  [1061, 1100, 1000],
  [900, 1000, 1000],
  [0, 300, 1_000_000],
]) {
  add("proof_lifetime", {
    iat,
    exp,
    nowSeconds,
    ok: linkChecks.hasBoundedCloudProofLifetime({ iat, exp, nowSeconds }),
  });
}
for (const req of [
  { originalUrl: "/api/connect/link-proof", headers: { host: "127.0.0.1:3773" } },
  { originalUrl: "/api/connect/link-proof", headers: { host: "127.0.0.1:3773, other" } },
  { originalUrl: "/api/connect/link-proof", headers: {} },
  { originalUrl: "/a?b=c#d", headers: { host: "localhost:1" } },
  { originalUrl: "http://127.0.0.1:3773/x", headers: { host: "other:1" } },
  { originalUrl: "//evil.example/x", headers: { host: "127.0.0.1:3773" } },
  { originalUrl: "/x", headers: { host: " " } },
  { originalUrl: "/x", headers: { host: "bad host" } },
  { originalUrl: "/x", headers: { host: "127.0.0.1:3773", "x-forwarded-host": "evil.example" } },
  { originalUrl: "/x", headers: { host: "127.0.0.1:3773", "x-forwarded-host": " , x" } },
  { originalUrl: "/x", headers: { host: "127.0.0.1:3773", "x-forwarded-proto": "https" } },
  { originalUrl: "/x", headers: { host: "127.0.0.1:3773", "x-forwarded-proto": " " } },
]) {
  add("request_url", {
    request: req,
    absolute: linkChecks.requestAbsoluteUrl(req),
    forwarded: linkChecks.hasForwardedAuthorityHeaders(req),
  });
}

// ---- relay responses -----------------------------------------------------------------------
const respond = (status, body, headers) =>
  HttpClientResponse.fromWeb(
    HttpClientRequest.post("https://relay.example.test/v1/client/environment-links"),
    typeof body === "string"
      ? new Response(body, { status, ...(headers ? { headers } : {}) })
      : Response.json(body, { status, ...(headers ? { headers } : {}) }),
  );
const tagged = (tag, code, extra = {}) => ({ _tag: tag, code, traceId: "trace-1", ...extra });
const relayCases = [
  [
    403,
    tagged("RelayEnvironmentLinkLimitExceededError", "environment_link_limit_exceeded", {
      maxTunnels: 3,
    }),
  ],
  [
    403,
    tagged("RelayEnvironmentLinkLimitExceededError", "environment_link_limit_exceeded", {
      maxTunnels: 2.5,
    }),
  ],
  [401, tagged("RelayAuthInvalidError", "auth_invalid", { reason: "invalid_bearer" })],
  [
    401,
    tagged("RelayAuthInvalidError", "auth_invalid", {
      reason: "invalid_dpop",
      dpopFailureReason: "replay",
    }),
  ],
  [401, tagged("RelayAuthInvalidError", "auth_invalid", { reason: "bogus" })],
  [
    401,
    tagged("RelayAuthInvalidError", "auth_invalid", {
      reason: "invalid_dpop",
      dpopFailureReason: "nope",
    }),
  ],
  [401, tagged("RelayEnvironmentLinkProofExpiredError", "environment_link_proof_expired")],
  [
    400,
    tagged("RelayEnvironmentLinkProofInvalidError", "environment_link_proof_invalid", {
      reason: "replayed_nonce",
    }),
  ],
  [403, tagged("RelayEnvironmentConnectNotAuthorizedError", "environment_connect_not_authorized")],
  [
    403,
    tagged("RelayEnvironmentConnectNotAuthorizedError", "environment_connect_not_authorized", {
      reason: "environment_link_not_found",
    }),
  ],
  [
    502,
    tagged("RelayEnvironmentEndpointUnavailableError", "environment_endpoint_unavailable", {
      reason: "endpoint_request_failed",
    }),
  ],
  [504, tagged("RelayEnvironmentEndpointTimedOutError", "environment_endpoint_timed_out")],
  [
    500,
    tagged("RelayEnvironmentLinkFailedError", "environment_link_failed", {
      reason: "internal_error",
    }),
  ],
  [
    503,
    tagged("RelayEnvironmentLinkUnavailableError", "environment_link_unavailable", {
      reason: "managed_endpoint_provisioning_failed",
    }),
  ],
  [
    401,
    tagged("RelayAgentActivityPublishProofExpiredError", "agent_activity_publish_proof_expired"),
  ],
  [
    401,
    tagged("RelayAgentActivityPublishProofInvalidError", "agent_activity_publish_proof_invalid", {
      reason: "replayed_nonce",
    }),
  ],
  [500, tagged("RelayInternalError", "internal_error", { reason: "upstream_unavailable" })],
  [500, { _tag: "RelayInternalError", code: "internal_error", reason: "upstream_unavailable" }],
  [
    500,
    { _tag: "RelayInternalError", code: "wrong", reason: "upstream_unavailable", traceId: "t" },
  ],
  [500, { _tag: "Other", code: "x", traceId: "t" }],
  [
    403,
    "<html>private upstream details</html>",
    { "content-type": "text/html", "cf-ray": "abcdef1234-IAD" },
  ],
  [403, "<html>x</html>", { "content-type": "text/html", "cf-ray": "bad ray!" }],
  [404, "missing"],
  [400, "bad"],
  [408, "slow"],
  [409, "conflict"],
  [429, "limit"],
  [500, "boom"],
  [502, "bad gateway"],
  [503, "unavailable"],
  [504, "timeout"],
  [200, '{"ok":true}'],
  [201, "{}"],
  [299, "x"],
  [301, "moved"],
];
for (const [status, body, headers] of relayCases) {
  const result = await settle(
    relayResponse
      .filterRelayResponse(respond(status, body, headers))
      .pipe(Effect.mapError(relayResponse.relayRequestError)),
  );
  add("relay_response", {
    status,
    body: typeof body === "string" ? body : JSON.stringify(body),
    cfRay: headers?.["cf-ray"] ?? null,
    ok: result.ok,
    rejection: result.ok ? null : result.error.rejection,
    description: result.ok ? null : result.error.description,
    retry: result.ok ? null : relayResponse.shouldRetryRelayRequest(result.error),
  });
}
add("relay_error_text", {
  unexpected: relayResponse.relayRequestError(new Error("timed out")).description,
});

// ---- startup / connector output -------------------------------------------------------------
const cfg = {
  providerKind: "cloudflare_tunnel",
  connectorToken: "connector-token",
  tunnelId: "tunnel-1",
};
for (const wantsCliLink of [true, false]) {
  for (const registration of [
    { status: "not_linked" },
    { status: "ready" },
    { status: "unavailable" },
    { status: "superseded" },
    { status: "recovery_required", config: cfg },
  ]) {
    add("startup_action", {
      wantsCliLink,
      registration,
      action: startup.managedTunnelStartupAction({ wantsCliLink, registration }),
    });
  }
}
add("startup_constants", {
  recoveryCooldownMs: 120_000,
  firstRegistrationJitterMs: 30_000,
});
for (const line of [
  "2026-01-01T00:00:00Z INF Registered tunnel connection connIndex=0",
  "INF registered tunnel connection",
  "ERR Failed to serve quic connection",
  "WRN retrying",
  "FTL fatal",
  "PNC panic",
  "INFO nothing",
  "ERROR not a token",
  "xERRx",
  "[ERR]",
  "ERR",
  "connection ERR_x",
  "just text",
  "",
  'ERR Register tunnel error from server side error="Unauthorized: Invalid tunnel secret" connIndex=0',
  'ERR Register tunnel error from server side error="Failed to get tunnel"',
  'ERR Register tunnel error from server side error="Tunnel not found"',
  'ERR Register tunnel error from server side error="Record for tunnel not found"',
  'ERR Register tunnel error from server side error="Invalid tunnel secret"',
  'ERR register tunnel error from server side ERROR="unauthorized: x"',
  'ERR Register tunnel error from server side error="Some other failure"',
  'ERR Serve tunnel error error="Unauthorized: x"',
  'ERR Failed to dial a quic connection error="timeout: no recent network activity"',
]) {
  add("connector_output", {
    line,
    class: endpointRuntime.classifyRelayClientOutput(line),
    rejected: endpointRuntime.isRejectedRelayClientTunnelOutput(line),
  });
}
for (const status of [
  { status: "failed", failure: "not-installed" },
  { status: "failed", failure: "spawn-failed" },
  { status: "failed", failure: "unsupported-platform" },
  { status: "unsupported", providerKind: "x" },
  { status: "running" },
  { status: "disabled" },
  null,
  "failed",
  {},
]) {
  add("retryable_status", {
    status,
    retryable: endpointRuntime.isRetryableManagedEndpointRuntimeStatus(status),
  });
}

// ---- connect auth ---------------------------------------------------------------------------
for (const [hostedAppUrl, state, challenge, loopbackPort] of [
  ["https://app.t3.codes", "s", "c", 34338],
  ["https://app.t3.codes/", "state+/=", "chal lenge&", 1],
  ["http://localhost:5733", "a b", "x#y", 65535],
]) {
  add("authorize_url", {
    hostedAppUrl,
    state,
    challenge,
    loopbackPort,
    url: connectAuth.buildConnectAuthorizeRequestUrl({
      hostedAppUrl,
      state,
      challenge,
      loopbackPort,
    }),
  });
}
add("redirect_uri", { port: 34338, uri: connectAuth.connectLoopbackRedirectUri(34338) });
add("clerk_authorize_url", {
  authorizationEndpoint: "https://clerk.example.test/oauth/authorize",
  clientId: "client id",
  redirectUri: "http://127.0.0.1:34338/callback",
  scopes: connectAuth.CONNECT_OAUTH_SCOPES,
  state: "st+ate",
  challenge: "chal",
  url: connectAuth.buildConnectClerkAuthorizeUrl({
    authorizationEndpoint: "https://clerk.example.test/oauth/authorize",
    clientId: "client id",
    redirectUri: "http://127.0.0.1:34338/callback",
    scopes: connectAuth.CONNECT_OAUTH_SCOPES,
    state: "st+ate",
    challenge: "chal",
  }),
});
for (const port of ["1", "65535", "0", "65536", "123456", "12a", "", "034338", "00001", "99999"]) {
  const url = new URL(`https://app.t3.codes/connect#state=s&challenge=c&port=${port}`);
  const parsed = connectAuth.readConnectAuthorizeRequest(url);
  add("authorize_request_port", { port, parsed });
}
for (const key of [
  "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA==",
  "pk_live_Y2xlcmsuZXhhbXBsZS50ZXN0JA",
  "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0",
  "pk_test_not-base64!!",
  "pk_live_ab!d",
  "pk_test_",
  "pk_test_JA==",
  "sk_x_Y2xlcmsuZXhhbXBsZS50ZXN0JA==",
  "pk_test_Y2xl cmsu ZXhh bXBs ZS50 ZXN0 JA==",
  "pk_test_YS9i",
  "pk_test_YQ$",
  "nonsense",
  "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA===",
  "pk_test_Y",
  "pk_test_eA==",
  "pk_test_Zm9v_YmFy",
]) {
  let result;
  try {
    result = {
      ok: true,
      url: relayAuth.clerkFrontendApiUrlFromPublishableKey(key),
      hostname: relayAuth.clerkFrontendApiHostnameFromPublishableKey(key),
    };
  } catch (e) {
    result = { ok: false, tag: e._tag, reason: e.reason ?? null, keyPrefix: e.keyPrefix };
  }
  add("clerk_key", { key, ...result });
}

// ---- public config (through the original Config schemas) ------------------------------------
const withEnv = (effect, env) =>
  effect.pipe(Effect.provide(ConfigProvider.layer(ConfigProvider.fromEnv({ env }))));
for (const [fallback, env] of [
  ["https://embedded.example.test///", {}],
  ["https://embedded.example.test", { T3CODE_RELAY_URL: "https://runtime.example.test///" }],
  ["", {}],
  ["https://embedded.example.test", { T3CODE_RELAY_URL: "http://runtime.example.test" }],
  ["https://embedded.example.test/path", {}],
  ["https://embedded.example.test", { T3CODE_RELAY_URL: "  " }],
  ["https://embedded.example.test", { T3CODE_RELAY_URL: "" }],
  ["", { T3CODE_RELAY_URL: "" }],
]) {
  const r = await settle(withEnv(publicConfig.makeRelayUrlConfig(fallback), env));
  add("relay_url_config", { fallback, env, ok: r.ok, value: r.ok ? r.value : null });
}
for (const env of [
  {},
  { T3CODE_HOSTED_APP_URL: "https://nightly.app.t3.codes" },
  { T3CODE_HOSTED_APP_URL: "http://localhost:5733" },
  { T3CODE_HOSTED_APP_URL: "http://127.0.0.1:5733" },
  { T3CODE_HOSTED_APP_URL: "app.t3.codes" },
  { T3CODE_HOSTED_APP_URL: "http://app.t3.codes" },
  { T3CODE_HOSTED_APP_URL: "https://app.t3.codes/nested" },
  { T3CODE_HOSTED_APP_URL: "https://app.t3.codes?alias=true" },
  { T3CODE_HOSTED_APP_URL: "https://app.t3.codes#x" },
  { T3CODE_HOSTED_APP_URL: "https://app.t3.codes:8443/" },
  { T3CODE_HOSTED_APP_URL: " https://app.t3.codes " },
  { T3CODE_HOSTED_APP_URL: "" },
  { T3CODE_HOSTED_APP_URL: "   " },
]) {
  const r = await settle(withEnv(publicConfig.hostedAppUrlConfig, env));
  add("hosted_app_url", { env, ok: r.ok, value: r.ok ? r.value : null });
}
for (const [defaults, env] of [
  [
    {
      clerkPublishableKeyFallback: "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA==",
      clerkCliOAuthClientIdFallback: "oauth_client_embedded",
    },
    {},
  ],
  [
    {
      clerkPublishableKeyFallback: "pk_test_ZW1iZWRkZWQuZXhhbXBsZS50ZXN0JA==",
      clerkCliOAuthClientIdFallback: "oauth_client_embedded",
    },
    {
      T3CODE_CLERK_PUBLISHABLE_KEY: "pk_test_cnVudGltZS5leGFtcGxlLnRlc3Qk",
      T3CODE_CLERK_CLI_OAUTH_CLIENT_ID: "oauth_client_runtime",
    },
  ],
  [{ clerkPublishableKeyFallback: "", clerkCliOAuthClientIdFallback: "" }, {}],
  [
    {
      clerkPublishableKeyFallback: "pk_test_not-base64!!",
      clerkCliOAuthClientIdFallback: "oauth_client_embedded",
    },
    {},
  ],
  [
    {
      clerkPublishableKeyFallback: "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA==",
      clerkCliOAuthClientIdFallback: "",
    },
    {},
  ],
  [
    {
      clerkPublishableKeyFallback: "  pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA==  ",
      clerkCliOAuthClientIdFallback: " id ",
    },
    {},
  ],
  [
    {
      clerkPublishableKeyFallback: "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA==",
      clerkCliOAuthClientIdFallback: "id",
    },
    { T3CODE_CLERK_CLI_OAUTH_CLIENT_ID: "" },
  ],
  [
    {
      clerkPublishableKeyFallback: "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA==",
      clerkCliOAuthClientIdFallback: "id",
    },
    { T3CODE_CLERK_CLI_OAUTH_CLIENT_ID: "   " },
  ],
]) {
  const r = await settle(withEnv(publicConfig.makeCloudCliOAuthConfig(defaults), env));
  add("oauth_config", { defaults, env, ok: r.ok, value: r.ok ? r.value : null });
}

// ---- boot service renderers ------------------------------------------------------------------
for (const baseDir of [
  "/home/theo/.t3",
  "/home/theo/T3 Data/100%",
  "/home/theo/a&b",
  '/home/theo/"q"\\x',
  "/Users/theo/a&b<c>",
]) {
  const plan = {
    program: [`${baseDir}/runtime/versions/1.2.3/t3`, "__service-launcher"],
    baseDir,
    logPath: `${baseDir}/userdata/logs/boot-service.log`,
    unitPath: "/home/theo/.config/systemd/user/t3code.service",
  };
  const unit = bootService.renderBootServiceUnit(plan);
  const plist = bootService.renderBootServicePlist(plan, {
    homeDir: "/Users/theo",
    environmentPath: "/usr/bin:/opt/a&b",
  });
  add("boot_service", {
    plan,
    homeDir: "/Users/theo",
    environmentPath: "/usr/bin:/opt/a&b",
    unit,
    plist,
    unitBase: bootService.bootServiceBaseDirOf(unit) ?? null,
    plistBase: bootService.bootServiceBaseDirOf(plist) ?? null,
  });
}
for (const problem of [
  "user-manager-unavailable",
  "linger-unavailable",
  "linger-disabled",
  "service-disabled",
  "service-stopped",
  "restart-pending",
]) {
  add("boot_problem", { problem, text: bootService.formatBootServiceProblem(problem) });
}

// ---- tailscale --------------------------------------------------------------------------------
for (const address of [
  "100.64.0.1",
  "100.127.255.254",
  "100.128.0.1",
  "100.63.255.255",
  "192.168.1.44",
  "100.64.0.1abc",
  "100.64.0",
  "100.64.0.256",
  "100..1.1",
  "",
  "100.64.0.1.1",
  " 100.64.0.1",
  "100.-64.0.1",
  "100.0x40.0.1",
  "100.64.0.1e3",
]) {
  add("tailscale_ip", { address, tailnet: tailscale.isTailscaleIpv4Address(address) });
}
for (const raw of [
  '{"Self":{"DNSName":"desktop.tail.ts.net.","TailscaleIPs":["100.100.100.100","fd7a:115c:a1e0::1","192.168.1.1"]}}',
  "{}",
  '{"Self":{}}',
  '{"Self":{"DNSName":"  host.ts.net  "}}',
  '{"Self":{"DNSName":"."}}',
  '{"Self":{"DNSName":5,"TailscaleIPs":"x"}}',
  '{"Self":null}',
  "[]",
  "{not-json",
  "",
  '{"Self":{"TailscaleIPs":["100.64.0.1",5,null,"100.64.0.2"]}}',
]) {
  const status = await settle(tailscale.parseTailscaleStatus(raw));
  const dns = await settle(tailscale.parseTailscaleMagicDnsName(raw));
  add("tailscale_status", {
    raw,
    ok: status.ok,
    status: status.ok ? status.value : null,
    dnsOk: dns.ok,
    dns: dns.ok ? dns.value : null,
  });
}
for (const [magicDnsName, servePort] of [
  ["desktop.tail.ts.net", undefined],
  ["desktop.tail.ts.net", 443],
  ["desktop.tail.ts.net", 8443],
  ["Host.TS.NET", 8443],
]) {
  add("tailscale_url", {
    magicDnsName,
    servePort: servePort ?? null,
    url: tailscale.buildTailscaleHttpsBaseUrl({
      magicDnsName,
      ...(servePort === undefined ? {} : { servePort }),
    }),
  });
}

// ---- ssh --------------------------------------------------------------------------------------
for (const [alias, stdout] of [
  ["devbox", "hostname devbox.example.com\nuser julius\nport 2222\n"],
  ["a", ""],
  ["a", "hostname\nuser  \nport x\n"],
  ["a", "port 22\nport 2222\nhostname h1\nhostname h2\n"],
  ["a", "  Hostname   spaced   words \r\nUser u\r\n"],
  ["a", "port 22 extra"],
  ["a", "port 0x10\n"],
  ["a", "port 22.5\n"],
]) {
  add("ssh_resolve", { alias, stdout, target: sshCommand.parseSshResolveOutput(alias, stdout) });
}
for (const target of [
  { alias: "devbox", hostname: "devbox.example.com", username: "julius", port: 2222 },
  { alias: "fixture", hostname: "fixture", username: null, port: null },
  { alias: "é", hostname: "ü", username: "u", port: 22 },
]) {
  add("ssh_target", {
    target,
    key: sshCommand.targetConnectionKey(target),
    args: sshCommand.baseSshArgs(target, { batchMode: "no" }),
    argsDefault: sshCommand.baseSshArgs(target),
  });
}
for (const stdout of [
  'Welcome\n\n{"credential":"pairing-token"}\n',
  "",
  "\n\n",
  "a\nb  \n  \n",
  "single",
  "x\r\ny\r\n",
]) {
  add("ssh_last_line", { stdout, line: sshCommand.getLastNonEmptyOutputLine(stdout) });
}
for (const raw of [
  [
    "github.com ssh-ed25519 AAAA",
    "gitlab.com,gitlab-alias ssh-ed25519 BBBB",
    "|1|hashed|entry ssh-ed25519 CCCC",
    "@cert-authority *.example.com ssh-ed25519 DDDD",
    "[ssh.example.com]:2200 ssh-ed25519 EEEE",
    "port.example.com:22 ssh-ed25519 HHHH",
    "::1 ssh-ed25519 FFFF",
    "2001:db8::1 ssh-ed25519 GGGG",
    "",
  ].join("\n"),
  "# comment\n\nZed.example ssh-rsa A\nalpha.example ssh-rsa B\nBeta.example ssh-rsa C\n10.0.0.2 ssh-rsa D\n10.0.0.10 ssh-rsa E\n@revoked old.example ssh-rsa F\n!neg.example ssh-rsa G\nwild?.example ssh-rsa H\n",
]) {
  add("ssh_known_hosts", { raw, hosts: sshConfig.parseKnownHostsHostnames(raw) });
}

// ---- ssh tunnel, ssh auth, shared release/semver helpers, service protocol, pair ---------------
// The tunnel module is run with the effect copy that packages/ssh resolves, against a fake process
// spawner, HTTP client and net service. Nothing here starts ssh, touches the network or reads
// ~/.ssh: every spawned command is only recorded.
const SSH_EFFECT = "packages/ssh/node_modules/effect/dist/";
const sshFx = await import(at(SSH_EFFECT + "Effect.js"));
const sshLayer = await import(at(SSH_EFFECT + "Layer.js"));
const sshStream = await import(at(SSH_EFFECT + "Stream.js"));
const sshSink = await import(at(SSH_EFFECT + "Sink.js"));
const sshProcess = await import(at(SSH_EFFECT + "process/index.js"));
const sshHttp = await import(at(SSH_EFFECT + "http/index.js"));
const sshNodeServices = await import(
  at("packages/ssh/node_modules/@effect/platform-node/dist/NodeServices.js")
);
const sshNet = await import(at("packages/shared/src/Net.ts"));
const sshHostProcess = await import(at("packages/shared/src/hostProcess.ts"));
const sshTunnel = await import(at("packages/ssh/src/tunnel.ts"));
const sshAuth = await import(at("packages/ssh/src/auth.ts"));
const cliRelease = await import(at("packages/shared/src/cliRelease.ts"));
const sharedSemver = await import(at("packages/shared/src/semver.ts"));
const sharedSchemaJson = await import(at("packages/shared/src/schemaJson.ts"));
const serviceProtocol = await import(at("apps/server/src/cloud/serviceProtocol.ts"));
const servicePreflight = await import(at("apps/server/src/cloud/servicePreflight.ts"));
const pairCli = await import(at("apps/server/src/cli/pair.ts"));
const { ChildProcessSpawner: SshSpawnerNs } = sshProcess;

const settleSsh = async (effect) => {
  const result = await sshFx.runPromise(sshFx.result(effect));
  return result._tag === "Success"
    ? { ok: true, value: result.success ?? result.value }
    : { ok: false, error: result.failure };
};
const attempt = (thunk) => {
  try {
    return { ok: true, value: thunk() };
  } catch (error) {
    return { ok: false, tag: error._tag ?? error.name, message: error.message };
  }
};

// Template files: the original's template literals after JavaScript evaluated their escapes.
{
  const { readFileSync } = await import("node:fs");
  const literal = (source, name) => {
    const head = `const ${name} = \``;
    let i = source.indexOf(head) + head.length;
    let body = "";
    while (source[i] !== "`") {
      if (source[i] === "\\") {
        body += source[i] + source[i + 1];
        i += 2;
      } else {
        body += source[i];
        i += 1;
      }
    }
    return new Function("return `" + body + "`")();
  };
  const tunnelSource = readFileSync(root + "packages/ssh/src/tunnel.ts", "utf8");
  const authSource = readFileSync(root + "packages/ssh/src/auth.ts", "utf8");
  const templates = {
    "node_env.tmpl": literal(tunnelSource, "REMOTE_NODE_ENV_SCRIPT"),
    "runner.tmpl": literal(tunnelSource, "REMOTE_RUNNER_SCRIPT"),
    "launch.tmpl": literal(tunnelSource, "REMOTE_LAUNCH_SCRIPT"),
    "pairing.tmpl": literal(tunnelSource, "REMOTE_PAIRING_SCRIPT"),
    "stop.tmpl": literal(tunnelSource, "REMOTE_STOP_SCRIPT"),
    "log_tail.tmpl": literal(tunnelSource, "REMOTE_LOG_TAIL_SCRIPT"),
    "pick_port.tmpl": sshTunnel.REMOTE_PICK_PORT_SCRIPT,
    "wait_ready.tmpl": literal(tunnelSource, "REMOTE_WAIT_READY_SCRIPT"),
    "askpass_posix.tmpl": literal(authSource, "ASKPASS_POSIX_SCRIPT"),
    "askpass_windows_launcher.tmpl": literal(authSource, "ASKPASS_WINDOWS_LAUNCHER_SCRIPT"),
    "askpass_windows.tmpl": literal(authSource, "ASKPASS_WINDOWS_SCRIPT"),
  };
  for (const [name, text] of Object.entries(templates)) add("ssh_template", { name, text });
}

// Remote script text: every builder, for runner options that exercise each substitution.
const runnerCases = [
  { archiveVersion: "1.2.3-preview.20260911.4" },
  { archiveVersion: "  1.2.3  " },
  { archiveVersion: "1.2.3", releaseBaseUrl: "https://mirror.example/t3/" },
  { archiveVersion: "1.2.3", releaseBaseUrl: "https://mirror.example/t3///" },
  { archiveVersion: "1.2.3", releaseBaseUrl: "   " },
  { archiveVersion: "1.2.3", releaseBaseUrl: "file:///tmp/mirror/v" },
  { archiveVersion: "1.2.3", releaseBaseUrl: null },
  { archiveVersion: "1.2.3+build" },
  { nodeScriptPath: "/Users/julius/Development/Work/codething-mvp/apps/server/dist/bin.mjs" },
  {
    nodeScriptPath: "/Users/julius/Development/Work/codething-mvp/apps/server/dist/bin.mjs",
    nodeEngineRange: "^22.16 || ^23.11 || >=24.10",
  },
  { nodeScriptPath: "/opt/it's here/bin.mjs", nodeEngineRange: "  >=22  " },
  // Placeholder values are JavaScript replacement strings: `$$`, `$&`, `$\`` and `$'` expand.
  { nodeScriptPath: "/tmp/a$$b/bin.mjs" },
  { nodeScriptPath: "/tmp/a$&b/bin.mjs" },
  { nodeScriptPath: "/tmp/a$`b/bin.mjs", nodeEngineRange: "$'x" },
  { nodeScriptPath: "/tmp/$1/$<n>/bin.mjs" },
  { archiveVersion: "1.2.3", nodeScriptPath: "/tmp/both.mjs" },
  { nodeScriptPath: "\u00a0\u3000 /tmp/unicode-space.mjs \ufeff" },
  { archiveVersion: "../other" },
  { archiveVersion: "1.2.3/evil" },
  { archiveVersion: "1.2.3\\evil" },
  { archiveVersion: "1.2.3-preview.1 x" },
  { archiveVersion: "1.2.3-preview.1\nrm -rf /" },
  { archiveVersion: "v1.2.3" },
  { archiveVersion: "01.2.3" },
  { archiveVersion: "1.2" },
  { archiveVersion: "  " },
  { nodeScriptPath: null },
  {},
  undefined,
];
const STATE_KEY = "711bc738002d72fd";
for (const options of runnerCases) {
  const runner = attempt(() => sshTunnel.buildRemoteT3RunnerScript(options));
  const launch = attempt(() => sshTunnel.buildRemoteLaunchScript(options));
  const pairing = attempt(() => sshTunnel.buildRemotePairingScript(STATE_KEY, options));
  add("ssh_scripts", {
    options: options === undefined ? null : options,
    undefinedOptions: options === undefined,
    runner,
    nodeEnv: sshTunnel.buildRemoteNodeEnvScript(options),
    launch,
    pairing,
  });
}
add("ssh_stop_script", { stateKey: STATE_KEY, script: sshTunnel.buildRemoteStopScript(STATE_KEY) });
add("ssh_stop_script", { stateKey: "x", script: sshTunnel.buildRemoteStopScript("x") });

// Reading what the remote printed: the real launch / pairing functions over a fake spawner.
const decoder = new TextDecoder();
const sshTarget = (alias, hostname, username, port) => ({ alias, hostname, username, port });
const fakeHandle = (stdout, exitCode = 0, stderr = "") =>
  SshSpawnerNs.makeHandle({
    pid: SshSpawnerNs.ProcessId(123),
    stdout: sshStream.make(new TextEncoder().encode(stdout)),
    stderr: sshStream.make(new TextEncoder().encode(stderr)),
    all: sshStream.empty,
    exitCode: sshFx.succeed(SshSpawnerNs.ExitCode(exitCode)),
    isRunning: sshFx.succeed(false),
    kill: () => sshFx.void,
    stdin: sshSink.drain,
    getInputFd: () => sshSink.drain,
    getOutputFd: () => sshStream.empty,
    unref: sshFx.succeed(sshFx.void),
  });
const spawnerLayer = (calls, respond) =>
  sshLayer.merge(
    sshNodeServices.layer,
    sshLayer.succeed(
      SshSpawnerNs.ChildProcessSpawner,
      SshSpawnerNs.make((command) =>
        sshFx.gen(function* () {
          const stream = command.options?.stdin?.stream;
          const chunks = stream ? yield* sshStream.runCollect(stream) : [];
          const call = {
            command: command.command,
            args: [...command.args],
            stdin: chunks.map((c) => decoder.decode(c)).join(""),
          };
          // Rows keep a digest of the script: the scripts themselves are rows of their own.
          calls.push({
            command: call.command,
            args: call.args,
            stdinSha256: createHash("sha256").update(call.stdin, "utf8").digest("hex"),
            stdinLength: call.stdin.length,
          });
          return respond(call);
        }),
      ),
    ),
  );
const errorRow = (error) => ({
  tag: error._tag,
  message: error.message,
  stdout: error.stdout ?? null,
});
const launchOutputs = [
  '{"remotePort":3773}\n',
  'loaded nvm default\n{"remotePort":3774}\n',
  '{"remotePort":3773,"serverKind":"external"}',
  '{"remotePort":3773,"serverKind":"managed"}\n',
  '{"remotePort":3773,"serverKind":"weird"}',
  '{"remotePort":3773,"serverKind":null}',
  '{"remotePort":3773.5}',
  '{"remotePort":-1}',
  '{"remotePort":0}',
  '{"remotePort":"3773"}',
  '{"remotePort":3773,"extra":true}',
  '{"remotePort":3773,}',
  '{"remotePort":3773 /* note */}',
  '// banner\n{"remotePort":3773}\n',
  '{"remotePort":3773}\ntrailing junk {',
  '{"remotePort":3773}\n{"remotePort":3774}\n',
  '{ not json } {"remotePort":1}',
  '{"note":"a,]","remotePort":3773}',
  "",
  "\n\n  \n",
  "not json at all",
  "[1]",
  "null",
  "42",
  '{"remotePort":3773',
  '  {\n  "remotePort": 3775,\n  "serverKind": "managed"\n}\n\n',
];
for (const stdout of launchOutputs) {
  const calls = [];
  const result = await settleSsh(
    sshTunnel
      .launchOrReuseRemoteServer(
        sshTarget("devbox", "devbox.example.com", "julius", 2222),
        undefined,
        {
          archiveVersion: "1.2.3",
        },
      )
      .pipe(sshFx.provide(spawnerLayer(calls, () => fakeHandle(stdout)))),
  );
  add("ssh_launch_output", {
    stdout,
    ok: result.ok,
    value: result.ok ? result.value : null,
    error: result.ok ? null : errorRow(result.error),
    calls,
  });
}
const pairingOutputs = [
  '{"credential":"LCL4R2TPHDKQ"}\n',
  '{\n  "id": "88941235",\n  "credential": "LCL4R2TPHDKQ",\n  "scopes": ["orchestration:read"],\n  "expiresAt": "2026-04-29T01:01:20.994Z"\n}\n\n',
  'loaded nvm default\n{\n  "id": "88941235",\n  "credential": "LCL4R2TPHDKQ"\n}\n\n',
  '{"credential":"   "}',
  '{"credential":""}',
  '{"credential":42}',
  '{"token":"x"}',
  "",
  "\n",
  "oops",
  '{"credential":"A",}',
];
for (const stdout of pairingOutputs) {
  for (const runner of [{ archiveVersion: "1.2.3" }, { nodeScriptPath: "/opt/t3/bin.mjs" }]) {
    const calls = [];
    const result = await settleSsh(
      sshTunnel
        .issueRemotePairingToken(
          sshTarget("devbox", "devbox.example.com", null, null),
          undefined,
          runner,
        )
        .pipe(sshFx.provide(spawnerLayer(calls, () => fakeHandle(stdout)))),
    );
    add("ssh_pairing_output", {
      stdout,
      runner,
      ok: result.ok,
      value: result.ok ? result.value : null,
      error: result.ok ? null : errorRow(result.error),
      calls,
    });
  }
}

// A whole connect / pair / disconnect, as the commands the manager spawns (no process runs).
{
  const runningHandle = () =>
    SshSpawnerNs.makeHandle({
      pid: SshSpawnerNs.ProcessId(321),
      stdout: sshStream.empty,
      stderr: sshStream.empty,
      all: sshStream.empty,
      exitCode: sshFx.never,
      isRunning: sshFx.succeed(true),
      kill: () => sshFx.void,
      stdin: sshSink.drain,
      getInputFd: () => sshSink.drain,
      getOutputFd: () => sshStream.empty,
      unref: sshFx.succeed(sshFx.void),
    });
  const respond = (call) => {
    if (call.args.includes("-G"))
      return fakeHandle("hostname devbox.example.com\nuser julius\nport 2222\n");
    if (call.args.includes("-N")) return runningHandle();
    if (call.args.includes("--")) return fakeHandle('{"remotePort":3773,"serverKind":"managed"}\n');
    if (call.stdin.includes("auth pairing create")) return fakeHandle('{"credential":"TOKEN"}\n');
    return fakeHandle('{"stopped":true}\n');
  };
  for (const [runner, requested] of [
    [{ archiveVersion: "1.2.3" }, sshTarget("devbox", "devbox", null, null)],
    [
      { nodeScriptPath: "/opt/t3/bin.mjs", nodeEngineRange: ">=22" },
      sshTarget("devbox", "devbox", "alice", 2200),
    ],
  ]) {
    const calls = [];
    const testHttp = sshHttp.HttpClient.make((request) =>
      sshFx.succeed(sshHttp.HttpClientResponse.fromWeb(request, new Response("", { status: 200 }))),
    );
    const net = sshNet.NetService.of({
      canListenOnHost: () => sshFx.succeed(true),
      isPortAvailableOnLoopback: () => sshFx.succeed(true),
      hasListenerOnHost: () => sshFx.succeed(false),
      reserveLoopbackPort: () => sshFx.succeed(41_773),
      findAvailablePort: (preferred) => sshFx.succeed(preferred),
    });
    const layer = sshLayer.mergeAll(
      spawnerLayer(calls, respond),
      sshLayer.succeed(sshHttp.HttpClient.HttpClient, testHttp),
      sshLayer.succeed(sshNet.NetService, net),
      sshAuth.SshPasswordPrompt.disabledLayer,
      sshTunnel.SshEnvironmentManager.layer({ resolveCliRunner: sshFx.succeed(runner) }),
    );
    const result = await settleSsh(
      sshFx
        .gen(function* () {
          const manager = yield* sshTunnel.SshEnvironmentManager;
          const bootstrap = yield* manager.ensureEnvironment(requested, {
            issuePairingToken: true,
          });
          yield* manager.disconnectEnvironment(requested);
          return bootstrap;
        })
        .pipe(sshFx.provide(layer), sshFx.scoped),
    );
    add("ssh_manager_flow", {
      runner,
      requested,
      ok: result.ok,
      bootstrap: result.ok ? result.value : null,
      calls,
    });
  }
}

for (const raw of [
  "http://127.0.0.1:41773/",
  "http://localhost:3773",
  "http://[::1]:8080/path?q=1",
  "http://LOCALHOST:80/",
  "http://127.0.0.1",
  "https://example.com/",
  "http://10.0.0.5:3773/",
  "not a url",
  "",
  "   ",
  "ftp://127.0.0.1/x",
  "http://[::2]/",
  "http://127.0.0.2/",
]) {
  const result = await settleSsh(sshTunnel.resolveLoopbackSshHttpBaseUrl(raw));
  add("ssh_loopback", {
    raw,
    ok: result.ok,
    value: result.ok ? result.value : null,
    message: result.ok ? null : result.error.message,
  });
}

// JSON helpers the remote output goes through.
for (const raw of [
  "",
  "  \n",
  "no braces",
  '{"a":1}',
  'noise {"a":{"b":"}"}} tail',
  'x {"a":"\\"}"} y',
  "{ unbalanced",
  '{"a":1}{"b":2}',
  "}{",
  'pre\n{\n  "k": [1, 2, {"z": "}"}]\n}\n\npost',
]) {
  add("shared_extract_json", { raw, value: sharedSchemaJson.extractJsonObject(raw) });
}

// ssh auth
for (const message of [
  "julius@100.65.180.100: Permission denied (publickey,password,keyboard-interactive).",
  "Permission denied (publickey).",
  "PERMISSION DENIED (GSSAPI-WITH-MIC,hostbased)",
  "Connection timed out",
  "mkdir: Permission denied",
  "Authentication failed.",
  "Received disconnect: Too many authentication failures",
  "permission denied (unknownmethod)",
  "",
]) {
  add("ssh_auth_failure", { message, failure: sshAuth.isSshAuthFailure(new Error(message)) });
}
for (const [directory, platform] of [
  ["/tmp/t3code-ssh-askpass", "linux"],
  ["/tmp/t3code-ssh-askpass/", "darwin"],
  ["/tmp//a/./b/../c", "linux"],
  ["relative/dir", "linux"],
  ["C:\\temp\\t3code-ssh-askpass", "win32"],
  ["C:\\temp\\t3code-ssh-askpass\\\\", "win32"],
  ["C:/temp/x/", "win32"],
]) {
  const nodePath = await import("node:path");
  const descriptor = await sshFx.runPromise(
    sshAuth
      .buildSshAskpassHelperDescriptor({ directory })
      .pipe(
        sshFx.provideService(sshHostProcess.HostProcessPlatform, platform),
        sshFx.provide(sshNodeServices.layer),
      ),
  );
  add("ssh_askpass_descriptor", { directory, platform, descriptor });
  void nodePath;
}

// semver range gate the remote evaluates
const semverVersions = [
  "22.16.0",
  "22.15.9",
  "23.11.1",
  "23.10.9",
  "24.10.0",
  "24.9.9",
  "v20.1.2",
  "20",
  "20.1",
  "not-a-version",
  "",
  "  22.16.0  ",
  "24.10.0-rc.1",
  "0.2.3",
  "0.2.9",
  "0.3.0",
  "0.0.3",
  "0.0.4",
  "1.2.3.4",
  "01.2.3",
  "99999999999999999999.0.0",
];
const semverRanges = [
  "^22.16 || ^23.11 || >=24.10",
  ">=24.0 <24.10",
  "~24.10",
  "^0.2.3",
  "^0.0.3",
  ">=20",
  "<=20.1",
  ">20",
  "<22",
  "=22.16.0",
  "22.16",
  "v22",
  "^22 ||",
  "",
  "||",
  ">= 22",
  "^22.16.0 junk",
  "<1",
  "^99999999999999999999",
];
for (const version of semverVersions) {
  for (const range of semverRanges) {
    add("semver_range", { version, range, ok: sharedSemver.satisfiesSemverRange(version, range) });
  }
}

// cliRelease
for (const [platform, arch] of [
  ["darwin", "arm64"],
  ["darwin", "x64"],
  ["linux", "x64"],
  ["linux", "arm64"],
  ["win32", "x64"],
  ["win32", "arm64"],
  ["freebsd", "x64"],
  ["linux", "ia32"],
]) {
  const key = cliRelease.cliArchivePlatformKey(platform, arch) ?? null;
  add("cli_platform_key", {
    platform,
    arch,
    key,
    file: key === null ? null : cliRelease.cliArchiveFileName("1.2.3-preview.20260911.4", key),
  });
}
for (const [platform, env] of [
  ["linux", {}],
  ["darwin", { SystemRoot: "x" }],
  ["win32", {}],
  ["win32", { SystemRoot: "D:\\Win" }],
  ["win32", { windir: "E:\\W" }],
  ["win32", { SystemRoot: "A", windir: "B" }],
]) {
  add("cli_tar_command", {
    platform,
    env,
    command: cliRelease.cliArchiveTarCommand(platform, env),
  });
}
for (const [version, base] of [
  ["1.2.3", undefined],
  ["1.2.3", null],
  ["1.2.3", ""],
  ["1.2.3", "  "],
  ["1.2.3", "https://mirror.example/t3"],
  ["1.2.3", " https://mirror.example/t3/// "],
  ["1.2.3-preview.20260911.4", "file:///tmp/m"],
  ["", "https://x/"],
]) {
  add("cli_base_url", {
    version,
    base: base ?? null,
    url: cliRelease.cliReleaseDownloadBaseUrl(version, base ?? undefined),
  });
}
const hash = "ab".repeat(32);
for (const text of [
  `${hash}  t3-1.2.3-linux-x64.tar.gz\n`,
  `${hash.toUpperCase()} *t3-1.2.3-darwin-arm64.tar.gz\r\n${"cd".repeat(32)}  other file.zip\r\n`,
  `${hash}\tt3-1.2.3-linux-arm64.tar.gz`,
  `  ${hash}   t3-1.2.3-win32-x64.zip  \n`,
  `${hash.slice(1)}  short-hash\n${hash}\n${hash}  ok\n\nnonsense line\n`,
  `${hash}  dup\n${"ee".repeat(32)}  dup\n`,
  `${hash}  a\u2028b\n`,
  "",
]) {
  add("cli_checksums", { text, entries: [...cliRelease.parseChecksums(text).entries()] });
}
for (const version of [
  "1.2.3",
  "1.2.3-nightly.20260911.4",
  "1.2.3-preview.20260911.4",
  "1.2.3-preview.2026091.4",
  "1.2.3-preview.20260911.x",
  "1.2.3-beta.1",
  "1.2.3+build",
  "a-b-nightly.20260911.4",
  "",
]) {
  add("cli_channel", { version, channel: cliRelease.cliReleaseChannelOf(version) });
}
{
  const releases = [
    { tag_name: "v2.0.0-nightly.20261001.3" },
    { tag_name: "v2.0.0-preview.20260930.1", draft: true },
    { tag_name: "v1.9.0-preview.20260925.2" },
    { tag_name: "latest" },
    { tag_name: "v1.8.0" },
    { tag_name: "v1.8" },
    { tag_name: "v1.7.0-rc.1" },
  ];
  for (const channel of ["stable", "nightly", "preview"]) {
    add("cli_newest", {
      channel,
      releases,
      version: cliRelease.newestCliReleaseVersion(releases, channel) ?? null,
    });
  }
  add("cli_index_url", { page: 3, url: cliRelease.cliReleaseIndexPageUrl(3) });
}

// serviceProtocol / servicePreflight
const versions = [
  "1.2.3",
  "0.0.0",
  "1.2.3-preview.20260911.4",
  "1.2.3-alpha",
  "1.2.3-alpha.1",
  "1.2.3-alpha.beta",
  "1.2.3-1",
  "1.2.3-01a",
  "1.2.3+build.5",
  "1.2.3-rc.1+build",
  "01.2.3",
  "1.2",
  "1.2.3.4",
  "v1.2.3",
  "1.2.3-",
  "1.2.3-01",
  "1.2.3+",
  "latest",
  "",
  "10.0.0",
  "9.9.9",
  "1.10.0",
  "1.2.10",
  "18446744073709551616.0.0",
  "1.2.3-0",
  "1.2.3-0.0",
];
for (const version of versions) {
  add("service_version", { version, exact: serviceProtocol.isExactServiceVersion(version) });
}
{
  const exact = versions.filter((v) => serviceProtocol.isExactServiceVersion(v));
  for (const left of exact) {
    for (const right of exact) {
      add("service_compare", {
        left,
        right,
        order: Math.sign(serviceProtocol.compareExactServiceVersions(left, right)),
      });
    }
  }
}
const update = (extra) => ({
  id: "u-1",
  fromVersion: "1.0.0",
  targetVersion: "1.1.0",
  ...extra,
});
const stateCases = [
  { protocol: 3, activeVersion: "1.0.0" },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "pending", dbPath: "/db" }) },
  { protocol: 3, activeVersion: "1.0.1", update: update({ status: "pending", dbPath: "/db" }) },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "pending", dbPath: "  " }) },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "pending" }) },
  { protocol: 3, activeVersion: "1.1.0", update: update({ status: "committed" }) },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "committed" }) },
  {
    protocol: 3,
    activeVersion: "1.0.0",
    update: update({ status: "rolled-back", reason: "boom" }),
  },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "failed" }) },
  { protocol: 3, activeVersion: "1.1.0", update: update({ status: "failed" }) },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "failed", reason: " " }) },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "failed", reason: null }) },
  { protocol: 3, activeVersion: "1.0.0", update: update({ status: "weird" }) },
  { protocol: 3, activeVersion: "1.0.0", update: update({ id: "  ", status: "failed" }) },
  { protocol: 3, activeVersion: "1.0.0", update: null },
  { protocol: 3, activeVersion: "1.0.0", update: [] },
  {
    protocol: 3,
    activeVersion: "1.1.0",
    update: { id: "u", fromVersion: "1.1.0", targetVersion: "1.0.0", status: "committed" },
  },
  {
    protocol: 3,
    activeVersion: "1.0.0",
    update: { id: "u", fromVersion: "1.0.0", targetVersion: "1.0.0", status: "failed" },
  },
  { protocol: 2, activeVersion: "1.0.0" },
  { protocol: 3.0, activeVersion: "1.0.0" },
  { protocol: "3", activeVersion: "1.0.0" },
  { activeVersion: "1.0.0" },
  { protocol: 3, activeVersion: "latest" },
  { protocol: 3 },
  [],
  null,
  "text",
];
for (const value of stateCases) {
  const text = JSON.stringify(value);
  add("service_state", {
    text,
    decoded: serviceProtocol.parseServiceState(text) ?? null,
    pending: serviceProtocol.serviceStateHasPendingUpdate(text),
    activeVersion: serviceProtocol.serviceStateActiveVersion(text) ?? null,
  });
}
for (const text of ["", "{", "not json", '{"update":{"status":"pending"}}', '{"update":[]}']) {
  add("service_state", {
    text,
    decoded: serviceProtocol.parseServiceState(text) ?? null,
    pending: serviceProtocol.serviceStateHasPendingUpdate(text),
    activeVersion: serviceProtocol.serviceStateActiveVersion(text) ?? null,
  });
}
const contextCases = [
  { protocol: 3, childVersion: "1.0.0" },
  { protocol: 3, childVersion: "1.1.0", update: update({ status: "pending", dbPath: "/db" }) },
  { protocol: 3, childVersion: "1.0.0", update: update({ status: "pending", dbPath: "/db" }) },
  { protocol: 3, childVersion: "1.1.0", update: update({ status: "committed" }) },
  { protocol: 3, childVersion: "1.0.0", update: update({ status: "committed" }) },
  { protocol: 3, childVersion: "1.0.0", update: update({ status: "rolled-back" }) },
  { protocol: 3, childVersion: "1.1.0", update: update({ status: "rolled-back" }) },
  { protocol: 3, childVersion: "1.0.0", update: update({ status: "failed", reason: "r" }) },
  { protocol: 3, childVersion: "1.0.0", update: { nope: true } },
  { protocol: 3, childVersion: "x" },
  { protocol: 4, childVersion: "1.0.0" },
  { childVersion: "1.0.0" },
];
for (const value of contextCases) {
  const text = JSON.stringify(value);
  add("service_context", {
    text,
    decoded: serviceProtocol.decodeServiceLauncherContext(text) ?? null,
  });
}
add("service_context", {
  text: "garbage",
  decoded: serviceProtocol.decodeServiceLauncherContext("garbage") ?? null,
});
for (const value of [
  { type: "request-update", targetVersion: "1.1.0", dbPath: "/db" },
  { type: "request-update", targetVersion: "1.1.0" },
  { type: "prepared", updateId: "u" },
  { type: "prepared", updateId: 1 },
  { type: "update-accepted", updateId: "u" },
  { type: "update-rejected", reason: "no" },
  { type: "update-rejected" },
  { type: "committed", updateId: "u" },
  { type: "other" },
  { updateId: "u" },
  [],
  null,
  "x",
]) {
  add("service_message", {
    value,
    child: serviceProtocol.decodeServiceLauncherChildMessage(value) ?? null,
    parent: serviceProtocol.decodeServiceLauncherParentMessage(value) ?? null,
  });
}
for (const [launcherProtocol, version] of [
  [3, "1.2.3"],
  [2, "1.2.3"],
  [4, "0.0.1"],
]) {
  add("service_preflight_run", {
    launcherProtocol,
    version,
    result: servicePreflight.runServicePreflight({
      databasePath: "/db",
      launcherProtocol,
      version,
    }),
  });
}
for (const value of [
  { status: "ready", version: "1.2.3", launcherProtocol: 3 },
  { status: "ready", version: "1.2.3", launcherProtocol: 2 },
  { status: "ready", version: "1.2.3" },
  { status: "ready", launcherProtocol: 3 },
  { status: "blocked", version: "1.2.3", reason: "r" },
  { status: "blocked", version: "1.2.3" },
  { status: "other", version: "1" },
  [],
  null,
  "x",
]) {
  add("service_preflight_decode", {
    value,
    decoded: servicePreflight.decodeServicePreflightResult(value) ?? null,
  });
}

// pair --tailscale
{
  const base = {
    version: 1,
    pid: 123,
    port: 3773,
    origin: "http://127.0.0.1:3773",
    startedAt: "2026-06-20T00:00:00.000Z",
  };
  for (const extra of [
    { devUrl: "http://localhost:5733/" },
    { devUrl: "http://192.168.1.10:5733/" },
    { devUrl: "http://[fd7a:115c::1]:5733/" },
    { devUrl: "http://[::1]:5733/" },
    { devUrl: "http://127.0.0.5:5733/" },
    { devUrl: "http://example.test/" },
    { devUrl: "http://example.test:80/" },
    { devUrl: "https://localhost:5733/" },
    {},
    { host: "0.0.0.0" },
    { host: "::" },
    { host: "[::]" },
    { host: "127.0.0.1" },
    { host: "localhost" },
    { host: "::1" },
    { host: "" },
    { host: "192.168.1.42" },
    { host: "fd7a:115c::1" },
    { host: "[fd7a:115c::1]" },
    { host: "100.64.0.7", devUrl: "http://localhost:5733/" },
  ]) {
    const state = { ...base, ...extra };
    const result = pairCli.resolveTailscaleLocalTarget(state);
    add("pair_local_target", {
      state: { host: state.host ?? null, port: state.port, devUrl: state.devUrl ?? null },
      target: result instanceof pairCli.DevServerNotProxiableError ? null : result,
      error: result instanceof pairCli.DevServerNotProxiableError ? result.message : null,
    });
  }
  const errors = [
    new pairCli.NoRunningServerError({
      checkedStatePaths: ["/a/userdata/server-runtime.json", "/b/dev/server-runtime.json"],
    }),
    new pairCli.NoRunningServerError({ checkedStatePaths: [] }),
    new pairCli.TailscaleUnavailableError({ cause: "x" }),
    new pairCli.MagicDnsNameMissingError(),
    new pairCli.ServesOtherEnvironmentError({ servePort: 8443 }),
    new pairCli.TailscaleServeFailedError({ servePort: 443, cause: "x" }),
    new pairCli.ServePortOccupiedError({ servePort: 8443 }),
    new pairCli.DevServerNotProxiableError({ devUrl: "https://localhost:5733/" }),
  ];
  for (const error of errors) add("pair_error", { tag: error._tag, message: error.message });
}

mkdirSync(root + "rust/crates/remote/tests/fixtures", { recursive: true });
const file = root + "rust/crates/remote/tests/fixtures/remote.txt";
writeFileSync(file + ".tmp", rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(file + ".tmp", file);
console.log(JSON.stringify({ cases: rows.length }));
