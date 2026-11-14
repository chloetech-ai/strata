// Screenshots of the journeys mock (JOURNEYS.md, "The mocks"): serves ui/
// itself, opens each shot on a desktop (1440x900) and on a phone (390x844,
// touch), both at 2x, in dark and light, with the clock fixed at 7:31 on
// 14 Nov 2026, and writes <out>/<name>-<theme>.png, <out>/<name>-phone-<theme>.png
// and <out>/ready.txt.
//   node chloedb-workbench/ui/dev/journeys/shoot.cjs OUT_DIR [--desktop|--phone] [name ...]
const http = require("http");
const fs = require("fs");
const path = require("path");
const { chromium } = require("playwright");

const UI = path.resolve(__dirname, "..", "..");
const OUT = path.resolve(process.argv[2] || "journeys-shots");
const ARGS = process.argv.slice(3);
const ONLY = ARGS.filter((a) => !a.startsWith("--"));
const DEVICES = [
  { key: "", label: "desktop", viewport: { width: 1440, height: 900 } },
  { key: "-phone", label: "phone", viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true },
].filter((d) => !ARGS.includes("--desktop") && !ARGS.includes("--phone") || ARGS.includes(`--${d.label}`));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".css": "text/css", ".md": "text/markdown" };

// [name, route, query, what it shows]
const SHOTS = [
  ["01-home", "#/", "", "Home: “What do you want to do?”, the four goals, projects by activity with Continue and Review, Needs you now with Allow and Deny"],
  ["02-first-run", "#/", "first=1", "First run: no project yet; open a folder (completion) or start a new project; the setup line"],
  ["03-new-what", "#/new/what", "", "New project, steps 2 and 3: what to build (with a done check), a lead agent or a single agent, Plan it"],
  ["04-new-plan", "#/new/plan", "", "New project, step 4: the lead's editable plan, and what each agent starts with (ChloeCtx wired, models, permissions, branch)"],
  ["05-continue", "#/p/shop", "", "Continue shop: since you left, agents with approvals inline, what changed, decisions made, open threads, Ask about this project"],
  ["06-understand", "#/p/shop/understand", "", "Continue › Understand: ask about the code, the real Map (map.js), what is known with the conflict first, rules, history"],
  ["07-start-change", "#/p/shop/change", "", "Continue › Start a change: the composer (done check, rule, overlap), may do without asking, model, then the work in progress"],
  ["08-agent", "#/a/c3e1a002", "", "An agent's page: its question first (full command, Claude Code's own options), then Progress (the real story.js)"],
  ["09-review-list", "#/p/shop/ship", "", "Review and ship, step 1: what is ready, with tests on the branch, merges cleanly, decisions to confirm, merge order"],
  ["10-review-check", "#/p/shop/ship/c3e1a001", "", "Review and ship, steps 2 and 3: checks, the decision to confirm, the diff, blast radius, then Merge or Send back with a note"],
  ["11-debug-pick", "#/debug", "", "Debug context and project, step 1: pick a session and a turn (turn 2 flagged)"],
  ["12-debug-knew", "#/debug/c3e1a004/2", "", "Debug, step 2: what freeship knew at turn 2 (prompt, files, tokens, memory injected) and what it missed"],
  ["13-debug-wrong", "#/debug/c3e1a004/2/wrong", "", "Debug, steps 3 and 4: the diff where it went wrong, and the fixes (pin the record, lower the threshold, send back)"],
  ["14-debug-code", "#/debug/code", "", "Debug from “why the code is like this”: the real Trace (trace.js) on money rounding"],
  ["15-switcher", "#/p/shop", "open=agents", "The agent switcher (a): grouped Needs you, Working, Ready to review, Finished; terminal one click away; Resume"],
  ["17-dense-ship", "#/dense/ship", "", "At scale (38 files, 186 turns), Ship: a one-line verdict, rings of functions, callers and public names, only the exceptions, pieces of work, decisions that change something"],
  ["18-dense-diff", "#/dense/diff", "", "At scale, the diff: 5 pieces of work riskiest first, each in words with its one hunk that matters, +/− as bars, mechanical changes folded"],
  ["19-dense-session", "#/dense/session", "", "At scale, a session: one tick per turn in named phases, 6 of 186 turns that stand out, memory as counts plus the top and the missed record, “41% full”"],
  ["20-dense-tests", "#/dense/tests", "", "At scale, tests (Health): “214 pass” on one line, a strip of the last runs, rows only for failing, flaky and untested"],
  ["21-dense-continue", "#/dense/continue", "", "At scale, Continue (today's Away) with 9 agents: Needs you open, Working one line each, Finished folded; what changed by piece; 2 decisions that matter, 31 folded; time waiting on you as a bar"],
  ["22-scale-rules", "#/dense/rules", "", "The scale rules every dense view follows"],
  ["16-needs-you", "#/p/shop/understand", "open=needs", "The Needs-you drawer (.): one count, full commands, Claude Code's own numbered options, alerts that ask you"],
  ["01b-home-needs", "#/", "at=.jh-now", "Phone: Home scrolled to Needs you now, with Allow once and Deny the full width and the whole command", "phone"],
  ["10b-review-check-pieces", "#/p/shop/ship/c3e1a001", "at=.jz-piece", "Phone: Review scrolled to the pieces of work, +/− printed beside each bar, the hunk wrapped", "phone"],
  ["13b-debug-wrong-fix", "#/debug/c3e1a004/2/wrong", "at=.jd-fix", "Phone: Debug's fixes (pin the record, lower the threshold, make it a rule, send back)", "phone"],
  ["23-terminal", "#/a/c3e1a002/terminal", "at=.ja-tabs", "An agent's terminal: keys go to the agent (desktop); read only with a Type button (phone)"],
  ["24-terminal-type", "#/a/c3e1a002/terminal", "type=1&at=.ja-tabs", "Phone: the terminal after Type, an input bar with Esc, Tab, arrows, 1 to 3 and Ctrl-C", "phone"],
  ["25-more", "#/", "open=more", "Phone: More as a bottom sheet over the tab bar", "phone"],
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
  const base = `http://127.0.0.1:${srv.address().port}/dev/journeys.html`;
  const browser = await chromium.launch();
  const done = [];
  const errors = [];
  for (const dev of DEVICES) for (const theme of ["dark", "light"]) {
    const ctx = await browser.newContext({ viewport: dev.viewport, deviceScaleFactor: 2, colorScheme: theme, isMobile: !!dev.isMobile, hasTouch: !!dev.hasTouch });
    const page = await ctx.newPage();
    await page.clock.setFixedTime(new Date("2026-11-14T07:31:00"));
    page.on("pageerror", (e) => errors.push(`${dev.label} ${theme}: ${e.message}`));
    for (const [name, hash, query, what, only] of SHOTS) {
      if (ONLY.length && !ONLY.includes(name)) continue;
      if (only && only !== dev.label) continue;
      const q = new URLSearchParams(query); q.set("theme", theme);
      await page.goto(`${base}?${q}${hash}`);
      await page.waitForTimeout(900);
      if (dev.isMobile) {
        const wide = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1 || [...document.querySelectorAll(".jp-scroll")].some((e) => e.scrollWidth > e.clientWidth + 1));
        if (wide) errors.push(`${dev.label} ${theme}: ${name} scrolls sideways`);
      }
      const file = path.join(OUT, `${name}${dev.key}-${theme}.png`);
      await page.screenshot({ path: file });
      done.push([path.basename(file), dev.isMobile ? `Phone, 390x844. ${what}` : what]);
    }
    await ctx.close();
  }
  await browser.close();
  srv.close();
  if (!ONLY.length && DEVICES.length === 2) {
    const lines = done.sort((a, b) => a[0].localeCompare(b[0])).map(([f, w]) => `${f}  ${w}`);
    fs.writeFileSync(path.join(OUT, "ready.txt"), `Workbench journeys mocks (chloedb-workbench/JOURNEYS.md): desktop at 1440x900 and phone at 390x844 (*-phone-*), both at 2x, dark and light.\n\n${lines.join("\n")}\n`);
  }
  for (const e of errors) console.error("page error:", e);
  console.log(`${done.length} screenshots in ${OUT}`);
})();
