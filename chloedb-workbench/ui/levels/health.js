// Health (SPEC.md, "Fourth build", rows 4 and 5): the fleet watched by
// rules that are SQL over Workbench's own tables, and every test run the
// agents made. Three tabs:
//   Incidents: firings grouped by agent, file or repository, each with a
//              board (its firings, what the agents say now, the events
//              around it, the failing tests, what ChloeCtx knows of its
//              files) and actions: pause the agent, ask me, resolve.
//   Rules:     the rules as rows; edit the SQL and test it now.
//   Tests:     the flaky test radar: flaky, newly failing, slow, and each
//              test's runs with p50/p99. Sixth build: failures fixed in
//              the same session are not counted against the pass rate, a
//              test says how long it has been stable, "Run it N times"
//              runs it again (POST /api/tests/rerun), and commands that
//              printed only a count say so instead of making up a test.
// Routes: #/health, #/health/rules, #/health/tests, #/health/<incident id>.
import {
  h, useStyle, avatar, agentChip, ctxChip, pill, empty, fmtDur, fmtClock, fmtAgo, fmtNum, fmtSpan, toMs, tooltip,
  repoOf, throttle, md, setFleet, routeParam, keyNav, later, inlineCode,
} from "./common.js";

const enc = encodeURIComponent;
const TABS = [["incidents", "Incidents"], ["rules", "Rules"], ["tests", "Tests"]];
const ACTIONS = [
  ["notify", "Notify", "an item in Needs you"],
  ["ask", "Ask me", "a question in Decide"],
  ["pause", "Pause the agent", "Esc in its terminal"],
  ["none", "Nothing", "only the incident"],
];
const WINDOWS = [[600, "10 min"], [1800, "30 min"], [3600, "1 h"], [6 * 3600, "6 h"], [86400, "24 h"]];
const CMPS = [">=", ">", "<=", "<", "=", "!="];
const TABLES = {
  events: "id, seq, ts, agent, kind, tool, summary, data",
  agents: "id, name, repo, cwd, branch, task, parent, state, state_since, narration, model, goal, outcome …",
  test_runs: "id, run, repo, agent, \"commit\", tree, runner, command, test, outcome, passed, ms, own_ms, run_ms, exit_code, error, use_id, ts, seq",
  file_edits: "id, repo, agent, file, tool, ts, seq",
  token_use: "id, repo, agent, input, output, cache_read, ts",
  questions: "id, agent, repo, kind, text, state, created, answered …",
  outcomes: "id, goal, title, state, agent, progress …",
};
// Rules to start a new one from: [label, name, sql, cmp, threshold, window_s, severity].
const TEMPLATES = [
  ["A test failing twice in an hour", "A test failing twice in an hour",
    "SELECT test AS entity, repo, test AS detail, MAX(agent) AS agent, COUNT(*) AS v\nFROM test_runs\nWHERE outcome = 'failed' AND ts >= :since\nGROUP BY repo, test", ">=", 2, 3600, "major"],
  ["An agent's tools failing", "An agent's tools failing 3 times in 30 min",
    "SELECT agent AS entity, agent, COUNT(*) AS v\nFROM events\nWHERE kind = 'PostToolUseFailure' AND ts >= :since\nGROUP BY agent", ">=", 3, 1800, "minor"],
  ["A file edited by several agents", "A file edited by 3 agents in an hour",
    "SELECT file AS entity, repo, file, COUNT(DISTINCT agent) AS v\nFROM file_edits\nWHERE ts >= :since\nGROUP BY repo, file", ">=", 3, 3600, "minor"],
];
const nameOf = (a) => (a && (a.name || a.id)) || "?";
const sevTone = (s) => (s === "major" || s === "critical" ? "crit" : "warn");
/** 4 ms, 640 ms, 3.4 s, 1 m 16 s. */
const fmtMs = (ms) => (ms == null ? "" : ms < 1000 ? `${Math.round(ms)} ms` : fmtSpan(ms));
const pct = (x) => (x == null ? "" : `${Math.round(x * 100)}%`);

