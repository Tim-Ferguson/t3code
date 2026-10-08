// Development-only witnesses from unchanged desktop token/policy sources.
import { writeFileSync } from "node:fs";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import {
  currentDesktopBootstrapToken,
  isValidDesktopBootstrapToken,
  DESKTOP_BOOTSTRAP_TOKEN_WINDOW_MS as window,
} from "../../packages/shared/src/desktopBootstrapToken.ts";
import * as Policy from "../../apps/server/src/auth/EnvironmentAuthPolicy.ts";
import * as Config from "../../apps/server/src/config.ts";
import { ServerEnvironmentIdentity } from "../../apps/server/src/environment/ServerEnvironment.ts";
const rows = [];
for (const secret of ["", "desktop-secret", "密钥👩‍💻"])
  for (const now of [-window - 1, -window, -1, 0, window - 1, window, 2 * window]) {
    const current = currentDesktopBootstrapToken(secret, now);
    for (const offset of [-2, -1, 0, 1, 2]) {
      const token = currentDesktopBootstrapToken(secret, now + offset * window);
      rows.push({
        op: "token",
        secret,
        now,
        current,
        token,
        valid: isValidDesktopBootstrapToken(secret, token, now),
      });
    }
    for (const token of [current.toUpperCase(), current + " ", current.slice(1), "密钥"])
      rows.push({
        op: "token",
        secret,
        now,
        current,
        token,
        valid: isValidDesktopBootstrapToken(secret, token, now),
      });
  }
for (const mode of ["web", "desktop"])
  for (const host of [
    "",
    "localhost",
    "127.0.0.1",
    "127.other",
    "::1",
    "[::1]",
    "0.0.0.0",
    "::",
    "[::]",
    "192.168.0.1",
  ]) {
    const service = await Effect.runPromise(
      Policy.make.pipe(
        Effect.provideService(Config.ServerConfig, {
          mode,
          host,
          port: 3774,
          stateDir: "/tmp/fixture",
        }),
        Effect.provideService(ServerEnvironmentIdentity, {
          getEnvironmentId: Effect.succeed("fixture-environment"),
        }),
      ),
    );
    const descriptor = await Effect.runPromise(service.getDescriptor());
    rows.push({
      op: "policy",
      mode,
      host,
      policy: descriptor.policy,
      bootstrapMethods: descriptor.bootstrapMethods,
    });
  }
writeFileSync(
  new URL("../crates/server/tests/fixtures/desktop-auth.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
