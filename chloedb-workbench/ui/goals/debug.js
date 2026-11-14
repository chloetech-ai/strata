// Debug: find out why (JOURNEYS.md, section D). Four steps on an agent's turn:
//   1 Session and turn        #/debug, #/debug/<agent>
//   2 What it knew            #/debug/<agent>/<turn>
//   3 Where it went wrong     #/debug/<agent>/<turn>/wrong
//   4 Fix                     #/debug/<agent>/<turn>/fix (the same page, at its fixes)
// and the other ways in: #/debug/code[?q=] (Trace, the code on main),
// #/debug/failing (Health with Hotspots, failing tests first) and
// #/debug/records (what agents remember: correct, forget, pin, make a rule).
// mount(root, ctx) -> { update(state), unmount() }. Built on the shared
// pieces of ../levels/common.js; every colour is a token. The backend is
// src/turns.rs (turns, a turn's context and blast, flags, records, the
// threshold).

import {
  h, useStyle, icon, label, btn, page, pane, bank, since, fmtTokens, fmtClock, turnStrip, meter, repoOf,
} from "../levels/common.js";
import { hunkView } from "./review.js";

const enc = encodeURIComponent;
const plural = (n, w, ws = w + "s") => `${n} ${n === 1 ? w : ws}`;
const pct = (x) => `${Math.round((x || 0) * 100)}%`;
const PHONE = "(max-width: 640px)";
const none = (text) => h("p", { class: "db-none" }, text);
const KIND_WORDS = { decision: "decision", preference: "rule", fact: "fact", error: "fix", note: "note", summary: "summary", turn: "earlier turn", file: "file note" };
const kindWord = (k) => KIND_WORDS[k] || k || "note";

