// Ship (SPEC.md, "Fourth build", use cases 7 and 11): one agent's change
// before it merges. Blast radius: from its diff to the functions it
// changed, the tests that cover them, the decisions about those lines
// (ChloeCtx's blame and graph) and the other agents touching the same
// files or the callers, drawn as rings around the change, with the lists
// below and a risk line. Pull request: the draft (title, body with the
// decisions behind it, the blast radius and the tests run), the capsule
// of the agent's session packed beside it, and a box to ask the capsule.
// Opening the pull request stays yours: Copy body, or the `gh` command.
import {
  h, useStyle, toMs, fmtClock, fmtAgo, fmtDur, avatar, agentColor, ctxChip, pill, routeParam, mergeAgents,
  inlineCode, keyNav, rerender, later, tooltip, fmtSpan,
} from "./common.js";

const enc = encodeURIComponent;
const TAB_KEY = "chloedb-workbench-ship-tab";
const RISK_TONE = { low: "good", medium: "warn", high: "crit" };
const KIND_LABEL = { same_file: "same file", calls_changed: "calls a changed function", reading: "reading it", heading_into: "reading it" };
const VIA_LABEL = { blame: "blame of these lines", file: "names the file", graph: "ChloeCtx's graph" };

const base = (p) => String(p || "").split("/").pop();
const short = (s, n = 7) => String(s || "").slice(0, n);
const fnOf = (id) => String(id || "").split("#").pop();
const plural = (n, one, many = one + "s") => `${n} ${n === 1 ? one : many}`;
const kb = (n) => (n == null ? "" : n < 1024 ? `${n} B` : n < 1 << 20 ? `${Math.round(n / 1024)} KB` : `${(n / (1 << 20)).toFixed(1)} MB`);

function readTab() { try { return localStorage.getItem(TAB_KEY) || "blast"; } catch { return "blast"; } }
function saveTab(t) { try { localStorage.setItem(TAB_KEY, t); } catch { /* private mode */ } }

async function copy(text) {
  try { await navigator.clipboard.writeText(text); return true; } catch { /* fall back */ }
  const ta = h("textarea", { style: { position: "fixed", opacity: "0" } });
  ta.value = text;
  document.body.append(ta);
  ta.select();
  let ok = false;
  try { ok = document.execCommand("copy"); } catch { ok = false; }
  ta.remove();
  return ok;
}

