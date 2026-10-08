// Actual DOM ABI lifecycle proof; no browser or provider account is opened.
import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
const code = readFileSync(
  new URL("../crates/ui/assets/provider_auth_browser.js", import.meta.url),
  "utf8",
);
const run = new Function("args", "window", "document", "dioxus", `return (async()=>{${code}})();`);
function fixture(native, consent, blocked = false) {
  let consentReceipt;
  const receipt = new Promise((resolve) => (consentReceipt = resolve));
  const calls = [];
  const tab = {
    opener: {},
    location: { href: "" },
    close() {
      calls.push("closed");
    },
  };
  const window = {
    location: {
      set href(url) {
        calls.push(["external", url]);
      },
    },
    open(url) {
      calls.push(["open", url]);
      return blocked ? null : tab;
    },
  };
  const document = {
    body: {
      append(link) {
        calls.push(["append", link.href]);
      },
    },
    createElement() {
      return {
        click() {
          calls.push(["external", this.href]);
        },
        remove() {
          calls.push("removed");
        },
      };
    },
  };
  const task = run({ native, consent, url: "https://example.test/login" }, window, document, {
    recv: () => receipt,
  });
  return { task, calls, tab, consentReceipt };
}
let count = 0;
for (const native of [false, true])
  for (const accepted of [false, true]) {
    const f = fixture(native, true);
    assert.deepEqual(f.calls, native ? [] : [["open", ""]]);
    assert.equal(f.tab.location.href, "");
    f.consentReceipt(accepted);
    assert.equal(await f.task, true);
    if (native) {
      assert.equal(
        f.calls.some((call) => Array.isArray(call) && call[0] === "external"),
        accepted,
      );
      assert.equal(
        f.calls.some((call) => Array.isArray(call) && call[0] === "append"),
        false,
      );
    } else {
      assert.equal(f.tab.opener, null);
      assert.equal(f.tab.location.href, accepted ? "https://example.test/login" : "");
      assert.equal(f.calls.includes("closed"), !accepted);
    }
    count++;
  }
for (const native of [false, true]) {
  const f = fixture(native, false);
  assert.equal(await f.task, true);
  assert.equal(
    f.calls.some((call) => Array.isArray(call) && call[0] === "external"),
    native,
  );
  count++;
}
const blocked = fixture(false, false, true);
assert.equal(await blocked.task, false);
count++;
console.log(`${count} provider sign-in browser/native consent ABI cases passed`);