function parseRoute() {
  const raw = (location.hash || "").replace(/^#\/?/, "");
  const [path, query = ""] = raw.split("?");
  const parts = path.split("/").map((p) => decodeURIComponent(p));
  const q = new URLSearchParams(query);
  if (parts[0] !== "debug") return { view: "turns", q };
  const second = parts[1] || "";
  if (["code", "failing", "records"].includes(second)) return { view: second, q };
  const n = parts[2] && /^\d+$/.test(parts[2]) ? Number(parts[2]) : null;
  const tail = parts[3] || "";
  return { view: "turns", agent: second || null, turn: n, step: !second || !n ? 1 : tail === "fix" ? 4 : tail === "wrong" ? 3 : 2, q };
}

function projectOf(ctx) {
  const p = ctx.project || {};
  return p.path || p.name || (ctx.params && (ctx.params.project || ctx.params.repo)) || repoOf(ctx) || "";
}
function projectName(ctx) {
  const p = ctx.project || {};
  return String(p.name || (ctx.params && (ctx.params.project || ctx.params.repo)) || repoOf(ctx) || "").split("/").filter(Boolean).pop() || "this project";
}

/** A score as a bar with a tick at the threshold and the percent printed. */
function scoreBar(score, threshold, tone) {
  const s = Math.max(0, Math.min(1, score || 0));
  return h("span", { class: `db-score ${tone || ""}` },
    h("b", { class: "num" }, pct(s)),
    h("span", { class: "bar", "aria-hidden": "true" }, h("i", { style: { width: `${s * 100}%` } }), threshold != null ? h("u", { style: { left: `${threshold * 100}%` } }) : null));
}

export function mount(root, ctx) {
  const el = h("div", { class: "db" });
  root.append(el);
  let alive = true;
  let route = parseRoute();
  const cache = new Map(); // path -> Promise of JSON (or the error)
  let child = null; // trace.js or health.js, mounted for code / failing
  let childKey = null;
  let applied = null; // what Apply fixes did, with its undos
  let sent = null;

  const api = (path, opts) => ctx.api(path, opts);
  const get = (path) => {
    if (!cache.has(path)) cache.set(path, api(path).then((v) => ({ v }), (e) => ({ e })));
    return cache.get(path);
  };
  const drop = (prefix) => { for (const k of [...cache.keys()]) if (k.startsWith(prefix)) cache.delete(k); };
  const go = (hash) => (ctx.go ? ctx.go(hash) : (location.hash = hash));
  const proj = () => projectOf(ctx);
  const pname = () => projectName(ctx);
  const turnHref = (a, n, tail) => `#/debug/${enc(a)}/${n}${tail ? "/" + tail : ""}`;

  function crumbs(extra) {
    try { ctx.crumbs && ctx.crumbs([{ label: "Start", href: "#/" }, { label: "Debug", href: "#/debug" }, ...(extra || [])]); } catch { /* optional */ }
  }

  const onKey = (e) => {
    if (!alive) return;
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      const p = el.querySelector("[data-primary]:not([disabled])");
      if (p) { e.preventDefault(); p.click(); }
    }
  };
  document.addEventListener("keydown", onKey);
  const onHash = () => {
    if (!alive || !el.isConnected || !/^#\/debug/.test(location.hash)) return;
    const r = parseRoute();
    const same = JSON.stringify([r.view, r.agent, r.turn, r.step]) === JSON.stringify([route.view, route.agent, route.turn, route.step]);
    if (r.turn !== route.turn || r.agent !== route.agent) applied = null;
    route = r;
    if (!same || r.view === "records") render();
  };
  window.addEventListener("hashchange", onHash);

  /** The "Start from" row: the four ways in. */
  function ways() {
    const at = route.view;
    const w = [["turns", "An agent's turn", "#/debug"], ["code", "Why the code is like this", "#/debug/code"], ["failing", "What is failing", "#/debug/failing"], ["records", "What agents remember", "#/debug/records"]];
    return h("nav", { class: "db-ways", "aria-label": "Start from" }, h("span", { class: "lbl" }, "Start from"),
      h("div", { class: "db-ways-x" }, w.map(([k, l, href]) => h("a", { href, class: k === at ? "on" : null, "aria-current": k === at ? "page" : null }, l))));
  }

  const steps = (at, agent, n) => ({
    list: [
      { label: "Session and turn", href: agent ? `#/debug/${enc(agent)}` : "#/debug", state: at === 1 ? "now" : "done" },
      { label: "What it knew", href: agent && n ? turnHref(agent, n) : null, state: at === 2 ? "now" : at > 2 ? "done" : "next" },
      { label: "Where it went wrong", href: agent && n ? turnHref(agent, n, "wrong") : null, state: at === 3 ? "now" : at > 3 ? "done" : "next" },
      { label: "Fix", href: agent && n ? turnHref(agent, n, "fix") : null, state: at === 4 ? "now" : "next" },
    ],
  });

  async function render() {
    if (!alive) return;
    const r = route;
    if (r.view !== "code" && r.view !== "failing") unmountChild();
    let node;
    try {
      if (r.view === "code") node = codeView();
      else if (r.view === "failing") node = await failingView();
      else if (r.view === "records") node = await recordsView();
      else if (!r.agent || r.step === 1) node = await sessionView();
      else if (r.step === 2) node = await knewView();
      else node = await wrongView();
    } catch (e) {
      node = page({ kicker: "Find out why", title: "Debug" }, none(`This page could not be drawn: ${e.message}.`));
    }
    if (!alive || r !== route) return;
    if (node) el.replaceChildren(ways(), node);
    if (r.step === 4) { const f = el.querySelector("#db-fix"); if (f) f.scrollIntoView({ block: "start" }); }
  }

  function unmountChild() {
    if (child && child.unmount) child.unmount();
    child = null; childKey = null;
  }

  // ------------------------------------------------------------ step 1: session and turn
  async function sessionView() {
    crumbs();
    const name = pname();
    el.replaceChildren(ways(), page({ kicker: "Find out why", title: "Which turn went wrong?", steps: steps(1, route.agent) }, none(`Reading the sessions of ${name}…`)));
    const s = await get(`/api/debug/sessions?repo=${enc(proj())}`);
    if (s.e) {
      const why = s.e.status === 404 ? `Workbench knows no project named ${name}: start an agent in it first.` : s.e.status === 0 ? "The Workbench server did not answer, so the sessions are not known." : s.e.status === 400 ? "Pick a project first: Debug reads the sessions of one project." : `The sessions could not be read: ${s.e.message}.`;
      return page({ kicker: "Find out why", title: "Which turn went wrong?", steps: steps(1) }, none(why));
    }
    const sessions = s.v.sessions || [];
    if (!sessions.length) return page({ kicker: `Find out why · ${name}`, title: "Which turn went wrong?", steps: steps(1) }, none(`No agent has worked in ${name} yet, so there is no turn to look at. Start one, or ask why the code is like this.`));
    const id = route.agent || s.v.flagged || sessions[0].agent.id;
    const t = await get(`/api/agents/${enc(id)}/turns`);
    const sess = sessions.find((x) => x.agent.id === id) || { agent: { id, name: id } };
    const list = h("section", { class: "pane db-sess" }, h("div", { class: "hd" }, label("Sessions", h("span", { class: "num" }, String(sessions.length)))),
      sessions.slice(0, 30).map((x) => h("a", { class: `db-srow${x.agent.id === id ? " on" : ""}`, href: `#/debug/${enc(x.agent.id)}`, "aria-current": x.agent.id === id ? "true" : null },
        h("b", {}, x.agent.name), h("span", { class: "num" }, plural(x.turns, "turn")),
        x.broke && x.broke.length ? h("span", { class: "flag crit" }, icon("x", "i sm"), `broke ${x.broke[0]}`) : (x.flags || []).length ? h("span", { class: "flag" }, icon("warn", "i sm"), "you flagged it") : null)));
    if (t.e) return page({ kicker: `Find out why · ${name}`, title: "Which turn went wrong?", steps: steps(1, id) }, list, none(t.e.status === 0 ? "The Workbench server did not answer." : `${sess.agent.name}'s turns could not be read: ${t.e.message}.`));
    const d = t.v;
    const turns = d.turns || [];
    if (!turns.length) return page({ kicker: `Find out why · ${name}`, title: `${d.agent.name} has no turn yet`, steps: steps(1, id) }, list, none(`${d.agent.name} has not been given a prompt yet, so there is no turn to look at.`), ...(d.missing || []).map(none));
    const bad = d.bad;
    const pick = bad || (d.standout[0] && d.standout[0].n) || turns.length;
    const title = bad ? `${d.agent.name} went wrong at turn ${bad}` : `Which of ${d.agent.name}'s turns went wrong?`;
    const sub = bad ? (d.bad_from === "you" ? `You flagged turn ${bad}.` : `Tests failed after turn ${bad}: ${(d.breaks || []).map((b) => b.test).slice(0, 3).join(", ")}.`) : `No test broke after a turn and you flagged none. Of ${plural(turns.length, "turn")}, these stand out; say which went wrong.`;
    const notes = Object.fromEntries((d.standout || []).map((x) => [x.n, x.text]));
    const strip = turnStrip(turns.map((x) => x.kind), d.phases || [], (d.standout || []).map((x) => x.n), notes, { href: (n) => turnHref(id, n) });
    const primary = btn(`Open turn ${pick}`, { primary: true, kbd: "⌘↵", href: turnHref(id, pick) });
    primary.setAttribute("data-primary", "");
    const cards = (d.standout || []).map((x) => turnCard(id, turns[x.n - 1], x, d));
    const all = h("details", { class: "db-all" }, h("summary", {}, label(`All ${plural(turns.length, "turn")}`, icon("down", "i sm"))),
      h("div", { class: "pane" }, turns.slice().reverse().slice(0, 400).map((x) => turnRow(id, x))));
    const m = d.memory || {};
    const insp = [
      h("section", {}, label("What Workbench reminded it of", h("span", { class: "num" }, String(m.total || 0))),
        m.total ? h("p", { class: "db-p" }, Object.entries(m.counts || {}).filter(([, n]) => n > 0).map(([k, n]) => plural(n, kindWord(k))).join(", ")) : none(m.receipts === 0 ? "ChloeCtx kept no record of what this session was reminded of." : "Nothing: no note matched its prompts."),
        m.top ? h("div", { class: "db-rec" }, h("div", { class: "s" }, `Most often, ${plural(m.top.times, "time")}`), h("p", {}, m.top.text)) : null,
        m.missed ? h("div", { class: "db-rec miss" }, h("div", { class: "s" }, `It had this note but was not reminded of it${m.missed_turn ? ` at turn ${m.missed_turn}` : ""}`), h("p", {}, m.missed.text), h("div", { class: "s" }, m.missed.reason)) : null),
      d.context && d.context.tokens ? h("section", {}, label("Context now"), meter(d.context.pct, `${fmtTokens(d.context.tokens)} of ${fmtTokens(d.context.window)} tokens`)) : null,
      h("div", { class: "db-act" }, primary),
    ];
    return page({ kicker: `Find out why · ${name}`, title, sub, steps: steps(1, id), inspector: insp },
      list,
      h("section", { class: "gp-sec" }, label(`${d.agent.name}'s session`, h("span", { class: "num" }, plural(turns.length, "turn"))), h("div", { class: "pane bd db-strip" }, strip)),
      h("section", { class: "gp-sec" }, label("Turns that stand out", h("span", { class: "num" }, String(cards.length))), cards.length ? h("div", { class: "db-cards" }, cards) : none("None stands out: no failure, no flag, nothing it missed.")),
      all,
      ...(d.missing || []).map(none),
      h("div", { class: "db-bar" }, primary.cloneNode(true)));
  }

  function flagBtn(agent, t, small) {
    const on = !!t.flagged;
    return btn(on ? "Flagged · undo" : "This went wrong", { sm: small, ghost: on, icon: on ? "x" : "warn", title: on ? "Take your flag back" : `Say turn ${t.n} went wrong`, onclick: async (e) => {
      e.preventDefault(); e.stopPropagation();
      e.currentTarget.disabled = true;
      try { await api(`/api/agents/${enc(agent)}/turns/${t.n}/flag`, { method: "POST", body: { on: !on } }); } catch { /* the page says what it has */ }
      drop(`/api/agents/${enc(agent)}/turns`); drop("/api/debug/sessions");
      render();
    } });
  }

  function turnMeta(t) {
    return [t.tokens ? `${fmtTokens(t.tokens)} tokens` : null, t.injected ? `reminded of ${t.injected}` : null, t.missed ? `missed ${t.missed}` : null, t.edited && t.edited.length ? `edited ${t.edited.slice(0, 2).join(", ")}${t.edited.length > 2 ? ` +${t.edited.length - 2}` : ""}` : null, t.failed ? `${plural(t.failed, "call")} failed` : null].filter(Boolean).join(" · ");
  }

  function turnCard(agent, t, x, d) {
    if (!t) return null;
    const bad = t.n === d.bad;
    return h("article", { class: `pane db-card${bad ? " bad" : ""}` },
      h("div", { class: "db-card-h" }, h("span", { class: "num n" }, `Turn ${t.n}`), bad ? h("span", { class: "tag crit" }, icon("x", "i sm"), "Went wrong") : null, h("span", { class: "num s" }, fmtClock(t.started_ms))),
      h("b", { class: "db-card-t" }, t.title || "(no prompt)"),
      h("p", { class: "s" }, x.text),
      h("p", { class: "s m" }, turnMeta(t)),
      h("div", { class: "db-card-b" }, btn(`Open turn ${t.n}`, { sm: true, href: turnHref(agent, t.n) }), flagBtn(agent, t, true)));
  }

  function turnRow(agent, t) {
    return h("div", { class: `db-trow${t.bad ? " bad" : ""}` },
      h("a", { href: turnHref(agent, t.n) }, h("span", { class: "num n" }, String(t.n)), h("span", { class: "t" }, t.title || "(no prompt)"), h("span", { class: "s" }, turnMeta(t))),
      flagBtn(agent, t, true));
  }

  // ------------------------------------------------------------ step 2: what it knew
  async function knewView() {
    const { agent, turn: n } = route;
    el.replaceChildren(ways(), page({ kicker: "Find out why", title: `Turn ${n}`, steps: steps(2, agent, n) }, none(`Reading what it knew at turn ${n}…`)));
    const [t, c] = await Promise.all([get(`/api/agents/${enc(agent)}/turns`), get(`/api/agents/${enc(agent)}/turns/${n}/context`)]);
    if (c.e) return page({ kicker: "Find out why", title: `Turn ${n}`, steps: steps(2, agent, n) }, none(c.e.status === 404 ? `${c.e.message}.` : c.e.status === 0 ? "The Workbench server did not answer." : `What it knew could not be read: ${c.e.message}.`));
    const d = c.v;
    const name = d.agent.name;
    crumbs([{ label: name, href: `#/debug/${enc(agent)}` }, { label: `Turn ${n}`, href: turnHref(agent, n) }]);
    const mem = d.memory || {};
    const missed = mem.missed || [];
    const th = mem.threshold ? mem.threshold.value : null;
    const turns = t.v ? t.v.turns || [] : [];
    const tt = turns[n - 1] || { n, flagged: false };
    const bad = t.v && t.v.bad === n;
    const title = bad ? `${name} went wrong at turn ${n}` : `What ${name} knew at turn ${n}`;
    const sub = missed.length
      ? `It had the note “${missed[0].text}” but was not reminded of it: it looked only slightly related (${pct(missed[0].score)} match; notes under ${pct(th)} are left out).`
      : mem.injected && mem.injected.length ? `Workbench reminded it of ${plural(mem.injected.length, "note")}; no note it was not reminded of is about this turn's change.` : null;

    // The turns around it, on a clock.
    const near = turns.filter((x) => x.n >= n - 1 && x.n <= n + 2);
    const timeline = near.length ? h("section", { class: "pane db-tl" },
      h("div", { class: "db-tl-x" }, near.map((x) => h("a", { class: `db-tc${x.n === n ? " on" : ""}${x.bad ? " bad" : ""}`, href: turnHref(agent, x.n), "aria-current": x.n === n ? "step" : null },
        h("span", { class: "num s" }, fmtClock(x.started_ms)),
        h("b", {}, `${x.n} · ${x.title || "(no prompt)"}`),
        h("span", { class: "num s" }, [x.tokens ? `${fmtTokens(x.tokens)} tokens` : null, x.injected ? `${x.injected} reminded` : null, x.missed ? `${x.missed} missed` : null, x.bad ? "went wrong" : null].filter(Boolean).join(" · ") || x.kind))))) : null;

    const ctxT = d.context || {};
    const files = d.files || [];
    const knew = bank(3,
      h("div", { class: "db-tile" }, label("Context", ctxT.pct != null ? h("span", { class: "num" }, `${ctxT.pct}%`) : null),
        h("blockquote", { class: "db-q" }, `“${(d.prompt && d.prompt.text) || ""}”`, d.prompt && d.prompt.by && d.prompt.by !== "you" ? h("span", { class: "s" }, ` · from ${d.prompt.by}`) : null),
        ctxT.tokens ? meter(ctxT.pct, `${fmtTokens(ctxT.tokens)} of ${fmtTokens(ctxT.window)} tokens`) : none("How full its context was is not known: the session's transcript had no usage."),
        ctxT.memory_tokens ? h("p", { class: "s" }, `${fmtTokens(ctxT.memory_tokens)} tokens of it were Workbench's reminders.`) : null,
        files.length ? h("ul", { class: "db-files" }, files.slice(0, 8).map((f) => h("li", {}, h("code", {}, f.path), h("span", {}, f.how)))) : h("p", { class: "s" }, "It read and edited no file in this turn.")),
      h("div", { class: "db-tile" }, label(`What Workbench reminded it of (${(mem.injected || []).length})`),
        (mem.injected || []).length ? mem.injected.slice(0, 5).map((r) => h("div", { class: "db-rec" }, h("div", { class: "db-rh" }, scoreBar(r.score, null), h("span", { class: "k" }, kindWord(r.kind))), h("p", {}, r.text))) : none("Nothing: no note matched this prompt well enough."),
        (mem.injected || []).length > 5 ? h("p", { class: "s" }, `and ${mem.injected.length - 5} more`) : null),
      h("div", { class: "db-tile" }, label(`What it missed (${missed.length})`),
        missed.length ? missed.slice(0, 3).map((r) => h("div", { class: "db-rec miss" }, h("div", { class: "db-rh" }, scoreBar(r.score, th, "miss"), h("span", { class: "k" }, kindWord(r.kind))), h("p", {}, h("b", {}, r.text)),
          h("p", { class: "why" }, icon("warn", "i sm"), `It had this note but didn't use it, because it looked only slightly related: ${r.reason}.`))) : none("Nothing about this turn's change was left out."),
        (mem.left_out || []).length ? [h("div", { class: "lbl db-sub" }, "Rightly left out"), mem.left_out.map((r) => h("div", { class: "db-rec out" }, h("div", { class: "db-rh" }, scoreBar(r.score, th), h("span", { class: "k" }, kindWord(r.kind))), h("p", {}, r.text), h("p", { class: "s" }, `${r.reason}; it is not about this turn's change.`)))] : null));

    const primary = btn("Where it went wrong", { primary: true, kbd: "⌘↵", href: turnHref(agent, n, "wrong") });
    primary.setAttribute("data-primary", "");
    // The inspector: the turn's diff.
    const b = await get(`/api/agents/${enc(agent)}/turns/${n}/blast`);
    const diffNode = b.v && b.v.diff && (b.v.diff.files || []).length ? diffView(b.v, 3, 14) : none(b.e ? `The diff of turn ${n} could not be read: ${b.e.message}.` : `Turn ${n} changed no file.`);
    const insp = [
      h("section", {}, label(`The diff of turn ${n}`, b.v && b.v.diff && b.v.diff.commits && b.v.diff.commits.length ? h("code", { class: "rv-where" }, b.v.diff.commits[0]) : null), diffNode),
      h("div", { class: "db-act" }, primary, flagBtn(agent, tt, false)),
    ];
    return page({ kicker: `Find out why · ${name} · turn ${n} of ${d.turns}`, title, sub, steps: steps(2, agent, n), inspector: insp },
      timeline, h("section", { class: "gp-sec" }, knew), ...(d.missing || []).map(none), h("div", { class: "db-bar" }, primary.cloneNode(true)));
  }

  function diffView(b, maxFiles, maxLines) {
    return h("div", { class: "db-diff" }, (b.diff.files || []).slice(0, maxFiles).map((f) => h("div", {},
      h("div", { class: "lbl" }, h("code", { class: "rv-where" }, f.path)),
      (f.hunks || []).slice(0, maxLines ? 2 : 50).map((hk) => hunkView(hk.lines, { max: maxLines || 0 })))),
    (b.diff.files || []).some((f) => (f.hunks || []).some((hk) => (hk.lines || []).some((l) => l.cur))) ? h("p", { class: "s" }, "Marked: the first changed line of each function it changed.") : null);
  }

  // ------------------------------------------------------------ steps 3 and 4: where it went wrong, and the fix
  async function wrongView() {
    const { agent, turn: n, step } = route;
    el.replaceChildren(ways(), page({ kicker: "Find out why", title: `Turn ${n}`, steps: steps(step, agent, n) }, none(`Tracing what turn ${n} changed…`)));
    const [b, c, t] = await Promise.all([get(`/api/agents/${enc(agent)}/turns/${n}/blast`), get(`/api/agents/${enc(agent)}/turns/${n}/context`), get(`/api/agents/${enc(agent)}/turns`)]);
    if (b.e) return page({ kicker: "Find out why", title: `Turn ${n}`, steps: steps(step, agent, n) }, none(b.e.status === 404 ? `${b.e.message}.` : b.e.status === 0 ? "The Workbench server did not answer." : `What turn ${n} changed could not be traced: ${b.e.message}.`));
    const d = b.v;
    const name = d.agent.name;
    crumbs([{ label: name, href: `#/debug/${enc(agent)}` }, { label: `Turn ${n}`, href: turnHref(agent, n) }, { label: "Where it went wrong", href: turnHref(agent, n, "wrong") }]);
    const rows = d.rows || [];
    const ranked = h("section", { class: "gp-sec" }, label(`Blast radius of turn ${n}`, h("span", {}, "Worst first")),
      h("div", { class: `pane verdict ${d.tone || "warn"}` }, h("span", { class: "g" }), h("p", {}, d.verdict)),
      h("ol", { class: "pane db-rank" }, rows.map((r) => {
        const mark = r.tone === "crit" ? ["x", "bad"] : r.tone === "warn" ? ["warn", "watch"] : r.tone === "ok" ? ["check", "fine"] : [null, ""];
        return h("li", { class: `t-${r.tone || "none"}` },
          h("span", { class: "m", title: mark[1] || null }, mark[0] ? icon(mark[0], "i sm") : null),
          h("b", { class: "num" }, String(r.n)),
          h("span", { class: "w" }, r.what),
          h("span", { class: "nm" }, (r.names || []).slice(0, 4).map((x, i) => [i ? ", " : "", r.code ? h("code", {}, x) : x]), r.more ? ` +${r.more}` : (r.names || []).length > 4 ? ` +${r.names.length - 4}` : ""));
      })));
    const tt = t.v ? (t.v.turns || [])[n - 1] : null;
    const replay = tt && ctx.openRewind ? btn(`Replay ${name}'s terminal at turn ${n} (read only)`, { icon: "rewind", onclick: () => ctx.openRewind({ agent, from_ms: tt.started_ms, to_ms: Math.max(tt.ended_ms || 0, tt.started_ms + 1000), label: `Turn ${n}: ${tt.title}` }) }) : null;
    const diff = h("section", { class: "gp-sec" }, label(`The diff of turn ${n}`, h("span", {}, d.diff && d.diff.from === "commits" ? `from ${plural(d.diff.commits.length, "commit")}: ${d.diff.commits.join(", ")}` : "from its edits")),
      d.diff && (d.diff.files || []).length ? diffView(d, 12, 0) : none(`Turn ${n} changed no file.`), replay ? h("div", { class: "db-row" }, replay) : null);
    const fix = fixPanel(agent, n, d, c.v, t.v);
    return page({ kicker: `Find out why · ${name} · turn ${n}`, title: `Where turn ${n} went wrong`, sub: tt ? `“${tt.title}”` : null, steps: steps(step, agent, n), inspector: fix },
      ranked, diff, ...(d.missing || []).map(none));
  }

  /** Step 4: fixes, each separate from sending the agent back. One is recommended and ticked. */
  function fixPanel(agent, n, blast, cx, tv) {
    const name = blast.agent.name;
    const mem = (cx && cx.memory) || {};
    const missed = (mem.missed || [])[0];
    const th = mem.threshold ? mem.threshold.value : null;
    const lower = mem.lower;
    const files = blast.files || [];
    const pinFile = missed ? files.find((f) => missed.text.toLowerCase().includes(String(f).split("/").pop().replace(/\.[^.]+$/, "").toLowerCase())) || files[0] : null;
    const project = pname();
    const choices = [];
    if (missed && pinFile) choices.push({ key: "pin", rec: true, title: `Always remind agents editing ${pinFile}: “${missed.text}”`, sub: `A note tied to ${pinFile} is given to any agent of ${project} before it edits that file, however well it matches the prompt.` });
    if (missed && lower && th != null) choices.push({ key: "lower", title: "Remind agents of more of your decisions (looser match). Project setting.", sub: `From ${pct(th)} to ${pct(lower.to)}, for every agent of ${project}, from its next prompt. At this turn it would have given ${plural(lower.more, "more note")}${lower.tokens ? ` (+${fmtTokens(lower.tokens)} tokens)` : ""}. You can undo it.`, notes: lower.notes || [] });
    if (missed) choices.push({ key: "rule", rec: !pinFile, title: `Make it a rule: “${missed.text}”`, sub: `Every agent of ${project} is told it when its session starts.` });
    const ticked = new Set(choices.filter((x) => x.rec).map((x) => x.key));
    const ruleText = !missed ? h("textarea", { class: "field", rows: "3", placeholder: "The rule it lacked, in your words", "aria-label": "A rule for every agent of the project" }) : null;
    const status = h("p", { class: "db-why", role: "status" });
    const list = choices.map((x) => {
      const cb = h("input", { type: "checkbox", checked: ticked.has(x.key) || null, onchange: (e) => { if (e.target.checked) ticked.add(x.key); else ticked.delete(x.key); applyBtn.disabled = !ticked.size; applyBtn.replaceChildren(...applyKids); } });
      return h("label", { class: "db-fx" }, cb,
        h("span", {}, h("b", {}, x.title), x.rec ? h("span", { class: "tag rec" }, "Recommended") : null, h("span", { class: "s" }, x.sub),
          x.notes && x.notes.length ? h("details", { class: "db-else" }, h("summary", {}, `What else it would have given at turn ${n}`), h("ul", {}, x.notes.slice(0, 6).map((z) => h("li", {}, `${pct(z.score)} · ${z.text}`)))) : null));
    });
    const applyBtn = btn("Apply fixes", { primary: true, kbd: "⌘↵", disabled: missed ? !ticked.size : false, onclick: () => apply() });
    applyBtn.setAttribute("data-primary", "");
    const applyKids = [...applyBtn.childNodes];
    const done = h("div", { class: "db-done", role: "status" }, applied && applied.turn === n ? appliedView() : null);
    if (applied && applied.turn === n) { applyBtn.disabled = true; applyBtn.replaceChildren("Applied"); }

    async function apply() {
      applyBtn.disabled = true; status.textContent = "Applying…";
      const res = { turn: n, lines: [], undos: [] };
      const repo = proj();
      try {
        if (!missed) {
          const text = ruleText.value.trim();
          if (!text) { status.textContent = "Write the rule first."; applyBtn.disabled = false; return; }
          await api("/api/rules", { method: "POST", body: { repo, text } });
          res.lines.push(`Rule added: every agent of ${project} is told “${text}” at session start.`);
        } else {
          if (ticked.has("pin") || ticked.has("rule")) {
            const body = {};
            if (ticked.has("pin")) body.pin = pinFile;
            if (ticked.has("rule")) body.rule = true;
            const r = await api(`/api/records/${enc(missed.id)}?repo=${enc(repo)}`, { method: "PATCH", body });
            res.lines.push(...(r.done || []));
          }
          if (ticked.has("lower")) {
            const r = await api(`/api/repos/${enc(repo)}/threshold`, { method: "PUT", body: { value: lower.to } });
            res.lines.push(r.text);
            res.undos.push({ label: `Undo: back to ${pct(r.was)}`, run: async () => { const u = await api(`/api/repos/${enc(repo)}/threshold`, { method: "PUT", body: { value: r.undo.value } }); return `Back to ${pct(u.value)} for every agent of ${project}.`; } });
          }
        }
        applied = res;
        status.textContent = "";
        drop(`/api/agents/${enc(agent)}/turns/${n}/context`);
        done.replaceChildren(appliedView());
        applyBtn.replaceChildren("Applied");
      } catch (e) {
        status.textContent = `Not applied: ${e.message}`;
        applyBtn.disabled = false;
      }
    }

    // Sending the agent back is its own action.
    const broke = (blast.rows || []).find((r) => r.what === "tests failed after it");
    const note = [
      `Turn ${n} went wrong${broke && broke.n ? `: ${broke.names.join(", ")} failed after it` : ""}.`,
      missed ? `You had this decision but were not reminded of it: “${missed.text.replace(/[.\s]+$/, "")}”. Change ${files.slice(0, 3).join(", ") || "the code"} so it holds.` : null,
      broke && broke.n ? "Then run the tests again." : null,
    ].filter(Boolean).join(" ");
    const ta = h("textarea", { class: "field", rows: "6", "aria-label": `What ${name} should do` }, note);
    const sStatus = h("p", { class: "db-why", role: "status" }, sent && sent.turn === n ? sent.text : "");
    const sendBtn = btn(`Send ${name} back`, { icon: "send", onclick: async () => {
      const text = ta.value.trim();
      if (!text) { sStatus.textContent = `Say what ${name} should do first.`; return; }
      sendBtn.disabled = true; sStatus.textContent = "Sending…";
      try {
        try { await api(`/api/agents/${enc(agent)}/steer`, { method: "POST", body: { text } }); }
        catch (e) { if (e.status !== 409) throw e; await api(`/api/agents/${enc(agent)}/restart`, { method: "POST", body: { prompt: text } }); }
        sent = { turn: n, text: `Sent: ${name} has your note.` };
        sStatus.textContent = sent.text;
      } catch (e) { sendBtn.disabled = false; sStatus.textContent = `Not sent: ${e.message}`; }
    } });

    return [
      h("section", { id: "db-fix" }, label("Fix"),
        missed ? h("p", { class: "db-p s" }, "So that agents are reminded of it next time. Each fix stands on its own; none sends the agent anything.") : h("p", { class: "db-p s" }, `Workbench had no note about this turn's change that it left out, so there is nothing to tie to ${files[0] || "a file"}. Write the rule ${name} lacked, for every agent of ${project}:`),
        list, ruleText, h("div", { class: "db-act" }, applyBtn, status), done),
      h("section", {}, label(`Send ${name} back`), ta, h("div", { class: "db-row" }, sendBtn), sStatus),
    ];
  }

  function appliedView() {
    const r = applied;
    return h("div", { class: "db-applied" }, r.lines.map((l) => h("p", {}, icon("check", "i sm"), l)),
      r.undos.map((u) => btn(u.label, { sm: true, icon: "rewind", onclick: async (e) => {
        const b = e.currentTarget; b.disabled = true;
        try { const said = await u.run(); r.undos = r.undos.filter((x) => x !== u); r.lines.push(said); } catch (err) { r.lines.push(`Undo failed: ${err.message}`); }
        const host = el.querySelector(".db-done"); if (host) host.replaceChildren(appliedView());
      } })));
  }

  // ------------------------------------------------------------ why the code is like this
  function codeView() {
    crumbs([{ label: "Why the code is like this", href: "#/debug/code" }]);
    const host = h("div", { class: "db-child" });
    const q = route.q.get("q") || "";
    const key = `code:${proj()}`;
    const node = page({ kicker: `Find out why · ${pname()}`, title: "Why the code is like this", sub: "When, why and how something changed: git on every branch, the agents' edits, their decisions and your answers.", wide: true },
      h("p", { class: "db-branch" }, icon("branch", "i sm"), "How it works now: the code on ", h("code", {}, "main"), ", in the project's own checkout. Changes still on an agent's branch show in the timeline with their branch."),
      host);
    if (childKey !== key) {
      unmountChild();
      childKey = key;
      import("/levels/trace.js").then((m) => {
        if (!alive || childKey !== key) return;
        child = m.mount(host, { ...ctx, params: { ...(ctx.params || {}), agent: null, repo: proj(), q } });
      }).catch((e) => host.replaceChildren(none(`Trace could not be loaded: ${e.message}.`)));
    } else if (child) {
      const old = el.querySelector(".db-child");
      if (old) host.replaceChildren(...old.childNodes);
    }
    return node;
  }

  // ------------------------------------------------------------ what is failing
  async function failingView() {
    crumbs([{ label: "What is failing", href: "#/debug/failing" }]);
    const name = pname();
    const t = await get(`/api/tests?repo=${enc(proj())}`);
    let line;
    if (t.e) line = t.e.status === 0 ? "The Workbench server did not answer, so the test runs are not known." : `The test runs could not be read: ${t.e.message}.`;
    else {
      const tot = t.v.totals || {};
      const failing = (t.v.tests || []).filter((x) => x.last && /fail/i.test(x.last.outcome || ""));
      if (!tot.runs && !(tot.quiet && tot.quiet.commands)) line = `No test runs recorded yet in ${name}. When an agent runs the tests, each test shows here by name.`;
      else if (failing.length) line = `${plural(failing.length, "test")} ${failing.length === 1 ? "fails" : "fail"} now: ${failing.slice(0, 3).map((x) => x.test).join(", ")}${failing.length > 3 ? ` and ${failing.length - 3} more` : ""}${tot.flaky ? `; ${tot.flaky} flaky` : ""}.`;
      else line = `Every test passed on its last run (${plural(tot.tests || 0, "test")}, ${plural(tot.runs || 0, "run")})${tot.flaky ? `; ${tot.flaky} flaky` : ""}.`;
    }
    const host = h("div", { class: "db-child" });
    // ?incident=<id> (an alert's Open in Debug) opens that incident;
    // ?tab=incidents|rules|hotspots|tests one of Health's tabs.
    const inc = route.q && route.q.get("incident");
    const tab = route.q && route.q.get("tab");
    const first = inc && /^i[0-9a-f]{8}$/.test(inc) ? inc : ["incidents", "rules", "hotspots", "tests"].includes(tab) ? tab : "tests";
    const key = `failing:${proj()}:${first}`;
    const node = page({ kicker: `Find out why · ${name}`, title: "What is failing", sub: line, wide: true }, host);
    if (childKey !== key) {
      unmountChild();
      childKey = key;
      import("/levels/health.js").then((m) => {
        if (!alive || childKey !== key) return;
        child = m.mount(host, { ...ctx, params: { ...(ctx.params || {}), agent: first, repo: proj() } });
      }).catch((e) => host.replaceChildren(none(`Health could not be loaded: ${e.message}.`)));
    } else {
      const old = el.querySelector(".db-child");
      if (old) host.replaceChildren(...old.childNodes);
    }
    return node;
  }

  // ------------------------------------------------------------ what agents remember
  async function recordsView() {
    crumbs([{ label: "What agents remember", href: "#/debug/records" }]);
    const name = pname();
    const q = route.q;
    const f = { kind: q.get("kind") || "", by: q.get("by") || "", branch: q.get("branch") || "", superseded: q.get("superseded") || "", id: q.get("id") || "" };
    const qs = new URLSearchParams({ repo: proj() });
    for (const [k, v] of Object.entries(f)) if (v) qs.set(k, v);
    const r = await get(`/api/records?${qs}`);
    const head = { kicker: `Find out why · ${name}`, title: `What agents remember about ${name}` };
    if (r.e) return page(head, none(r.e.status === 404 ? `Workbench knows no project named ${name}.` : r.e.status === 0 ? "The Workbench server did not answer." : `The notes could not be read: ${r.e.message}.`));
    const d = r.v;
    const recs = d.records || [];
    const setF = (k, v) => {
      const p = new URLSearchParams();
      for (const [kk, vv] of Object.entries({ ...f, [k]: v, id: k === "id" ? v : "" })) if (vv) p.set(kk, vv);
      const s = p.toString();
      history.replaceState(null, "", `#/debug/records${s ? "?" + s : ""}`);
      route = parseRoute();
      render();
    };
    const facet = (k, title, words) => {
      const opts = Object.entries((d.facets || {})[k] || {}).sort((a, b) => b[1] - a[1]);
      return h("label", { class: "db-f" }, h("span", { class: "lbl" }, title),
        h("select", { class: "field", onchange: (e) => setF(k, e.target.value) }, h("option", { value: "" }, "Any"), opts.map(([v, n]) => h("option", { value: v, selected: v === f[k] || null }, `${words ? words(v) : v} (${n})`))));
    };
    const filters = h("div", { class: "db-filters" },
      facet("kind", "Kind", kindWord), facet("by", "Who said it"), facet("branch", "Branch"),
      h("label", { class: "db-chk" }, h("input", { type: "checkbox", checked: f.superseded ? true : null, onchange: (e) => setF("superseded", e.target.checked ? "1" : "") }), `Show replaced notes${d.superseded ? ` (${d.superseded})` : ""}`),
      f.id ? btn("Show all notes", { sm: true, ghost: true, onclick: () => setF("id", "") }) : null);
    const body = recs.length ? h("div", { class: "pane db-recs" }, recs.map((x) => recordRow(x))) : none((d.missing || []).length ? "" : d.total ? "No note matches these filters." : `Agents remember nothing about ${name} yet: notes appear as they decide things and you answer them.`);
    return page({ ...head, sub: d.total ? `${plural(d.total, "note")}, newest first, with who said each and on which branch. Correct what is wrong; forget what should not be kept.` : null, wide: true },
      d.total ? filters : null, body, ...(d.missing || []).map((m) => none(`The notes could not be read: ${m}`)));
  }

  function recordRow(x) {
    const box = h("div", { class: "db-rx" });
    const status = h("p", { class: "db-why", role: "status" });
    const repo = proj();
    const act = async (method, body, said) => {
      status.textContent = "Saving…";
      try {
        const r = await api(`/api/records/${enc(x.id)}?repo=${enc(repo)}`, { method, body });
        status.textContent = said || (r.done || []).join(" ");
        drop("/api/records");
        setTimeout(() => { if (alive) render(); }, 900);
      } catch (e) { status.textContent = `Not done: ${e.message}`; }
    };
    const edit = () => {
      const ta = h("textarea", { class: "field", rows: "3", "aria-label": "The note, corrected" }, x.text);
      box.replaceChildren(ta, h("div", { class: "db-row" }, btn("Save the correction", { primary: false, sm: true, onclick: () => act("PATCH", { text: ta.value }) }), btn("Cancel", { sm: true, ghost: true, onclick: () => box.replaceChildren() })));
      ta.focus();
    };
    const pin = () => {
      const inp = h("input", { class: "field", placeholder: "a file, such as cart.py", "aria-label": "File to tie the note to", value: (x.files || [])[0] || "" });
      box.replaceChildren(h("p", { class: "s" }, "Agents editing this file are reminded of the note, however well it matches their prompt."), inp,
        h("div", { class: "db-row" }, btn("Always remind", { sm: true, onclick: () => inp.value.trim() && act("PATCH", { pin: inp.value.trim() }) }), btn("Cancel", { sm: true, ghost: true, onclick: () => box.replaceChildren() })));
      inp.focus();
    };
    const forget = () => box.replaceChildren(h("p", {}, "Forget this note? No agent is reminded of it again, and it is taken out of the records of what agents were shown."),
      h("div", { class: "db-row" }, btn("Forget it", { sm: true, danger: true, onclick: () => act("DELETE", undefined, "Forgotten.") }), btn("Keep it", { sm: true, ghost: true, onclick: () => box.replaceChildren() })));
    const meta = [kindWord(x.kind), `said by ${x.by_label || "someone"}`, x.branch, x.ts ? `${since(x.ts)} ago` : null].filter(Boolean).join(" · ");
    return h("article", { class: `db-recrow${x.superseded ? " old" : ""}` },
      h("div", { class: "db-rt" }, h("p", {}, x.text),
        h("div", { class: "s" }, meta,
          x.superseded ? h("span", { class: "tag" }, "Replaced") : null,
          x.task_only ? h("span", { class: "tag warn" }, "For one task only") : null,
          x.files && x.files.length ? h("span", { class: "tag ctx" }, `tied to ${x.files.slice(0, 2).join(", ")}`) : null)),
      x.superseded ? null : h("div", { class: "db-ra" },
        btn("Correct", { sm: true, onclick: edit }), btn("Always remind…", { sm: true, onclick: pin }), btn("Make it a rule", { sm: true, onclick: () => act("PATCH", { rule: true }) }), btn("Forget", { sm: true, ghost: true, onclick: forget })),
      box, status);
  }

  render();
  return {
    update(state) { if (child && child.update) child.update(state); },
    unmount() {
      alive = false;
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("hashchange", onHash);
      unmountChild();
      el.remove();
    },
    destroy() { this.unmount(); },
  };
}

useStyle("goal-debug", `
.db{min-height:100%;overflow-wrap:anywhere}
.db .btn[disabled]{background:var(--surface-2);color:var(--text-2);border-color:var(--line);cursor:not-allowed}
.db-none{margin:var(--s-3) 0;color:var(--text-2);font-size:var(--fs-md);line-height:var(--lh-md);max-width:68ch}
.db-none:empty{display:none}
.db-ways{display:flex;align-items:center;gap:var(--s-3);padding:var(--s-2) var(--s-7);border-bottom:var(--hair) solid var(--line);background:var(--surface-2);min-width:0}
.db-ways-x{display:flex;gap:var(--s-1);overflow-x:auto;min-width:0}
.db-ways a{padding:var(--s-1) var(--s-3);border-radius:var(--r-sm);color:var(--text-2);text-decoration:none;white-space:nowrap;font-size:var(--fs-sm);font-weight:var(--w-mid);min-height:28px;display:inline-flex;align-items:center}
.db-ways a:hover{background:var(--line)}
.db-ways a.on{background:var(--surface);color:var(--text);box-shadow:inset 0 0 0 var(--hair) var(--line-2)}
.db .lbl>span:first-child{flex:none;white-space:nowrap}
.db .lbl .rv-where{text-align:right;min-width:0;overflow-wrap:anywhere}
.gp-insp .rv-hunk .hl{min-width:0}.gp-insp .rv-hunk code{white-space:pre-wrap;overflow-wrap:anywhere}
.db .s{color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.db-p{margin:var(--s-2) 0 0}
.db-sess{margin-bottom:var(--s-2)}
.db-srow{display:grid;grid-template-columns:minmax(0,1fr) auto auto;gap:var(--s-3);align-items:center;padding:var(--s-2) var(--s-4);min-height:40px;border-bottom:var(--hair) solid var(--line);color:var(--text);text-decoration:none}
.db-srow:last-child{border-bottom:0}
.db-srow:hover{background:var(--surface-2)}
.db-srow.on{background:var(--accent-soft);box-shadow:inset 3px 0 0 var(--accent)}
.db-srow .num{color:var(--text-2);font-size:var(--fs-sm)}
.flag{display:inline-flex;align-items:center;gap:4px;font-size:var(--fs-sm);color:var(--warn-ink)}
.flag.crit{color:var(--crit-ink)}
.db-strip{padding:var(--s-3) var(--s-4)}
.db-cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(260px,1fr));gap:var(--s-3)}
.db-card{padding:var(--s-3) var(--s-4);display:flex;flex-direction:column;gap:var(--s-1);min-width:0}
.db-card.bad{border-color:var(--crit);box-shadow:inset 3px 0 0 var(--crit)}
.db-card-h{display:flex;align-items:center;gap:var(--s-2)}
.db-card-h .n{font-weight:var(--w-mid)}
.db-card-h .s{margin-left:auto}
.db-card-t{font-weight:var(--w-title);line-height:var(--lh-body)}
.db-card p{margin:0}
.db-card .m{font-family:var(--mono);font-size:var(--fs-xs)}
.db-card-b{display:flex;gap:var(--s-2);flex-wrap:wrap;margin-top:var(--s-2)}
.tag{display:inline-flex;align-items:center;gap:3px;font:var(--w-mid) var(--fs-xs)/18px var(--mono);letter-spacing:.04em;text-transform:uppercase;padding:0 6px;border-radius:var(--r-xs);background:var(--line);color:var(--text-2);margin-left:var(--s-2)}
.tag.crit{background:var(--crit-soft);color:var(--crit-ink);margin-left:0}
.tag.warn{background:var(--warn-soft);color:var(--warn-ink)}
.tag.ctx{background:var(--accent-soft);color:var(--accent-text)}
.tag.rec{background:var(--accent-soft);color:var(--accent-text)}
.db-all{margin-top:var(--s-5)}
.db-all>summary{list-style:none;cursor:pointer;min-height:var(--touch);display:flex;align-items:center}
.db-all>summary::-webkit-details-marker{display:none}
.db-all>summary .lbl{flex:1;color:var(--accent-text)}
.db-all .pane{max-height:480px;overflow:auto}
.db-trow{display:flex;align-items:center;gap:var(--s-2);padding:var(--s-1) var(--s-4) var(--s-1) 0;border-bottom:var(--hair) solid var(--line)}
.db-trow>a{flex:1;display:grid;grid-template-columns:3em minmax(0,1fr);column-gap:var(--s-2);padding:var(--s-1) 0 var(--s-1) var(--s-4);color:var(--text);text-decoration:none;min-width:0}
.db-trow .n{color:var(--text-2);grid-row:span 2}
.db-trow .t{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.db-trow .s{font-family:var(--mono);font-size:var(--fs-xs)}
.db-trow.bad{box-shadow:inset 3px 0 0 var(--crit)}
.db-act{display:flex;flex-direction:column;gap:var(--s-2);margin-top:var(--s-3)}
.gp-insp>.db-act{margin-top:auto;padding-top:var(--s-4);border-top:var(--hair) solid var(--line)}
.db-act .btn{width:100%}
.db-act .btn.primary{height:var(--ctl-h-lg)}
.db-rec{padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line)}
.db-rec:last-child{border-bottom:0}
.db-rec p{margin:var(--s-1) 0 0;line-height:var(--lh-body)}
.db-rec.miss b{font-weight:var(--w-mid)}
.db-rec .why{display:flex;gap:var(--s-1);align-items:flex-start;color:var(--crit-ink);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.db-rec .why svg{margin-top:2px;flex:none}
.db-rec.out p{color:var(--text-2)}
.db-rh{display:flex;align-items:center;gap:var(--s-2)}
.db-rh .k{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2)}
.db-score{display:inline-flex;align-items:center;gap:var(--s-2)}
.db-score b{font-size:var(--fs-sm);min-width:3.2em}
.db-score .bar{position:relative;width:120px;height:4px;border-radius:2px;background:var(--line-2)}
.db-score .bar i{position:absolute;left:0;top:0;bottom:0;background:var(--text);border-radius:2px}
.db-score .bar u{position:absolute;top:-4px;bottom:-4px;width:1.5px;background:var(--text-2)}
.db-score.miss b{color:var(--crit-ink)}.db-score.miss .bar i{background:var(--crit)}
.db-sub{margin-top:var(--s-3)}
.db-tile{padding:var(--s-4);display:flex;flex-direction:column;gap:var(--s-2);min-width:0}
.db-q{margin:0;font-size:var(--fs-md);line-height:var(--lh-md);font-weight:var(--w-mid)}
.db-files{list-style:none;margin:var(--s-2) 0 0;padding:0}
.db-files li{display:flex;justify-content:space-between;gap:var(--s-2);padding:6px 0;border-bottom:var(--hair) solid var(--line);font-size:var(--fs-sm)}
.db-files li span{color:var(--text-2)}
.db-tl{padding:var(--s-3)}
.db-tl-x{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:var(--s-2)}
.db-tc{display:flex;flex-direction:column;gap:var(--s-1);padding:var(--s-3);border:var(--hair) solid var(--line);border-radius:var(--r-sm);color:var(--text);text-decoration:none;min-width:0}
.db-tc b{font-weight:var(--w-mid);line-height:var(--lh-body)}
.db-tc:hover{background:var(--surface-2)}
.db-tc.on{border-color:var(--accent);background:var(--accent-soft)}
.db-tc.bad .s:last-child{color:var(--crit-ink)}
.db .verdict{margin-top:var(--s-2)}
.db-rank{list-style:none;margin:var(--s-3) 0 0;padding:0}
.db-rank li{display:grid;grid-template-columns:20px 3em minmax(0,14em) minmax(0,1fr);gap:var(--s-3);align-items:baseline;padding:10px var(--s-4);border-bottom:var(--hair) solid var(--line)}
.db-rank li:last-child{border-bottom:0}
.db-rank b{font-size:var(--fs-lg);font-weight:var(--w-mid)}
.db-rank .m{align-self:center;display:inline-flex}
.db-rank .t-crit .m,.db-rank .t-crit b{color:var(--crit-ink)}
.db-rank .t-warn .m{color:var(--warn)}
.db-rank .t-ok .m{color:var(--good)}
.db-rank .nm{color:var(--text-2);font-size:var(--fs-sm)}
.db-diff>div{margin-top:var(--s-2)}
.db-row{display:flex;gap:var(--s-2);flex-wrap:wrap;margin-top:var(--s-3)}
.db-fx{display:grid;grid-template-columns:20px minmax(0,1fr);gap:var(--s-3);padding:var(--s-3) 0;border-bottom:var(--hair) solid var(--line);cursor:pointer;align-items:start}
.db-fx input{width:18px;height:18px;margin:2px 0 0;accent-color:var(--accent)}
.db-fx b{font-weight:var(--w-mid)}
.db-fx .s{display:block;margin-top:2px}
.db-else{margin-top:var(--s-1);font-size:var(--fs-sm)}
.db-else summary{color:var(--accent-text);cursor:pointer}
.db-else ul{margin:var(--s-1) 0 0;padding-left:var(--s-4);color:var(--text-2)}
.db-why{margin:0;color:var(--text-2);font-size:var(--fs-sm)}
.db-why:empty{display:none}
.db-applied p{display:flex;gap:var(--s-2);align-items:flex-start;margin:var(--s-2) 0 0}
.db-applied p svg{color:var(--good);margin-top:3px;flex:none}
.db-applied .btn{margin-top:var(--s-2)}
#db-fix textarea{margin-top:var(--s-2)}
.gp-insp textarea.field{min-height:84px}
.db-branch{display:flex;align-items:center;gap:var(--s-2);flex-wrap:wrap;margin:0 0 var(--s-4);padding:var(--s-2) var(--s-3);border-radius:var(--r-sm);background:var(--surface);border:var(--hair) solid var(--line);color:var(--text-2)}
.db-branch code{color:var(--text)}
.db-filters{display:flex;flex-wrap:wrap;gap:var(--s-3);align-items:flex-end;margin-bottom:var(--s-4)}
.db-f{display:flex;flex-direction:column;gap:var(--s-1);min-width:160px}
.db-f select.field{height:var(--ctl-h)}
.db-chk{display:inline-flex;gap:var(--s-2);align-items:center;min-height:var(--ctl-h)}
.db-recrow{padding:var(--s-3) var(--s-4);border-bottom:var(--hair) solid var(--line);display:grid;grid-template-columns:minmax(0,1fr) auto;gap:var(--s-2) var(--s-4)}
.db-recrow:last-child{border-bottom:0}
.db-recrow.old .db-rt p{color:var(--text-2);text-decoration:line-through}
.db-rt p{margin:0;line-height:var(--lh-body)}
.db-ra{display:flex;gap:var(--s-1);flex-wrap:wrap;justify-content:flex-end;align-items:flex-start}
.db-rx,.db-recrow>.db-why{grid-column:1/-1}
.db-rx:empty{display:none}
.db-bar{display:none}
.db-child{position:relative;min-width:0}
.db-child>.lv-screen{position:static;inset:auto;height:auto;overflow:visible;padding-left:0;padding-right:0}
@media (max-width:1024px){
  .db-ways{padding:var(--s-2) var(--s-5)}
  .db-tl-x{grid-template-columns:repeat(2,minmax(0,1fr))}
  .db-rank li{grid-template-columns:20px 3em minmax(0,1fr)}
  .db-rank .nm{grid-column:3}
}
@media (max-width:640px){
  .db .btn{white-space:normal;height:auto;min-height:var(--touch);text-align:center;line-height:var(--lh-body);padding-top:var(--s-1);padding-bottom:var(--s-1)}
  .db-ways{padding:var(--s-2) var(--s-4);gap:var(--s-2)}
  .db-ways>.lbl{display:none}
  .db .kc{display:none}
  .db-tl-x{display:flex;overflow-x:auto}
  .db-tc{flex:0 0 72%}
  .db-recrow{grid-template-columns:minmax(0,1fr)}
  .db-ra{justify-content:flex-start}
  .db-f{min-width:0;flex:1 1 100%}
  .db-f select.field{height:48px;font-size:16px}
  .db-score .bar{width:80px}
  .db .gp-insp>.db-act{display:none}
  .db-bar{display:flex;position:sticky;bottom:0;z-index:5;margin:var(--s-5) calc(-1 * var(--s-4)) 0;padding:var(--s-3) var(--s-4) max(var(--s-3),env(safe-area-inset-bottom));background:var(--glass);-webkit-backdrop-filter:var(--glass-blur);backdrop-filter:var(--glass-blur);border-top:var(--hair) solid var(--line)}
  .db-bar .btn{flex:1;height:48px}
  .db-trow .btn,.db-card .btn,.db-ra .btn{min-height:var(--touch)}
}
@media (pointer:coarse){.db-ways a,.db-srow,.db .btn,.db-fx{min-height:var(--touch)}}
`);