// Hotspots: a fourth tab (ui/levels/hotspots.js).
import { withHotspots } from "./hotspots.js";
export const mount = withHotspots(mountHealth);
function mountHealth(root, ctx) {
  const first = routeParam(ctx, "agent");
  const S = {
    tab: first && TABS.some(([k]) => k === first) ? first : "incidents",
    sel: first && /^i[0-9a-f]{8}$/.test(first) ? first : null,
    inc: null, incErr: null, boards: new Map(), showResolved: false,
    rules: null, rulesErr: null, ruleSel: null, draft: null, result: null, testing: false, saving: false, ruleMsg: null,
    tests: null, testsErr: null, filter: "", only: "all", testSel: null, hist: new Map(), reruns: new Map(), rerunN: 10, rrCmd: new Map(), rrCheckout: new Map(),
    acting: new Set(), actErr: null, agents: new Map(),
  };
  if (S.sel) S.tab = "incidents";
  const tip = tooltip();
  const el = h("div", { class: "lv-screen hl" });
  root.append(el);
  let alive = true;
  const render = () => { if (alive) draw(); };
  const renderSoon = later(render, 40);

  const agentOf = (id) => S.agents.get(id) || (ctx.state?.agents || []).find((a) => a.id === id) || { id, name: id };
  function learn(list) {
    for (const a of list || []) if (a && a.id) S.agents.set(a.id, a);
    setFleet([...S.agents.values(), ...(ctx.state?.agents || [])].filter((a, i, all) => all.findIndex((b) => b.id === a.id) === i));
  }
  const repoQ = () => { const r = repoOf(ctx); return r ? `repo=${enc(r)}` : ""; };

  // -------------------------------------------------------------- data
  async function loadIncidents() {
    try {
      const d = await ctx.api(`/api/incidents?state=all${repoQ() ? "&" + repoQ() : ""}`);
      S.inc = d; S.incErr = null;
      learn(d.agents);
      for (const i of d.incidents || []) learn(i.agents);
      const open = (d.incidents || []).filter((i) => i.state === "open");
      if (!S.sel || !(d.incidents || []).some((i) => i.id === S.sel)) S.sel = (open[0] || (d.incidents || [])[0] || {}).id || null;
      if (S.sel) await loadBoard(S.sel);
    } catch (e) { S.incErr = e.message || String(e); }
    render();
  }
  async function loadBoard(id) {
    try {
      const d = await ctx.api(`/api/incidents/${enc(id)}`);
      S.boards.set(id, d);
      learn(d.incident?.agents);
      learn((d.board?.sentences || []).map((s) => s.agent));
    } catch (e) { S.boards.set(id, { error: e.message || String(e) }); }
  }
  async function loadRules() {
    try {
      const d = await ctx.api(`/api/alerts/rules${repoQ() ? "?" + repoQ() : ""}`);
      S.rules = d; S.rulesErr = null;
      if (!S.ruleSel || !(d.rules || []).some((r) => r.id === S.ruleSel)) pickRule((d.rules || [])[0] || null);
    } catch (e) { S.rulesErr = e.message || String(e); }
    render();
  }
  async function loadTests() {
    try {
      const d = await ctx.api(`/api/tests${repoQ() ? "?" + repoQ() : ""}`);
      S.tests = d; S.testsErr = null;
      learn(d.agents);
    } catch (e) { S.testsErr = e.message || String(e); }
    render();
  }
  async function loadHist(test) {
    if (S.hist.has(test)) return;
    S.hist.set(test, null);
    try {
      const q = new URLSearchParams({ test });
      const r = repoOf(ctx); if (r) q.set("repo", r);
      const d = await ctx.api(`/api/tests/history?${q}`);
      learn(d.agents);
      S.hist.set(test, d);
    } catch (e) { S.hist.set(test, { error: e.message || String(e) }); }
    render();
  }
  // Run it N times: Workbench runs the test itself, then reads it again.
  async function rerun(test, n, opts = {}) {
    S.reruns.set(test, { state: "starting", n, done: 0 });
    render();
    try {
      const body = { test, n };
      if (opts.command) body.command = opts.command;
      if (opts.checkout) body.checkout = opts.checkout;
      const r = repoOf(ctx); if (r) body.repo = r;
      const d = await ctx.api("/api/tests/rerun", { method: "POST", body });
      S.reruns.set(test, d.rerun);
      followRerun(test, d.rerun.id);
    } catch (e) { S.reruns.set(test, { state: "failed", n, done: 0, error: e.message || String(e) }); }
    render();
  }
  async function followRerun(test, id) {
    while (alive) {
      await new Promise((res) => setTimeout(res, 1000));
      let d;
      try { d = (await ctx.api(`/api/tests/rerun/${enc(id)}`)).rerun; } catch (e) { S.reruns.set(test, { ...S.reruns.get(test), state: "failed", error: e.message || String(e) }); render(); return; }
      S.reruns.set(test, d);
      if (d.state !== "running") { S.hist.delete(test); await loadHist(test); await loadTests(); return; }
      render();
    }
  }
  function loadTab() {
    if (S.tab === "incidents") loadIncidents();
    else if (S.tab === "rules") loadRules();
    else loadTests();
  }
  const refresh = throttle(() => { if (S.tab === "incidents") loadIncidents(); else if (S.tab === "tests") loadTests(); else if (!S.draft?.dirty) loadRules(); }, 10000);

  function setTab(t) {
    S.tab = t;
    const hash = t === "incidents" ? (S.sel ? `#/health/${S.sel}` : "#/health") : `#/health/${t}`;
    try { if (location.hash.startsWith("#/health")) history.replaceState(history.state, "", hash); } catch {}
    loadTab();
    render();
  }

  // ------------------------------------------------------------ render
  function draw() {
    tip.hide();
    const counts = S.inc?.counts || {};
    const head = h("header", { class: "lv-head hl-head" },
      h("h1", { class: "lv-h1" }, "Health"),
      h("span", { class: "lv-sub" }, "rules are SQL over Workbench's own tables, checked every 30 s · every test run the agents made"),
      h("div", { class: "hl-tabs", role: "tablist", "aria-label": "Health" }, TABS.map(([k, label]) => h("button", {
        role: "tab", "aria-selected": S.tab === k ? "true" : "false", class: S.tab === k ? "on" : "", "data-key": `tab:${k}`,
        onclick: () => setTab(k),
        onkeydown: (e) => {
          const i = TABS.findIndex(([x]) => x === k), d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
          if (d) { e.preventDefault(); setTab(TABS[(i + d + TABS.length) % TABS.length][0]); requestAnimationFrame(() => el.querySelector(".hl-tabs .on")?.focus()); }
        },
      }, label,
        k === "incidents" && counts.open ? h("b", { class: "hl-tn crit" }, String(counts.open)) : null,
        k === "rules" && S.rules ? h("b", { class: "hl-tn" }, String((S.rules.rules || []).length)) : null,
        k === "tests" && S.tests?.totals?.flaky ? h("b", { class: "hl-tn warn" }, String(S.tests.totals.flaky)) : null))),
      evalNote());
    let body;
    if (S.tab === "incidents") body = incidentsTab();
    else if (S.tab === "rules") body = rulesTab();
    else body = testsTab();
    el.replaceChildren(head, body);
  }

  function evalNote() {
    const src = S.inc || S.rules;
    const last = src?.last_eval_ms;
    return h("span", { class: "hl-eval" }, h("i", { class: "hl-live" }), last ? `rules ran ${fmtAgo(last)}` : "rules run every 30 s");
  }

  // ------------------------------------------------------- incidents
  function incidentsTab() {
    if (!S.inc) return S.incErr ? errorBox(S.incErr, loadIncidents) : empty("Reading the incidents…");
    const all = S.inc.incidents || [];
    const open = all.filter((i) => i.state === "open");
    const done = all.filter((i) => i.state !== "open");
    const shown = S.showResolved ? done : done.slice(0, 4);
    const list = h("aside", { class: "hl-list" },
      h("h2", { class: "lv-h" }, "Open", h("span", { class: "n" }, `· ${open.length}`)),
      open.length ? open.map(incCard) : h("div", { class: "hl-calm" }, h("b", null, "All quiet."), " No rule fires now."),
      done.length ? [h("h2", { class: "lv-h hl-h-res" }, "Resolved", h("span", { class: "n" }, `· ${done.length}`)),
        shown.map(incCard),
        done.length > shown.length ? h("button", { class: "btn sm ghost hl-more", onclick: () => { S.showResolved = true; render(); } }, `${done.length - shown.length} more`) : null] : null);
    const sel = all.find((i) => i.id === S.sel);
    return h("div", { class: "hl-inc" }, list, sel ? boardView(sel) : h("section", { class: "lv-card hl-board" }, empty(all.length ? "Pick an incident." : "No incident yet. When a rule fires, its firings group here by agent, file or repository, each with a board.")));
  }

  function incCard(i) {
    const on = i.id === S.sel;
    const asked = (i.actions || []).find((a) => a.kind === "ask" && a.state === "open");
    return h("button", {
      class: `hl-ic ${i.state}${on ? " on" : ""}`, "data-nav": "", "data-key": `inc:${i.id}`, "aria-pressed": on ? "true" : "false",
      onclick: async () => { S.sel = i.id; try { history.replaceState(history.state, "", `#/health/${i.id}`); } catch {} render(); if (!S.boards.has(i.id)) { await loadBoard(i.id); render(); } },
    },
      h("i", { class: `hl-sev ${i.state === "open" ? sevTone(i.severity) : "done"}` }),
      h("div", { class: "hl-ic-b" },
        h("div", { class: "hl-ic-t" }, i.title),
        h("div", { class: "hl-ic-m" },
          h("span", { class: "hl-avs" }, (i.agents || []).slice(0, 4).map((a) => avatar(a, 16))),
          h("span", null, i.state === "open" ? `${fmtDur(Date.now() - i.started_ms)} · ${i.firing_now} firing` : `resolved ${fmtAgo(i.resolved_ms)}`),
          asked ? h("span", { class: "hl-asked" }, "asked you") : i.needs_you ? h("span", { class: "hl-nu" }, "needs you") : null)));
  }

  function boardView(i) {
    const d = S.boards.get(i.id);
    const b = d?.board;
    const inc = d?.incident || i;
    const agents = inc.agents || [];
    // The agent to pause: never one whose branch fixed its test.
    const fixers = new Set(inc.fixed_by || []);
    const lead = agents.find((a) => !fixers.has(a.id));
    const fixer = agents.find((a) => fixers.has(a.id));
    const busy = (k) => S.acting.has(`${i.id}:${k}`);
    const open = inc.state === "open";
    const head = h("div", { class: "hl-bh" },
      h("div", { class: "hl-bh-l" },
        pill(open ? (inc.severity || "minor") : "resolved", open ? sevTone(inc.severity) : "good"),
        h("span", { class: "lv-sub" }, open ? `open · started ${fmtClock(inc.started_ms)}, ${fmtAgo(inc.started_ms)} · ${inc.firing_now} rule${inc.firing_now === 1 ? "" : "s"} firing` : `${fmtClock(inc.started_ms)} – ${fmtClock(inc.resolved_ms)} · lasted ${fmtDur(inc.resolved_ms - inc.started_ms)}`)),
      open ? h("div", { class: "hl-acts" },
        fixer ? h("a", { class: "btn sm", href: `#/ship/${enc(fixer.id)}`, title: `${nameOf(fixer)}'s branch passes it: review and merge it` }, `Review ${nameOf(fixer)}'s fix`) : null,
        lead && lead.running !== false ? h("button", { class: "btn sm warn", disabled: busy("pause") || null, title: `Send Esc to ${nameOf(lead)}: it stops what it is doing and waits`, onclick: () => act(i, "pause", lead.id) }, busy("pause") ? "Pausing…" : `Pause ${nameOf(lead)}`) : null,
        h("button", { class: "btn sm", disabled: busy("ask") || null, title: "A question in Decide, with what to tell the agent", onclick: () => act(i, "ask", (lead || fixer)?.id) }, busy("ask") ? "Asking…" : "Ask me in Decide"),
        h("button", { class: "btn sm", disabled: busy("resolve") || null, onclick: () => resolve(i) }, busy("resolve") ? "Resolving…" : "Resolve")) : null);
    if (!d) return h("section", { class: "lv-card hl-board" }, head, h("h2", { class: "hl-bt" }, inc.title), empty("Reading its board…"));
    if (d.error) return h("section", { class: "lv-card hl-board" }, head, h("h2", { class: "hl-bt" }, inc.title), h("div", { class: "err" }, d.error));
    return h("section", { class: "lv-card hl-board", "aria-label": `Incident: ${inc.title}` },
      head,
      h("h2", { class: "hl-bt" }, inc.title),
      inc.files?.length ? h("div", { class: "hl-files" }, inc.files.map((f) => h("code", { class: "lv-code" }, f))) : null,
      whereCard(inc),
      S.actErr ? h("div", { class: "err" }, S.actErr) : null,
      sentences(b.sentences || []),
      h("div", { class: "hl-bgrid" },
        h("div", { class: "hl-bcol" }, firingsCard(b.firings || inc.firings || []), b.error ? errorCard(b.error) : null, (b.tests || []).length ? testsCard(b.tests) : null, actionsCard(inc)),
        h("div", { class: "hl-bcol" }, eventsCard(b, inc), relatedCard(b))),
      (b.missing || []).length ? h("div", { class: "hl-missing" }, b.missing.join(" ")) : null);
  }

  // Where its tests pass and fail now, checkout by checkout.
  function whereCard(inc) {
    const ws = inc.where || [];
    if (!ws.length) return null;
    return h("div", { class: "hl-where" }, ws.map((w) => h("div", { class: "hl-where-r" },
      h("code", { class: "lv-code" }, w.test), " ", (w.fixed_by || []).length ? pill("fixed on a branch", "good") : null, " ", w.sentence)));
  }

  function sentences(list) {
    if (!list.length) return null;
    return h("div", { class: "hl-says" }, list.map((s) => {
      const a = s.agent || {};
      return h("div", { class: "hl-say" },
        avatar(a, 26),
        h("div", { class: "hl-say-b" },
          h("div", { class: "hl-say-h" }, agentChip(a, ctx), pill(s.state === "waiting" ? "waiting for you" : s.state || "", s.state === "working" ? "acc" : s.state === "waiting" ? "warn" : "mut"),
            h("span", { class: "lv-sub" }, s.narration_ts ? fmtAgo(s.narration_ts) : "")),
          h("div", { class: "hl-say-t" }, s.narration ? `“${s.narration}”` : h("span", { class: "lv-sub" }, "No sentence yet."))),
        h("div", { class: "hl-say-a" },
          h("button", { class: "btn sm ghost", onclick: () => ctx.go(`#/story/${a.id}`) }, "Story"),
          h("button", { class: "btn sm ghost", onclick: () => ctx.zoom(a.id) }, "Terminal")));
    }));
  }

  function firingsCard(fs) {
    return h("div", { class: "hl-sec" },
      h("h3", { class: "lv-h" }, "Firings", h("span", { class: "n" }, `· ${fs.length}`)),
      h("ul", { class: "hl-fl" }, fs.map((f) => {
        const ratio = f.threshold ? Math.min(1.6, f.value / f.threshold) : 1;
        return h("li", { class: `hl-f ${f.state}` },
          h("div", { class: "hl-f-h" }, h("b", null, f.rule_name), f.state === "firing" ? pill("firing", sevTone(f.severity)) : pill(f.state === "silenced" ? "silenced" : "cleared", "mut")),
          h("div", { class: "hl-f-s" }, inlineCode(f.summary || f.entity)),
          h("div", { class: "hl-f-v" },
            h("div", { class: "hl-meter", title: `${fmtNum(f.value)} ${f.cmp} ${fmtNum(f.threshold)}` },
              h("b", { style: { width: `${Math.round((Math.max(0, ratio) / 1.6) * 100)}%` } }), h("i", { style: { left: `${100 / 1.6}%` } })),
            h("span", { class: "hl-f-n" }, `${fmtNum(f.value)} ${f.cmp} ${fmtNum(f.threshold)}`),
            h("span", { class: "lv-sub" }, f.state === "firing" || !f.ended_ms ? `since ${fmtClock(f.started_ms)}` : `${fmtClock(f.started_ms)} – ${fmtClock(f.ended_ms)}`)),
          h("button", { class: "lv-link hl-f-r", onclick: () => { S.ruleSel = f.rule; S.draft = null; setTab("rules"); } }, "Open the rule"));
      })));
  }

  function errorCard(text) {
    return h("div", { class: "hl-sec" }, h("h3", { class: "lv-h" }, "The error"), h("pre", { class: "hl-err" }, text));
  }

  function dots(history, big) {
    return h("span", { class: `hl-dots${big ? " big" : ""}` }, (history || []).map((x) => {
      const d = h("i", { class: x.outcome === "passed" ? "p" : x.outcome === "failed" ? "f" : "s" });
      d.addEventListener("mousemove", (e) => tip.show([h("b", null, x.outcome), h("div", null, `${fmtClock(x.ts_ms)} · ${nameOf(agentOf(x.agent))}${x.commit ? " · " + String(x.commit).slice(0, 8) : ""}`), x.ms != null ? h("div", { class: "lv-sub" }, fmtMs(x.ms)) : null], e.clientX, e.clientY));
      d.addEventListener("mouseleave", () => tip.hide());
      return d;
    }));
  }

  function testsCard(ts) {
    return h("div", { class: "hl-sec" },
      h("h3", { class: "lv-h" }, "Tests in it"),
      ts.map((t) => h("div", { class: "hl-bt-t" },
        h("code", { class: "hl-tname" }, t.test),
        h("div", { class: "hl-bt-r" }, dots(t.history, true), h("span", { class: "lv-sub" }, `${t.failed} of ${t.runs} runs failed${t.flaky ? " · flaky" : ""}`),
          h("button", { class: "lv-link", onclick: () => { S.testSel = t.test; S.filter = ""; loadHist(t.test); setTab("tests"); } }, "History")))));
  }

  function actionsCard(inc) {
    const acts = inc.actions || [];
    return h("div", { class: "hl-sec" },
      h("h3", { class: "lv-h" }, "Done about it"),
      acts.length ? h("ul", { class: "hl-al" }, acts.map((a) => h("li", null,
        h("span", { class: `hl-ak k-${a.kind}` }, { notify: "Needs you", ask: "Decide", pause: "Paused", resolve: "Resolved", steer: "Told" }[a.kind] || a.kind),
        h("span", { class: "hl-ad" }, a.detail),
        h("span", { class: "lv-sub" }, fmtClock(a.at_ms)),
        a.kind === "ask" && a.state === "open" ? h("button", { class: "lv-link", onclick: () => ctx.go("#/decide") }, "Answer") : null)))
        : h("div", { class: "lv-sub" }, "Nothing yet. A rule's action runs when it first fires; the buttons above do it by hand."));
  }

  function eventsCard(b, inc) {
    const evs = b.events || [];
    const w = b.window || {};
    return h("div", { class: "hl-sec" },
      h("h3", { class: "lv-h" }, "Around it", h("span", { class: "n" }, w.from_ms ? `· ${fmtClock(w.from_ms)} – ${w.to_ms >= Date.now() - 60000 ? "now" : fmtClock(w.to_ms)}` : "")),
      evs.length ? h("ol", { class: "hl-ev" }, evs.map((e) => {
        const a = agentOf(e.agent);
        const fired = e.kind === "fired";
        const started = a.started_ms || toMs(a.created);
        return h("li", { class: `${fired ? "fired" : ""}${e.failed && !fired ? " bad" : ""}${e.kind === "question" ? " q" : ""}` },
          h("span", { class: "hl-et" }, new Date(e.ts_ms ?? toMs(e.ts)).toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" })),
          avatar(a, 16),
          fired ? null : h("code", { class: "hl-tool" }, (e.tool || e.kind || "").replace(/^mcp__(\w+)__/, "$1·")),
          h("span", { class: "hl-es" }, e.summary || e.kind),
          started && !fired ? h("button", { class: "btn sm ghost hl-rw", title: "Rewind its terminal to this moment", onclick: () => { const t = e.ts_ms ?? toMs(e.ts); ctx.openRewind({ agent: a.id, from_ms: Math.max(0, t - started - 15000), to_ms: t - started + 3000 }); } }, "▶") : null);
      })) : h("div", { class: "lv-sub" }, "No events in its window."));
  }

  function relatedCard(b) {
    const rel = b.related || [];
    if (!rel.length) return null;
    return h("div", { class: "hl-sec hl-rel" },
      h("h3", { class: "lv-h" }, ctxChip("ChloeCtx knows about these files")),
      h("ul", null, rel.map((r) => h("li", null,
        h("div", null, md(r.text)),
        h("div", { class: "lv-sub" }, `${r.kind} · ${r.ts ? new Date(r.ts).toLocaleDateString([], { day: "numeric", month: "short" }) : ""}${(r.files || []).length ? " · " + r.files.join(", ") : ""}`)))));
  }

  async function act(i, kind, agent) {
    const k = `${i.id}:${kind}`;
    S.acting.add(k); S.actErr = null; render();
    try {
      const d = await ctx.api(`/api/incidents/${enc(i.id)}/act`, { method: "POST", body: { kind, agent } });
      const b = S.boards.get(i.id);
      if (b && d.incident) b.incident = d.incident;
      if (d.incident) Object.assign(i, d.incident);
    } catch (e) { S.actErr = e.message || String(e); }
    S.acting.delete(k);
    render();
  }
  async function resolve(i) {
    const k = `${i.id}:resolve`;
    S.acting.add(k); S.actErr = null; render();
    try {
      const d = await ctx.api(`/api/incidents/${enc(i.id)}/resolve`, { method: "POST", body: {} });
      Object.assign(i, d.incident || {});
      const b = S.boards.get(i.id);
      if (b) b.incident = { ...b.incident, ...d.incident };
    } catch (e) { S.actErr = e.message || String(e); }
    S.acting.delete(k);
    render();
  }

  // ------------------------------------------------------------- rules
  function pickRule(r) {
    S.ruleSel = r ? r.id : null;
    S.result = null; S.ruleMsg = null;
    S.draft = r ? { id: r.id, name: r.name, description: r.description || "", sql: r.sql, cmp: r.cmp || ">=", threshold: r.threshold, window_s: r.window_s, action: r.action || "notify", severity: r.severity || "minor", enabled: r.enabled !== false, builtin: !!r.builtin, dirty: false } : null;
  }
  function newRule() {
    S.ruleSel = null; S.result = null; S.ruleMsg = null;
    const [, , sql, cmp, threshold, window_s, severity] = TEMPLATES[1];
    S.draft = { id: null, name: "", description: "", sql, cmp, threshold, window_s, action: "notify", severity, enabled: true, builtin: false, dirty: true };
    render();
    requestAnimationFrame(() => el.querySelector(".hl-rname")?.focus());
  }

  function rulesTab() {
    if (!S.rules) return S.rulesErr ? errorBox(S.rulesErr, loadRules) : empty("Reading the rules…");
    const rules = S.rules.rules || [];
    if (!S.draft && rules.length) pickRule(rules.find((r) => r.id === S.ruleSel) || rules[0]);
    const list = h("aside", { class: "hl-list" },
      h("div", { class: "hl-lh" }, h("h2", { class: "lv-h" }, "Rules", h("span", { class: "n" }, `· ${rules.filter((r) => r.enabled !== false).length} on`)),
        h("button", { class: "btn sm", onclick: newRule }, "+ New rule")),
      rules.map((r) => h("button", {
        class: `hl-rc${r.id === S.ruleSel ? " on" : ""}${r.enabled === false ? " off" : ""}`, "data-nav": "", "data-key": `rule:${r.id}`,
        onclick: () => { pickRule(r); render(); },
      },
        h("div", { class: "hl-rc-h" }, h("b", null, r.name), r.firing ? pill(`${r.firing} firing`, sevTone(r.severity)) : r.enabled === false ? pill("off", "mut") : null),
        h("div", { class: "hl-rc-m" },
          h("span", null, `${r.cmp} ${fmtNum(r.threshold)} in ${winLabel(r.window_s)}`),
          h("span", null, "→ " + (ACTIONS.find(([k]) => k === r.action) || [, r.action])[1]),
          r.builtin ? h("span", { class: "hl-bi" }, "built-in") : null,
          r.last_error ? h("span", { class: "hl-bad" }, "error") : r.last_ms != null ? h("span", { class: "lv-sub" }, `${r.last_ms} ms`) : null))));
    return h("div", { class: "hl-rules" }, list, editor());
  }
  const winLabel = (s) => (WINDOWS.find(([x]) => x === s) || [, s >= 3600 ? `${Math.round(s / 3600)} h` : `${Math.round(s / 60)} min`])[1];

  function editor() {
    const d = S.draft;
    if (!d) return h("section", { class: "lv-card hl-ed" }, empty("Pick a rule, or make one."));
    const set = (k, v) => { d[k] = v; d.dirty = true; };
    const name = h("input", { class: "hl-in hl-rname", value: d.name, placeholder: "What it watches, in a few words", "aria-label": "Name" });
    name.addEventListener("input", () => set("name", name.value));
    const desc = h("input", { class: "hl-in hl-rdesc", value: d.description, placeholder: "Why it matters (optional)", "aria-label": "Description" });
    desc.addEventListener("input", () => set("description", desc.value));
    const sql = h("textarea", { class: "hl-sql", rows: String(Math.max(5, d.sql.split("\n").length + 1)), spellcheck: "false", "aria-label": "The rule's SQL" }, d.sql);
    import("./complete.js").then((m) => m.attachComplete(sql, { field: "sql", ctx })).catch(() => {}); // Fifth build: autocomplete
    sql.addEventListener("input", () => { set("sql", sql.value); sql.rows = Math.max(5, sql.value.split("\n").length + 1); });
    // Where the cursor was in the SQL, kept across renders: a column is
    // put there, and only when you pick one.
    const keep = () => { d.cur = { start: sql.selectionStart, end: sql.selectionEnd }; };
    for (const ev of ["input", "keyup", "click", "select", "blur"]) sql.addEventListener(ev, keep);
    const insert = (text) => {
      const at = d.cur ? Math.min(d.cur.start, sql.value.length) : sql.value.length;
      const to = d.cur ? Math.min(d.cur.end, sql.value.length) : at;
      const before = sql.value.slice(0, at);
      const pad = before && !/\s$/.test(before) && !/[(,]$/.test(before) ? " " : "";
      sql.setRangeText(pad + text, at, to, "end");
      set("sql", sql.value); keep(); sql.focus();
    };
    const open = S.tblOpen && TABLES[S.tblOpen] ? S.tblOpen : null;
    sql.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); testNow(); } });
    const cmp = h("select", { class: "hl-sel", "aria-label": "Compare" }, CMPS.map((c) => h("option", { value: c, selected: c === d.cmp || null }, c)));
    cmp.addEventListener("change", () => set("cmp", cmp.value));
    const th = h("input", { class: "hl-in hl-thr", type: "number", value: String(d.threshold ?? 0), "aria-label": "Threshold" });
    th.addEventListener("input", () => set("threshold", Number(th.value)));
    const win = h("select", { class: "hl-sel", "aria-label": "Window" }, WINDOWS.map(([s, l]) => h("option", { value: String(s), selected: s === d.window_s || null }, l)));
    win.addEventListener("change", () => set("window_s", Number(win.value)));
    const sev = h("select", { class: "hl-sel", "aria-label": "Severity" }, ["minor", "major"].map((s) => h("option", { value: s, selected: s === d.severity || null }, s)));
    sev.addEventListener("change", () => set("severity", sev.value));
    const rule = (S.rules?.rules || []).find((r) => r.id === d.id);
    return h("section", { class: "lv-card hl-ed", "aria-label": "Rule editor" },
      h("div", { class: "hl-ed-h" },
        h("div", { class: "hl-ed-t" }, name, desc),
        h("label", { class: "hl-tog", title: d.enabled ? "On: it runs every 30 s" : "Off" },
          h("input", { type: "checkbox", checked: d.enabled || null, onchange: (e) => { set("enabled", e.target.checked); if (d.id) save({ enabled: e.target.checked }); } }), h("span", null, d.enabled ? "on" : "off"))),
      !d.id ? h("div", { class: "hl-tpl" }, h("span", { class: "lv-sub" }, "Start from:"), TEMPLATES.map(([label, nm, q, c, t, w, sv]) =>
        h("button", { class: "hl-tb", onclick: () => { d.sql = q; d.cmp = c; d.threshold = t; d.window_s = w; d.severity = sv; if (!d.name.trim()) d.name = nm; d.cur = null; d.dirty = true; S.result = null; render(); } }, label))) : null,
      h("div", { class: "hl-sqlw" },
        h("div", { class: "hl-sql-h" }, h("span", null, "SQL"), h("span", { class: "lv-sub" }, "returns ", h("code", { class: "lv-code" }, "entity"), " and ", h("code", { class: "lv-code" }, "v"), "; ", h("code", { class: "lv-code" }, "agent"), ", ", h("code", { class: "lv-code" }, "file"), ", ", h("code", { class: "lv-code" }, "repo"), " and ", h("code", { class: "lv-code" }, "detail"), " when it has them · ", h("code", { class: "lv-code" }, ":since"), " is now less the window")),
        sql,
        h("div", { class: "hl-tables" }, h("span", { class: "lv-sub" }, "Tables:"), Object.keys(TABLES).map((t) =>
          h("button", { class: `hl-tb${open === t ? " on" : ""}`, "aria-expanded": open === t ? "true" : "false", title: `Show the columns of ${t}`, onclick: () => { S.tblOpen = open === t ? null : t; render(); } }, t))),
        open ? h("div", { class: "hl-cols" },
          h("span", { class: "lv-sub" }, `${open}:`),
          h("button", { class: "hl-col tbl", title: `Put ${open} where the cursor is in the SQL`, onclick: () => insert(open) }, `+ ${open}`),
          TABLES[open].split(", ").filter((c) => c !== "…" && !c.endsWith("…")).map((c) => h("button", { class: "hl-col", title: `Put ${c} where the cursor is in the SQL`, onclick: () => insert(c) }, c)),
          h("span", { class: "lv-sub" }, d.cur ? "Click one to put it at the cursor." : "Click one to put it at the end of the SQL, or click in the SQL first.")) : null),
      h("div", { class: "hl-when" },
        h("span", null, "Fire for each row whose ", h("code", { class: "lv-code" }, "v"), " is"), cmp, th, h("span", null, "over the last"), win, h("span", null, "· severity"), sev),
      h("div", { class: "hl-then" }, h("span", { class: "hl-then-l" }, "Then"),
        h("div", { class: "hl-seg", role: "radiogroup", "aria-label": "Action" }, ACTIONS.map(([k, label, note]) => h("button", {
          role: "radio", "aria-checked": d.action === k ? "true" : "false", class: d.action === k ? "on" : "", title: note,
          onclick: () => { set("action", k); render(); },
        }, h("b", null, label), h("small", null, note))))),
      h("div", { class: "hl-ed-a" },
        h("button", { class: "btn primary", disabled: S.testing || null, onclick: testNow }, S.testing ? "Running…" : "Test now", h("kbd", null, "⌘↵")),
        h("button", { class: "btn", disabled: S.saving || !d.dirty || !d.name.trim() || null, onclick: () => save() }, S.saving ? "Saving…" : d.id ? "Save" : "Save the rule"),
        d.id && !d.builtin ? h("button", { class: "btn ghost hl-del", onclick: del }, "Delete") : null,
        d.builtin ? h("span", { class: "lv-sub" }, "Built-in: edit it or switch it off.") : null,
        S.ruleMsg ? h("span", { class: S.ruleMsg.bad ? "hl-bad" : "hl-ok" }, S.ruleMsg.text) : null,
        rule?.last_eval_ms ? h("span", { class: "lv-sub hl-last" }, `ran ${fmtAgo(rule.last_eval_ms)}: ${rule.last_error ? "error" : `${rule.last_rows} rows`}`) : null),
      result());
  }

  function result() {
    const r = S.result;
    if (!r) return h("div", { class: "hl-res hl-res-0" }, h("span", { class: "lv-sub" }, "Test now runs the SQL against the live tables and shows what it would fire on. Nothing is saved, and no action runs."));
    if (!r.ok) return h("div", { class: "hl-res bad" }, h("b", null, "It does not run. "), r.error || "error", r.note ? h("div", { class: "lv-sub" }, r.note) : null);
    const fire = new Set((r.would_fire || []).map((f) => f.entity + "\u0001" + (f.agent || "")));
    const ci = (n) => (r.columns || []).indexOf(n);
    return h("div", { class: "hl-res" },
      h("div", { class: "hl-res-h" },
        h("b", null, (r.would_fire || []).length ? `Would fire on ${(r.would_fire || []).length}` : "Would fire on nothing"),
        h("span", { class: "lv-sub" }, `ran in ${r.ms} ms · ${r.count} row${r.count === 1 ? "" : "s"}${r.params?.since ? ` · :since ${fmtClock(r.params.since)}` : ""}`),
        r.note ? h("span", { class: "hl-bad" }, r.note) : null),
      (r.would_fire || []).length ? h("ul", { class: "hl-wf" }, r.would_fire.map((f) => h("li", null, h("i", { class: "hl-tri" }, "▲"), f.agent ? avatar(agentOf(f.agent), 16) : null, h("span", null, inlineCode(f.summary || f.entity))))) : null,
      r.rows?.length ? h("div", { class: "hl-tw" }, h("table", { class: "hl-tbl" },
        h("thead", null, h("tr", null, h("th", null, ""), (r.columns || []).map((c) => h("th", { class: c === "v" ? "num" : "" }, c)))),
        h("tbody", null, r.rows.map((row) => {
          const key = String(row[ci("entity")]) + "\u0001" + (ci("agent") >= 0 ? row[ci("agent")] || "" : "");
          const on = fire.has(key);
          return h("tr", { class: on ? "fire" : "" }, h("td", null, on ? "▲" : ""), row.map((v, i) => {
            const c = r.columns[i];
            if (c === "agent" && v) return h("td", null, h("span", { class: "hl-ag" }, avatar(agentOf(v), 14), nameOf(agentOf(v))));
            return h("td", { class: c === "v" ? "num" : "" }, v == null ? "—" : typeof v === "number" ? fmtNum(v) === String(v) ? String(v) : v.toLocaleString() : String(v));
          }));
        })))) : null);
  }

  async function testNow() {
    const d = S.draft;
    if (!d) return;
    S.testing = true; render();
    try {
      const r = repoOf(ctx);
      S.result = await ctx.api("/api/alerts/test", { method: "POST", body: { sql: d.sql, cmp: d.cmp, threshold: d.threshold, window_s: d.window_s, repo: r || undefined } });
    } catch (e) { S.result = { ok: false, error: e.message || String(e) }; }
    S.testing = false; render();
  }
  async function save(only) {
    const d = S.draft;
    S.saving = true; S.ruleMsg = null; render();
    const body = only || { name: d.name.trim(), description: d.description, sql: d.sql, cmp: d.cmp, threshold: d.threshold, window_s: d.window_s, action: d.action, severity: d.severity, enabled: d.enabled };
    try {
      const repo = repoOf(ctx);
      const out = d.id ? await ctx.api(`/api/alerts/rules/${enc(d.id)}`, { method: "PATCH", body }) : await ctx.api("/api/alerts/rules", { method: "POST", body: { ...body, repo: repo || undefined } });
      const rule = out.rule;
      const list = S.rules.rules;
      const at = list.findIndex((x) => x.id === rule.id);
      if (at >= 0) list[at] = rule; else list.push(rule);
      if (!only) { pickRule(rule); S.ruleMsg = { text: "Saved: it runs from the next check." }; } else S.ruleSel = rule.id;
    } catch (e) { S.ruleMsg = { text: e.message || String(e), bad: true }; }
    S.saving = false; render();
  }
  async function del() {
    const d = S.draft;
    if (!d?.id || !confirm(`Delete the rule “${d.name}”? Its incidents stay.`)) return;
    try {
      await ctx.api(`/api/alerts/rules/${enc(d.id)}`, { method: "DELETE" });
      S.rules.rules = S.rules.rules.filter((r) => r.id !== d.id);
      pickRule(S.rules.rules[0] || null);
    } catch (e) { S.ruleMsg = { text: e.message || String(e), bad: true }; }
    render();
  }

  // ------------------------------------------------------------- tests
  function testsTab() {
    if (!S.tests) return S.testsErr ? errorBox(S.testsErr, loadTests) : empty("Reading the test runs…");
    const d = S.tests;
    const t = d.totals || {};
    const tests = d.tests || [];
    if (!tests.length) return h("div", { class: "hl-tests" }, h("section", { class: "lv-card" }, empty("No test runs yet. Every cargo test, pytest, jest, vitest, go test or npm test an agent runs lands here, test by test.")), (d.missing || []).length ? h("div", { class: "hl-missing" }, d.missing.join(" ")) : null);
    return h("div", { class: "hl-tests" },
      h("section", { class: "lv-card hl-nums" },
        num(pct(t.pass_rate), "pass rate", `${fmtNum(t.runs)} runs of ${t.tests} tests in ${d.window?.days ?? 7} days${t.fixed ? ` · ${t.fixed} failed and fixed in the same session, not counted` : ""}`, t.pass_rate >= 0.95 ? "good" : "warn"),
        num(String(t.flaky ?? 0), "flaky", "passed and failed on the same code", "warn", () => { S.only = "flaky"; render(); }),
        num(String(t.newly_failing ?? 0), "newly failing", "passed before, fail in the last runs", "crit", () => { S.only = "newly"; render(); }),
        num(String(t.slow ?? 0), "slow", "its own time: p99 over 5 s, or 3× its p50; waits for you excluded", "lane-4", () => { S.only = "slow"; render(); }),
        h("div", { class: "hl-num hl-cmds" }, h("div", { class: "hl-n" }, fmtNum(t.commands)), h("div", { class: "hl-l" }, "test commands"), h("div", { class: "hl-s" }, `by ${new Set(tests.flatMap((x) => x.agents || [])).size} agents`))),
      h("div", { class: "hl-tgrid" },
        radar(tests),
        h("section", { class: "lv-card hl-flags" },
          flagList("Flaky", tests.filter((x) => x.flaky), "warn", (x) => x.flaky_why),
          flagList("Newly failing", tests.filter((x) => x.newly_failing), "crit", (x) => `last pass ${lastPass(x)} · ${x.failed} of ${x.runs} failed`),
          flagList("Slow", tests.filter((x) => x.slow).sort((a, b) => b.p99_ms - a.p99_ms), "lane-4", (x) => `p50 ${fmtMs(x.p50_ms)} · p99 ${fmtMs(x.p99_ms)}`))),
      quietNote(t),
      table(tests, d),
      recent(d.commands || []),
      personaCard(),
      h("p", { class: "hl-foot" }, d.percentiles === "approx_percentile" ? "p50 and p99 are ChloeDB's approx_percentile over test_runs (a DDSketch, within 1%)." : "p50 and p99 are computed from the runs.", " A test's time is its own when the runner reports it (cargo --report-time, pytest's junit report or --durations, jest, vitest, go -v), else its command's when it was the command's only test. The time a command waited for your permission is never test time."));
  }
  // Commands that printed only a count (or only an exit code): counted,
  // and said so, but no test is made up for them.
  function quietNote(t) {
    const q = t.quiet || {};
    if (!q.commands && !t.waited_ms) return null;
    const counts = [q.passed ? `${fmtNum(q.passed)} passed` : null, q.failed ? `${fmtNum(q.failed)} failed` : null, q.skipped ? `${fmtNum(q.skipped)} skipped` : null].filter(Boolean).join(", ");
    return h("section", { class: "lv-card hl-quiet" },
      q.commands ? h("div", null,
        h("b", null, `${q.commands} test command${q.commands === 1 ? "" : "s"} named no test: `),
        [q.counted ? `${q.counted} printed only a summary${counts ? ` (${counts})` : ""}` : null, q.commands - q.counted ? `${q.commands - q.counted} printed only an exit code` : null].filter(Boolean).join(", "),
        ". They are under Recent test commands, not in the table: Workbench names a test only when the runner does (pytest -v or -rA, or the junit report pytest writes by itself in an agent's session; cargo test, jest, vitest, go test -v).") : null,
      t.waited_ms ? h("div", { class: "lv-sub" }, `Agents' test commands waited ${fmtSpan(t.waited_ms)} in all for your permission; that time is left out of every test time here.`) : null);
  }
  // Test with a persona: the Preview lab's drawer, for an agent whose app
  // is running (ui/levels/preview-lab.js reads the flag).
  function personaCard() {
    const withApp = (ctx.state?.agents || []).filter((a) => a.preview_url);
    const go = (a) => {
      try { sessionStorage.setItem("wb.persona", JSON.stringify({ agent: a.id })); } catch {}
      ctx.go(`#/preview/${enc(a.id)}`);
    };
    return h("section", { class: "lv-card hl-persona" },
      h("div", { class: "hl-pc-t" }, h("h2", { class: "lv-h2" }, "Test with a persona"),
        h("span", { class: "lv-sub" }, "Tests check code; a persona checks the app: a simulated user tries a task on an agent's running app, checks the numbers it shows against everything on the page, does the task's main action once, and reports what got in the way.")),
      withApp.length ? h("div", { class: "hl-pc-l" }, withApp.slice(0, 6).map((a) => h("button", { class: "btn sm", onclick: () => go(a) }, avatar(a, 14), `Test ${nameOf(a)}'s app`)))
        : h("div", { class: "lv-sub" }, "No agent has a running app with a preview URL yet; set one in Preview."));
  }
  function lastPass(x) { const p = [...(x.history || [])].reverse().find((r) => r.outcome === "passed"); return p ? fmtAgo(p.ts_ms) : "never"; }

  function num(n, label, sub, tone, onclick) {
    return h(onclick ? "button" : "div", { class: `hl-num${onclick ? " click" : ""}`, onclick },
      h("div", { class: "hl-n", style: { color: `var(--${tone})` } }, n), h("div", { class: "hl-l" }, label), h("div", { class: "hl-s" }, sub));
  }

  function flagList(title, list, tone, why) {
    return h("div", { class: "hl-fg" },
      h("h3", { class: "lv-h" }, h("i", { class: "hl-fdot", style: { background: `var(--${tone})` } }), title, h("span", { class: "n" }, `· ${list.length}`)),
      list.length ? h("ul", null, list.slice(0, 4).map((x) => h("li", null,
        h("button", { class: "hl-tl", onclick: () => openTest(x.test) }, h("code", null, x.test)),
        h("div", { class: "hl-tl-m" }, dots(x.history.slice(-12)), h("span", { class: "lv-sub" }, why(x)))))) : h("div", { class: "lv-sub hl-none" }, "None."));
  }

  function openTest(test) {
    S.testSel = S.testSel === test ? null : test;
    if (S.testSel) loadHist(test);
    render();
    requestAnimationFrame(() => el.querySelector(`[data-key="t:${CSS.escape(test)}"]`)?.scrollIntoView({ block: "nearest", behavior: "smooth" }));
  }

  function radar(tests) {
    const W = 560, H = 330, L = 44, B = 26, T = 14, R = 14;
    const xs = (ms) => L + (Math.log10(Math.max(1, ms)) / 5) * (W - L - R); // 1 ms to 100 s
    const maxF = Math.max(0.25, ...tests.map((t) => 1 - t.pass_rate));
    // A square-root scale: the few percent that make a test flaky get room.
    const ys = (f) => H - B - 10 - Math.sqrt(Math.max(0, f) / maxF) * (H - B - T - 10);
    const ns = "http://www.w3.org/2000/svg";
    const s = (tag, attrs, ...kids) => { const e = document.createElementNS(ns, tag); for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v); for (const k of kids) if (k != null) e.append(k); return e; };
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "hl-radar", role: "img", "aria-label": "Each test by its p50 time and failure rate" });
    for (const [ms, lab] of [[1, "1 ms"], [10, "10 ms"], [100, "100 ms"], [1000, "1 s"], [10000, "10 s"], [100000, "100 s"]]) {
      svg.append(s("line", { x1: xs(ms), x2: xs(ms), y1: T, y2: H - B, class: "g" }), s("text", { x: xs(ms), y: H - 8, class: "ax", "text-anchor": ms === 100000 ? "end" : "middle" }, lab));
    }
    for (const f of [...[0, 0.02, 0.1].filter((f) => f < maxF * 0.7), maxF]) svg.append(s("line", { x1: L, x2: W - R, y1: ys(f), y2: ys(f), class: "g" }), s("text", { x: L - 6, y: ys(f) + 4, class: "ax", "text-anchor": "end" }, `${Math.round(f * 100)}%`));
    const flagged = (t) => t.flaky || t.newly_failing || t.slow;
    const sorted = [...tests].sort((a, b) => flagged(a) - flagged(b));
    const labels = [], points = [];
    for (const t of sorted) {
      const cls = t.newly_failing ? "nf" : t.flaky ? "fl" : t.slow ? "sl" : "ok";
      const r = 3.5 + Math.min(5, Math.sqrt(t.runs) * 0.9);
      const cx = xs(t.p50_ms), cy = ys(1 - t.pass_rate);
      const c = s("circle", { cx, cy, r, class: `pt ${cls}${S.testSel === t.test ? " on" : ""}`, tabindex: "0" });
      const show = (x, y) => tip.show([h("code", { class: "hl-tip-t" }, t.test), h("div", null, `${t.runs} runs · ${pct(t.pass_rate)} passed`), h("div", null, `p50 ${fmtMs(t.p50_ms)} · p99 ${fmtMs(t.p99_ms)}`), t.flaky_why ? h("div", { class: "lv-sub" }, t.flaky_why) : null], x, y);
      c.addEventListener("mousemove", (e) => show(e.clientX, e.clientY));
      c.addEventListener("mouseleave", () => tip.hide());
      c.addEventListener("focus", () => { const b = c.getBoundingClientRect(); show(b.right, b.top); });
      c.addEventListener("blur", () => tip.hide());
      c.addEventListener("click", () => openTest(t.test));
      c.addEventListener("keydown", (e) => { if (e.key === "Enter") openTest(t.test); });
      svg.append(c);
      points.push({ cx, cy, r, t, cls });
    }
    // Labels for the flagged tests: right of the point, else left, else a
    // row above or below, wherever it clears the other labels and points.
    for (const p of points.filter((p) => p.cls !== "ok").sort((a, b) => a.cy - b.cy)) {
      let short = p.t.test.split(/::|›|\//).pop().trim();
      if (short.length > 24) short = short.slice(0, 23) + "…";
      const w = short.length * 6.3, lh = 13;
      const clear = (x0, y) => x0 >= L && x0 + w <= W - 2 && y - 10 >= 0 && y <= H - B - 2
        && !labels.some((b) => Math.abs(b.y - y) < lh && b.x0 < x0 + w && x0 < b.x0 + b.w)
        && !points.some((q) => q !== p && q.cx + q.r > x0 && q.cx - q.r < x0 + w && q.cy + q.r > y - 10 && q.cy - q.r < y + 2);
      const tries = [];
      for (const dy of [0, -1, 1, -2, 2, -3, 3, -4]) for (const side of [1, -1]) tries.push([side, p.cy + 4 + dy * lh]);
      const at = tries.map(([side, y]) => [side > 0 ? p.cx + p.r + 4 : p.cx - p.r - 4 - w, y]).find(([x0, y]) => clear(x0, y));
      if (!at) continue;
      labels.push({ x0: at[0], w, y: at[1] });
      svg.append(s("text", { x: at[0], y: at[1], class: `lb ${p.cls}` }, short));
    }
    return h("section", { class: "lv-card hl-rad" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Radar"), h("span", { class: "lv-sub" }, "each test by its p50 and how often it fails"),
        h("div", { class: "hl-leg" }, [["nf", "newly failing"], ["fl", "flaky"], ["sl", "slow"], ["ok", "steady"]].map(([c, l]) => h("span", null, h("i", { class: c }), l)))),
      svg);
  }

  function table(tests, d) {
    const q = S.filter.trim().toLowerCase();
    const rows = tests.filter((t) => (!q || t.test.toLowerCase().includes(q)) && (S.only === "all" || (S.only === "flaky" && t.flaky) || (S.only === "newly" && t.newly_failing) || (S.only === "slow" && t.slow) || (S.only === "failing" && t.last?.outcome === "failed" && !t.gone)))
      .sort((a, b) => (b.newly_failing - a.newly_failing) || (b.flaky - a.flaky) || (a.pass_rate - b.pass_rate) || (b.p99_ms - a.p99_ms));
    const filter = h("input", { class: "hl-in hl-filter", placeholder: "Filter tests", value: S.filter, "aria-label": "Filter tests" });
    filter.addEventListener("input", later(() => { S.filter = filter.value; render(); requestAnimationFrame(() => { const f = el.querySelector(".hl-filter"); if (f) { f.focus(); f.setSelectionRange(f.value.length, f.value.length); } }); }, 120));
    return h("section", { class: "lv-card hl-all" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, `Every test · ${rows.length}${rows.length !== tests.length ? ` of ${tests.length}` : ""}`),
        h("div", { class: "hl-fil" },
          h("div", { class: "hl-chips" }, [["all", "All"], ["flaky", "Flaky"], ["newly", "Newly failing"], ["slow", "Slow"], ["failing", "Failing now"]].map(([k, l]) => h("button", { class: S.only === k ? "on" : "", onclick: () => { S.only = k; render(); } }, l))),
          filter)),
      h("div", { class: "hl-tw" }, h("table", { class: "hl-tbl hl-tt" },
        h("thead", null, h("tr", null, h("th", null, "Test"), h("th", null, "Runner"), h("th", { class: "num" }, "Runs"), h("th", null, "Passed"), h("th", null, "Last 20"), h("th", { class: "num" }, "p50"), h("th", { class: "num" }, "p99"), h("th", null, "Last run"))),
        h("tbody", null, rows.map((t) => [
          h("tr", { class: `${S.testSel === t.test ? "on" : ""}`, tabindex: "0", "data-nav": "", "data-key": `t:${t.test}`, onclick: () => openTest(t.test), onkeydown: (e) => { if (e.key === "Enter") openTest(t.test); } },
            h("td", { class: "hl-tn-c" }, h("code", { class: "hl-tname" }, t.test), t.newly_failing ? pill("newly failing", "crit") : null, t.flaky ? pill("flaky", "warn") : null, t.slow ? h("span", { class: "lv-pill hl-slow" }, "slow") : null,
              t.gone ? h("span", { class: "lv-pill hl-gone", title: "its command ran again after its last run and did not run it: renamed or removed" }, "not run any more") : null,
              t.fixed && !t.flaky ? h("span", { class: "lv-pill hl-fixed", title: "failed, then passed for the same agent after it changed the code: not counted against the pass rate" }, `${t.fixed} fixed in session`) : null),
            h("td", null, h("span", { class: "hl-run" }, t.runner)),
            h("td", { class: "num" }, String(t.runs)),
            h("td", null, h("div", { class: "hl-pr" }, h("div", { class: "lv-bar" }, h("b", { style: { width: `${t.pass_rate * 100}%`, background: t.pass_rate >= 0.999 ? "var(--good)" : t.pass_rate >= 0.8 ? "var(--warn)" : "var(--crit)" } })), h("span", null, pct(t.pass_rate)))),
            h("td", null, dots(t.history)),
            h("td", { class: "num" }, t.own_ms === false ? h("span", { title: "the command's time: this runner did not report the test's own" }, "~" + fmtMs(t.p50_ms)) : fmtMs(t.p50_ms)),
            h("td", { class: "num" }, fmtMs(t.p99_ms)),
            h("td", null, h("span", { class: "hl-ag" }, avatar(agentOf(t.last?.agent), 14), h("span", { class: t.last?.outcome === "failed" ? "hl-bad" : "" }, t.last?.outcome), h("span", { class: "lv-sub" }, fmtAgo(t.last?.ts_ms))))),
          S.testSel === t.test ? h("tr", { class: "hl-det" }, h("td", { colspan: "8" }, history(t))) : null,
        ])))));
  }

  function history(t) {
    const d = S.hist.get(t.test);
    if (d === undefined || d === null) return h("div", { class: "lv-sub" }, "Reading its runs…");
    if (d.error) return h("div", { class: "err" }, d.error);
    const runs = d.runs || [];
    return h("div", { class: "hl-hist" },
      t.flaky_why ? h("div", { class: "hl-why" }, h("b", null, "Flaky: "), t.flaky_why) : null,
      stableLine(d.stable, d),
      codesBox(d),
      rerunBox(t, d),
      h("table", { class: "hl-tbl hl-ht" },
        h("thead", null, h("tr", null, h("th", null, "When"), h("th", null, "Agent"), h("th", null, "Outcome"), h("th", { class: "num" }, "Time"), h("th", null, "Commit"), h("th", null, "Command"))),
        h("tbody", null, runs.slice(0, 30).map((r) => h("tr", null,
          h("td", null, `${fmtClock(r.ts_ms)} · ${fmtAgo(r.ts_ms)}`),
          h("td", null, h("span", { class: "hl-ag" }, avatar(agentOf(r.agent), 14), r.by_workbench ? (r.agent ? `Workbench, in ${nameOf(agentOf(r.agent))}'s worktree` : "Workbench, in the project's checkout") : nameOf(agentOf(r.agent)))),
          h("td", null, h("span", { class: r.outcome === "failed" ? "hl-bad" : r.outcome === "passed" ? "hl-ok" : "lv-sub" }, r.outcome),
            r.fixed ? h("span", { class: "lv-sub", title: "the same agent changed the code and it passed: not counted against the pass rate" }, " · fixed in this session") : null),
          h("td", { class: "num", title: r.wait_ms ? `the command also waited ${fmtSpan(r.wait_ms)} for your permission, not counted` : "" }, r.ms == null ? "—" : fmtMs(r.ms), r.wait_ms ? h("span", { class: "lv-sub" }, ` +${fmtSpan(r.wait_ms)} wait`) : null),
          h("td", null, h("code", { class: "lv-code" }, String(r.commit || "").slice(0, 8)), r.tree && r.tree !== "clean" ? h("span", { class: "lv-sub", title: "uncommitted changes: the hash of git diff" }, ` +${String(r.tree).slice(0, 4)}`) : null),
          h("td", null, h("code", { class: "hl-cmd" }, r.command)))))));
  }

  // The code of its last normal run, when it never failed there: its runs
  // on that commit only (never another commit's), against the commit it
  // last failed on, and whether that many passes in a row tell a fix from
  // luck. "Not enough runs yet" when they do not.
  const sha = (c) => (c ? String(c).slice(0, 8) : "no commit");
  const branchOf = (d, c) => { const k = (d.checkouts || []).find((x) => c && x.commit && x.commit === c); return k ? k.branch : null; };
  function stableLine(s, d) {
    if (!s || !s.runs) return null;
    const dirty = s.since_tree && s.since_tree !== "clean" ? " with uncommitted changes" : "";
    const br = branchOf(d, s.since_commit);
    const parts = [h("b", null, "On "), h("code", { class: "lv-code" }, sha(s.since_commit)), br ? ` (${br})` : "", dirty, s.env ? ` with ${s.env}` : "",
      `: ${s.runs} run${s.runs === 1 ? "" : "s"}, no failure`, s.by_workbench ? ` (${s.by_workbench} by Run it N times)` : "", "."];
    let ok = false;
    if (s.after_failure) {
      parts.push(" On ", h("code", { class: "lv-code" }, sha(s.before_commit)), ` it failed ${s.failed_before} of ${s.runs_before} runs.`);
      if (s.enough) { ok = true; parts.push(` A test still failing that often passes ${s.runs} times in a row less than 5% of the time: it looks fixed on ${sha(s.since_commit)}.`); }
      else if (s.needed) parts.push(h("b", null, " Not enough runs yet"), ` to call it fixed: it needs ${s.needed} passes in a row on this code to tell it from that, and has ${s.runs}.`);
    } else if (s.runs >= 10 && s.upper_95 != null) { ok = true; parts.push(` It fails less than ${pct(s.upper_95)} of the time on this code (95% confidence).`); }
    else parts.push(h("b", null, " Not enough runs yet"), " to say how often it fails.");
    return h("div", { class: `hl-stable${ok ? "" : " warn"}` }, ...parts);
  }
  // Its runs per commit, newest first: where it passes and where it fails.
  function codesBox(d) {
    const codes = d.codes || [];
    if (codes.length < 2) return null;
    return h("ul", { class: "hl-codes" }, codes.slice(0, 6).map((c) => {
      const br = branchOf(d, c.commit);
      return h("li", null, h("code", { class: "lv-code" }, sha(c.commit)), br ? h("span", { class: "lv-sub" }, ` ${br}`) : null,
        c.tree && c.tree !== "clean" ? h("span", { class: "lv-sub", title: "uncommitted changes: the hash of git diff" }, ` +${String(c.tree).slice(0, 4)}`) : null,
        c.env ? h("span", { class: "lv-sub" }, ` with ${c.env}`) : null, " · ",
        h("span", { class: c.failed ? "hl-bad" : "hl-ok" }, c.failed ? `${c.failed} of ${c.runs} failed` : `${c.runs} passed`),
        c.failed && c.passed ? h("span", { class: "lv-sub" }, " · flaky here") : null,
        h("span", { class: "lv-sub" }, ` · last ${fmtAgo(c.last_ms)}`));
    }));
  }
  // Run it N times: the command for that one test (editable; no shell),
  // the checkout it runs in, its progress and its result on that commit.
  function rerunBox(t, d) {
    const r = S.reruns.get(t.test);
    const running = r && (r.state === "running" || r.state === "starting");
    const n = h("select", { class: "hl-in hl-rn", "aria-label": "How many times", disabled: running ? true : null, onchange: (e) => { S.rerunN = Number(e.target.value); } },
      [5, 10, 20, 50].map((k) => h("option", { value: String(k), selected: S.rerunN === k ? true : null }, `${k} times`)));
    const cos = d.checkouts || [];
    const pick = S.rrCheckout.get(t.test) || d.checkout || (cos[0] && cos[0].id);
    const where = h("select", { class: "hl-in hl-rco", "aria-label": "Which checkout it runs in", disabled: running ? true : null, onchange: (e) => { S.rrCheckout.set(t.test, e.target.value); } },
      cos.map((c) => h("option", { value: c.id, selected: c.id === pick ? true : null }, `${c.label}${c.branch ? ` · ${c.branch}` : ""} · ${sha(c.commit)}${c.dirty ? " +changes" : ""}`)));
    const cmd = h("input", { class: "hl-in hl-rcmd", "aria-label": "The command Workbench runs, without a shell", spellcheck: "false", disabled: running ? true : null,
      value: S.rrCmd.has(t.test) ? S.rrCmd.get(t.test) : d.rerun || "", placeholder: `the command that runs ${t.test} by itself`,
      oninput: (e) => { S.rrCmd.set(t.test, e.target.value); } });
    const last = (d.runs || [])[0];
    const ask = last && agentOf(last.agent);
    const askBtn = ask && ask.id && ask.running !== false ? h("button", { class: "btn sm ghost", onclick: async (e) => {
      const b = e.currentTarget; b.disabled = true;
      try { await ctx.api(`/api/agents/${enc(ask.id)}/steer`, { method: "POST", body: { text: `Please run ${t.test} 10 times in a row, each run as its own command (only that test), and tell me how many runs failed, with the output of a failure if there is one.` } }); b.textContent = `Asked ${nameOf(ask)}`; } catch (err) { b.textContent = err.message || String(err); }
    } }, `Ask ${nameOf(ask)} to run it 10 times`) : null;
    let res = null;
    if (r && r.state === "failed") res = h("span", { class: "err" }, r.error || "It did not run.");
    else if (r && r.state === "starting") res = h("span", { class: "lv-sub" }, "Starting…");
    else if (r) {
      const fin = r.state !== "running";
      const on = `${r.commit ? ` on ${sha(r.commit)}` : ""}${r.dirty ? " with uncommitted changes" : ""}${r.branch ? ` (${r.branch}, ${r.checkout_label || "its checkout"})` : r.checkout_label ? ` (${r.checkout_label})` : ""}`;
      res = h("span", { class: "hl-rr-res" },
        h("span", { class: "lv-bar hl-rr-bar" }, h("b", { style: { width: `${(r.done / r.n) * 100}%`, background: r.failed ? "var(--crit)" : "var(--good)" } })),
        h("span", { class: r.failed ? "hl-bad" : fin ? "hl-ok" : "lv-sub" }, fin ? `${r.passed} of ${r.n} passed${r.failed ? `, ${r.failed} failed` : ""}${on}` : `run ${r.done + 1} of ${r.n}${on}: ${r.passed} passed, ${r.failed} failed so far`));
    }
    const go = () => rerun(t.test, S.rerunN, { command: cmd.value.trim(), checkout: where.value || pick });
    return h("div", { class: "hl-rr" },
      h("div", { class: "hl-rr-row" }, cmd),
      h("div", { class: "hl-rr-row" },
        h("button", { class: "btn sm", disabled: running ? true : null, onclick: go }, running ? "Running…" : "Run it N times"), n,
        cos.length ? [h("span", { class: "lv-sub" }, "in"), where] : null,
        !d.rerun ? h("span", { class: "lv-sub" }, `Workbench cannot aim this ${d.runner || ""} command at one test: write it above, or`) : null,
        !d.rerun ? askBtn : null),
      h("div", { class: "lv-sub hl-rr-note" }, "Workbench runs it without a shell, one run after another, and records each run on the commit it ran on."),
      res);
  }
  function recent(cmds) {
    if (!cmds.length) return null;
    return h("section", { class: "lv-card hl-recent" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Recent test commands"), h("span", { class: "lv-sub" }, "what each printed: its tests, a summary's counts, or only its exit code")),
      h("ul", { class: "hl-cl" }, cmds.slice(0, 12).map((c) => {
        const what = c.running ? pill("running", "acc")
          : c.counted === false ? pill(`exit ${c.exit_code}`, c.exit_code ? "crit" : "good")
          : c.failed ? pill(`${c.failed} failed`, "crit") : pill(`${c.passed ?? 0} passed`, "good");
        const extra = [c.failed && c.passed ? `${c.passed} passed` : null, c.ignored ? `${c.ignored} skipped` : null,
          c.counted === false ? "printed no count" : c.named === 0 ? "only a summary, no test names" : null].filter(Boolean).join(" · ");
        return h("li", null,
          avatar(agentOf(c.agent), 16),
          h("code", { class: "hl-cmd" }, c.command),
          what,
          extra ? h("span", { class: "lv-sub" }, extra) : null,
          h("span", { class: "lv-sub hl-cm-t" }, `${c.ms != null ? fmtSpan(c.ms) + " · " : ""}${c.wait_ms ? `waited ${fmtSpan(c.wait_ms)} for you · ` : ""}${c.by_workbench ? "Workbench · " : ""}${fmtAgo(c.ts_ms)}`));
      })));
  }

  function errorBox(text, retry) {
    return h("section", { class: "lv-card" }, h("div", { class: "err" }, text), h("button", { class: "btn sm", onclick: retry }, "Try again"));
  }

  const offKeys = keyNav(el, {
    onKey: (e) => {
      if (e.key === "1" || e.key === "2" || e.key === "3") { setTab(TABS[Number(e.key) - 1][0]); return true; }
      return false;
    },
  });

  loadTab();
  if (S.tab !== "incidents") loadIncidents();
  render();
  return {
    update(state) {
      if (state) ctx.state = { ...ctx.state, ...state };
      const evs = state?.events || [];
      // A new firing or a test run: read again soon (at most every 10 s).
      if (evs.length) refresh();
    },
    unmount() { alive = false; refresh.cancel(); renderSoon.cancel(); tip.remove(); offKeys(); el.remove(); },
  };
}

