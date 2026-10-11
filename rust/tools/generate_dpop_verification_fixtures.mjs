// Development-only: what the unchanged `verifyDpopProof` (packages/shared/src/dpop.ts) makes of proofs whose key or signature is broken
// in each way a proof can be broken, for `crates/server/src/dpop.rs` to be held to. The original tells a signature that does not verify
// (`invalid_signature`: anything noble's `verify` answers false to, including a signature of the wrong length, a value out of range
// and a point off the curve) from a key or signature that cannot be read at all (`invalid_proof`: base64url that does not decode, a
// coordinate that is not 32 bytes). Written to a `.jsonl` file so the formatter leaves it alone.
import { createPrivateKey, generateKeyPairSync, sign } from "node:crypto";
import { writeFileSync } from "node:fs";
import { verifyDpopProof } from "../../packages/shared/src/dpop.ts";

const N = 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551n;
const url = "http://127.0.0.1:3774/oauth/token";
const now = 1_790_000_000;
const base64url = (bytes) => Buffer.from(bytes).toString("base64url");
const json = (value) => base64url(Buffer.from(JSON.stringify(value)));
const pair = generateKeyPairSync("ec", { namedCurve: "P-256" });
const jwk = pair.publicKey.export({ format: "jwk" });
const header = {
  typ: "dpop+jwt",
  alg: "ES256",
  jwk: { kty: "EC", crv: "P-256", x: jwk.x, y: jwk.y },
};
const claims = { htm: "POST", htu: url, jti: "proof-1", iat: now };
const signed = (head, body) => {
  const input = `${json(head)}.${json(body)}`;
  return {
    input,
    signature: sign("sha256", Buffer.from(input), {
      key: pair.privateKey,
      dsaEncoding: "ieee-p1363",
    }),
  };
};
const compact = (input, signature) =>
  `${input}.${typeof signature === "string" ? signature : base64url(signature)}`;
const big = (bytes) => BigInt(`0x${Buffer.from(bytes).toString("hex")}`);
const bytesOf = (value) => Buffer.from(value.toString(16).padStart(64, "0"), "hex");

const rows = [];
const record = (name, proof) => {
  const result = verifyDpopProof({ proof, method: "POST", url, nowEpochSeconds: now });
  rows.push({ name, proof, method: "POST", url, now, result });
};
const good = signed(header, claims);
record("valid", compact(good.input, good.signature));
record(
  "payload tampered after signing",
  compact(good.input.replace(/\.[^.]+$/, `.${json({ ...claims, jti: "other" })}`), good.signature),
);
record("signature of 63 bytes", compact(good.input, good.signature.subarray(0, 63)));
record(
  "signature of 65 bytes",
  compact(good.input, Buffer.concat([good.signature, Buffer.from([1])])),
);
record("signature of 1 byte", compact(good.input, Buffer.from([7])));
record(
  "signature of 64 bytes, r zero",
  compact(good.input, Buffer.concat([Buffer.alloc(32), good.signature.subarray(32)])),
);
record(
  "signature of 64 bytes, s zero",
  compact(good.input, Buffer.concat([good.signature.subarray(0, 32), Buffer.alloc(32)])),
);
record(
  "signature of 64 bytes, r at the order",
  compact(good.input, Buffer.concat([bytesOf(N), good.signature.subarray(32)])),
);
record(
  "signature of 64 bytes, s above the order",
  compact(good.input, Buffer.concat([good.signature.subarray(0, 32), bytesOf(N + 5n)])),
);
record("signature of 64 bytes, all ones", compact(good.input, Buffer.alloc(64, 0xff)));
const s = big(good.signature.subarray(32));
record(
  "high-s twin of a valid signature",
  compact(good.input, Buffer.concat([good.signature.subarray(0, 32), bytesOf(N - s)])),
);
record("signature not base64url", compact(good.input, "!!!not*base64!!!"));
record("signature with padding", compact(good.input, `${base64url(good.signature)}==`));

const withKey = (jwkChange) => signed({ ...header, jwk: { ...header.jwk, ...jwkChange } }, claims);
const x = Buffer.from(jwk.x, "base64url");
const y = Buffer.from(jwk.y, "base64url");
const flipped = Buffer.from(x);
flipped[31] ^= 1;
for (const [name, change] of [
  ["x off the curve", { x: base64url(flipped) }],
  ["x and y zero", { x: base64url(Buffer.alloc(32)), y: base64url(Buffer.alloc(32)) }],
  ["x above the field", { x: base64url(Buffer.alloc(32, 0xff)) }],
  ["x of 31 bytes", { x: base64url(x.subarray(1)) }],
  ["y of 33 bytes", { y: base64url(Buffer.concat([y, Buffer.from([1])])) }],
  ["x not base64url", { x: "***" }],
  ["y not base64url", { y: "@@@@" }],
]) {
  const other = withKey(change);
  record(name, compact(other.input, good.signature));
}
// Failures that come before the signature is looked at keep their own codes.
record("wrong method", compact(signed(header, { ...claims, htm: "GET" }).input, good.signature));
record(
  "old proof",
  (() => {
    const old = signed(header, { ...claims, iat: now - 1000 });
    return compact(old.input, old.signature);
  })(),
);
writeFileSync(
  new URL("../crates/server/tests/fixtures/dpop-verification.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(rows.map((row) => `${row.name}: ${row.result.ok ? "ok" : row.result.code}`).join("\n"));
