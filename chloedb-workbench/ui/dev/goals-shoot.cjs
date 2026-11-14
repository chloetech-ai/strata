// Screenshots of R2's goal pages (Review and ship, Debug) on dev/goals.html,
// against dev/levels-mock.mjs, at 1440x900 and 390x844 (touch), 2x, light
// and dark. Writes <out>/<part>-<screen>-<desktop|phone>-<theme>.png and
// appends a line per shot to <out>/ready.txt; says which phone shots
// scroll sideways.
//   node chloedb-workbench/ui/dev/levels-mock.mjs --port 7791 &
//   node chloedb-workbench/ui/dev/goals-shoot.cjs OUT_DIR [http://127.0.0.1:7791] [screen ...]
const fs = require("fs");
const path = require("path");
const { chromium } = require("playwright");

const OUT = path.resolve(process.argv[2] || "goals-shots");
const BASE = (process.argv[3] && process.argv[3].startsWith("http") ? process.argv[3] : "http://127.0.0.1:7791").replace(/\/$/, "");
const ONLY = process.argv.slice(3).filter((a) => !a.startsWith("http"));
const DEVICES = [
  { label: "desktop", viewport: { width: 1440, height: 900 } },
  { label: "phone", viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true },
];
const clickText = (t) => async (page) => { await page.getByRole("button", { name: t }).first().click(); await page.waitForTimeout(400); };
const send = (method) => (p, body) => async (page) => { await page.evaluate(([method, p, body]) => fetch(p, { method, headers: { "content-type": "application/json" }, body: JSON.stringify(body) }), [method, p, body]); };
const post = send("POST");
// Puts back what Apply fixes changed, so each shot starts from the story.
const unfix = async (page) => { await send("PUT")("/api/repos/shop/threshold", { value: 0.6 })(page); await send("PATCH")("/api/records/d-3", { pin: false, rule: false })(page); };

// [part, screen, query, hash, what, before, after]
const SHOTS = [
  ["review", "ready", "m=review", "#/p/shop/ship", "Step 1, what is ready: three branches in merge order with tests, merge and risk; receipts still working; the order and why"],
  ["review", "check", "m=review", "#/p/shop/ship/g1c0ffee", "Step 2, check it (checkout-v2): the verdict, the blast radius as three gauges, only the exceptions, pieces riskiest first; Merge is off until the decision that replaces an earlier one is confirmed, and Send back is the primary because tests fail"],
  ["review", "anyway", "m=review", "#/p/shop/ship/g1c0ffee/merge", "Step 3 for checkout-v2 after Still right: tests fail, so Merge anyway asks for the branch's name in the merge sheet", post("/api/review/confirm", { record: "d-41", ok: true }), post("/api/review/confirm", { record: "d-41", ok: false })],
  ["review", "merge", "m=review", "#/p/shop/ship/g2f5ee01/merge", "Step 3 (freeship): the merge sheet says exactly what is merged where, how, what is not done, and what Undo does"],
  ["review", "merged", "m=review", "#/p/shop/ship/g2f5ee01/merge", "After Merge: the result and Undo while main is still at the merge and nothing was pushed", null, post("/api/review/undo", { agent: "g2f5ee01" }), async (page) => { await page.locator(".rv-sheet .btn.primary").click(); await page.waitForTimeout(600); }],
  ["review", "empty", "m=review&empty=1", "#/p/shop/ship", "No branch ready: a plain sentence"],
  ["review", "failing", "m=review&fail=1", "#/p/shop/ship/g1c0ffee", "The server does not answer: a plain sentence"],
  ["debug", "session", "m=debug", "#/debug", "Step 1, session and turn: the sessions, freeship flagged by its tests; its turn strip, the turns that stand out with This went wrong, what Workbench reminded it of"],
  ["debug", "strip", "m=debug", "#/debug/g1c0ffee", "Step 1 on a long session: 186 turns as a strip in five phases with the six that stand out"],
  ["debug", "knew", "m=debug", "#/debug/g2f5ee01/2", "Step 2, what it knew at turn 2: the turns around it, the prompt, the context meter, what it was reminded of, the note it missed in plain words, the diff of the turn"],
  ["debug", "wrong", "m=debug", "#/debug/g2f5ee01/2/wrong", "Steps 3 and 4: the blast radius of the turn worst first, its diff with the line that matters marked, Replay; the fixes with one recommended and ticked, kept apart from Send freeship back"],
  ["debug", "fixed", "m=debug", "#/debug/g2f5ee01/2/fix", "Step 4 after Apply fixes with the looser match ticked too: what was done, and Undo for the project-wide setting", null, unfix, async (page) => { await page.locator(".db-fx input").nth(1).check(); await page.getByRole("button", { name: "Apply fixes" }).first().click(); await page.waitForTimeout(500); }],
  ["debug", "records", "m=debug", "#/debug/records", "What agents remember: notes with who said each and the branch, filters, Correct, Always remind, Make it a rule, Forget; a one-task note marked"],
  ["debug", "code", "m=debug", "#/debug/code?q=rounding", "Why the code is like this: Trace, saying the code is main's"],
  ["debug", "failing", "m=debug", "#/debug/failing", "What is failing: the summary line, then Health with Hotspots on its Tests tab"],
  ["debug", "empty", "m=debug&empty=1", "#/debug", "A project with no session: a plain sentence"],
  ["debug", "failing-server", "m=debug&fail=1", "#/debug/g2f5ee01/2", "The server does not answer: a plain sentence"],
];

(async () => {
  fs.mkdirSync(OUT, { recursive: true });
  const browser = await chromium.launch();
  const lines = [];
  const wide = [];
  for (const [part, screen, query, hash, what, before, after, act] of SHOTS) {
    if (ONLY.length && !ONLY.includes(screen) && !ONLY.includes(`${part}-${screen}`)) continue;
    for (const d of DEVICES) for (const theme of ["light", "dark"]) {
      const ctx = await browser.newContext({ viewport: d.viewport, deviceScaleFactor: 2, isMobile: !!d.isMobile, hasTouch: !!d.hasTouch, colorScheme: theme });
      const page = await ctx.newPage();
      page.on("pageerror", (e) => console.error(`${part}-${screen}-${d.label}-${theme}: ${e.message}`));
      await page.goto(`${BASE}/dev/goals.html`);
      if (before) await before(page);
      await page.goto(`${BASE}/dev/goals.html?${query}&theme=${theme}${hash}`);
      await page.waitForFunction(() => window.ready === true);
      await page.waitForTimeout(900);
      if (act) await act(page);
      await page.evaluate(() => document.fonts && document.fonts.ready);
      const file = `${part}-${screen}-${d.label}-${theme}.png`;
      const sideways = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
      if (sideways) wide.push(file);
      await page.screenshot({ path: path.join(OUT, file), fullPage: d.label === "desktop" });
      if (after) await after(page);
      lines.push(`${file}: ${what} (${d.label}, ${theme})`);
      await ctx.close();
    }
  }
  await browser.close();
  fs.appendFileSync(path.join(OUT, "ready.txt"), lines.join("\n") + "\n");
  console.log(`${lines.length} shots in ${OUT}`);
  if (wide.length) console.log(`scrolls sideways: ${wide.join(", ")}`);
})();
