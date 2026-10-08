// Development oracle: Node24 and original checkout dependencies.
// PATH=/tmp/node-v24.13.1-darwin-arm64/bin:$PATH node rust/tools/generate_acp_startup_fixtures.mjs
// Runtime and tests use the generated artifact without TypeScript or Node.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as Runtime from "../../apps/server/src/provider/acp/AcpSessionRuntime.ts";
const cwd = fs.mkdtempSync(path.join(os.tmpdir(), "t3port-acp-startup-oracle-"));
try {
  const fixture = fileURLToPath(
    new URL("../crates/server/tests/fixtures/acp-coordinator-provider.py", import.meta.url),
  );
  const result = await Effect.runPromise(
    Effect.gen(function* () {
      const runtime = yield* Runtime.make({
        spawn: { command: "python3", args: [fixture, "state", path.join(cwd, "starts")] },
        cwd,
        clientInfo: { name: "original-proof", version: "0" },
      });
      const started = yield* runtime.start();
      return {
        setup: started.sessionSetupResult,
        configOptions: yield* runtime.getConfigOptions,
        modeState: yield* runtime.getModeState,
      };
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );
  result.configOnlyMode = await Effect.runPromise(
    Effect.gen(function* () {
      const runtime = yield* Runtime.make({
        spawn: {
          command: "python3",
          args: [
            fileURLToPath(
              new URL(
                "../crates/server/tests/fixtures/acp-coordinator-provider.py",
                import.meta.url,
              ),
            ),
            "config-only",
            path.join(cwd, "config-only.starts"),
          ],
        },
        cwd,
        clientInfo: { name: "original-proof", version: "0" },
      });
      const started = yield* runtime.start();
      const initialModeState = yield* runtime.getModeState;
      yield* runtime.request("x/mode", { mode: "alt" });
      return {
        setup: started.sessionSetupResult,
        initialModeState,
        latestModeState: yield* runtime.getModeState,
      };
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );
  fs.writeFileSync(
    new URL("../crates/server/tests/fixtures/acp-startup.json", import.meta.url),
    JSON.stringify(result, null, 2) + "\n",
  );
  console.log("Generated 2 actual-source ACP startup/mode witnesses");
} finally {
  fs.rmSync(cwd, { recursive: true, force: true });
}
