// Original echo policy/transcript oracle. Generated data is the only runtime dependency.
import { readFileSync, writeFileSync } from "node:fs";
const text = readFileSync(
  new URL("../../apps/web/src/components/settings/SettingsFontPreviews.tsx", import.meta.url),
  "utf8",
);
const constants = text.slice(
  text.indexOf("const TERMINAL_PROMPT ="),
  text.indexOf("/** The surface treats"),
);
const { prompt, transcript } = new Function(
  constants + ";return {prompt:TERMINAL_PROMPT,transcript:TERMINAL_PREVIEW_TRANSCRIPT};",
)();
const echo = text.slice(
  text.indexOf("const echo = (data: string) => {"),
  text.indexOf("\n\n    void GhosttyTerminalSurface.create"),
);
const create = new Function(
  "TERMINAL_PROMPT",
  `let lineLength=0;const output=[];const surfaceRef={current:{write:data=>output.push(data)}};${echo.replace("(data: string)", "(data)")};return {echo,output};`,
);
const inputs = [
  "a",
  "abc",
  "hello world",
  "\r",
  "\b",
  "\x7f",
  "\x1b[A",
  "\x1b",
  "\t",
  "\n",
  "\x01",
  "😀",
  "é",
  "é",
  " a\n\t😀\x7fb",
  "中",
  "\ufeff",
  "\u0085",
];
const fixtures = [];
for (const first of inputs)
  for (const second of inputs) {
    const instance = create(prompt);
    instance.echo(first);
    instance.echo(second);
    instance.echo("\b");
    fixtures.push({ input: [first, second, "\b"], output: instance.output });
  }
writeFileSync(
  new URL("../crates/ui/assets/terminal-preview.json", import.meta.url),
  JSON.stringify({ prompt, transcript }) + "\n",
);
writeFileSync(
  new URL("../crates/client/tests/fixtures/terminal-preview.jsonl", import.meta.url),
  fixtures.map((v) => JSON.stringify(v)).join("\n") + "\n",
);
console.log(`Generated ${fixtures.length} original preview echo witnesses.`);
