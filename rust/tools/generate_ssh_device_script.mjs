// Writes the unchanged script an SSH device host runs on the remote machine (apps/server/src/device/sshDeviceScript.ts) and the
// environment it is started with, for `ssh_device_host.rs`. The owner and mode are placeholders the Rust server fills in.
//   node rust/tools/generate_ssh_device_script.mjs          writes the two files beside ssh_device_host.rs
//   node rust/tools/generate_ssh_device_script.mjs --check  compares with them
import * as NodeFS from "node:fs";

import {
  remoteDeviceEnvironment,
  remoteDeviceScript,
} from "../../apps/server/src/device/sshDeviceScript.ts";

const files = [
  [
    new URL("../crates/server/src/ssh_device_script.txt", import.meta.url),
    remoteDeviceScript("@@OWNER@@", "@@MODE@@"),
  ],
  [
    new URL("../crates/server/src/ssh_device_environment.txt", import.meta.url),
    remoteDeviceEnvironment,
  ],
];
for (const [target, content] of files) {
  if (!content.includes('"@@OWNER@@"') && target.pathname.endsWith("script.txt"))
    throw new Error("the owner placeholder moved");
  if (process.argv.includes("--check")) {
    if (NodeFS.readFileSync(target, "utf8") !== content) {
      console.error(
        `${target.pathname} is out of date; run rust/tools/generate_ssh_device_script.mjs`,
      );
      process.exit(1);
    }
  } else {
    NodeFS.writeFileSync(target, content);
    console.log(`wrote ${target.pathname} (${content.length} bytes)`);
  }
}
