// Development oracle: the capability constants AntigravityAdapterV2 declares (before negotiation). The Rust
// capability file is compared with them and may differ only where the port documents a feature it has not
// ported (tests/provider_capabilities.rs).
import { writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import { ProviderInstanceId } from "../../packages/contracts/src/index.ts";
import { makeAntigravityAcpAdapterFlavor } from "../../apps/server/src/orchestration-v2/Adapters/AntigravityAdapterV2.ts";

const flavor = makeAntigravityAcpAdapterFlavor({
  instanceId: ProviderInstanceId.make("antigravity"),
  crypto: undefined,
  fileSystem: undefined,
  path: undefined,
  idAllocator: undefined,
  serverConfig: undefined,
  selfInvocation: undefined,
  makeRuntime: () => Effect.die("not spawned"),
  withProcess: (_stop, task) => task,
  defaultModel: Effect.succeed(undefined),
});
writeFileSync(
  new URL(
    "../crates/server/tests/fixtures/antigravity-capabilities-declared.json",
    import.meta.url,
  ),
  JSON.stringify(JSON.parse(JSON.stringify(flavor.capabilities)), null, 1) + "\n",
);
console.log("wrote the antigravity capability set");
process.exit(0);
