import { StringDecoder } from "node:string_decoder";
import { writeFileSync } from "node:fs";
const inputs = [
  Buffer.from("前😀é\ufeff\nlast"),
  Buffer.from([0xf0, 0x9f, 0x98]),
  Buffer.from([0xe2, 0x82]),
  Buffer.from([0xff, 0xc0, 0xaf, 0x61]),
  Buffer.from([0xed, 0xa0, 0x80, 0x62]),
  Buffer.from([0xe2, 0x82, 0x61]),
  Buffer.from([0xf0, 0x9f, 0x61, 0x80]),
  Buffer.from([0xe4, 0xb8, 0xad, 0xe2, 0x28, 0xa1, 0x63]),
];
const fixtures = [];
for (const input of inputs) {
  for (let start = 0; start <= input.length; start++) {
    const chunks = [input.subarray(0, start), input.subarray(start)];
    const decoder = new StringDecoder("utf8");
    const outputs = chunks.map((chunk) => decoder.write(chunk));
    outputs.push(decoder.end());
    fixtures.push({ chunks: chunks.map((chunk) => Array.from(chunk)), outputs });
  }
  const chunks = Array.from(input, (byte) => Buffer.from([byte]));
  const decoder = new StringDecoder("utf8");
  const outputs = chunks.map((chunk) => decoder.write(chunk));
  outputs.push(decoder.end());
  fixtures.push({ chunks: chunks.map((chunk) => Array.from(chunk)), outputs });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/terminal-utf8.json", import.meta.url),
  JSON.stringify(fixtures) + "\n",
);
console.log(`${fixtures.length} UTF-8 fixtures`);