export function mount(root, ctx) {
  const pick = () => {
    const list = ((ctx.state && ctx.state.agents) || []).filter((a) => a.running !== false);
    return (list.find((a) => a.state === "idle") || list[0] || {}).id || null;
  };
  const S = {
    id: routeParam(ctx, "agent") || pick(), tab: readTab() === "pr" ? "pr" : "blast",
    blast: null, blastErr: null, pr: null, prErr: null, loading: false,
    stream: (ctx.state && ctx.state.agents) || [], since: null,
    showReasons: false, raw: false, title: null, copied: null,
    packing: false, packErr: null,
    q: "", asking: false, answer: null, askErr: null,
    stack: null, stackErr: null, stacking: false, noAgent: false,
  };
  const el = h("div", { class: "lv-screen sh" });
  root.append(el);
  const tip = tooltip();
  const render = () => rerender(el, view);
  const reloadSoon = later(() => load(), 3000);
  let alive = true;

  const agentOf = () => mergeAgents(S.blast ? [S.blast.agent] : [], S.pr ? [S.pr.agent] : [], S.stream).get(S.id) || (S.blast && S.blast.agent) || null;

  // ------------------------------------------------------------- data
  async function load() {
    if (!S.id) { S.noAgent = true; render(); return; }
    S.noAgent = false;
    S.loading = true;
    render();
    const id = S.id;
    const b = ctx.api(`/api/agents/${enc(id)}/blast`).then((v) => { if (id === S.id) { S.blast = v; S.blastErr = null; } }, (e) => {
      if (id !== S.id) return;
      // An id that names no agent (an old link): pick one instead.
      if (e.status === 404 && /no agent/i.test(e.message || "")) { S.noAgent = true; return; }
      S.blastErr = e.status === 404 && !/agent/i.test(e.message) ? "This server has no blast radius route yet." : e.message || String(e);
    });
    const p = ctx.api(`/api/agents/${enc(id)}/pr`).then((v) => { if (id === S.id) { S.pr = v; S.prErr = null; } }, (e) => { if (id === S.id) S.prErr = e.message || String(e); });
    await Promise.allSettled([S.tab === "blast" ? b : p]);
    if (alive) render();
    await Promise.allSettled([b, p]);
    S.loading = false;
    if (alive) render();
  }

  // Ship together: one draft for the stack of branches, in merge order.
  async function together() {
    S.stacking = true; S.stackErr = null; render();
    try { S.stack = await ctx.api(`/api/agents/${enc(S.id)}/pr/stack`); }
    catch (e) { S.stackErr = e.message || String(e); }
    S.stacking = false;
    if (alive) render();
  }

  async function pack() {
    S.packing = true; S.packErr = null; render();
    try {
      const r = await ctx.api(`/api/agents/${enc(S.id)}/capsule`, { method: "POST", body: {} });
      if (r.available === false) S.packErr = r.why || "ChloeCtx could not pack a capsule.";
      else {
        if (S.pr) S.pr.capsule = r.capsule;
        const p = await ctx.api(`/api/agents/${enc(S.id)}/pr`).catch(() => null);
        if (p) S.pr = p;
      }
    } catch (e) { S.packErr = e.message || String(e); }
    S.packing = false;
    if (alive) render();
  }

  async function ask(q) {
    q = String(q || S.q).trim();
    if (!q) return;
    S.q = q; S.asking = true; S.askErr = null; render();
    try {
      const r = await ctx.api(`/api/agents/${enc(S.id)}/capsule/ask`, { method: "POST", body: { question: q } });
      if (r.available === false) { S.askErr = r.why; S.answer = null; } else S.answer = r;
    } catch (e) { S.askErr = e.message || String(e); }
    S.asking = false;
    if (alive) render();
  }

  function setTab(t) { S.tab = t; saveTab(t); render(); }

  async function doCopy(key, text) {
    const ok = await copy(text);
    S.copied = ok ? key : `fail:${key}`;
    render();
    setTimeout(() => { if (S.copied && S.copied.endsWith(key)) { S.copied = null; if (alive) render(); } }, 1600);
  }

  // ------------------------------------------------------------- view
  function pickView() {
    const list = (S.stream || []).slice().sort((x, y) => (toMs(y.created) || 0) - (toMs(x.created) || 0));
    return h("div", { class: "sh-wrap" }, h("div", { class: "lv-card sh-err" },
      h("b", {}, "Which agent's change do you want to ship?"),
      list.length ? h("div", { class: "sh-picklist" }, list.map((x) => h("button", { class: "sh-pickrow", "data-key": `pick:${x.id}`, onclick: () => ctx.go(`#/ship/${x.id}`) },
        avatar(x, 22), h("b", {}, x.name), h("code", { class: "muted sm" }, x.branch || ""), h("span", { class: "sh-sp" }), h("span", { class: "muted sm" }, x.running === false ? "exited" : x.state || ""))))
        : h("div", { class: "muted" }, "No agent yet. Ship shows what an agent's branch changes before it merges: start one with New agent.")));
  }

  function view() {
    if (S.noAgent) return pickView();
    const a = agentOf();
    if (!S.blast && !S.pr && (S.blastErr || S.prErr)) {
      return h("div", { class: "sh-wrap" }, h("div", { class: "lv-card sh-err" }, h("b", {}, "Ship could not load."), h("div", { class: "muted" }, S.blastErr || S.prErr),
        h("div", { class: "sh-row" }, h("button", { class: "btn sm", "data-key": "retry", onclick: () => load() }, "Try again"), h("button", { class: "btn sm ghost", "data-key": "back", onclick: () => ctx.go("#/map") }, "Map"))));
    }
    if (!a) return h("div", { class: "sh-wrap" }, h("div", { class: "sh-skel" }), h("div", { class: "sh-skel tall" }));
    return h("div", { class: "sh-wrap" }, header(a), riskLine(), S.tab === "blast" ? blastView(a) : prView(a));
  }

  function header(a) {
    const b = S.blast, p = S.pr;
    const t = b && b.totals;
    const others = ((ctx.state && ctx.state.agents) || []).filter((x) => x.repo === a.repo && x.id !== a.id && x.running !== false);
    return h("header", { class: "sh-head" },
      h("button", { class: "sh-crumb", "data-key": "crumb", onclick: () => ctx.go("#/map") }, "← ", base(a.repo) || "Map"),
      h("div", { class: "sh-title" },
        avatar(a, 38),
        h("div", { class: "sh-tt" },
          h("h1", {}, a.name, h("span", {}, " · Ship")),
          h("div", { class: "sh-sub" }, [
            h("span", {}, h("code", {}, a.branch || "its checkout"), " → ", h("code", {}, (p && p.base_branch) || (b && b.landing && b.landing.onto) || short(b && b.base) || "base")),
            t ? h("span", {}, h("b", { class: "n-add" }, `+${t.added}`), " ", h("b", { class: "n-del" }, `−${t.removed}`), ` in ${plural(t.files, "file")}`) : null,
            a.created ? h("span", {}, `started ${fmtClock(a.created)}, ${fmtDur((toMs(a.ended) || Date.now()) - (a.started_ms || toMs(a.created)))}`) : null,
            h("span", {}, a.state === "idle" ? "turn over" : a.state || ""),
          ].filter(Boolean))),
        h("div", { class: "sh-hr" },
          others.length ? h("select", { class: "sh-pick", "aria-label": "Another agent", "data-key": "pick", onchange: (e) => { if (e.target.value) ctx.go(`#/ship/${e.target.value}`); } },
            h("option", { value: "" }, "Another agent…"), others.map((o) => h("option", { value: o.id }, o.name))) : null,
          h("div", { class: "sh-tabs", role: "tablist", "aria-label": "Ship" },
            [["blast", "Blast radius", "b"], ["pr", "Pull request", "p"]].map(([k, label, key]) =>
              h("button", { role: "tab", "aria-selected": String(S.tab === k), class: S.tab === k ? "on" : null, "data-key": `tab:${k}`, title: `${label} (${key})`, onclick: () => setTab(k) }, label))),
          h("button", { class: "btn ghost icon sm", "data-key": "reload", title: "Read it again", "aria-label": "Read it again", onclick: () => load() }, S.loading ? h("span", { class: "sh-spin" }) : "↻"))));
  }

  function riskLine() {
    const r = S.blast && S.blast.risk;
    if (!r) return S.blastErr ? h("div", { class: "sh-risk mut" }, h("b", {}, "Blast radius unknown"), h("span", {}, S.blastErr)) : h("div", { class: "sh-risk mut sh-loading" }, "Reading the blast radius…");
    const tone = RISK_TONE[r.level] || "mut";
    const land = S.blast.landing || {};
    return h("div", { class: `sh-risk ${tone}` },
      h("span", { class: "sh-dot" }),
      h("div", { class: "sh-risk-t" },
        h("div", {}, h("b", {}, `${r.level[0].toUpperCase()}${r.level.slice(1)} risk`), " ", h("span", {}, r.line.replace(/^\w+ risk:\s*/i, ""))),
        (land.under || []).length ? h("div", { class: "sh-stack" }, (land.under || []).map((u) => h("span", { class: "sh-stack-i" },
          u.merged ? "has " : "on top of ", avatar(u.agent, 14), " ", h("b", {}, u.agent.name), " ", h("code", {}, u.branch),
          h("span", { class: "muted" }, ` · ${plural(u.commits, "commit")} of it ${u.merged ? "merged in" : "under this one"}, not counted here`)))) : null,
        (land.above || []).length ? h("div", { class: "sh-stack" }, h("span", { class: "muted" }, "On top of this branch: "), (land.above || []).map((u, i) => [i ? ", " : "", h("b", {}, u.agent.name)])) : null,
        S.showReasons && r.reasons && r.reasons.length ? h("ul", { class: "sh-reasons" }, r.reasons.map((x) => h("li", {}, inlineCode(x)))) : null),
      r.reasons && r.reasons.length ? h("button", { class: "lv-link", "data-key": "reasons", onclick: () => { S.showReasons = !S.showReasons; render(); } }, S.showReasons ? "Less" : `${plural(r.reasons.length, "reason")} ▾`) : null);
  }

  // ------------------------------------------------------------- blast
  function blastView(a) {
    const b = S.blast;
    if (!b) return h("div", { class: "sh-grid" }, h("div", { class: "sh-skel tall" }), h("div", { class: "sh-skel" }));
    return h("div", {},
      h("div", { class: "sh-top" },
        h("section", { class: "lv-card sh-graph" },
          h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Blast radius"),
            h("div", { class: "sh-legend" },
              h("span", {}, h("i", { class: "lg-fn" }), "functions"), h("span", {}, h("i", { class: "lg-test" }), "tests"),
              h("span", {}, h("i", { class: "lg-dec" }), "decisions"), h("span", {}, h("i", { class: "lg-ag" }), "agents"))),
          radial(b, a)),
        sideCard(b)),
      h("div", { class: "sh-lists" }, functionsCard(b, a), testsCard(b), decisionsCard(b, a), agentsCard(b)));
  }

  function sideCard(b) {
    const t = b.totals || {};
    const src = b.sources || {};
    const tile = (n, label, tone) => h("div", { class: `sh-tile${tone ? " " + tone : ""}` }, h("b", {}, String(n ?? 0)), h("span", {}, label));
    const untested = (b.functions || []).filter((f) => !(f.tests || []).length && f.status !== "deleted");
    return h("aside", { class: "lv-card sh-side" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "If this merges")),
      h("div", { class: "sh-tiles" },
        tile(t.functions, "functions changed"), tile(t.callers, t.test_callers ? `callers (+${t.test_callers} in tests)` : "callers"), tile(t.tests, "tests cover them", untested.length ? "warn" : null),
        tile(b.decided ? b.decided.count : t.decisions, "decisions", "ctx"), tile(t.agents, "other agents", t.agents ? "warn" : null), tile(t.files, "files")),
      untested.length ? h("div", { class: "sh-note warn" }, h("b", {}, "No test names "), untested.map((f, i) => [i ? ", " : "", h("code", { class: "lv-code" }, f.name)]))
        : (b.functions || []).length ? h("div", { class: "sh-note good" }, `Every changed function is named by a test (${plural(b.functions.length, "function")}).`)
        : (b.test_functions || []).length ? h("div", { class: "sh-note" }, h("b", {}, "Only tests changed: "), b.test_functions.map((f, i) => [i ? ", " : "", h("code", { class: "lv-code" }, f.name), h("span", { class: "muted" }, ` ${f.status === "added" ? "new" : f.status === "deleted" ? "removed" : "changed"}`)]))
        : h("div", { class: "sh-note" }, "No function changed, so there is nothing for a test to name."),
      (b.related_files || []).length ? h("div", { class: "sh-rel" }, h("span", { class: "muted" }, "ChloeCtx's graph also links "),
        b.related_files.map((r, i) => [i ? ", " : "", h("code", { class: "lv-code", title: `via ${r.via.join(", ")}` }, r.path)]), h("span", { class: "muted" }, " to these decisions.")) : null,
      h("div", { class: "sh-src" },
        h("span", { class: src.test_runs ? "ok" : "no" }, src.test_runs ? "test runs read" : "no test_runs table"),
        h("span", { class: src.chloectx ? "ok" : "no" }, src.chloectx ? `ChloeCtx: ${plural(src.blame || 0, "line")} blamed` : "no ChloeCtx"),
        h("span", { class: src.graph ? "ok" : "no" }, src.graph ? "graph read" : "no graph")),
      (b.missing || []).length ? h("ul", { class: "sh-missing" }, b.missing.map((m) => h("li", {}, m))) : null,
      h("button", { class: "btn primary sm sh-cta", "data-key": "to-pr", onclick: () => setTab("pr") }, "Draft the pull request →"));
  }

  // The rings: the change in the middle, then functions, tests, decisions
  // and agents, each placed near the functions it touches.
  function radial(b, a) {
    // Rings are ellipses, wider than tall, to use a wide card.
    const W = 1000, H = 520, cx = W / 2, cy = H / 2 + 4, KX = 1.75;
    const R = { fn: 84, test: 138, dec: 184, ag: 226 };
    const at = (r, t) => [cx + KX * r * Math.cos(t), cy + r * Math.sin(t)];
    const fns = [...(b.functions || [])].sort((x, y) => x.path.localeCompare(y.path) || (x.line || 0) - (y.line || 0));
    if (!fns.length) {
      return h("div", { class: "sh-empty" }, (b.files || []).length ? `${plural(b.files.length, "file")} changed, none with a function this finder knows (it reads Rust, TypeScript, JavaScript, Python, Go, Java, C and Ruby).` : "No changes yet: nothing can break.");
    }
    const ang = new Map();
    fns.forEach((f, i) => ang.set(f.id, -Math.PI / 2 + (i * 2 * Math.PI) / fns.length));
    const mean = (ids) => {
      let x = 0, y = 0;
      for (const id of ids) { const t = ang.get(id); if (t != null) { x += Math.cos(t); y += Math.sin(t); } }
      return x || y ? Math.atan2(y, x) : null;
    };
    const place = (list, r, gapPx) => {
      const items = list.map((n, i) => ({ ...n, t: mean(n.links) ?? (-Math.PI / 2 + ((i + 0.5) * 2 * Math.PI) / list.length) }));
      items.sort((p, q) => p.t - q.t);
      const gap = Math.min(gapPx / r, (2 * Math.PI) / Math.max(1, items.length));
      for (let k = 0; k < 60; k++) {
        let moved = false;
        for (let i = 0; i < items.length; i++) {
          const p = items[i], q = items[(i + 1) % items.length];
          if (items.length < 2) break;
          let d = q.t - p.t;
          if (i === items.length - 1) d += 2 * Math.PI;
          if (d < gap) { const push = (gap - d) / 2; p.t -= push; q.t += push; moved = true; }
        }
        if (!moved) break;
      }
      return items.map((n) => { const [x, y] = at(r, n.t); return { ...n, r, x, y }; });
    };
    const runsTone = (t) => !t.runs ? "none" : t.runs.flaky ? "flaky" : t.runs.last_passed === false ? "fail" : "pass";
    const tests = place((b.tests || []).map((t) => ({ key: `t:${t.path}`, type: "test", item: t, links: t.covers || [], label: base(t.path), tone: runsTone(t) })), R.test, 74);
    const decs = place((b.decisions || []).map((d) => ({ key: `d:${d.id}`, type: "dec", item: d, links: d.functions || [], label: (d.superseded ? "was: " : "") + d.text.split(/\s+/).slice(0, 3).join(" ") + "…" })), R.dec, 92);
    const ags = place((b.agents || []).map((g) => ({ key: `a:${g.agent.id}`, type: "ag", item: g, links: g.functions || [], label: g.agent.name })), R.ag, 90);
    // An agent at a decision's angle would sit on that decision's label:
    // it moves aside.
    for (const n of ags) {
      const d = decs.find((x) => Math.abs(Math.atan2(Math.sin(x.t - n.t), Math.cos(x.t - n.t))) < 0.2);
      if (!d) continue;
      n.t = d.t + (Math.sin(n.t - d.t) >= 0 ? 0.24 : -0.24);
      [n.x, n.y] = at(n.r, n.t);
    }
    const fnNodes = fns.map((f) => { const t = ang.get(f.id), [x, y] = at(R.fn, t); return { key: `f:${f.id}`, type: "fn", item: f, t, r: R.fn, x, y }; });
    const fnPos = new Map(fnNodes.map((n) => [n.item.id, n]));

    const svgNS = "http://www.w3.org/2000/svg";
    const s = (tag, attrs, ...kids) => {
      const e = document.createElementNS(svgNS, tag);
      for (const [k, v] of Object.entries(attrs || {})) if (v != null) e.setAttribute(k, v);
      for (const k of kids.flat()) if (k != null) e.append(k instanceof Node ? k : document.createTextNode(String(k)));
      return e;
    };
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "sh-svg", role: "img", "aria-label": `Blast radius of ${a.name}'s change` });
    for (const r of [R.fn, R.test, R.dec, R.ag]) svg.append(s("ellipse", { cx, cy, rx: KX * r, ry: r, class: "ring" }));

    const curve = (p, q) => {
      const m = (p.r + q.r) / 2;
      const c1 = at(m, p.t), c2 = at(m, q.t);
      return `M${p.x.toFixed(1)},${p.y.toFixed(1)} C${c1[0].toFixed(1)},${c1[1].toFixed(1)} ${c2[0].toFixed(1)},${c2[1].toFixed(1)} ${q.x.toFixed(1)},${q.y.toFixed(1)}`;
    };
    const edges = s("g", { class: "edges" });
    const edgeList = [];
    for (const f of fnNodes) {
      const e = s("line", { x1: cx, y1: cy, x2: f.x, y2: f.y, class: "spoke" });
      edgeList.push([e, "c", f.key]);
      edges.append(e);
    }
    for (const n of [...tests, ...decs, ...ags]) {
      for (const id of n.links) {
        const f = fnPos.get(id);
        if (!f) continue;
        const style = n.type === "ag" ? { stroke: agentColor(n.item.agent) } : {};
        const e = s("path", { d: curve(f, n), class: `edge e-${n.type}${n.type === "test" ? " " + n.tone : ""}`, style: style.stroke ? `stroke:${style.stroke}` : null });
        edgeList.push([e, f.key, n.key]);
        edges.append(e);
      }
    }
    svg.append(edges);

    const nodes = s("g", { class: "nodes" });
    const nodeEls = new Map();
    const side = (n) => (Math.cos(n.t) > 0.25 ? "start" : Math.cos(n.t) < -0.25 ? "end" : "middle");
    // Tests' names go under them and decisions' over them, so that rings
    // at one angle do not write over each other; agents' go outward.
    const below = (n, d) => ({ x: n.x, y: n.y + d + 8, "text-anchor": "middle" });
    const above = (n, d) => ({ x: n.x, y: n.y - d, "text-anchor": "middle" });
    const labelAt = (n, d) => {
      const anchor = side(n);
      const dx = anchor === "start" ? d : anchor === "end" ? -d : 0;
      const dy = anchor === "middle" ? (Math.sin(n.t) > 0 ? d + 9 : -d) : 4;
      return { x: n.x + dx, y: n.y + dy, "text-anchor": anchor };
    };
    // The change.
    const t = b.totals || {};
    nodes.append(s("g", { class: "center" },
      s("circle", { cx, cy, r: 52 }),
      s("text", { x: cx, y: cy - 6, "text-anchor": "middle", class: "c1" }, base(a.branch || a.name)),
      s("text", { x: cx, y: cy + 12, "text-anchor": "middle", class: "c2" }, `+${t.added || 0} −${t.removed || 0}`)));
    for (const n of fnNodes) {
      const f = n.item;
      const w = Math.max(46, f.name.length * 7 + 18);
      const g = s("g", { class: `node fn ${f.status}${(f.tests || []).length ? "" : " untested"}`, tabindex: 0, "data-key": n.key },
        s("rect", { x: n.x - w / 2, y: n.y - 11, width: w, height: 22, rx: 11 }),
        s("text", { x: n.x, y: n.y + 4, "text-anchor": "middle" }, f.name));
      nodes.append(g);
      nodeEls.set(n.key, [g, n]);
    }
    for (const n of tests) {
      const g = s("g", { class: `node test ${n.tone}`, tabindex: 0, "data-key": n.key },
        s("circle", { cx: n.x, cy: n.y, r: 7 }),
        s("text", Math.sin(n.t) < -0.8 ? above(n, 12) : below(n, 10), n.label));
      nodes.append(g);
      nodeEls.set(n.key, [g, n]);
    }
    for (const n of decs) {
      const d = n.item;
      const g = s("g", { class: `node dec${d.superseded ? " old" : ""}`, tabindex: 0, "data-key": n.key },
        s("rect", { x: n.x - 7, y: n.y - 7, width: 14, height: 14, rx: 2.5, transform: `rotate(45 ${n.x} ${n.y})` }),
        s("text", Math.sin(n.t) > 0.8 ? below(n, 12) : above(n, 13), n.label));
      nodes.append(g);
      nodeEls.set(n.key, [g, n]);
    }
    for (const n of ags) {
      const ag = n.item.agent;
      const g = s("g", { class: "node ag", tabindex: 0, "data-key": n.key },
        s("rect", { x: n.x - 12, y: n.y - 12, width: 24, height: 24, rx: 7, style: `fill:${agentColor(ag)}` }),
        s("text", { x: n.x, y: n.y + 4.5, "text-anchor": "middle", class: "ini" }, (ag.name || "?")[0].toUpperCase()),
        s("text", labelAt(n, 18), n.label));
      nodes.append(g);
      nodeEls.set(n.key, [g, n]);
    }
    svg.append(nodes);

    // Hover: the node, its edges and what they reach stay; the rest dims.
    const near = (key) => {
      const keep = new Set([key]);
      const n = nodeEls.get(key);
      if (n && n[1].type !== "fn") for (const id of n[1].links || []) keep.add(`f:${id}`);
      for (const [, p, q] of edgeList) if (p === key || q === key) { keep.add(p); keep.add(q); }
      return keep;
    };
    const focus = (key) => {
      if (!key) { svg.classList.remove("hot"); for (const [g] of nodeEls.values()) g.classList.remove("on"); for (const [e] of edgeList) e.classList.remove("on"); return; }
      const keep = near(key);
      svg.classList.add("hot");
      for (const [k, [g]] of nodeEls) g.classList.toggle("on", keep.has(k));
      for (const [e, p, q] of edgeList) e.classList.toggle("on", p === key || q === key || (p === "c" && keep.has(q) && key.startsWith("f:")));
    };
    const describe = (n) => {
      const it = n.item;
      if (n.type === "fn") return [h("b", {}, it.name), h("div", { class: "muted" }, `${it.path}:${it.line} · ${it.status} · +${it.added} −${it.removed}`), h("div", {}, `${plural(it.callers_total || 0, "caller")} · ${plural((it.tests || []).length, "test")} · ${plural((it.decisions || []).length, "decision")}`)];
      if (n.type === "test") return [h("b", {}, it.path), h("div", { class: "muted" }, (it.why || []).join("; ")), h("div", {}, runsText(it.runs))];
      if (n.type === "dec") return [ctxChip(`${it.kind} ${it.id}`), h("div", {}, it.text), it.superseded ? h("div", { class: "muted" }, `superseded${it.superseded_by ? ` by ${it.superseded_by}` : ""}`) : null];
      return [h("b", {}, it.agent.name), h("div", {}, it.detail)];
    };
    svg.addEventListener("pointerover", (e) => {
      const g = e.target.closest && e.target.closest(".node");
      if (!g) return;
      focus(g.dataset.key);
      const n = nodeEls.get(g.dataset.key);
      if (n) tip.show(describe(n[1]), e.clientX, e.clientY);
    });
    svg.addEventListener("pointermove", (e) => { if (e.target.closest && e.target.closest(".node")) { const n = nodeEls.get(e.target.closest(".node").dataset.key); if (n) tip.show(describe(n[1]), e.clientX, e.clientY); } });
    svg.addEventListener("pointerout", (e) => { if (!e.relatedTarget || !svg.contains(e.relatedTarget) || !e.relatedTarget.closest(".node")) { focus(null); tip.hide(); } });
    svg.addEventListener("focusin", (e) => { const g = e.target.closest(".node"); if (g) focus(g.dataset.key); });
    svg.addEventListener("focusout", () => focus(null));
    svg.addEventListener("click", (e) => {
      const g = e.target.closest && e.target.closest(".node");
      if (!g) return;
      const n = nodeEls.get(g.dataset.key)[1];
      openNode(n.type, n.item, a);
    });
    hoverHook = focus;
    return h("div", { class: "sh-svgbox" }, svg);
  }
  let hoverHook = null;
  const hover = (key) => ({ onpointerenter: () => hoverHook && hoverHook(key), onpointerleave: () => hoverHook && hoverHook(null) });

  function openNode(type, it, a) {
    if (type === "fn") ctx.openWhy({ agent: a.id, path: it.path, line: it.line });
    else if (type === "dec" && it.line) ctx.openWhy({ agent: a.id, path: it.line.path, line: it.line.line });
    else if (type === "ag") ctx.go(`#/story/${it.agent.id}`);
  }

  function runsText(r) {
    if (!r) return "not run by an agent yet";
    const base_ = `${r.passed} of ${r.total} passed`;
    return `${base_}${r.flaky ? ", flaky" : ""}${r.p50_ms != null ? ` · p50 ${fmtSpan(r.p50_ms)}` : ""}${r.last ? ` · last ${fmtAgo(r.last)}` : ""}`;
  }

  function functionsCard(b, a) {
    const fns = b.functions || [];
    return h("section", { class: "lv-card sh-list" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Functions changed ", h("span", { class: "n" }, String(fns.length))), h("span", { class: "muted sm" }, "click: why this line")),
      fns.length ? fns.map((f) => h("button", { class: "sh-row-i", "data-nav": "", "data-key": `fn:${f.id}`, ...hover(`f:${f.id}`), onclick: () => openNode("fn", f, a) },
        h("div", { class: "sh-r1" },
          h("span", { class: `sh-st ${f.status}` }, f.status === "added" ? "new" : f.status === "deleted" ? "gone" : "changed"),
          h("code", { class: "sh-fn" }, f.name), f.kind && f.kind !== "function" ? h("span", { class: "muted sm" }, f.kind) : null,
          h("span", { class: "sh-sp" }),
          h("span", { class: "n-add" }, f.added ? `+${f.added}` : ""), h("span", { class: "n-del" }, f.removed ? `−${f.removed}` : "")),
        h("div", { class: "sh-r2" },
          h("span", { class: "sh-path" }, `${f.path}:${f.line}`),
          h("span", {}, plural(f.callers_total || 0, "caller"), f.test_callers ? ` · ${f.test_callers} in tests` : ""),
          (f.tests || []).length ? h("span", { class: "good-t" }, plural(f.tests.length, "test")) : h("span", { class: "crit-t" }, "no test"),
          (f.decisions || []).length ? ctxChip(String(f.decisions.length), "decisions about it") : null)))
        : h("div", { class: "lv-empty" }, "No function changed."));
  }

  function testsCard(b) {
    const ts = b.tests || [];
    const src = b.sources || {};
    return h("section", { class: "lv-card sh-list" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Tests that cover them ", h("span", { class: "n" }, String(ts.length))), h("span", { class: "muted sm" }, "ranked")),
      ts.length ? ts.map((t, i) => h("div", { class: "sh-row-i", tabindex: 0, "data-nav": "", "data-key": `t:${t.path}`, ...hover(`t:${t.path}`) },
        h("div", { class: "sh-r1" }, h("span", { class: "sh-rank" }, String(i + 1)), h("code", { class: "sh-fn" }, t.path), h("span", { class: "sh-sp" }),
          !t.runs ? pill("not run", "mut") : t.runs.flaky ? pill(`flaky ${t.runs.passed}/${t.runs.total}`, "warn") : t.runs.last_passed === false ? pill(`failing: ${(t.runs.failing || []).slice(0, 2).join(", ") || "last run"}`, "crit") : pill("passing on its last run", "good")),
        h("div", { class: "sh-r2" }, h("span", {}, (t.why || []).join(" · ")),
          t.runs && (t.runs.fixed || []).length ? h("span", { class: "good-t", title: "Failed first, then passed after a change: a failure before the fix" }, `fixed: ${t.runs.fixed.slice(0, 2).join(", ")}`) : null,
          t.runs && t.runs.on_head ? h("span", {}, `on this commit ${t.runs.on_head.passed}/${t.runs.on_head.total}`) : null,
          t.runs && t.runs.p50_ms != null ? h("span", {}, `p50 ${fmtSpan(t.runs.p50_ms)}`) : null)))
        : h("div", { class: "lv-empty" }, "No test file names these functions or imports their files."),
      h("div", { class: "sh-foot" }, src.test_runs ? "Runs from the test_runs table (Health): what agents ran on these tests." : "No test_runs table yet (Health keeps one): ranked by the test files that name the functions or import their files."));
  }

  function decisionsCard(b, a) {
    const ds = b.decisions || [];
    return h("section", { class: "lv-card sh-list" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Decisions about these lines ", h("span", { class: "n" }, String(ds.length))), ctxChip("ChloeCtx")),
      ds.length ? ds.map((d) => h("div", { class: `sh-row-i sh-dec${d.superseded ? " old" : ""}`, tabindex: 0, "data-nav": "", "data-key": `d:${d.id}`, ...hover(`d:${d.id}`), onclick: () => openNode("dec", d, a) },
        h("div", { class: "sh-r1" }, ctxChip(`${d.kind} ${d.id}`), h("span", { class: "sh-sp" }), (d.via || []).map((v) => h("span", { class: "sh-via", title: VIA_LABEL[v] || v }, v))),
        h("div", { class: "sh-dtext" }, d.text),
        h("div", { class: "sh-r2" },
          d.agent ? h("span", {}, avatar(d.agent, 14), " ", d.agent.name) : h("span", {}, "another session"),
          d.ts ? h("span", {}, fmtAgo(d.ts)) : null,
          d.superseded ? h("span", { class: "warn-t" }, `superseded${d.superseded_by ? ` by ${d.superseded_by}` : ""}`) : null,
          d.line ? h("span", { class: "lv-link" }, "Why? →") : null)))
        : h("div", { class: "lv-empty" }, (b.sources || {}).chloectx ? "ChloeCtx holds no decision about these lines." : "ChloeCtx is not installed, so no decisions can be found."));
  }

  function agentsCard(b) {
    const gs = b.agents || [];
    return h("section", { class: "lv-card sh-list" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Other agents in the way ", h("span", { class: "n" }, String(gs.length)))),
      gs.length ? gs.map((g) => h("div", { class: "sh-row-i", tabindex: 0, "data-nav": "", "data-key": `a:${g.agent.id}`, ...hover(`a:${g.agent.id}`) },
        h("div", { class: "sh-r1" }, avatar(g.agent, 18), h("b", {}, g.agent.name), h("span", { class: "sh-sp" }),
          g.merges_clean === true ? pill("merges cleanly", "good") : g.merges_clean === false ? pill("conflicts", "crit") : null,
          (g.kinds || []).map((k) => pill(KIND_LABEL[k] || k, k === "same_file" ? (g.merges_clean === true ? "mut" : "crit") : k === "calls_changed" ? "warn" : "mut"))),
        h("div", { class: "sh-dtext" }, g.detail),
        h("div", { class: "sh-r2" },
          h("button", { class: "lv-link", onclick: () => ctx.go(`#/story/${g.agent.id}`) }, "Story"),
          h("button", { class: "lv-link", onclick: () => ctx.zoom(g.agent.id) }, "Terminal"),
          h("button", { class: "lv-link", onclick: () => ctx.go(`#/ship/${g.agent.id}`) }, "Its blast radius"))))
        : h("div", { class: "lv-empty" }, "No other agent touches these files or their callers."));
  }

  // ------------------------------------------------------------- pull request
  function prView(a) {
    const p = S.pr;
    if (!p) return S.prErr ? h("div", { class: "lv-card sh-err" }, h("b", {}, "The draft could not be made."), h("div", { class: "muted" }, S.prErr)) : h("div", { class: "sh-pr" }, h("div", { class: "sh-skel tall" }), h("div", { class: "sh-skel" }));
    const title = S.title ?? p.title;
    const gh = ghCommand(p, title);
    return h("div", { class: "sh-pr" },
      h("section", { class: "lv-card sh-draft" },
        h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Pull request draft"),
          h("span", { class: "sh-bh" }, h("code", {}, p.base_branch || "main"), " ← ", h("code", {}, p.branch || a.branch))),
        h("input", { class: "sh-title-in", "data-key": "title", value: title, "aria-label": "Title", oninput: (e) => { S.title = e.target.value; const c = el.querySelector(".sh-gh code"); if (c) c.textContent = ghCommand(p, S.title); } }),
        p.title_from ? h("div", { class: "muted sm sh-from" }, `Title from ${{ outcome: "its outcome", goal: "the goal it leads", commit: "its commit", summary: "what it said when it finished", task: "what it was asked to do", name: "its name" }[p.title_from] || p.title_from}.`) : null,
        stackCard(p),
        h("div", { class: "sh-bodybar" },
          h("div", { class: "sh-seg" }, h("button", { class: S.raw ? null : "on", "data-key": "md-off", onclick: () => { S.raw = false; render(); } }, "Preview"), h("button", { class: S.raw ? "on" : null, "data-key": "md-on", onclick: () => { S.raw = true; render(); } }, "Markdown")),
          h("span", { class: "sh-sp" }),
          h("span", { class: "muted sm" }, p.body_file ? `saved to ${p.body_file}` : "")),
        S.raw ? h("pre", { class: "sh-raw" }, p.body) : h("div", { class: "sh-md" }, markdown(p.body)),
        h("div", { class: "sh-actions" },
          h("button", { class: "btn primary sm", "data-key": "copy-body", onclick: () => doCopy("body", p.body) }, S.copied === "body" ? "Copied" : "Copy body"),
          h("button", { class: "btn sm", "data-key": "copy-gh", onclick: () => doCopy("gh", gh) }, S.copied === "gh" ? "Copied" : "Copy gh command"),
          h("button", { class: "btn sm ghost", "data-key": "copy-title", onclick: () => doCopy("title", title) }, S.copied === "title" ? "Copied" : "Copy title"),
          S.copied && S.copied.startsWith("fail:") ? h("span", { class: "crit-t" }, "The browser did not allow copying; select the text instead.") : null),
        h("div", { class: "sh-gh" },
          h("div", { class: "muted sm" }, "Workbench does not open the pull request: push the branch, then run this in the repository."),
          p.push ? h("code", { class: "sh-cmd" }, p.push) : null,
          h("code", { class: "sh-cmd" }, gh)),
        (p.missing || []).length ? h("ul", { class: "sh-missing" }, p.missing.map((m) => h("li", {}, m))) : null),
      h("aside", { class: "sh-pside" }, capsuleCard(a, p), askCard(a, p), testsRunCard(p)));
  }

  function stackCard(p) {
    const chain = p.stack || [];
    if (chain.length < 2) return null;
    const st = S.stack;
    return h("div", { class: "sh-stackcard" },
      h("div", { class: "sh-r1" }, h("b", {}, "Stacked branches"), h("span", { class: "muted sm" }, " in merge order: "),
        chain.map((g, i) => [i ? h("span", { class: "muted" }, " → ") : null, h("span", { class: g.id === S.id ? "sh-me" : null }, g.name)]),
        h("span", { class: "sh-sp" }),
        h("button", { class: "btn sm", "data-key": "together", disabled: S.stacking, onclick: () => (st ? (S.stack = null, render()) : together()) }, S.stacking ? "Drafting…" : st ? "Back to this branch" : "Ship together")),
      S.stackErr ? h("div", { class: "sh-note crit" }, S.stackErr) : null,
      st ? h("div", { class: "sh-together" },
        h("div", { class: "muted sm" }, `One pull request of ${st.branch} onto ${st.base_branch}, its body in ${plural((st.parts || []).length, "part")}, saved to ${st.body_file}.`),
        h("div", { class: "sh-parts" }, (st.parts || []).map((x, i) => h("div", { class: "sh-part" }, h("span", { class: "sh-rank" }, String(i + 1)), avatar(x.agent, 16), h("b", {}, x.title), h("code", { class: "muted sm" }, x.branch || ""),
          x.risk ? pill(`${x.risk.level} risk`, RISK_TONE[x.risk.level] || "mut") : null))),
        h("div", { class: "sh-md" }, markdown(st.body)),
        h("div", { class: "sh-actions" },
          h("button", { class: "btn primary sm", "data-key": "copy-stack", onclick: () => doCopy("stack", st.body) }, S.copied === "stack" ? "Copied" : "Copy the stack's body"),
          h("button", { class: "btn sm", "data-key": "copy-stack-gh", onclick: () => doCopy("stackgh", st.gh) }, S.copied === "stackgh" ? "Copied" : "Copy gh command")),
        h("code", { class: "sh-cmd" }, st.push), h("code", { class: "sh-cmd" }, st.gh)) : null);
  }

  function ghCommand(p, title) {
    const q = (s) => `"${String(s).replace(/(["\\$`])/g, "\\$1")}"`;
    return `gh pr create --base ${p.base_branch || "main"} --head ${p.branch} --title ${q(title)} --body-file ${p.body_file}`;
  }

  function capsuleCard(a, p) {
    const c = p.capsule;
    const kinds = c ? Object.entries(c.by_kind || {}).sort((x, y) => y[1] - x[1]) : [];
    const max = Math.max(1, ...kinds.map((k) => k[1]));
    const KEY = ["decision", "preference", "fact"];
    return h("section", { class: "lv-card sh-cap" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Capsule"), ctxChip("chloectx pack")),
      c ? [
        h("div", { class: "sh-cap-t" }, h("b", {}, plural(c.records, "record")), h("span", { class: "muted" }, ` · ${kb(c.bytes)} · packed ${fmtAgo(c.created)}`)),
        h("div", { class: "muted sm" }, `Scope: ${c.scope || "its session"}.`),
        h("div", { class: "sh-kinds" }, kinds.slice(0, 8).map(([k, n]) => h("div", { class: `sh-kind${KEY.includes(k) ? " key" : ""}` }, h("span", {}, k), h("i", { style: { width: `${Math.max(4, (n / max) * 100)}%` } }), h("b", {}, String(n))))),
        h("div", { class: "sh-mount" }, h("div", { class: "muted sm" }, "Whoever reviews it mounts it in Claude Code:"),
          h("code", { class: "sh-cmd" }, c.claude || c.mount),
          h("div", { class: "sh-row" }, h("button", { class: "btn sm", "data-key": "copy-mount", onclick: () => doCopy("mount", c.claude || c.mount) }, S.copied === "mount" ? "Copied" : "Copy"), h("span", { class: "muted sm", title: c.sha256 }, `sha256 ${short(c.sha256, 12)}`))),
      ] : h("div", { class: "sh-cap-none" }, h("p", {}, `No capsule yet. Packing one puts the decisions, prompts and answers of ${a.name}'s session in a read-only file next to the pull request, so a reviewer can ask why instead of guessing.`)),
      S.packErr ? h("div", { class: "sh-note crit" }, S.packErr) : null,
      h("div", { class: "sh-row" }, h("button", { class: `btn sm${c ? "" : " primary"}`, "data-key": "pack", disabled: S.packing, onclick: () => pack() }, S.packing ? "Packing…" : c ? "Pack again" : "Pack the capsule"),
        c ? h("span", { class: "muted sm" }, c.dir) : null));
  }

  function askCard(a, p) {
    const ds = (p.sections && p.sections.decisions) || [];
    const suggest = ["Why was it done this way?", ...ds.filter((d) => !d.superseded).slice(0, 2).map((d) => `Why: ${d.text.split(/[,;.]/)[0].toLowerCase()}?`)];
    const ans = S.answer;
    return h("section", { class: "lv-card sh-ask" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Ask the capsule"), h("span", { class: "muted sm" }, p.capsule ? "no model call" : "no capsule: asks the repository's memory")),
      h("form", { class: "sh-askf", onsubmit: (e) => { e.preventDefault(); ask(); } },
        h("input", { "data-key": "q", placeholder: "Why per line and not on the total?", onfocus: (e) => import("./complete.js").then((m) => m.attachComplete(e.target, { field: "why", ctx })).catch(() => {}), value: S.q, oninput: (e) => { S.q = e.target.value; } }),
        h("button", { class: "btn primary sm", type: "submit", disabled: S.asking }, S.asking ? "Asking…" : "Ask")),
      !ans && !S.askErr ? h("div", { class: "sh-sugg" }, suggest.map((q) => h("button", { class: "sh-chip", onclick: () => ask(q) }, q))) : null,
      S.askErr ? h("div", { class: "sh-note crit" }, S.askErr) : null,
      ans ? h("div", { class: "sh-ans" },
        h("div", { class: "sh-r2" }, pill(ans.via === "capsule" ? "from the capsule" : "from the repository's memory", ans.via === "capsule" ? "vio" : "mut"), ans.ms != null ? h("span", {}, `${Math.round(ans.ms)} ms`) : null, h("span", {}, plural((ans.ids || []).length, "record"))),
        h("div", { class: "sh-ans-t" }, answerText(ans)),
        (ans.missing || []).length ? h("ul", { class: "sh-missing" }, ans.missing.map((m) => h("li", {}, m))) : null) : null);
  }

  function answerText(ans) {
    const agents = ans.agents || {};
    return String(ans.text || "").split("\n").map((line) => h("div", { class: line.trim() ? null : "gap" }, line.split(/(\[[0-9a-z]+:[0-9]+\])/i).map((part) => {
      const m = part.match(/^\[([0-9a-z]+:[0-9]+)\]$/i);
      if (!m) return part;
      const ag = agents[m[1]];
      return ctxChip(m[1], ag ? `from ${ag.name}'s session` : "a ChloeCtx record");
    })));
  }

  function testsRunCard(p) {
    const sec = p.sections || {};
    const ts = sec.tests || [];
    const merged = sec.merged || [];
    if (!ts.length && !merged.length) return null;
    const named = ts.filter((t) => t.kind === "test");
    const mark = (ok) => h("span", { class: `sh-ok ${ok === true ? "y" : ok === false ? "n" : "u"}` }, ok === true ? "✓" : ok === false ? "✕" : "?");
    return h("section", { class: "lv-card sh-list" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Tests it ran ", h("span", { class: "n" }, String(ts.length))),
        named.length ? h("span", { class: "muted sm" }, `${named.filter((t) => t.ok).length} of ${named.length} pass on their last run`) : null),
      ts.map((t) => h("div", { class: "sh-trun" }, mark(t.ok),
        h("code", { title: t.name }, t.kind === "command" ? `$ ${t.name}` : t.name), h("span", { class: "sh-sp" }),
        h("span", { class: "muted sm" }, [
          t.kind === "test" ? (t.runs > 1 ? `${t.passed}/${t.runs} runs passed` : t.ok ? "passed" : "failed") : t.summary || (t.ok == null ? "result unknown" : ""),
          t.fixed ? " · fixed in this work" : "", t.flaky ? " · flaky" : "", t.ts ? ` · ${fmtClock(t.ts)}` : ""].join("")))),
      merged.map((m) => h("div", { class: "sh-trun sh-merged" }, mark(m.failed === 0),
        h("span", {}, "Merged tree ", h("code", {}, short(m.commit)), (m.branches || []).length ? ` (${m.branches.join(" + ")})` : ""), h("span", { class: "sh-sp" }),
        h("span", { class: "muted sm" }, `${m.passed}/${m.tests} passed${m.by ? ` · ${m.by}` : ""}`))),
      sec.tests_from === "events" ? h("div", { class: "sh-foot" }, "No test_runs rows for this branch: these are the test commands from its terminal, with the counts their output printed.") : null);
  }

  // ------------------------------------------------------------- life
  const off = keyNav(el, {
    onKey: (e) => {
      if (e.key === "b") { setTab("blast"); return true; }
      if (e.key === "p") { setTab("pr"); return true; }
      return false;
    },
  });
  load();
  return {
    update(state) {
      S.stream = state.agents || S.stream;
      if (S.noAgent) render();
      const a = S.stream.find((x) => x.id === S.id);
      const stamp = a ? `${a.state}:${a.state_since}` : null;
      if (S.since && stamp && stamp !== S.since) reloadSoon();
      S.since = stamp;
    },
    unmount() { alive = false; off(); tip.remove(); reloadSoon.cancel(); el.remove(); },
  };
}

/** A pull request body's Markdown as nodes: headings, lists, fenced code,
 *  `code`, **bold**; the ◆ of a decision as ChloeCtx's chip. */
export function markdown(text) {
  const out = [];
  const lines = String(text || "").split("\n");
  let list = null, para = [];
  const inline = (s) => {
    const parts = [];
    const re = /\*\*([^*]+)\*\*|`([^`]+)`|◆ `([^`]+)`/g;
    let at = 0, m;
    while ((m = re.exec(s))) {
      if (m.index > at) parts.push(s.slice(at, m.index));
      parts.push(m[3] != null ? ctxChip(m[3]) : m[1] != null ? h("b", {}, m[1]) : h("code", { class: "lv-code" }, m[2]));
      at = re.lastIndex;
    }
    if (at < s.length) parts.push(s.slice(at));
    return parts;
  };
  const flush = () => {
    if (para.length) { out.push(h("p", {}, inline(para.join(" ")))); para = []; }
    if (list) { out.push(list); list = null; }
  };
  for (let i = 0; i < lines.length; i++) {
    const l = lines[i];
    if (l.startsWith("```")) {
      flush();
      const code = [];
      for (i++; i < lines.length && !lines[i].startsWith("```"); i++) code.push(lines[i]);
      out.push(h("pre", { class: "sh-pre" }, code.join("\n")));
    } else if (/^#{1,3} /.test(l)) {
      flush();
      const n = l.match(/^#+/)[0].length;
      out.push(h(`h${n + 2}`, {}, inline(l.replace(/^#+ /, ""))));
    } else if (/^\s*[-*] /.test(l)) {
      if (para.length) { out.push(h("p", {}, inline(para.join(" ")))); para = []; }
      list = list || h("ul", {});
      list.append(h("li", {}, inline(l.replace(/^\s*[-*] /, ""))));
    } else if (!l.trim()) flush();
    else { if (list) { out.push(list); list = null; } para.push(l.trim()); }
  }
  flush();
  return out;
}

useStyle("ship", `
.sh-wrap{padding:18px 22px 32px;max-width:1880px;margin:0 auto}
.sh-head{display:block;padding-bottom:10px}
.sh-crumb{display:block;border:0;background:none;padding:0;margin-bottom:10px;color:var(--muted);font:inherit;font-size:12.5px;cursor:pointer}
.sh-crumb:hover{color:var(--text)}
.sh-title{display:flex;align-items:center;gap:12px}
.sh-title>.lv-av{border-radius:10px;font-size:15px}
.sh-tt{min-width:0;flex:1}
.sh-tt h1{margin:0;font-size:19px;font-weight:680;letter-spacing:-.01em;line-height:1.25}
.sh-tt h1 span{font-weight:600;color:var(--text-2)}
.sh-sub{display:flex;flex-wrap:wrap;color:var(--muted);margin-top:3px;font-size:13px}
.sh-sub>span+span::before{content:"·";margin:0 7px}
.sh-sub code{font-family:var(--mono);font-size:12px;color:var(--text-2)}
.sh .n-add{color:var(--add-ink);font-weight:600}.sh .n-del{color:var(--del-ink);font-weight:600}
.sh-hr{display:flex;align-items:center;gap:10px;flex:none}
.sh-pick{height:28px;border:1px solid var(--line);border-radius:8px;background:var(--surface);color:var(--text-2);font:inherit;font-size:12.5px;padding:0 6px}
.sh-tabs,.sh-seg{display:flex;align-items:center;gap:2px;background:var(--sunken);border:1px solid var(--line);border-radius:999px;padding:3px}
.sh-tabs button,.sh-seg button{border:0;background:none;color:var(--muted);font:inherit;font-size:12px;font-weight:600;padding:3px 12px;border-radius:999px;cursor:pointer;white-space:nowrap}
.sh-tabs button:hover,.sh-seg button:hover{color:var(--text)}
.sh-tabs button.on,.sh-seg button.on{background:var(--accent-soft);color:var(--accent);box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--accent) 35%,transparent)}
.sh-spin{width:12px;height:12px;border-radius:50%;border:2px solid var(--line-2);border-top-color:var(--accent);animation:sh-spin .8s linear infinite;display:inline-block}
@keyframes sh-spin{to{transform:rotate(360deg)}}
.sh-risk{display:flex;align-items:flex-start;gap:10px;border:1px solid var(--line);border-left-width:4px;border-radius:10px;padding:10px 14px;margin:4px 0 16px;background:var(--surface)}
.sh-risk.warn{border-color:var(--warn-line);background:var(--warn-soft)}.sh-risk.warn b{color:var(--warn-ink)}
.sh-risk.crit{border-color:var(--crit);background:var(--crit-soft)}.sh-risk.crit b{color:var(--crit-ink)}
.sh-risk.good{border-color:var(--good);background:var(--good-soft)}.sh-risk.good b{color:var(--good-ink)}
.sh-risk.mut{color:var(--muted)}
.sh-dot{width:9px;height:9px;border-radius:50%;background:currentColor;margin-top:6px;flex:none}
.sh-risk.warn .sh-dot{color:var(--warn)}.sh-risk.crit .sh-dot{color:var(--crit)}.sh-risk.good .sh-dot{color:var(--good)}
.sh-risk-t{flex:1;min-width:0;font-size:13.5px}
.sh-reasons{margin:6px 0 2px;padding-left:18px;color:var(--text-2)}
.sh-top{display:grid;grid-template-columns:minmax(0,1fr) 340px;gap:16px;align-items:start}
.sh-graph{padding-bottom:6px}
.sh-legend{display:flex;gap:14px;color:var(--muted);font-size:12px}
.sh-legend span{display:inline-flex;align-items:center;gap:6px}
.sh-legend i{display:inline-block;width:10px;height:10px}
.lg-fn{border-radius:5px;background:var(--accent-soft);box-shadow:inset 0 0 0 1.5px var(--accent)}
.lg-test{border-radius:50%;background:var(--good)}
.lg-dec{background:var(--ctx);transform:rotate(45deg) scale(.85);border-radius:2px}
.lg-ag{border-radius:3px;background:var(--lane-2)}
.sh-svgbox{display:flex;justify-content:center}
.sh-svg{width:100%;max-height:600px;height:auto;display:block;font-family:var(--sans)}
.sh-svg .ring{fill:none;stroke:var(--line);stroke-dasharray:3 5}
.sh-svg .spoke{stroke:var(--line-2);stroke-width:1.5}
.sh-svg .edge{fill:none;stroke-width:1.6;opacity:.55;transition:opacity .15s}
.sh-svg .e-test.pass{stroke:var(--good)}.sh-svg .e-test.flaky{stroke:var(--warn)}.sh-svg .e-test.fail{stroke:var(--crit)}.sh-svg .e-test.none{stroke:var(--muted);stroke-dasharray:4 4}
.sh-svg .e-dec{stroke:var(--ctx)}
.sh-svg .e-ag{stroke-dasharray:6 4;stroke-width:2}
.sh-svg .center circle{fill:var(--surface-2);stroke:var(--text-2);stroke-width:1.5}
.sh-svg .center .c1{font-size:11px;font-weight:700;fill:var(--text);font-family:var(--mono)}
.sh-svg .center .c2{font-size:11px;fill:var(--muted);font-family:var(--mono)}
.sh-svg .node{cursor:pointer;outline:none;transition:opacity .15s}
.sh-svg .node text{font-size:11.5px;fill:var(--text-2);paint-order:stroke;stroke:var(--surface);stroke-width:4px;stroke-linejoin:round}
.sh-svg .fn rect{fill:var(--accent-soft);stroke:var(--accent);stroke-width:1.5}
.sh-svg .fn text{font-family:var(--mono);font-size:11.5px;font-weight:650;fill:var(--accent);stroke:none}
.sh-svg .fn.added rect{fill:var(--good-soft);stroke:var(--good)}.sh-svg .fn.added text{fill:var(--good-ink)}
.sh-svg .fn.deleted rect{fill:var(--crit-soft);stroke:var(--crit)}.sh-svg .fn.deleted text{fill:var(--crit-ink)}
.sh-svg .fn.untested rect{stroke-dasharray:4 3;stroke:var(--crit)}
.sh-svg .test circle{fill:var(--good);stroke:var(--surface);stroke-width:2}
.sh-svg .test.flaky circle{fill:var(--warn)}.sh-svg .test.fail circle{fill:var(--crit)}.sh-svg .test.none circle{fill:var(--surface);stroke:var(--muted);stroke-width:1.5;stroke-dasharray:3 2}
.sh-svg .test text{font-family:var(--mono);font-size:11px}
.sh-svg .dec rect{fill:var(--ctx);stroke:var(--surface);stroke-width:2}
.sh-svg .dec text{fill:var(--ctx);font-weight:600}
.sh-svg .dec.old rect{fill:var(--surface);stroke:var(--ctx);stroke-width:1.5}
.sh-svg .dec.old text{fill:var(--muted);font-weight:500}
.sh-svg .ag rect{stroke:var(--surface);stroke-width:2}
.sh-svg .ag .ini{font-size:11px;font-weight:800;fill:#0b0d10;stroke:none}
.sh-svg .ag text:not(.ini){font-weight:650;fill:var(--text)}
.sh-svg.hot .node:not(.on){opacity:.22}.sh-svg.hot .edge:not(.on),.sh-svg.hot .spoke:not(.on){opacity:.08}.sh-svg.hot .edge.on{opacity:1;stroke-width:2.4}
.sh-svg .node:focus-visible rect,.sh-svg .node:focus-visible circle{stroke:var(--accent);stroke-width:3}
.sh-side{display:flex;flex-direction:column;gap:12px}
.sh-side .lv-card-head{margin-bottom:0}
.sh-tiles{display:grid;grid-template-columns:repeat(3,1fr);gap:8px}
.sh-tile{background:var(--surface-2);border:1px solid var(--line);border-radius:9px;padding:8px 10px}
.sh-tile b{display:block;font-size:20px;font-weight:700;line-height:1.1}
.sh-tile span{font-size:11.5px;color:var(--muted)}
.sh-tile.warn b{color:var(--warn-ink)}.sh-tile.ctx b{color:var(--ctx)}
.sh-note{border-radius:8px;padding:8px 10px;font-size:12.5px}
.sh-note.warn{background:var(--warn-soft);color:var(--warn-ink)}.sh-note.good{background:var(--good-soft);color:var(--good-ink)}.sh-note.crit{background:var(--crit-soft);color:var(--crit-ink)}
.sh-rel{font-size:12.5px;line-height:1.7}
.sh-src{display:flex;flex-wrap:wrap;gap:6px}
.sh-src span{font-size:11px;border-radius:999px;padding:1px 8px;border:1px solid var(--line);color:var(--muted)}
.sh-src span.ok::before{content:"● ";color:var(--good)}.sh-src span.no::before{content:"○ ";color:var(--muted)}
.sh-missing{margin:0;padding-left:16px;color:var(--muted);font-size:12px}
.sh-cta{align-self:flex-start}
.sh-lists{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:16px;margin-top:16px;align-items:start}
.sh-list .lv-card-head{margin-bottom:4px}
.sh-list .n{color:var(--muted);font-weight:500;margin-left:2px}
.sh .muted{color:var(--muted)}.sh .sm{font-size:12px}
.sh-row-i{display:block;width:100%;text-align:left;border:0;border-top:1px solid var(--line);background:none;color:inherit;font:inherit;padding:9px 4px;cursor:pointer;border-radius:0;outline:none}
.sh-row-i:first-of-type{border-top:0}
.sh-row-i:hover,.sh-row-i:focus-visible{background:var(--surface-2)}
.sh-r1{display:flex;align-items:center;gap:7px;min-width:0}
.sh-r2{display:flex;flex-wrap:wrap;align-items:center;gap:4px 10px;color:var(--muted);font-size:12px;margin-top:3px}
.sh-sp{flex:1}
.sh-fn{font-family:var(--mono);font-size:12.5px;font-weight:650;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.sh-path{font-family:var(--mono);font-size:11.5px}
.sh-st{font-size:10.5px;font-weight:700;border-radius:5px;padding:0 6px;text-transform:uppercase;letter-spacing:.04em;background:var(--accent-soft);color:var(--accent)}
.sh-st.added{background:var(--good-soft);color:var(--good-ink)}.sh-st.deleted{background:var(--crit-soft);color:var(--crit-ink)}
.good-t{color:var(--good-ink)}.crit-t{color:var(--crit-ink);font-weight:600}.warn-t{color:var(--warn-ink)}
.sh-rank{width:18px;height:18px;border-radius:5px;background:var(--surface-3);color:var(--text-2);font-size:11px;font-weight:700;display:inline-grid;place-items:center;flex:none}
.sh-foot{border-top:1px dashed var(--line);margin-top:6px;padding-top:8px;color:var(--muted);font-size:11.5px}
.sh-dtext{margin-top:4px;font-size:13px;color:var(--text)}
.sh-dec.old .sh-dtext{text-decoration:line-through;color:var(--muted)}
.sh-via{font-size:10.5px;border:1px solid color-mix(in srgb,var(--ctx) 40%,transparent);color:var(--ctx);border-radius:999px;padding:0 6px}
.sh-pr{display:grid;grid-template-columns:minmax(0,1fr) 400px;gap:16px;align-items:start}
.sh-pside{display:flex;flex-direction:column;gap:16px;min-width:0}
.sh-bh code{font-family:var(--mono);font-size:12px;background:var(--surface-3);border-radius:5px;padding:1px 6px}
.sh-bh{color:var(--muted);font-size:12px}
.sh-title-in{width:100%;box-sizing:border-box;font:inherit;font-size:17px;font-weight:650;border:1px solid var(--line);border-radius:9px;padding:8px 11px;background:var(--surface-2);color:var(--text)}
.sh-title-in:focus{outline:2px solid var(--focus);border-color:var(--accent)}
.sh-bodybar{display:flex;align-items:center;gap:10px;margin:12px 0 8px}
.sh-md{border:1px solid var(--line);border-radius:10px;padding:4px 18px 10px;background:var(--surface);font-size:13.5px;line-height:1.55}
.sh-md h3{font-size:15px;margin:16px 0 6px;padding-bottom:5px;border-bottom:1px solid var(--line)}
.sh-md h4{font-size:13.5px;margin:12px 0 4px}
.sh-md p{margin:6px 0}.sh-md ul{margin:6px 0;padding-left:20px}.sh-md li{margin:3px 0}
.sh-md .lv-ctx{vertical-align:-1px;margin-right:2px}
.sh-pre,.sh-raw{font-family:var(--mono);font-size:12px;background:var(--sunken);border:1px solid var(--line);border-radius:8px;padding:9px 11px;white-space:pre-wrap;word-break:break-all;margin:6px 0}
.sh-raw{max-height:520px;overflow:auto;margin:0}
.sh-actions{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-top:12px}
.sh-gh{margin-top:12px;display:flex;flex-direction:column;gap:6px}
.sh-cmd{display:block;font-family:var(--mono);font-size:11.5px;background:var(--sunken);border:1px solid var(--line);border-radius:7px;padding:7px 10px;white-space:pre-wrap;word-break:break-all;color:var(--text-2)}
.sh-cap-t{font-size:14px}
.sh-cap .muted.sm{margin-top:2px}
.sh-kinds{display:grid;gap:4px;margin:10px 0}
.sh-kind{display:grid;grid-template-columns:76px 1fr 30px;align-items:center;gap:8px;font-size:12px;color:var(--text-2)}
.sh-kind i{height:6px;border-radius:999px;background:var(--line-2)}
.sh-kind.key i{background:var(--ctx)}.sh-kind.key span{color:var(--ctx);font-weight:600}
.sh-kind b{text-align:right;font-weight:600;font-size:11.5px;color:var(--muted)}
.sh-mount{display:flex;flex-direction:column;gap:6px;margin-bottom:10px}
.sh-row{display:flex;align-items:center;gap:10px;flex-wrap:wrap;min-width:0}
.sh-row .muted.sm{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0;flex:1;font-family:var(--mono);font-size:11px}
.sh-cap-none p{margin:4px 0 10px;color:var(--text-2)}
.sh-askf{display:flex;gap:8px}
.sh-askf input{flex:1;min-width:0;height:32px;border:1px solid var(--line);border-radius:8px;background:var(--surface-2);color:var(--text);font:inherit;padding:0 10px}
.sh-askf input:focus{outline:2px solid var(--focus);border-color:var(--accent)}
.sh-askf .btn{height:32px}
.sh-sugg{display:flex;flex-wrap:wrap;gap:6px;margin-top:10px}
.sh-chip{border:1px solid var(--line);background:var(--surface-2);color:var(--text-2);border-radius:999px;font:inherit;font-size:12px;padding:3px 10px;cursor:pointer;text-align:left}
.sh-chip:hover{color:var(--text);border-color:var(--line-2)}
.sh-ans{margin-top:10px;border-top:1px dashed var(--line);padding-top:8px}
.sh-ans-t{margin-top:8px;font-size:12.5px;line-height:1.55;color:var(--text-2)}
.sh-ans-t .gap{height:8px}
.sh-ans-t .lv-ctx{margin-right:2px}
.sh-trun{display:flex;align-items:center;gap:8px;padding:6px 0;border-top:1px solid var(--line);min-width:0}
.sh-trun:first-of-type{border-top:0}
.sh-trun code{font-family:var(--mono);font-size:12px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.sh-trun .muted{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:55%}
.sh-ok{width:18px;height:18px;border-radius:50%;display:inline-grid;place-items:center;font-size:11px;font-weight:800;flex:none}
.sh-ok.y{background:var(--good-soft);color:var(--good-ink)}.sh-ok.n{background:var(--crit-soft);color:var(--crit-ink)}.sh-ok.u{background:var(--surface-3);color:var(--muted)}
.sh-stack{display:flex;flex-wrap:wrap;gap:4px 14px;margin-top:5px;font-size:12.5px;color:var(--text-2)}
.sh-stack-i{display:inline-flex;align-items:center;gap:3px}
.sh-stack code,.sh-stackcard code{font-family:var(--mono);font-size:11.5px}
.sh-from{margin-top:4px}
.sh-stackcard{border:1px solid var(--line);border-radius:10px;padding:9px 12px;margin-top:10px;background:var(--surface-2);font-size:13px}
.sh-me{font-weight:700;color:var(--accent)}
.sh-together{margin-top:10px;display:flex;flex-direction:column;gap:8px}
.sh-parts{display:flex;flex-direction:column;gap:4px}
.sh-part{display:flex;align-items:center;gap:7px}
.sh-merged{background:var(--surface-2)}
.sh-picklist{display:flex;flex-direction:column;gap:2px}
.sh-pickrow{display:flex;align-items:center;gap:9px;border:0;background:none;color:inherit;font:inherit;padding:7px 6px;border-radius:7px;cursor:pointer;text-align:left}
.sh-pickrow:hover{background:var(--surface-2)}
.sh-skel{height:120px;border-radius:12px;background:var(--surface-2);margin-bottom:14px;animation:sh-pulse 1.4s ease-in-out infinite}
.sh-skel.tall{height:420px}
@keyframes sh-pulse{50%{opacity:.5}}
.sh-err{max-width:640px;display:flex;flex-direction:column;gap:10px}
.sh-empty{color:var(--muted);padding:40px 20px;text-align:center}
.sh-loading{font-size:13px}
@media (max-width:1380px){.sh-lists{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media (max-width:1100px){.sh-top,.sh-pr{grid-template-columns:minmax(0,1fr)}}
@media (max-width:640px){.sh-wrap{padding:14px}.sh-lists{grid-template-columns:minmax(0,1fr)}.sh-title{flex-wrap:wrap}.sh-hr{width:100%}.sh-tiles{grid-template-columns:repeat(2,1fr)}.sh-legend{display:none}}
`);
