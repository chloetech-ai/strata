// Screenshots of the harness mock (HARNESS.md, "The mocks"), copied from the
// journeys' shoot.cjs: serves ui/ itself, opens each shot on a desktop
// (1440x900, the whole page, dark and light) and on a phone (390x844, touch,
// the first screen, light), all at 2x, with the clock at 07:31 on 14 Nov
// 2026, and writes <out>/<name>-<theme>.png, <out>/<name>-phone-light.png
// and <out>/ready.txt. It reports any phone shot whose page scrolls sideways.
//   node chloedb-workbench/ui/dev/harness/shoot.cjs OUT_DIR [--desktop|--phone] [name ...]
const http = require("http");
const fs = require("fs");
const path = require("path");
const { chromium } = require("playwright");

const UI = path.resolve(__dirname, "..", "..");
const OUT = path.resolve(process.argv[2] || "harness-shots");
const ARGS = process.argv.slice(3);
const ONLY = ARGS.filter((a) => !a.startsWith("--"));
const DEVICES = [
  { key: "", label: "desktop", viewport: { width: 1440, height: 900 }, themes: ["light", "dark"], full: true },
  { key: "-phone", label: "phone", viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, themes: ["light"] },
].filter((d) => !ARGS.includes("--desktop") && !ARGS.includes("--phone") || ARGS.includes(`--${d.label}`));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".woff2": "font/woff2" };

// [name, route, query, what it shows, only on]
const SHOTS = [
  ["01-overview", "#/harness", "", "Overview: the week's scorecard live from ChloeDB, where the harness cost you (worst first, with evidence levels), agents by version; suggestions in the inspector, auto-learn mode, Test 3 changes", "desktop"],
  ["02-optimize-run", "#/harness/run/7", "", "An optimize run in progress: Find › Propose › Reproduce › Score › Keep; 10 sessions × 3 runs under 1.4 (A) and 1.5-rc (B), held-out marked, live scores, the replay in progress and its sandbox", "desktop"],
  ["03-results", "#/harness/run/7/results", "", "Results: the scorecard A against B with paired intervals and verdicts; the divergence of freeship's runs at turn 2 and what differed in its context; keep 2 of 3 changes", "desktop"],
  ["04-edit-permissions", "#/harness/edit/permissions", "", "Editor › Permissions: rules with hits, autocomplete from commands agents ran (22 prompts), the raw settings.json in sync, lint, and the counted impact of the new rule", "desktop"],
  ["05-edit-hooks", "#/harness/edit/hooks", "", "Editor › Hooks: one turn as a waterfall, every hook with p50/p95, runs and tokens added; the slow chloectx UserPromptSubmit with its last runs", "desktop"],
  ["06-edit-memory", "#/harness/edit/memory", "", "Editor › Memory: recall settings as scales, pins, the live preview of freeship's turn 2 and every recall of the week recomputed on its snapshot", "desktop"],
  ["07-edit-tools", "#/harness/edit/tools", "", "Editor › Tools and routing: MCP servers, skills and subagents with their context cost and use, and the model routing table from task_features", "desktop"],
  ["08-what-the-model-sees", "#/harness/sees", "", "What the model sees: freeship's turn 2 assembled part by part, token shares under 1.4 and 1.5-rc, measured against estimated, the recall text with the pinned record", "desktop"],
  ["09-evals", "#/harness/evals", "", "Evals: the suite shop core, cases made from sessions with their checks and pass³ on each version, runs of the suite, Make this a test from paybutton", "desktop"],
  ["10-versions", "#/harness/versions", "", "Versions: the history as a graph, the diff 1.4 → 1.5-rc by part with impact, Promote, Roll back and Branch", "desktop"],
  ["11-plugs-in", "#/", "", "Where it plugs in: Home's sixth goal (H), Continue's harness chip, agents with the version they ran, and Find out why's Fix leading into the harness", "desktop"],
  ["20-suggestions", "#/harness/approve", "", "Suggestions to approve: run 7's scorecard, then each change with its evidence, impact and level; one that widens says so", "phone"],
  ["21-suggestions-more", "#/harness/approve", "at=.stackq .q:nth-child(2)", "Suggestions to approve, scrolled: the memory pin and the inconclusive briefing change", "phone"],
  ["22-scorecard", "#/harness", "", "The scorecard: the week live from ChloeDB as gauges, two to a row", "phone"],
  ["23-scorecard-findings", "#/harness", "at=.tbl.find", "The scorecard scrolled to where the harness cost you, one finding a row", "phone"],
];

function serve() {
  return new Promise((resolve) => {
    const srv = http.createServer((req, res) => {
      const url = new URL(req.url, "http://x");
      const file = path.join(UI, path.normalize(decodeURIComponent(url.pathname)));
      if (!file.startsWith(UI) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) { res.writeHead(404); return res.end("not found"); }
      res.writeHead(200, { "content-type": TYPES[path.extname(file)] || "application/octet-stream" });
      fs.createReadStream(file).pipe(res);
    }).listen(0, "127.0.0.1", () => resolve(srv));
  });
}

(async () => {
  fs.mkdirSync(OUT, { recursive: true });
  const srv = await serve();
  const base = `http://127.0.0.1:${srv.address().port}/dev/harness.html`;
  const browser = await chromium.launch();
  const done = [];
  const errors = [];
  for (const dev of DEVICES) for (const theme of dev.themes) {
    const ctx = await browser.newContext({ viewport: dev.viewport, deviceScaleFactor: 2, colorScheme: theme, isMobile: !!dev.isMobile, hasTouch: !!dev.hasTouch });
    const page = await ctx.newPage();
    await page.clock.setFixedTime(new Date("2026-11-14T07:31:00"));
    page.on("pageerror", (e) => errors.push(`${dev.label} ${theme}: ${e.message}`));
    for (const [name, hash, query, what, only] of SHOTS) {
      if (ONLY.length && !ONLY.includes(name)) continue;
      if (only && only !== dev.label) continue;
      const q = new URLSearchParams(query); q.set("t", theme);
      await page.goto(`${base}?${q}${hash}`);
      await page.evaluate(() => document.fonts.ready);
      await page.waitForTimeout(500);
      if (dev.isMobile) {
        const wide = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
        if (wide) errors.push(`${dev.label} ${theme}: ${name} scrolls sideways`);
      }
      const file = path.join(OUT, `${name}${dev.key}-${theme}.png`);
      await page.screenshot({ path: file, fullPage: !!dev.full });
      done.push([path.basename(file), dev.isMobile ? `Phone, 390x844, the first screen. ${what}` : `Desktop, 1440 wide, the whole page. ${what}`]);
    }
    await ctx.close();
  }
  await browser.close();
  srv.close();
  if (!ONLY.length && DEVICES.length === 2) {
    const lines = done.sort((a, b) => a[0].localeCompare(b[0])).map(([f, w]) => `${f}  ${w}`);
    fs.writeFileSync(path.join(OUT, "ready.txt"), `Workbench harness mocks (chloedb-workbench/HARNESS.md), in style direction C, Instrument: desktop at 1440x900 (whole page, light and dark) and phone at 390x844 (*-phone-*, light), all at 2x.\n\n${lines.join("\n")}\n`);
  }
  for (const e of errors) console.error("page error:", e);
  console.log(`${done.length} screenshots in ${OUT}`);
})();
