// Rules and notifications (JOURNEYS.md): record the harness's fixture from a
// real server, and shoot the harness.
//
//   node ui/dev/rules-notify-shoot.cjs --record target/ci/chloedb-workbench
//       starts `serve` with a stand-in `claude` (it only sleeps: the hooks are
//       posted as Claude Code would post them), plays a morning of prompts
//       on a repository named shop, and writes ui/dev/fixtures/rules-notify.json
//       from the API's own answers (the temporary paths renamed).
//   node ui/dev/rules-notify-shoot.cjs OUT_DIR
//       serves ui/ and writes OUT_DIR/P-<screen>-<desktop|phone>-<light|dark>.png
//       and lines for OUT_DIR/ready.txt; says when a phone page scrolls sideways.
const fs = require("fs");
const path = require("path");
const os = require("os");
const http = require("http");
const { spawn, execFileSync } = require("child_process");

const UI = path.resolve(__dirname, "..");
const FIXTURE = path.join(__dirname, "fixtures", "rules-notify.json");
const FONTS = process.env.FONTS_DIR || "/mnt/project-files/workbench/style/mocks/fonts";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function record(bin) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "rn-"));
  const repo = path.join(tmp, "shop");
  fs.mkdirSync(repo);
  const git = (...a) => execFileSync("git", ["-C", repo, "-c", "user.name=Dev", "-c", "user.email=dev@example.com", "-c", "commit.gpgsign=false", ...a]);
  git("init", "-q", "-b", "main");
  fs.writeFileSync(path.join(repo, "README.md"), "shop\n");
  git("add", "."); git("commit", "-q", "-m", "first");
  const fake = path.join(tmp, "claude");
  fs.writeFileSync(fake, "#!/bin/sh\nexec sleep 100000\n", { mode: 0o755 });
  const srv = spawn(bin, ["serve", "--addr", "127.0.0.1:0", "--token", "dev", "--data-dir", path.join(tmp, "data"), "--claude-bin", fake, "--no-chloectx"], { stdio: ["ignore", "pipe", "inherit"] });
  const url = await new Promise((res) => srv.stdout.once("data", (d) => res(String(d).trim().replace(/^Workbench: /, "").split("/?token=")[0])));
  const call = async (method, p, body, agent) => {
    const r = await fetch(url + p, { method, headers: { authorization: "Bearer dev", "content-type": "application/json", ...(agent ? { "x-workbench-agent": agent } : {}) }, body: body ? JSON.stringify(body) : undefined });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(`${method} ${p}: ${r.status} ${JSON.stringify(j)}`);
    return j;
  };
  const real = fs.realpathSync(repo);
  const ids = {};
  for (const name of ["cents", "paybutton", "freeship"]) ids[name] = (await call("POST", "/api/agents", { repo: real, name, worktree: false })).agent.id;
  await sleep(1500);
  let n = 0;
  const hook = (who, body) => call("POST", "/api/hook", body, ids[who]);
  async function prompt(who, command, { ok = true, wait = 2000 } = {}) {
    const u = `tu${++n}`;
    await hook(who, { hook_event_name: "PreToolUse", tool_name: "Bash", tool_use_id: u, tool_input: { command, description: "Run it" } });
    await hook(who, { hook_event_name: "Notification", notification_type: "permission_prompt", message: "Claude needs your permission to use Bash" });
    if (ok == null) return;
    await sleep(wait);
    if (ok) await hook(who, { hook_event_name: "PostToolUse", tool_name: "Bash", tool_use_id: u, tool_input: { command }, tool_response: { stdout: "ok" } });
    else await hook(who, { hook_event_name: "UserPromptSubmit", prompt: "no, not that" });
  }
  async function ran(who, command) {
    const u = `tr${++n}`;
    await hook(who, { hook_event_name: "PreToolUse", tool_name: "Bash", tool_use_id: u, tool_input: { command } });
    await hook(who, { hook_event_name: "PostToolUse", tool_name: "Bash", tool_use_id: u, tool_input: { command }, tool_response: { stdout: "ok" } });
  }
  const tests = ["tests/test_promo.py -q", "tests/test_money.py -q", "tests/test_cart.py", "-q", "tests/test_promo.py -k rounding", "tests/test_money.py"];
  for (let i = 0; i < 12; i++) await prompt(["cents", "paybutton", "freeship"][i % 3], `python3 -m pytest ${tests[i % tests.length]}`, { wait: 1500 + (i % 4) * 900 });
  await prompt("paybutton", "python3 -m orders.migrate --db dev.sqlite3", { wait: 3000 });
  await prompt("freeship", "git push origin freeship", { ok: false, wait: 1200 });
  for (let i = 0; i < 3; i++) await prompt("cents", "npm run lint", { wait: 1200 });
  await ran("cents", "python3 -m pytest tests/test_migrations.py --create-db");
  await ran("freeship", "python3 -m pytest -q > /tmp/pytest.log");
  // Grants: the narrower rule from a grant sheet, and one Claude Code wrote.
  await call("POST", "/api/rules/grants", { repo: real, pattern: "Bash(npm run lint)" });
  fs.mkdirSync(path.join(real, ".claude"), { recursive: true });
  fs.writeFileSync(path.join(real, ".claude", "settings.local.json"), JSON.stringify({ permissions: { allow: ["Bash(python3:*)"] } }));
  const enc = encodeURIComponent;
  const out = { repo: real };
  out.suggested = await call("GET", `/api/rules/suggested?repo=${enc(real)}`);
  out.grants = await call("GET", `/api/rules/grants?repo=${enc(real)}`);
  out.check = await call("GET", `/api/rules/check?repo=${enc(real)}&agent=${ids.paybutton}&pattern=${enc("Bash(python3:*)")}&command=${enc("python3 -m orders.migrate --db dev.sqlite3")}`);
  // An open prompt, a project that notifies every question, an alert.
  await prompt("paybutton", "python3 -m orders.migrate --db dev.sqlite3", { ok: null });
  for (const who of ["cents", "freeship"]) await hook(who, { hook_event_name: "PostToolUse", tool_name: "Edit", tool_input: { file_path: path.join(real, "shop/money.py"), old_string: "a", new_string: "b" }, tool_response: {} });
  await call("POST", "/api/alerts/check", {});
  await call("PUT", "/api/notify/settings", { repo: real, level: "all" });
  out.settings = await call("GET", "/api/notify/settings");
  out.pending = await call("GET", "/api/notify/pending");
  const open = await call("GET", `/api/questions?agent=${ids.paybutton}&state=open`);
  // The options Claude Code's screen shows for it (a hook alone does not carry them).
  out.question = { ...open.questions[0], options: [
    { label: "Yes", n: 1, no: false },
    { label: `Yes, and don't ask again for python3 commands in ${real}`, n: 2, no: false, grants: "stops it asking again for commands like this one" },
    { label: "No, and tell Claude what to do differently (esc)", n: 3, no: true },
  ] };
  out.agent = (await call("GET", `/api/agents/${ids.paybutton}`)).agent;
  await call("POST", "/api/notify/quiet", { minutes: 25 });
  await prompt("freeship", "python3 scripts/seed_coupons.py --env staging", { ok: null });
  out.pending_quiet = await call("GET", "/api/notify/pending");
  await call("POST", "/api/alerts/rules", { repo: real, name: "Payments sandbox down", sql: "SELECT kind AS entity, COUNT(*) AS v FROM events WHERE ts >= :since AND kind = 'spawned' GROUP BY kind", cmp: ">=", threshold: 1, window_s: 3600, severity: "critical", action: "notify" });
  await call("POST", "/api/alerts/check", {});
  await sleep(2000);
  await call("POST", "/api/notify/quiet", { minutes: 0 });
  out.pending_digest = await call("GET", "/api/notify/pending");
  srv.kill("SIGTERM");
  let text = JSON.stringify(out, null, 1);
  text = text.split(real).join("/home/you/src/shop").split(path.join(tmp, "data")).join("/home/you/.local/share/chloedb-workbench").split(repo).join("/home/you/src/shop");
  fs.writeFileSync(FIXTURE, text + "\n");
  console.log(`wrote ${FIXTURE}`);
}

