import { main } from "./lib/start.mjs";

main(process.argv.slice(2)).catch((error) => {
  console.error(`error: ${error.stack ?? error}`);
  process.exit(1);
});
