// Development-only witnesses of the pairing-link and client-session administration the original
// serves over `/api/auth/pairing-links` and `/api/auth/clients` (and the `subscribeAuthAccess`
// snapshot). The scenario runs the unchanged `EnvironmentAuth` service over an in-memory database;
// ids are replaced by the names the steps give them, so the Rust test replays the same steps and
// compares the normalised answers.
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as Layer from "../../packages/contracts/node_modules/effect/dist/Layer.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as ServerConfig from "../../apps/server/src/config.ts";
import * as ServerEnvironment from "../../apps/server/src/environment/ServerEnvironment.ts";
import * as SqlitePersistence from "../../apps/server/src/persistence/Sqlite.ts";
import * as EnvironmentAuth from "../../apps/server/src/auth/EnvironmentAuth.ts";
import * as ServerSecretStore from "../../apps/server/src/auth/ServerSecretStore.ts";
import * as SessionStore from "../../apps/server/src/auth/SessionStore.ts";

import { DatabaseSync } from "node:sqlite";

// The original binds a JavaScript boolean in its consume query; the runtime this script runs on
// refuses to, so statements get 1/0 (what SQLite stores for a boolean).
const statement = Object.getPrototypeOf(new DatabaseSync(":memory:").prepare("select 1"));
for (const method of ["run", "all", "get"]) {
  const original = statement[method];
  statement[method] = function (...args) {
    return original.apply(
      this,
      args.map((arg) => (typeof arg === "boolean" ? Number(arg) : arg)),
    );
  };
}

const directory = mkdtempSync(join(tmpdir(), "t3code-rust-port-test-auth-access-"));
const layer = EnvironmentAuth.layer.pipe(
  Layer.provideMerge(ServerSecretStore.layer),
  Layer.provideMerge(SqlitePersistence.layerMemory),
  Layer.provide(ServerEnvironment.layerIdentity),
  Layer.provide(ServerConfig.layerTest(directory, { prefix: "t3-auth-access-" })),
);

const read = ["orchestration:read"];
const readWrite = ["orchestration:read", "orchestration:operate"];
const manage = ["orchestration:read", "access:read", "access:write"];
const steps = [
  { op: "link", name: "a", label: "CI phone", scopes: read },
  { op: "link", name: "b" },
  { op: "link", name: "boot", subject: "administrative-bootstrap", scopes: manage },
  { op: "link", name: "c", label: "Tablet", scopes: readWrite },
  { op: "listLinks" },
  { op: "revokeLink", name: "a" },
  { op: "revokeLink", name: "a" },
  { op: "revokeLink", name: "nobody" },
  { op: "listLinks" },
  { op: "consume", link: "b", session: "bPhone", scopes: null, device: "mobile" },
  { op: "revokeLink", name: "b" },
  {
    op: "consume",
    link: "c",
    session: "cTablet",
    scopes: ["orchestration:read"],
    device: "desktop",
  },
  { op: "listLinks" },
  { op: "issue", session: "admin", label: "deploy-bot", scopes: manage },
  { op: "issue", session: "reader", label: "reader", scopes: read },
  { op: "issue", session: "writer", label: "writer", scopes: manage },
  { op: "listClients", current: "admin" },
  { op: "connect", session: "reader" },
  { op: "listClients", current: "reader" },
  { op: "connect", session: "writer" },
  { op: "connect", session: "writer" },
  { op: "disconnect", session: "writer" },
  { op: "listClients", current: "writer" },
  { op: "disconnect", session: "writer" },
  { op: "revokeClient", current: "admin", target: "admin" },
  { op: "revokeClient", current: "admin", target: "reader" },
  { op: "revokeClient", current: "admin", target: "reader" },
  { op: "listClients", current: "admin" },
  { op: "revokeOthers", current: "admin" },
  { op: "revokeOthers", current: "admin" },
  { op: "listClients", current: "admin" },
];

const names = new Map();
const nameOf = (id) => names.get(id) ?? `unknown:${id}`;
const sleep = () => Effect.sleep("3 millis");