useStyle("health", `
.hl{padding:20px 24px 28px;display:flex;flex-direction:column;gap:16px}
.hl-head{align-items:center}
.hl-tabs{display:flex;gap:2px;padding:3px;border-radius:999px;background:var(--sunken);border:1px solid var(--line);margin-left:auto}
.hl-tabs button{display:inline-flex;align-items:center;gap:6px;border:0;background:none;border-radius:999px;padding:4px 14px;font:600 12.5px var(--sans);color:var(--muted);cursor:pointer}
.hl-tabs button:hover{color:var(--text)}
.hl-tabs button.on{background:var(--accent-soft);color:var(--accent);box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--accent) 35%,transparent)}
.hl-tn{font-size:10.5px;min-width:17px;height:17px;padding:0 5px;border-radius:999px;display:inline-grid;place-items:center;background:var(--surface-3);color:var(--text-2)}
.hl-tn.crit{background:var(--crit);color:#fff}.hl-tn.warn{background:var(--warn-soft);color:var(--warn-ink)}
.hl-eval{display:inline-flex;align-items:center;gap:7px;color:var(--muted);font-size:12px}
.hl-live{width:7px;height:7px;border-radius:50%;background:var(--good);box-shadow:0 0 0 3px var(--good-soft)}
.hl .lv-h{display:flex;align-items:center;gap:4px}
.hl-inc,.hl-rules{display:grid;grid-template-columns:360px minmax(0,1fr);gap:18px;align-items:start}
.hl-list{display:flex;flex-direction:column;gap:8px;min-width:0}
.hl-lh{display:flex;align-items:center;justify-content:space-between}.hl-lh .lv-h{margin:0}
.hl-h-res{margin-top:12px}
.hl-calm{padding:14px;border:1px dashed var(--line-2);border-radius:12px;color:var(--muted)}.hl-calm b{color:var(--good-ink)}
.hl-ic{display:flex;gap:11px;text-align:left;width:100%;border:1px solid var(--line);background:var(--surface);border-radius:12px;padding:12px 14px 12px 12px;cursor:pointer;font:inherit;color:inherit;box-shadow:var(--shadow-1)}
.hl-ic:hover{border-color:var(--line-2)}
.hl-ic.on{border-color:var(--accent);background:color-mix(in srgb,var(--accent-soft) 55%,var(--surface));box-shadow:0 0 0 1px var(--accent)}
.hl-ic.resolved{background:transparent;box-shadow:none;padding:9px 12px}
.hl-ic.resolved .hl-ic-t{font-weight:550;color:var(--text-2);font-size:13px}
.hl-sev{width:4px;border-radius:3px;flex:none;align-self:stretch}.hl-sev.crit{background:var(--crit)}.hl-sev.warn{background:var(--warn)}.hl-sev.done{background:var(--line-2)}
.hl-ic-b{min-width:0;flex:1}
.hl-ic-t{font-weight:650;font-size:13.5px;line-height:1.35}
.hl-ic-m{display:flex;align-items:center;gap:8px;margin-top:6px;color:var(--muted);font-size:12px;flex-wrap:wrap}
.hl-avs{display:inline-flex;gap:3px}
.hl-asked,.hl-nu{font-size:11px;font-weight:650;border-radius:999px;padding:1px 8px}
.hl-asked{background:color-mix(in srgb,var(--ctx) 14%,transparent);color:var(--ctx)}.hl-nu{background:var(--warn-soft);color:var(--warn-ink)}
.hl-more{align-self:flex-start}
.hl-board{padding:18px 20px;display:flex;flex-direction:column;gap:14px;min-width:0}
.hl-bh{display:flex;align-items:center;justify-content:space-between;gap:10px;flex-wrap:wrap}
.hl-bh-l{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.hl-acts{display:flex;gap:8px;flex-wrap:wrap}
.btn.warn{border-color:color-mix(in srgb,var(--warn) 55%,var(--line-2));color:var(--warn-ink);background:var(--warn-soft)}
.hl-bt{margin:-4px 0 0;font-size:19px;font-weight:680;letter-spacing:-.01em;line-height:1.3}
.hl-files{display:flex;gap:6px;flex-wrap:wrap;margin-top:-6px}
.hl-where{display:flex;flex-direction:column;gap:4px;font-size:13px}
.hl-says{display:flex;flex-direction:column;gap:2px;border:1px solid var(--line);border-radius:10px;background:var(--surface-2);padding:6px 12px}
.hl-say{display:flex;align-items:center;gap:11px;padding:7px 0}
.hl-say+.hl-say{border-top:1px solid var(--line)}
.hl-say-b{flex:1;min-width:0}
.hl-say-h{display:flex;align-items:center;gap:8px}
.hl-say-t{margin-top:2px;color:var(--text);font-size:13.5px}
.hl-say-a{display:flex;gap:4px;flex:none}
.hl-bgrid{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1.08fr);gap:18px}
.hl-bcol{display:flex;flex-direction:column;gap:16px;min-width:0}
.hl-sec .lv-h{margin-bottom:8px}
.hl-fl{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:8px}
.hl-f{border:1px solid var(--line);border-radius:10px;padding:10px 12px;position:relative}
.hl-f.firing{border-left:3px solid var(--crit)}.hl-f.resolved{opacity:.85}
.hl-f-h{display:flex;align-items:center;justify-content:space-between;gap:8px}
.hl-f-h b{font-weight:650}
.hl-f-s{color:var(--text-2);margin-top:3px}
.hl-f-v{display:flex;align-items:center;gap:10px;margin-top:8px;font-size:12px}
.hl-meter{position:relative;width:90px;height:6px;border-radius:999px;background:var(--surface-3);flex:none}
.hl-meter b{position:absolute;left:0;top:0;bottom:0;border-radius:999px;background:var(--crit);max-width:100%}
.hl-meter i{position:absolute;top:-3px;bottom:-3px;width:2px;background:var(--text-2);border-radius:1px}
.hl-f-n{font:600 12px var(--mono)}
.hl-f-r{position:absolute;right:12px;bottom:10px;font-size:12px}
.hl-err{margin:0;font:12px/1.5 var(--mono);color:var(--crit-ink);background:var(--crit-soft);border-radius:8px;padding:9px 11px;white-space:pre-wrap;overflow-wrap:anywhere}
.hl-bt-t{display:flex;flex-direction:column;gap:5px;padding:6px 0}
.hl-bt-r{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.hl-tname{font:12px var(--mono);color:var(--text);overflow-wrap:anywhere}
.hl-dots{display:inline-flex;gap:2px;align-items:center}
.hl-dots i{width:6px;height:12px;border-radius:2px;display:inline-block}
.hl-dots.big i{width:9px;height:16px;border-radius:3px}
.hl-dots i.p{background:var(--good)}.hl-dots i.f{background:var(--crit)}.hl-dots i.s{background:var(--line-2)}
.hl-al{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:7px}
.hl-al li{display:flex;align-items:center;gap:9px;flex-wrap:wrap}
.hl-ak{font-size:11px;font-weight:700;border-radius:6px;padding:2px 7px;background:var(--surface-3);color:var(--text-2);flex:none}
.hl-ak.k-ask{background:color-mix(in srgb,var(--ctx) 14%,transparent);color:var(--ctx)}.hl-ak.k-pause{background:var(--warn-soft);color:var(--warn-ink)}.hl-ak.k-notify{background:var(--accent-soft);color:var(--accent)}
.hl-ad{flex:1;min-width:0}
.hl-ev{list-style:none;margin:0;padding:0;border-left:2px solid var(--line);margin-left:6px}
.hl-ev li{display:flex;align-items:center;gap:8px;padding:4px 0 4px 12px;position:relative;min-width:0;font-size:12.5px}
.hl-ev li::before{content:"";position:absolute;left:-5px;top:50%;width:8px;height:8px;margin-top:-4px;border-radius:50%;background:var(--surface);border:2px solid var(--line-2)}
.hl-ev li.bad .hl-es{color:var(--crit-ink)}.hl-ev li.bad::before{border-color:var(--crit)}
.hl-ev li.fired{background:var(--crit-soft);border-radius:0 7px 7px 0;font-weight:650}.hl-ev li.fired .hl-es{color:var(--crit-ink)}
.hl-ev li.fired::before{background:var(--crit);border-color:var(--crit)}
.hl-ev li.q .hl-es{color:var(--ctx)}.hl-ev li.q::before{border-color:var(--ctx)}
.hl-et{color:var(--muted);font-variant-numeric:tabular-nums;width:76px;flex:none;font-size:11.5px}
.hl-tool{font:11px var(--mono);color:var(--text-2);background:var(--surface-3);border-radius:4px;padding:1px 6px;flex:none;max-width:150px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.hl-es{flex:1;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;color:var(--text-2)}
.hl-rw{height:20px;padding:0 6px;font-size:10.5px;opacity:0}
.hl-ev li:hover .hl-rw,.hl-rw:focus-visible{opacity:1}
.hl-rel ul{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:9px}
.hl-rel li{padding-left:11px;border-left:2px solid color-mix(in srgb,var(--ctx) 45%,transparent)}
.hl-missing{color:var(--muted);font-size:12px}
.hl-rc{text-align:left;width:100%;border:1px solid var(--line);background:var(--surface);border-radius:11px;padding:10px 12px;cursor:pointer;font:inherit;color:inherit}
.hl-rc:hover{border-color:var(--line-2)}
.hl-rc.on{border-color:var(--accent);box-shadow:0 0 0 1px var(--accent)}
.hl-rc.off b{color:var(--muted)}
.hl-rc-h{display:flex;align-items:center;justify-content:space-between;gap:8px}.hl-rc-h b{font-weight:640}
.hl-rc-m{display:flex;gap:10px;flex-wrap:wrap;margin-top:4px;color:var(--muted);font-size:12px}
.hl-bi{color:var(--text-2);font-weight:600}
.hl-bad{color:var(--crit-ink);font-weight:600}.hl-ok{color:var(--good-ink);font-weight:600}
.hl-ed{padding:18px 20px;display:flex;flex-direction:column;gap:14px;min-width:0}
.hl-ed-h{display:flex;gap:14px;align-items:flex-start}
.hl-ed-t{flex:1;display:flex;flex-direction:column;gap:4px;min-width:0}
.hl-in{border:1px solid transparent;border-radius:8px;background:none;color:var(--text);font:inherit;padding:4px 8px;outline:none}
.hl-in:hover{border-color:var(--line)}.hl-in:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus);background:var(--surface)}
.hl-rname{font-size:18px;font-weight:680;margin-left:-8px}.hl-rdesc{color:var(--text-2);margin-left:-8px}
.hl-tog{display:inline-flex;align-items:center;gap:6px;font-weight:600;color:var(--text-2);cursor:pointer;padding-top:6px}
.hl-sqlw{border:1px solid var(--line-2);border-radius:10px;overflow:hidden;background:var(--term-bg)}
.hl-sql-h{display:flex;align-items:baseline;gap:12px;padding:7px 12px;border-bottom:1px solid var(--term-line);font-size:12px;flex-wrap:wrap}
.hl-sql-h>span:first-child{font-weight:700;letter-spacing:.06em;color:var(--muted);font-size:11px}
.hl-sql{display:block;width:100%;border:0;background:none;color:var(--term-fg);font:13px/1.6 var(--mono);padding:10px 12px;resize:vertical;outline:none;tab-size:2}
.hl-tables{display:flex;gap:6px;align-items:center;flex-wrap:wrap;padding:7px 12px;border-top:1px solid var(--term-line)}
.hl-cols{display:flex;gap:4px;align-items:center;flex-wrap:wrap;padding:6px 12px;border-top:1px solid var(--term-line)}
.hl-col{border:1px solid var(--term-line);background:none;color:inherit;border-radius:4px;padding:1px 6px;font:12px var(--mono,monospace);cursor:pointer}
.hl-col.tbl{font-weight:600}
.hl-tpl{display:flex;gap:6px;align-items:center;flex-wrap:wrap}
.hl-tb{border:1px solid var(--line);background:var(--surface);color:var(--text-2);border-radius:6px;padding:1px 7px;font:11.5px var(--mono);cursor:pointer}
.hl-tb:hover{color:var(--text);border-color:var(--line-2)}
.hl-tb.on{color:var(--accent);border-color:var(--accent)}
.hl-tip-c,.hl-tip-t{font:11.5px var(--mono);overflow-wrap:anywhere}
.hl-when{display:flex;align-items:center;gap:8px;flex-wrap:wrap;color:var(--text-2)}
.hl-sel{border:1px solid var(--line-2);border-radius:7px;background:var(--surface);color:var(--text);font:inherit;padding:3px 6px}
.hl .hl-when .hl-sel,.hl .hl-when .hl-thr{width:auto;display:inline-block;height:30px;padding:0 8px;flex:none}
.hl .hl-when .hl-thr{width:96px;border:1px solid var(--line-2);border-color:var(--line-2);background:var(--surface);font-family:var(--mono)}
.hl-then{display:flex;align-items:center;gap:12px;flex-wrap:wrap}
.hl-then-l{color:var(--text-2)}
.hl-seg{display:flex;gap:6px;flex-wrap:wrap}
.hl-seg button{display:flex;flex-direction:column;align-items:flex-start;border:1px solid var(--line);background:var(--surface);border-radius:9px;padding:6px 11px;cursor:pointer;font:inherit;color:var(--text-2);text-align:left}
.hl-seg button b{font-weight:640;font-size:12.5px}.hl-seg button small{color:var(--muted);font-size:11px}
.hl-seg button.on{border-color:var(--accent);background:var(--accent-soft);color:var(--accent)}
.hl-ed-a{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.hl-ed-a kbd{margin-left:8px;opacity:.75}
.hl-last{margin-left:auto}
.hl-del{color:var(--crit-ink)}
.hl-res{border:1px solid var(--line);border-radius:10px;padding:12px 14px;background:var(--surface-2);display:flex;flex-direction:column;gap:10px;min-width:0}
.hl-res-0{background:none;border-style:dashed}
.hl-res.bad{border-color:color-mix(in srgb,var(--crit) 50%,var(--line));background:var(--crit-soft);color:var(--crit-ink)}
.hl-res-h{display:flex;align-items:baseline;gap:12px;flex-wrap:wrap}.hl-res-h b{font-size:14px}
.hl-wf{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:5px}
.hl-wf li{display:flex;align-items:center;gap:8px}
.hl-tri{color:var(--crit);font-style:normal;font-size:11px}
.hl-tw{overflow:auto;min-width:0}
.hl-tbl{border-collapse:collapse;width:100%;font-size:12.5px}
.hl-tbl th{text-align:left;font-weight:650;color:var(--muted);font-size:11.5px;padding:5px 10px;border-bottom:1px solid var(--line);white-space:nowrap}
.hl-tbl td{padding:6px 10px;border-bottom:1px solid var(--line);vertical-align:middle}
.hl-tbl .num{text-align:right;font-variant-numeric:tabular-nums;font-family:var(--mono);font-size:12px;white-space:nowrap}
.hl-tbl tr.fire td{background:var(--crit-soft)}.hl-tbl tr.fire td:first-child{color:var(--crit);font-size:10px}
.hl-ag{display:inline-flex;align-items:center;gap:6px;white-space:nowrap}
.hl-tests{display:flex;flex-direction:column;gap:16px}
.hl-nums{display:grid;grid-template-columns:repeat(5,minmax(0,1fr));gap:12px;padding:16px 20px}
.hl-num{border:0;background:none;text-align:left;padding:4px 6px;border-radius:10px;font:inherit;color:inherit;width:auto}
.hl-num.click{cursor:pointer}.hl-num.click:hover{background:var(--surface-2)}
.hl-n{font-size:38px;font-weight:750;letter-spacing:-.025em;line-height:1.05;font-variant-numeric:tabular-nums}
.hl-cmds .hl-n{color:var(--text)}
.hl-l{margin-top:4px;font-weight:650;font-size:13.5px}
.hl-s{margin-top:2px;color:var(--muted);font-size:12px}
.hl-tgrid{display:grid;grid-template-columns:minmax(0,1.1fr) minmax(0,1fr);gap:16px;align-items:stretch}
.hl-rad .lv-card-head{margin-bottom:4px;justify-content:flex-start}.hl-rad .hl-leg{margin-left:auto}
.hl-radar{width:100%;height:auto;display:block}
.hl-radar .g{stroke:var(--line);stroke-width:1}
.hl-radar .ax{fill:var(--muted);font:11px var(--sans)}
.hl-radar .pt{stroke-width:1.5;cursor:pointer;outline:none}
.hl-radar .pt.ok{fill:color-mix(in srgb,var(--muted) 30%,transparent);stroke:var(--muted)}
.hl-radar .pt.fl{fill:var(--warn-soft);stroke:var(--warn);stroke-width:2.5}
.hl-radar .pt.nf{fill:var(--crit);stroke:var(--crit)}
.hl-radar .pt.sl{fill:color-mix(in srgb,var(--lane-4) 30%,transparent);stroke:var(--lane-4);stroke-width:2}
.hl-radar .pt:hover,.hl-radar .pt:focus,.hl-radar .pt.on{stroke:var(--text);stroke-width:2.5}
.hl-radar .lb{font:600 11px var(--sans)}.hl-radar .lb.fl{fill:var(--warn-ink)}.hl-radar .lb.nf{fill:var(--crit-ink)}.hl-radar .lb.sl{fill:var(--lane-4)}
.hl-leg{display:flex;gap:12px;flex-wrap:wrap;font-size:12px;color:var(--text-2)}
.hl-leg span{display:inline-flex;align-items:center;gap:5px}
.hl-leg i{width:9px;height:9px;border-radius:50%;display:inline-block;border:2px solid}
.hl-leg i.nf{background:var(--crit);border-color:var(--crit)}.hl-leg i.fl{background:var(--warn-soft);border-color:var(--warn)}.hl-leg i.sl{border-color:var(--lane-4)}.hl-leg i.ok{border-color:var(--muted)}
.hl-flags{display:flex;flex-direction:column;gap:12px;padding:14px 16px}
.hl-fg ul{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:7px}
.hl-fg .lv-h{margin-bottom:6px}
.hl-fdot{width:8px;height:8px;border-radius:50%;display:inline-block;margin-right:3px}
.hl-tl{border:0;background:none;padding:0;cursor:pointer;text-align:left;font:inherit;color:var(--text);max-width:100%}
.hl-tl code{font:12px var(--mono);overflow-wrap:anywhere}.hl-tl:hover code{text-decoration:underline}
.hl-tl-m{display:flex;align-items:center;gap:8px;margin-top:2px;font-size:12px;min-width:0}
.hl-tl-m .lv-sub{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.hl-none{font-size:12px}
.hl-fil{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.hl-chips{display:flex;gap:4px;flex-wrap:wrap}
.hl-chips button{border:1px solid var(--line);background:var(--surface);color:var(--text-2);border-radius:999px;padding:2px 10px;font:600 12px var(--sans);cursor:pointer}
.hl-chips button.on{background:var(--accent-soft);color:var(--accent);border-color:color-mix(in srgb,var(--accent) 40%,transparent)}
.hl-filter{border-color:var(--line-2);background:var(--sunken);width:200px}
.hl-tt tbody tr{cursor:pointer;outline:none}
.hl-tt tbody tr:hover td,.hl-tt tbody tr:focus-visible td{background:var(--surface-2)}
.hl-tt tbody tr.on td{background:var(--accent-soft)}
.hl-tt tbody tr.hl-det{cursor:default}.hl-tt tbody tr.hl-det td{background:var(--surface-2)}
.hl-tn-c{display:flex;align-items:center;gap:6px;flex-wrap:wrap;min-width:260px}
.hl-slow{background:color-mix(in srgb,var(--lane-4) 16%,transparent);color:var(--lane-4)}
.hl-gone{background:var(--surface-2);color:var(--muted);border:1px dashed var(--line-2)}
.hl-fixed{background:color-mix(in srgb,var(--good) 14%,transparent);color:var(--good)}
.hl-quiet{display:flex;flex-direction:column;gap:6px;font-size:13px;line-height:1.5;padding:12px 16px}
.hl-persona{display:flex;flex-direction:column;gap:10px}
.hl-pc-t{display:flex;flex-direction:column;gap:4px}
.hl-pc-l{display:flex;flex-wrap:wrap;gap:8px}
.hl-pc-l .btn{display:inline-flex;align-items:center;gap:6px}
.hl-stable{font-size:13px;line-height:1.5;padding:8px 12px;border-radius:8px;background:color-mix(in srgb,var(--good) 9%,transparent);border:1px solid color-mix(in srgb,var(--good) 30%,transparent)}
.hl-stable.warn{background:var(--warn-soft);border-color:color-mix(in srgb,var(--warn) 30%,transparent)}
.hl-rr{display:flex;flex-wrap:wrap;align-items:center;gap:8px;font-size:13px;margin:8px 0}
.hl-rr .hl-rn{width:auto;padding:4px 8px}
.hl-rr-c{max-width:420px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.hl-rr{flex-direction:column;align-items:stretch}
.hl-rr-row{display:flex;flex-wrap:wrap;align-items:center;gap:8px}
.hl-rcmd{width:100%;font-family:var(--mono,monospace);font-size:12px}
.hl-rco{width:auto;max-width:100%;padding:4px 8px}
.hl-rr-note{font-size:12px}
.hl-codes{list-style:none;margin:6px 0;padding:0;font-size:13px;display:flex;flex-direction:column;gap:2px}
.hl-rr-res{display:inline-flex;align-items:center;gap:8px}
.hl-rr-bar{width:120px}
.hl-run{font:11px var(--mono);background:var(--surface-3);border-radius:4px;padding:1px 6px;color:var(--text-2)}
.hl-pr{display:flex;align-items:center;gap:8px;min-width:110px}.hl-pr .lv-bar{flex:1}.hl-pr span{font:12px var(--mono);color:var(--text-2);width:34px;text-align:right}
.hl-hist{display:flex;flex-direction:column;gap:8px;padding:4px 0}
.hl-why{color:var(--warn-ink)}
.hl-ht td{background:none!important}
.hl-cmd{font:11.5px var(--mono);color:var(--text-2);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:420px;display:inline-block;vertical-align:middle}
.hl-cl{list-style:none;margin:0;padding:0;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:4px 24px}
.hl-cl li{display:flex;align-items:center;gap:8px;padding:5px 0;border-bottom:1px solid var(--line);min-width:0}
.hl-cl .hl-cmd{flex:1;min-width:0}
.hl-cm-t{white-space:nowrap;font-size:12px}
.hl-foot{margin:0;color:var(--muted);font-size:12px}
@media (max-width:1240px){.hl-inc,.hl-rules{grid-template-columns:300px minmax(0,1fr)}.hl-bgrid{grid-template-columns:1fr}.hl-nums{grid-template-columns:repeat(3,minmax(0,1fr))}}
@media (max-width:980px){.hl-inc,.hl-rules,.hl-tgrid{grid-template-columns:1fr}.hl-cl{grid-template-columns:1fr}}
@media (max-width:640px){.hl{padding:14px}.hl-tabs{margin-left:0}.hl-nums{grid-template-columns:repeat(2,minmax(0,1fr))}.hl-n{font-size:30px}.hl-say{flex-wrap:wrap}}
`);
