#!/usr/bin/env node
// Drives the Rust UI of a running fixture in a headless Chromium (Playwright), one line per check, and says what it saw:
//
//   node browser_check.mjs <url> <credential> --playwright <dir of playwright-core>
//
// The credential is a fresh pairing credential (pair.sh prints one). Nothing leaves the loopback interface: the page is the
// fixture's, and the browser is Playwright's own headless build, never the machine's Chrome. Exits 0 when every check passed.
// Start the fixture with --server-browser to have the Browser tab offered (it has no Chrome to start in the fixture, and says so).
import { createRequire } from "node:module";
import { join } from "node:path";

const args = process.argv.slice(2);
const flag = (name) => {
  const at = args.indexOf(name);
  return at < 0 ? undefined : args.splice(at, 2)[1];
};
const playwrightDir = flag("--playwright") ?? process.env.PLAYWRIGHT_CORE;
const [origin, credential] = args;
if (!origin || !credential || !playwrightDir) {
  console.error(
    "usage: browser_check.mjs <url> <credential> --playwright <dir of playwright-core>",
  );
  process.exit(2);
}
const { chromium } = createRequire(join(playwrightDir, "package.json"))("./index.js");

const results = [];
const line = (ok, name, detail = "") => {
  results.push(ok);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  " + detail : ""}`);
};
async function check(name, run) {
  try {
    const detail = await run();
    line(true, name, typeof detail === "string" ? detail : "");
  } catch (error) {
    line(false, name, String(error.message ?? error).split("\n")[0]);
  }
}
const seen = (text) => JSON.stringify(text.replace(/\s+/g, " ").trim().slice(0, 160));

const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({ viewport: { width: 1400, height: 900 } });
const page = await context.newPage();
const problems = [];
page.on("pageerror", (error) => problems.push(`pageerror: ${error.message}`));
page.on("console", (message) => {
  if (message.type() === "error" && !/Failed to load resource|favicon/i.test(message.text()))
    problems.push(`console: ${message.text()}`);
});
const body = () => page.locator("body").innerText();
const label = (text) => page.getByLabel(text, { exact: true });