const run = Effect.gen(function* () {
  const auth = yield* EnvironmentAuth.EnvironmentAuth;
  const sessions = yield* SessionStore.SessionStore;
  const links = new Map();
  const credentials = new Map();
  const sessionIds = new Map();
  const results = [];
  for (const step of steps) {
    switch (step.op) {
      case "link": {
        const issued = yield* auth.createPairingLink({
          ...(step.label ? { label: step.label } : {}),
          ...(step.scopes ? { scopes: step.scopes } : {}),
          ...(step.subject ? { subject: step.subject } : {}),
        });
        links.set(step.name, issued.id);
        names.set(issued.id, step.name);
        credentials.set(step.name, issued.credential);
        yield* sleep();
        results.push({
          credentialLength: issued.credential.length,
          expiresInMs:
            Math.round(
              (issued.expiresAt.epochMilliseconds - issued.createdAt.epochMilliseconds) / 1000,
            ) * 1000,
        });
        break;
      }
      case "listLinks": {
        const listed = yield* auth.listPairingLinks();
        results.push(
          listed.map((link) => ({
            name: nameOf(link.id),
            subject: link.subject,
            label: link.label ?? null,
            scopes: link.scopes,
            permissions: link.permissions,
            keys: Object.keys(link).sort(),
          })),
        );
        break;
      }
      case "revokeLink": {
        results.push(yield* auth.revokePairingLink(links.get(step.name) ?? "missing"));
        break;
      }
      case "consume": {
        const token = yield* auth.exchangeBootstrapCredentialForAccessToken(
          credentials.get(step.link),
          step.scopes ?? undefined,
          { deviceType: step.device },
        );
        const verified = yield* sessions.verify(token.access_token);
        sessionIds.set(step.session, verified.sessionId);
        names.set(verified.sessionId, step.session);
        yield* sleep();
        results.push({ scope: token.scope });
        break;
      }
      case "issue": {
        const issued = yield* auth.issueSession({
          label: step.label,
          scopes: step.scopes,
          subject: step.session,
        });
        sessionIds.set(step.session, issued.sessionId);
        names.set(issued.sessionId, step.session);
        yield* sleep();
        results.push(null);
        break;
      }
      case "connect":
        yield* sessions.markConnected(sessionIds.get(step.session));
        results.push(null);
        break;
      case "disconnect":
        yield* sessions.markDisconnected(sessionIds.get(step.session));
        results.push(null);
        break;
      case "listClients": {
        const listed = yield* auth.listClientSessions(sessionIds.get(step.current));
        results.push(
          listed.map((client) => ({
            name: nameOf(client.sessionId),
            subject: client.subject,
            method: client.method,
            scopes: client.scopes,
            permissions: client.permissions,
            client: client.client,
            connected: client.connected,
            current: client.current,
            hasLastConnectedAt: client.lastConnectedAt !== null,
            keys: Object.keys(client).sort(),
          })),
        );
        break;
      }
      case "revokeClient": {
        const outcome = yield* auth
          .revokeClientSession(sessionIds.get(step.current), sessionIds.get(step.target))
          .pipe(
            Effect.catchTag("ServerAuthForbiddenOperationError", () => Effect.succeed("forbidden")),
          );
        results.push(outcome);
        break;
      }
      case "revokeOthers":
        results.push(yield* auth.revokeOtherClientSessions(sessionIds.get(step.current)));
        break;
    }
  }
  return results;
});

try {
  const results = await Effect.runPromise(
    run.pipe(Effect.provide(layer), Effect.provide(NodeServices.layer), Effect.scoped),
  );
  const rows = steps.map((step, index) => ({ step, result: results[index] }));
  writeFileSync(
    new URL("../crates/server/tests/fixtures/auth-access.jsonl", import.meta.url),
    rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
  );
  console.log(JSON.stringify({ steps: rows.length }));
} finally {
  rmSync(directory, { recursive: true, force: true });
}