const SHOTS = [
  ["rules", "More › Rules: the suggestions, each with what it would have allowed, and every grant with Revoke"],
  ["grant", "The grant sheet over Rules: the exact rule and file, what it would also allow (risky first), the narrower choice"],
  ["card", "Start/Continue's card (\"12 of 14 waits were …\") over the question card whose wide option opens the sheet"],
  ["notify", "Notifications: Notify me, quiet (on, holding), a level per project"],
  ["digest", "The digest after quiet: one summary, what still waits, the alerts"],
];
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".cjs": "text/javascript", ".css": "text/css", ".json": "application/json", ".woff2": "font/woff2" };

async function shoot(outDir) {
  const { chromium } = require("playwright");
  fs.mkdirSync(outDir, { recursive: true });
  const server = http.createServer((req, res) => {
    const u = decodeURIComponent(req.url.split("?")[0]);
    const file = u.startsWith("/fonts/") ? path.join(FONTS, u.slice(7)) : path.join(UI, u);
    fs.readFile(file, (e, b) => {
      if (e) { res.writeHead(404); res.end(); return; }
      res.writeHead(200, { "content-type": TYPES[path.extname(file)] || "application/octet-stream" });
      res.end(b);
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const base = `http://127.0.0.1:${server.address().port}`;
  const browser = await chromium.launch();
  const ready = [];
  for (const [name, what] of SHOTS) for (const dev of [{ k: "desktop", viewport: { width: 1440, height: 900 } }, { k: "phone", viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true }]) for (const t of ["light", "dark"]) {
    const page = await browser.newPage({ viewport: dev.viewport, deviceScaleFactor: 2, isMobile: !!dev.isMobile, hasTouch: !!dev.hasTouch, colorScheme: t });
    const errs = [];
    page.on("pageerror", (e) => errs.push(String(e)));
    await page.goto(`${base}/dev/rules-notify.html?v=${name}&t=${t}`);
    await page.evaluate(() => window.__ready && window.__ready());
    await page.evaluate(() => document.fonts.ready);
    const wide = await page.evaluate(() => document.scrollingElement.scrollWidth - window.innerWidth);
    const file = `P-${name}-${dev.k}-${t}.png`;
    // Headless Chromium now and then fails a capture ("Unable to capture
    // screenshot"); a second try takes it.
    for (let i = 0; ; i++) {
      try { await page.screenshot({ path: path.join(outDir, file), fullPage: name === "rules" || (name === "card" && dev.k === "desktop") }); break; }
      catch (e) { if (i >= 2) throw e; await sleep(500); }
    }
    if (errs.length) console.log(`${file}: page errors: ${errs.join("; ")}`);
    if (wide > 0) console.log(`${file}: scrolls sideways by ${wide}px`);
    ready.push(`${file}  ${what}`);
    await page.close();
  }
  await browser.close();
  server.close();
  fs.appendFileSync(path.join(outDir, "ready.txt"), ready.join("\n") + "\n");
  console.log(ready.join("\n"));
}

const a = process.argv.slice(2);
(a[0] === "--record" ? record(path.resolve(a[1])) : shoot(path.resolve(a[0] || "rules-notify-shots"))).catch((e) => { console.error(e); process.exit(1); });