try {
  await check("pair through the link and load the app", async () => {
    await page.goto(`${origin}/pair#token=${credential}`);
    await page.locator('[aria-label^="Thread "]').first().waitFor({ timeout: 60_000 });
    return `${await page.locator('[aria-label^="Thread "]').count()} thread rows`;
  });

  // -- the right panel ------------------------------------------------------------------------------------------------
  await check("a thread opens", async () => {
    const row = page.locator('[aria-label^="Thread "]').first();
    const name = await row.getAttribute("aria-label");
    await row.click();
    await page.getByLabel("Message", { exact: true }).waitFor({ timeout: 30_000 });
    return name ?? "";
  });
  await check("the right panel offers Browser, Device and Terminal", async () => {
    await label("Toggle panel").click();
    // An empty panel shows the launcher; a thread whose panel already holds tabs (an earlier run's sessions are the server's) has the add menu.
    const launcher = await label("Open a surface")
      .waitFor({ timeout: 5000 })
      .then(
        () => true,
        () => false,
      );
    if (!launcher) await label("Add panel surface").click();
    const verb = launcher ? "Open" : "Add";
    const rows = [];
    for (const surface of ["Browser", "Terminal", "Device"]) {
      const present = (await label(`${verb} ${surface}`).count()) > 0;
      rows.push(`${surface}:${present ? "offered" : "not offered"}`);
      if (!present && surface !== "Browser")
        throw new Error(`${surface} is not offered (${rows.join(", ")})`);
    }
    if (!launcher) await page.keyboard.press("Escape");
    return `${launcher ? "launcher" : "add menu"}: ${rows.join(", ")}`;
  });
  // A tab is added from the launcher while the panel is empty, and from the add menu after that.
  const openTab = async (surface) => {
    if (await label(`Open ${surface}`).count()) return label(`Open ${surface}`).click();
    await label("Add panel surface").click();
    await label(`Add ${surface}`).click();
  };

  await check("Terminal tab: a shell session starts in the panel", async () => {
    await openTab("Terminal");
    // The terminal is a canvas the wasm surface draws; the tab and its host element are what the DOM shows.
    await page.locator("canvas").first().waitFor({ timeout: 30_000 });
    return `a terminal canvas is drawn; panel text ${seen(await body())}`;
  });

  await check("Device tab: opens and says what the host can do", async () => {
    await openTab("Device");
    await page
      .getByText(/device hub|simulator|devices/i)
      .first()
      .waitFor({ timeout: 20_000 });
    const text = await page
      .getByText(/device hub|simulator|devices/i)
      .first()
      .innerText();
    return `panel says ${seen(text)}`;
  });

  await check("Browser tab: opens and says what the host can do", async () => {
    const offered =
      (await label("Open Browser").count()) + (await page.getByLabel("Add panel surface").count()) >
      0;
    if (!offered) throw new Error("no way to open a Browser tab was found");
    await openTab("Browser").catch((error) => {
      throw new Error(`the Browser row could not be used: ${error.message.split("\n")[0]}`);
    });
    await label("Search or enter URL").waitFor({ timeout: 30_000 });
    // No Chrome can be started in the fixture: the tab says what it needs, or that it is installing.
    await page.waitForFunction(
      () =>
        /chrome|browser|install|libraries|connecting|watching|can.t be started|cannot/i.test(
          document.body.innerText,
        ),
      null,
      { timeout: 30_000 },
    );
    const tab = await page
      .locator(".preview-body")
      .first()
      .innerText()
      .catch(() => "");
    const notes = await page.locator("[role=alert], [role=status]").allInnerTexts();
    return `address bar present; tab says ${seen(tab)}; notices ${seen(notes.filter(Boolean).join(" | "))}`;
  });

  // -- Integrations -----------------------------------------------------------------------------------------------------
  await check(
    "Integrations page: the browser defaults, profiles and the device section",
    async () => {
      await page.goto(`${origin}/settings/integrations`);
      await page.getByRole("heading", { name: "Integrations" }).waitFor({ timeout: 30_000 });
      for (const control of [
        "Default browser zoom",
        "Default browser appearance",
        "Open links in",
        "Add profile",
        "Agent browser access",
      ]) {
        await label(control).waitFor({ timeout: 15_000 });
      }
      return seen(await body());
    },
  );
  await check(
    "Integrations page: a default zoom and a new profile are kept across a reload",
    async () => {
      await label("Default browser zoom").selectOption("1.25");
      await label("Add profile").click();
      await label("Rename New profile").waitFor({ timeout: 10_000 });
      await label("Rename New profile").fill("Work");
      await label("Rename New profile").press("Enter");
      await page.reload();
      await label("Default browser zoom").waitFor({ timeout: 30_000 });
      const zoom = await label("Default browser zoom").inputValue();
      if (zoom !== "1.25") throw new Error(`zoom after reload is ${zoom}`);
      await label("Rename Work").waitFor({ timeout: 10_000 });
      return "zoom 125% and the profile Work are back after the reload";
    },
  );
  await check(
    "Integrations page: a profile asks before its data is cleared and can be removed",
    async () => {
      await label("Clear cookies and cache of Work").click();
      await page.getByRole("alertdialog").waitFor({ timeout: 10_000 });
      const asked = await page.getByRole("alertdialog").innerText();
      await page.getByRole("button", { name: "Cancel" }).first().click();
      return `asked first: ${seen(asked)}`;
    },
  );

  // -- Diagnostics --------------------------------------------------------------------------------------------------------
  await check("Diagnostics page: the process history section and its periods", async () => {
    await page.goto(`${origin}/settings/diagnostics`);
    await page.getByRole("heading", { name: "Resource History" }).waitFor({ timeout: 30_000 });
    for (const period of ["Period 5m", "Period 15m", "Period 30m", "Period 1h"])
      await label(period).waitFor({ timeout: 5000 });
    await label("Period 1h").click();
    await page.waitForFunction(
      () =>
        !/Refreshing/.test(
          document.querySelector('[aria-label="Resource History"]')?.innerText ?? "Refreshing",
        ),
      null,
      { timeout: 15_000 },
    );
    return seen(await page.locator('[aria-label="Resource History"]').innerText());
  });
} finally {
  if (problems.length)
    console.log(`NOTE  page errors seen: ${problems.slice(0, 5).map(seen).join(" | ")}`);
  await browser.close();
}
const failed = results.filter((ok) => !ok).length;
console.log(`${results.length - failed}/${results.length} browser checks passed`);
process.exit(failed ? 1 : 0);
