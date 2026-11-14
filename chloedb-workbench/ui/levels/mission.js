// Mission (SPEC.md, "Levels", Intent for the fleet): the goal, its outcomes
// in four columns (Done, In motion, Needs you, Next), one live sentence per
// agent with what ChloeCtx gave it (◆ chips), and a side column with the
// questions waiting for you (answered in one click), collisions ahead and
// what the fleet learned today. Without a goal: a box to set one.
import {
  h, useStyle, toMs, fmtDur, fmtClock, avatar, ctxChip, pill, repoOf, mergeAgents, inlineCode, parseJSON,
  fmtTokens, barChart, keyNav, rerender, later, precedentOf,
} from "./common.js";

const COLS = [["done", "Done"], ["active", "In motion"], ["needs_you", "Needs you"], ["next", "Next"]];
const enc = encodeURIComponent;
const base = (p) => (p || "").split("/").filter(Boolean).pop() || p || "";
const names = (xs) => (xs.length < 2 ? xs.join("") : `${xs.slice(0, -1).join(", ")} and ${xs.at(-1)}`);
const plural = (n, one, many = one + "s") => `${n} ${n === 1 ? one : many}`;

// Time travel: the scrubber above the screen (ui/levels/timebar.js).
import { timeTravel } from "./timebar.js";
// Hotspots: a badge on an agent working on one (ui/levels/hotspots.js).
import { withHotBadges } from "./hotspots.js";
export const mount = timeTravel(withHotBadges(mountMission), { screen: "mission" });
function mountMission(root, ctx) {
  const S = {
    repo: repoOf(ctx), goal: null, detail: null, questions: [], rules: [], loaded: false, error: null,
    stream: (ctx.state && ctx.state.agents) || [], stories: new Map(), used: new Set(), allLearned: false, flash: new Map(), steered: new Set(), sending: false, seen: "",
  };
  const el = h("div", { class: "lv-screen mi" });
  root.append(el);
  const render = () => rerender(el, view);
  const renderSoon = later(render, 60);
  const reloadSoon = later(() => load(), 900);
  let alive = true;

  // ------------------------------------------------------------- data
  async function load() {
    try {
      const { goals } = await ctx.api(`/api/goals${S.repo ? `?repo=${enc(S.repo)}` : ""}`);
      const g = goals.filter((x) => x.state === "active").sort((a, b) => toMs(b.created) - toMs(a.created))[0] || null;
      S.goal = g;
      if (!g) { S.detail = null; S.loaded = true; S.error = null; if (alive) render(); return; }
      const repo = g.repo;
      const [detail, qs, rs] = await Promise.all([
        ctx.api(`/api/goals/${enc(g.id)}`),
        ctx.api(`/api/questions?repo=${enc(repo)}&state=open`).catch(() => ({ questions: [] })),
        ctx.api(`/api/rules?repo=${enc(repo)}`).catch(() => ({ rules: [] })),
      ]);
      Object.assign(S, { detail, questions: qs.questions || [], rules: rs.rules || [], loaded: true, error: null });
      if (!alive) return;
      render();
      loadStories();
      // ChloeCtx answers about a new question a moment after it is asked.
      if (S.questions.some((q) => q.kind !== "permission" && precedentOf(q).pending) && (S.tries = (S.tries || 0) + 1) <= 15) reloadSoon();
      else S.tries = 0;
    } catch (e) {
      S.error = e.message || String(e);
      S.loaded = true;
      if (alive) render();
    }
  }

  // Each outcome's agent's story, for the ◆ chips: what it recalled, the
  // rules it kept, its déjà vus. Fetched in parallel, kept for a minute.
  function loadStories() {
    const ids = new Set((S.detail?.outcomes || []).map((o) => o.agent).filter(Boolean));
    for (const id of ids) {
      const had = S.stories.get(id);
      if (had && Date.now() - had.at < 60000) continue;
      S.stories.set(id, { at: Date.now(), story: had?.story || null });
      ctx.api(`/api/agents/${enc(id)}/story`).then((s) => { S.stories.set(id, { at: Date.now(), story: s }); if (alive) renderSoon(); }).catch(() => {});
    }
  }

  const agentMap = () => mergeAgents(S.detail?.agents, S.stream);
  const ruleIds = () => new Set(S.rules.map((r) => r.ctx_id).filter(Boolean));

  // ------------------------------------------------------------- view
  function view() {
    if (!S.loaded) return h("div", { class: "mi-wrap mi-loading" }, h("div", { class: "mi-skel" }), h("div", { class: "mi-skel short" }));
    if (S.error && !S.detail && !S.goal) return h("div", { class: "mi-wrap" }, h("div", { class: "lv-card mi-err" }, h("b", {}, "Mission could not load."), h("div", { class: "muted" }, S.error),
      h("button", { class: "btn sm", "data-key": "retry", onclick: () => { S.loaded = false; render(); load(); } }, "Try again")));
    if (!S.goal || S.newGoal) return setGoal();
    const agents = agentMap();
    S.used = new Set();
    return h("div", { class: "mi-wrap" }, header(agents), columns(agents), pulseCard(), learnedCard(), side(agents));
  }

  function header(agents) {
    const g = S.goal, d = S.detail;
    const os = (d?.outcomes || []).filter((o) => o.state !== "dropped");
    const pct = os.length ? Math.round(os.reduce((n, o) => n + (o.state === "done" ? 100 : Math.max(0, Math.min(100, o.progress || 0))), 0) / os.length) : 0;
    const by = (s) => os.filter((o) => o.state === s).length;
    const working = new Set([g.lead, ...os.map((o) => o.agent)].filter(Boolean).filter((id) => agents.get(id)?.running !== false));
    const elapsed = Date.now() - toMs(g.created);
    const eta = pct >= 100 ? "all done" : pct >= 3 ? `ETA about ${fmtDur((elapsed * (100 - pct)) / pct)}` : null;
    const tok = d?.tokens ? (d.tokens.input || 0) + (d.tokens.output || 0) : null;
    const facts = [
      plural(os.length, "outcome"), plural(working.size, "agent"), `${by("done")} done`,
      by("needs_you") ? h("span", { class: "mi-warn-t" }, `${by("needs_you")} need${by("needs_you") === 1 ? "s" : ""} you`) : null,
      eta, tok ? h("span", { title: `${(d.tokens.input || 0).toLocaleString()} in, ${(d.tokens.output || 0).toLocaleString()} out, ${(d.tokens.cache_read || 0).toLocaleString()} read from cache` }, `${fmtTokens(tok)} tokens`) : null,
    ].filter(Boolean);
    const lead = g.lead && agents.get(g.lead);
    return h("header", { class: "mi-head" },
      h("div", { class: "mi-head-l" },
        h("div", { class: "mi-kicker" }, "Goal", h("span", {}, `set at ${fmtClock(g.created)}`), h("span", {}, base(g.repo)),
          h("button", { class: "lv-link mi-newgoal", "data-key": "new-goal", title: "Set another goal: a lead, your own outcomes, or agents already running", onclick: () => { S.newGoal = true; S.form = null; render(); } }, "+ New goal")),
        S.goalNote ? h("div", { class: "mi-gmsg bad" }, S.goalNote) : null,
        h("h1", { class: "mi-goal" }, g.text),
        h("div", { class: "mi-sub" }, facts.map((f) => h("span", {}, f))),
        lead ? h("button", { class: "mi-lead", "data-nav": "", "data-key": "lead", onclick: () => ctx.go(`#/story/${lead.id}`), title: `Open ${lead.name}'s story` },
          avatar(lead, 18), h("b", {}, lead.name), h("span", {}, lead.narration || lead.last_event || ""),
          lead.state === "waiting" ? pill("waiting for you", "warn") : null) : null),
      h("div", { class: "mi-pct" }, h("b", {}, `${pct}%`), h("span", {}, "of the goal")),
      h("div", { class: "mi-prog", role: "progressbar", "aria-valuenow": pct, "aria-valuemin": 0, "aria-valuemax": 100 }, h("b", { style: { width: pct + "%" } })));
  }

  function columns(agents) {
    const os = (S.detail?.outcomes || []).filter((o) => o.state !== "dropped").sort((a, b) => (a.ord ?? 0) - (b.ord ?? 0));
    if (!os.length) {
      const lead = S.goal.lead && agents.get(S.goal.lead);
      return h("section", { class: "mi-cols mi-planning" }, h("div", { class: "lv-card" },
        h("div", { class: "mi-spin" }), h("div", {}, h("b", {}, lead ? `${lead.name} is breaking the goal into outcomes.` : "No outcomes yet."),
          h("div", { class: "muted" }, lead ? lead.narration || "Each outcome gets its own agent; they show up here as they start." : "Add outcomes with add_outcome, or set the goal again with a lead to plan it."))));
    }
    return h("section", { class: "mi-cols" }, COLS.map(([state, label]) => {
      const list = os.filter((o) => o.state === state);
      return h("div", { class: "mi-col" },
        h("h2", { class: "lv-h" }, label, h("span", { class: "n" }, list.length)),
        list.length ? list.map((o) => card(o, agents, os)) : h("div", { class: "mi-none" }, { done: "Nothing finished yet.", active: "No one is working.", needs_you: "Nothing waits for you.", next: "Nothing queued." }[state]));
    }));
  }

  function statusOf(o, a, q) {
    if (o.state === "done") return ["done", "good"];
    if (o.state === "next") return ["queued", "mut"];
    if (o.state === "needs_you") return [q?.kind === "permission" ? "permission" : "asks you", "warn"];
    if (!a) return ["unassigned", "mut"];
    if (a.state === "exited") return ["stopped", "crit"];
    if (a.state === "idle") return ["turn over", "mut"];
    if (a.state === "starting") return ["starting", "acc"];
    const ev = String(a.last_event || "");
    if (/\b(test|pytest|jest|vitest)\b/.test(ev)) return ["testing", "acc"];
    if (/^(Edit|MultiEdit|Write|NotebookEdit)\b/.test(ev)) return ["building", "acc"];
    if (/^(Read|Grep|Glob|LS)\b/.test(ev)) return ["exploring", "acc"];
    if (/chloectx|memory_/.test(ev)) return ["recalling", "acc"];
    if (/^Bash\b/.test(ev)) return ["running", "acc"];
    return ["working", "acc"];
  }

  // The ◆ chips of an outcome: from the question's precedent while it asks,
  // else from its agent's story (déjà vu, rules kept, records recalled).
  function chipsOf(o, a, q, os) {
    const out = [];
    if (q) {
      const p = precedentOf(q);
      const recs = p.records;
      const opts = parseJSON(q.options, []) || [];
      const hit = recs[0];
      if (q.kind === "permission") return out;
      if (p.pending) out.push(["asking ChloeCtx…", "ChloeCtx is looking for what was decided before"]);
      else if (opts.some((x) => x.matches_precedent) && hit) out.push([`precedent: ${hit.by === "you" ? "you decided" : "decided"} this before`, "ChloeCtx found a past decision that matches an option"]);
      else if (recs.length || p.ids.length) out.push([`${plural(recs.length || p.ids.length, "related record")} in memory`, "ChloeCtx found related records, none deciding it"]);
      else out.push(["no precedent in memory", p.missing[0] || "ChloeCtx found nothing about this question"]);
      return out;
    }
    if (o.state === "next") {
      const after = o.after && os.find((x) => x.id === o.after);
      if (after) out.push([`after ${after.title}`, "the outcome it waits for", true]);
      return out;
    }
    const st = a && S.stories.get(a.id)?.story;
    if (!st) return out;
    const knew = (st.knew || []).filter((k) => !String(k.decision || "").startsWith("skip"));
    const rules = ruleIds();
    const deja = knew.find((k) => k.deja_vu);
    const kept = knew.find((k) => rules.has(k.id));
    const short = (t) => String(t).replace(/`/g, "").replace(/[.:]\s.*$|\.$/, "");
    if (o.state !== "done" && deja) out.push([`déjà vu: ${short(deja.text)}`, "recalled a fix for something it hit, or right after a failure"]);
    if (kept) out.push([`kept rule: ${short(kept.text)}`, "a rule you made, given to it by ChloeCtx"]);
    if (out.length < 2 && knew.length) {
      const kinds = new Set(knew.map((k) => k.kind));
      out.push([`recalled ${plural(knew.length, kinds.size === 1 ? [...kinds][0] : "record")}`, "records ChloeCtx put in its context"]);
    }
    // The same chip on many cards says nothing: each text once per board.
    const fresh = out.filter(([t]) => !S.used.has(t)).slice(0, 2);
    fresh.forEach(([t]) => S.used.add(t));
    return fresh;
  }

  function card(o, agents, os) {
    const a = o.agent ? agents.get(o.agent) : null;
    const q = o.state === "needs_you" ? S.questions.find((x) => x.outcome === o.id) || (a && S.questions.find((x) => x.agent === a.id)) : null;
    const [status, tone] = statusOf(o, a, q);
    const said = q ? `“${q.text}”` : o.state === "done" || o.state === "next" || !a ? o.detail || "" : a.narration || o.detail || "";
    const pct = o.state === "done" ? 100 : Math.max(0, Math.min(100, o.progress || 0));
    const open = a ? () => ctx.go(`#/story/${a.id}`) : null;
    return h("article", {
      class: `mi-oc s-${o.state}`, "data-nav": "", "data-key": `o:${o.id}`, tabindex: 0, role: a ? "link" : null,
      "aria-label": `${o.title}, ${status}${a ? `, ${a.name}` : ""}`, onclick: open, style: { cursor: a ? "pointer" : "default" },
    },
      h("div", { class: "mi-oc-h" }, h("b", {}, o.title), pill(status, tone)),
      h("div", { class: "mi-who" },
        a ? avatar(a, 26) : h("span", { class: "mi-av-none" }),
        h("div", {}, a ? h("span", { class: "mi-name" }, o.state === "active" && a.state === "working" ? h("i", { class: "mi-live", title: "working now" }) : null, a.name, " · ") : null, inlineCode(said))),
      h("div", { class: "lv-bar mi-ocbar" }, h("b", { style: { width: pct + "%", background: `var(--${{ done: "good", needs_you: "warn" }[o.state] || "accent"})` } })),
      (() => { const c = chipsOf(o, a, q, os); return c.length ? h("div", { class: "mi-chips" }, c.map(([t, why, plain]) => plain ? h("span", { class: "mi-after", title: why }, "↳ ", t) : ctxChip(h("span", { class: "mi-chip-t" }, t), `${t}: ${why}`))) : null; })());
  }

  function pulseCard() {
    const p = S.detail?.pulse || [];
    if (!p.length) return null;
    const asked = new Set(S.questions.map((q) => Math.floor(toMs(q.created) / 60000)));
    const total = p.reduce((n, [, c]) => n + c, 0);
    if (!total) return null;
    return h("section", { class: "lv-card mi-pulse" },
      h("div", { class: "mi-pulse-h" }, h("h2", { class: "lv-h" }, "Fleet pulse · last 60 min"),
        h("span", { class: "muted" }, `${total.toLocaleString()} events · `, h("i", { class: "mi-key" }), "an agent asked you")),
      barChart(p.map(([, c]) => c), {
        height: 66,
        tone: (_, i) => (asked.has(Math.floor(p[i][0] / 60000)) ? "warn" : "accent"),
        title: (v, i) => `${fmtClock(p[i][0])}: ${v} events`,
      }),
      h("div", { class: "mi-axis" }, h("span", {}, "60 min ago"), h("span", {}, "30"), h("span", {}, "now")));
  }

  // ------------------------------------------------------------- side
  function side(agents) {
    return h("aside", { class: "mi-side" }, needsYou(agents), headsUp(agents));
  }

  function blocks(q) {
    const os = S.detail?.outcomes || [];
    if (!q.outcome) return 0;
    const seen = new Set([q.outcome]);
    let grew = true;
    while (grew) { grew = false; for (const o of os) if (o.after && seen.has(o.after) && !seen.has(o.id)) { seen.add(o.id); grew = true; } }
    return seen.size;
  }

  async function answer(q, option, label) {
    S.flash.set(q.id, { text: `Answered: ${label}`, ok: null });
    render();
    try {
      await ctx.api(`/api/questions/${enc(q.id)}/answer`, { method: "POST", body: q.kind === "permission" ? { option, label } : { option } });
      const a = agentMap().get(q.agent);
      S.flash.set(q.id, { text: `Answered “${label}”.${a ? ` ${a.name} goes on.` : ""}`, ok: true });
      setTimeout(() => { S.flash.delete(q.id); if (alive) render(); }, 4000);
      await load();
    } catch (e) {
      // A prompt that left its agent's screen: nothing was typed, and its
      // question is closed (sixth build); say why for a while.
      S.flash.set(q.id, { text: `Not answered. ${e.message}`, ok: false });
      if (e.status === 409) { setTimeout(() => { S.flash.delete(q.id); if (alive) render(); }, 9000); await load(); }
      render();
    }
  }

  function needsYou(agents) {
    const qs = [...S.questions].sort((a, b) => toMs(a.created) - toMs(b.created));
    const done = [...S.flash.entries()].filter(([id]) => !qs.some((q) => q.id === id));
    if (!qs.length && !done.length) return h("section", { class: "lv-card mi-need quiet" }, h("h2", { class: "lv-h" }, "Needs you"), h("div", { class: "muted" }, "Nothing waits for you. Questions from agents land here, with what you decided before."));
    return h("section", { class: "lv-card mi-need" },
      h("h2", { class: "lv-h" }, "Needs you", h("span", { class: "n" }, `· ${qs.length}`)),
      qs.map((q) => question(q, agents)),
      done.map(([id, f]) => h("div", { class: `mi-flash ${f.ok === false ? "bad" : "ok"}`, "data-key": `f:${id}` }, f.ok === null ? h("i", { class: "mi-spin sm" }) : f.ok ? "✓ " : "", f.text)));
  }

  function question(q, agents) {
    const a = agents.get(q.agent);
    const opts = parseJSON(q.options, []) || [];
    const f = S.flash.get(q.id);
    const n = blocks(q);
    const open = () => { try { sessionStorage.setItem("wb.decide.q", q.id); } catch {} ctx.go("#/decide"); };
    const perm = q.kind === "permission";
    // Sixth build: a prompt's own options (as its screen lists them): its
    // plain first one and its "No" here, the rest in Decide; none until
    // they are read.
    const plain = opts.findIndex((o, i) => i === 0 && !o.no && !o.grants);
    const no = opts.findIndex((o) => o.no);
    const pick = perm ? [plain, no].filter((i) => i >= 0).map((i) => [i, opts[i].label]) : opts.length <= 2 ? opts.map((o, i) => [i, o.label]) : [];
    const best = perm ? plain : opts.findIndex((o) => o.matches_precedent) >= 0 ? opts.findIndex((o) => o.matches_precedent) : q.recommended ?? -1;
    const d = q.detail || {};
    return h("div", { class: `mi-q${perm ? " perm" : ""}` },
      h("div", { class: "mi-q-top" }, a ? avatar(a, 24) : avatar({ name: "?" }, 24),
        h("div", { class: "mi-q-body" },
          h("button", { class: "mi-q-t", "data-nav": "", "data-key": `q:${q.id}`, onclick: open, title: "Open in Decide" }, perm ? (d.menu?.kind === "trust" ? `${a ? a.name : "An agent"} asks: trust this folder?` : d.action ? `${a ? a.name : "An agent"} wants to: ${d.action}` : `${a ? a.name : "An agent"} wants to run`) : q.text),
          perm && q.context && !d.action ? h("code", { class: "mi-cmd" }, q.context) : null,
          h("div", { class: "mi-q-m" }, [a?.name, `waiting ${fmtDur(Date.now() - toMs(q.created))}`, n ? `blocks ${plural(n, "outcome")}` : null].filter(Boolean).join(" · "))),
        pick.length ? h("button", { class: "btn sm ghost icon mi-open", "data-key": `qo:${q.id}`, onclick: open, title: "Open in Decide", "aria-label": "Open in Decide" }, "↗") : null),
      f ? h("div", { class: `mi-flash ${f.ok === false ? "bad" : "ok"}` }, f.ok === null ? h("i", { class: "mi-spin sm" }) : null, f.text) :
        h("div", { class: "mi-q-acts" },
          pick.map(([i, label]) => h("button", { class: `btn sm${i === best ? " primary" : ""}`, "data-key": `qa:${q.id}:${i}`, title: perm ? `Choose “${label}” in its prompt` : opts[i]?.detail || "", onclick: () => answer(q, i, label) }, label)),
          perm && opts.length > pick.length ? h("button", { class: "btn sm ghost", "data-key": `qm:${q.id}`, onclick: open, title: "Every option of its prompt, and the whole command" }, `${opts.length - pick.length} more…`) : null,
          pick.length ? null : h("button", { class: "btn sm", "data-key": `qo:${q.id}`, onclick: open }, perm && !opts.length ? "Reading its prompt… open in Decide" : "Answer in Decide")));
  }

  function headsUp(agents) {
    const hs = S.detail?.heads_up || [];
    if (!hs.length) return null;
    return h("section", { class: "lv-card mi-heads" }, h("h2", { class: "lv-h" }, "Heads up"),
      hs.map((c, i) => {
        // both_changed: [changer, changer…]; heading_into: [changer, reader].
        const who = c.agents.map((id) => agents.get(id)).filter(Boolean);
        const [first, ...rest] = who;
        const heading = c.kind === "heading_into";
        const key = `${c.path}:${c.agents.join(",")}`;
        const told = S.steered.has(key);
        const wait = async (btn) => {
          btn.disabled = true;
          try {
            await Promise.all(rest.map((x) => ctx.api(`/api/agents/${enc(x.id)}/steer`, { method: "POST", body: { text: `Leave ${c.path} alone until ${first.name} has finished with it; ${first.name} is changing it now.` } })));
            S.steered.add(key);
          } catch (e) { btn.disabled = false; btn.title = e.message; }
          render();
        };
        return h("div", { class: "mi-hu" },
          heading && first && rest.length
            ? h("div", {}, h("b", {}, `${names(rest.map((x) => x.name))} is heading into `), h("code", { class: "lv-code" }, c.path), `, which ${first.name} changed.`)
            : h("div", {}, h("b", {}, `${names(who.map((x) => x.name))} ${who.length > 2 ? "all" : "both"} changed `), h("code", { class: "lv-code" }, c.path), "."),
          c.detail && heading ? h("div", { class: "muted mi-hu-d" }, c.detail) : null,
          h("div", { class: "mi-q-acts" },
            first && rest.length ? h("button", { class: "btn sm", "data-key": `hu:${i}`, disabled: told, onclick: (e) => wait(e.currentTarget) }, told ? `✓ Told ${names(rest.map((x) => x.name))} to wait` : `Let ${first.name} finish first`) : null,
            h("button", { class: "btn sm ghost", "data-key": `hm:${i}`, onclick: () => ctx.go("#/map") }, "Show map")));
      }));
  }

  function learnedCard() {
    const ls = S.detail?.learned || [];
    const rules = ruleIds();
    return h("section", { class: "lv-card mi-learn" }, h("h2", { class: "lv-h" }, "The fleet learned today", ls.length ? h("span", { class: "n" }, `· ${ls.length}`) : null),
      ls.length ? h("ul", {}, [...ls].sort((a, b) => toMs(b.ts) - toMs(a.ts)).slice(0, S.allLearned ? undefined : 6).map((r) => h("li", {},
        ctxChip(rules.has(r.id) || r.kind === "rule" ? "rule" : r.kind === "error-fix" ? "fix" : r.kind),
        " ", inlineCode(r.text), " ",
        h("span", { class: "muted" }, [r.by === "you" ? "from your answer" : r.agent?.name || r.by, fmtClock(r.ts)].filter(Boolean).join(", "))))) : null,
      ls.length > 6 ? h("button", { class: "btn sm ghost mi-more", "data-key": "learn-more", onclick: () => { S.allLearned = !S.allLearned; render(); } }, S.allLearned ? "Fewer" : `${ls.length - 6} more`) : null,
      ls.length ? null :
        h("div", { class: "muted" }, "Nothing new yet today. Decisions, fixes and facts the agents' sessions leave in ChloeCtx show up here."));
  }

  // ------------------------------------------------------------- set a goal
  // Three ways to staff a goal (sixth build, Dev): a lead that plans it; the
  // outcomes you already know, one agent each and no planner; or agents
  // already running, each made an outcome. The lead's (or the agents')
  // model and effort are picked here. The form is built once and kept, so
  // that the stream's re-renders keep what was typed.
  const MODES = [["plan", "A lead plans it"], ["outcomes", "I know the outcomes"], ["agents", "Agents already running"]];
  function setGoal() {
    if (S.form) { S.form.sync(); return S.form.el; }
    const G = { mode: "plan", picked: new Set() };
    const text = h("textarea", { class: "mi-gt", rows: 3, "data-key": "goal-text", placeholder: "Refunds v2: partial refunds, idempotent, visible in the dashboard", "aria-label": "The goal" });
    import("./complete.js").then((m) => m.attachComplete(text, { field: "goal", ctx, repo: () => repo.value.trim() })).catch(() => {}); // Fifth build: autocomplete
    const repo = h("input", { class: "mi-gr", "data-key": "goal-repo", value: S.repo || "", placeholder: "/path/to/the/repository", "aria-label": "Repository", spellcheck: "false" });
    const outs = h("textarea", { class: "mi-gt mi-gos", rows: 4, "data-key": "goal-outcomes", "aria-label": "The outcomes, one per line",
      placeholder: "Totals in integer cents — test_cart passes with 0.1 + 0.2\nPay button submits the order — test_web clicks Pay\nCSV export of orders — a test reads the file back" });
    const model = h("input", { class: "mi-gr mi-gm", "data-key": "goal-model", list: "mi-models", placeholder: "default", "aria-label": "Model", spellcheck: "false" });
    const effort = h("select", { class: "mi-gr mi-ge", "data-key": "goal-effort", "aria-label": "Effort" },
      h("option", { value: "" }, "default"), ["low", "medium", "high", "xhigh", "max"].map((e) => h("option", { value: e }, e)));
    const who = h("div", { class: "mi-gwho", role: "tablist", "aria-label": "Who does the work" });
    const panel = h("div", { class: "mi-gpanel" });
    const msg = h("div", { class: "mi-gmsg muted" });
    const go = h("button", { class: "btn primary", type: "submit", "data-key": "goal-go" }, "Plan it", h("kbd", {}, "⌘↵"));
    const lines = () => outs.value.split("\n").map((l) => l.trim().replace(/^([-*]|\d+[.)])\s+/, "")).filter(Boolean);
    const spec = (l) => { const m = l.split(/\s+[—–]\s+|\s+--\s+/); return m.length > 1 ? { title: m[0].trim(), detail: m.slice(1).join(" — ").trim() } : { title: l }; };
    const running = () => (S.stream || []).filter((a) => a.running !== false && a.state !== "exited" && (!repo.value.trim() || a.repo === repo.value.trim()));
    const modelRow = (whose) => h("div", { class: "mi-gmodel" },
      h("label", { class: "mi-glab" }, h("span", {}, `${whose} model`), model),
      h("label", { class: "mi-glab" }, h("span", {}, "Effort"), effort),
      h("datalist", { id: "mi-models" }, ["haiku", "sonnet", "opus"].map((m) => h("option", { value: m }))));

    function sync() {
      who.replaceChildren(...MODES.map(([k, label]) => h("button", {
        type: "button", role: "tab", "aria-selected": String(G.mode === k), class: "mi-gw" + (G.mode === k ? " on" : ""), "data-key": `gw:${k}`,
        onclick: () => { G.mode = k; sync(); },
      }, label, k === "agents" ? h("span", { class: "n" }, String(running().length)) : null)));
      if (G.mode === "plan") {
        panel.replaceChildren(h("p", { class: "muted mi-gp" }, "A lead agent reads the repository, breaks the goal into 3 to 8 outcomes, starts an agent for each, and asks you here rather than in a terminal. It costs a planner's session; pick a cheaper model for it if you like."), modelRow("Lead's"));
        go.firstChild.textContent = "Plan it";
      } else if (G.mode === "outcomes") {
        const n = lines().length;
        panel.replaceChildren(
          h("label", { class: "mi-gl2" }, "The outcomes, one per line", h("span", { class: "muted" }, " · after “ — ”, how to tell it is done")),
          outs,
          h("p", { class: "muted mi-gp" }, n ? `${n} agent${n === 1 ? "" : "s"} start, one per outcome, each in its own worktree; no planner.` : "Each line starts its own agent, in its own worktree; no planner."),
          modelRow("Agents'"));
        go.firstChild.textContent = n ? `Start ${n} agent${n === 1 ? "" : "s"}` : "Start the agents";
      } else {
        const list = running();
        for (const id of [...G.picked]) if (!list.some((a) => a.id === id)) G.picked.delete(id);
        panel.replaceChildren(...[list.length
          ? h("div", { class: "mi-gagents" }, list.map((a) => h("label", { class: "mi-ga" + (G.picked.has(a.id) ? " on" : "") },
            h("input", { type: "checkbox", checked: G.picked.has(a.id), "data-key": `ga:${a.id}`, onchange: (e) => { if (e.target.checked) G.picked.add(a.id); else G.picked.delete(a.id); sync(); } }),
            avatar(a, 20), h("b", {}, a.name), h("span", { class: "muted mi-gat" }, a.task ? String(a.task).split("\n")[0].slice(0, 140) : a.state))))
          : h("p", { class: "muted mi-gp" }, "No agent is running in this repository. Start them with New agent (n), then come back to set their goal."),
        list.length ? h("p", { class: "muted mi-gp" }, "Each one you tick becomes an outcome of the goal (its task is the title) and is told so at its next step.") : null].flat().filter((k) => k != null && k !== false));
        const n = G.picked.size;
        go.firstChild.textContent = n ? `Set the goal for ${n} agent${n === 1 ? "" : "s"}` : "Set the goal";
      }
      go.disabled = S.sending || (G.mode === "agents" && !G.picked.size) || (G.mode === "outcomes" && !lines().length);
    }
    outs.addEventListener("input", () => sync());
    repo.addEventListener("change", () => sync());

    const submit = async (e) => {
      e && e.preventDefault();
      if (S.sending) return;
      const t = text.value.trim(), r = repo.value.trim();
      if (!t) { text.focus(); return; }
      if (!r) { repo.focus(); return; }
      const body = { repo: r, text: t };
      if (G.mode === "plan") body.plan = true;
      if (G.mode === "outcomes") { if (!lines().length) { outs.focus(); return; } body.outcomes = lines().map(spec); }
      if (G.mode === "agents") { if (!G.picked.size) return; body.agents = [...G.picked]; }
      if (G.mode !== "agents") { if (model.value.trim()) body.model = model.value.trim(); if (effort.value) body.effort = effort.value; }
      S.sending = true; go.disabled = true; msg.className = "mi-gmsg muted";
      msg.textContent = G.mode === "plan" ? "Setting the goal and starting a lead…" : G.mode === "outcomes" ? "Setting the goal and starting its agents…" : "Setting the goal…";
      try {
        const out = await ctx.api("/api/goals", { method: "POST", body });
        S.repo = r;
        if ((out.missing || []).length) { S.goalNote = out.missing.join(" "); }
        S.form = null; S.newGoal = false;
        await load();
      } catch (err) { msg.textContent = err.message; msg.className = "mi-gmsg bad"; }
      S.sending = false;
      sync();
    };
    text.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) submit(e); });
    outs.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) submit(e); });
    queueMicrotask(() => { if (!document.activeElement || document.activeElement === document.body) text.focus(); });
    const el2 = h("div", { class: "mi-wrap mi-empty" },
      h("form", { class: "lv-card mi-set", onsubmit: submit },
        h("div", { class: "mi-kicker" }, "Mission", h("span", {}, S.repo ? base(S.repo) : "no repository yet")),
        h("h1", { class: "mi-goal" }, "Set a goal"),
        h("p", { class: "muted" }, "Say what the fleet should get done, in a sentence. You steer outcomes here; the terminals stay one level down."),
        text,
        h("label", { class: "mi-glab" }, h("span", {}, "Repository"), repo),
        h("div", { class: "mi-gl2 mi-gwho-l" }, "Who does the work"), who, panel,
        h("div", { class: "mi-gfoot" }, msg, S.goal ? h("button", { class: "btn ghost", type: "button", "data-key": "goal-cancel", onclick: () => { S.newGoal = false; S.form = null; render(); } }, "Cancel") : null, go)));
    S.form = { el: el2, sync };
    sync();
    return el2;
  }

  // ------------------------------------------------------------- life
  const offKeys = keyNav(el);
  render();
  load();

  return {
    update(state) {
      if (!state) return;
      S.stream = state.agents || S.stream;
      // A changed state, outcome or goal is worth a reload; a new sentence is not.
      const sig = (S.stream || []).filter((a) => !S.goal || a.goal === S.goal.id || a.repo === S.goal.repo).map((a) => `${a.id}:${a.state}:${a.outcome || ""}:${a.goal || ""}`).join("|");
      if (sig !== S.seen) { if (S.seen) reloadSoon(); S.seen = sig; }
      renderSoon();
    },
    unmount() {
      alive = false;
      offKeys(); renderSoon.cancel(); reloadSoon.cancel();
      el.remove();
    },
  };
}

useStyle("mission", `
.mi-wrap{display:grid;grid-template-columns:minmax(0,1fr) 352px;grid-template-rows:auto auto auto 1fr;grid-template-areas:"head side" "cols side" "pulse side" "learn side";gap:18px 22px;padding:20px 22px 32px;max-width:1880px;margin:0 auto;align-items:start}
.mi-head{grid-area:head;display:grid;grid-template-columns:minmax(0,1fr) auto;gap:4px 20px;align-items:start}
.mi-head-l{min-width:0}
.mi-kicker{display:flex;flex-wrap:wrap;gap:0 7px;font-size:11.5px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--muted)}
.mi-kicker span{font-weight:500;letter-spacing:0;text-transform:none;font-size:12px}
.mi-kicker span::before{content:"·";margin-right:7px}
.mi-goal{margin:5px 0 0;font-size:22px;font-weight:680;letter-spacing:-.012em;line-height:1.25;text-wrap:balance}
.mi-sub{display:flex;flex-wrap:wrap;color:var(--muted);margin-top:5px;font-variant-numeric:tabular-nums}
.mi-sub>span+span::before{content:"·";margin:0 7px}
.mi-warn-t{color:var(--warn-ink);font-weight:600}
.mi-lead{display:inline-flex;align-items:center;gap:7px;max-width:100%;margin-top:9px;padding:4px 10px 4px 5px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);font-size:12.5px;cursor:pointer}
.mi-lead:hover{border-color:var(--line-2);color:var(--text)}
.mi-lead>span:not(.lv-av):not(.lv-pill){white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.mi-lead b{font-weight:600;color:var(--text)}
.mi-pct{text-align:right;display:flex;flex-direction:column;align-items:flex-end;padding-top:2px}
.mi-pct b{font-size:32px;font-weight:720;line-height:1;letter-spacing:-.02em;font-variant-numeric:tabular-nums}
.mi-pct span{color:var(--muted);margin-top:5px}
.mi-prog{grid-column:1/-1;height:8px;border-radius:999px;background:var(--surface-3);overflow:hidden;margin-top:10px}
.mi-prog>b{display:block;height:100%;border-radius:999px;background:linear-gradient(90deg,var(--good),var(--accent));transition:width .5s ease}
.mi-cols{grid-area:cols;display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:14px}
.mi-col{display:flex;flex-direction:column;gap:11px;min-width:0}
.mi-col>.lv-h{margin:2px 0 0}
.mi-none{border:1px dashed var(--line-2);border-radius:12px;padding:14px;color:var(--muted);font-size:12.5px;text-align:center}
.mi-oc{background:var(--surface);border:1px solid var(--line);border-radius:12px;padding:13px;box-shadow:var(--shadow-1);transition:border-color .12s,box-shadow .12s,transform .12s;min-width:0}
.mi-oc[role=link]:hover{border-color:var(--line-2);transform:translateY(-1px)}
.mi-oc.s-needs_you{border-color:color-mix(in srgb,var(--warn) 60%,var(--line));box-shadow:0 0 0 3px var(--warn-glow)}
.mi-oc.s-next{background:var(--surface-2)}
.mi-oc-h{display:flex;justify-content:space-between;align-items:flex-start;gap:8px}
.mi-oc-h b{font-size:13.5px;line-height:1.3;font-weight:650}
.mi-who{display:flex;gap:9px;align-items:flex-start;margin-top:9px;font-size:12.5px;line-height:1.45;color:var(--text-2)}
.mi-who>div{min-width:0;overflow-wrap:anywhere}
.mi-name{color:var(--muted)}
.mi-live{display:inline-block;width:6px;height:6px;border-radius:50%;background:var(--good);margin:0 5px 1px 0;vertical-align:middle;box-shadow:0 0 0 0 var(--good);animation:mi-live 1.8s ease-out infinite}
@keyframes mi-live{0%{box-shadow:0 0 0 0 color-mix(in srgb,var(--good) 60%,transparent)}100%{box-shadow:0 0 0 6px transparent}}
.mi-av-none{width:26px;height:26px;border-radius:8px;border:1.5px dashed var(--line-2);flex:none}
.mi-ocbar{margin-top:10px}
.mi-ocbar>b{transition:width .5s ease}
.mi-chips{display:flex;flex-direction:column;gap:4px;margin-top:9px;min-width:0}
.mi-chips .lv-ctx{max-width:100%;min-width:0}
.mi-chip-t{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.mi-after{font-size:11px;font-weight:600;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.mi-pulse{grid-area:pulse;padding:14px 16px 10px}
.mi-pulse-h{display:flex;justify-content:space-between;align-items:baseline;gap:12px;flex-wrap:wrap;margin-bottom:10px}
.mi-pulse-h .lv-h{margin:0}
.mi-pulse-h .muted{font-size:12px;display:inline-flex;align-items:center;gap:5px}
.mi-key{display:inline-block;width:9px;height:9px;border-radius:2px;background:var(--warn)}
.mi-axis{display:flex;justify-content:space-between;color:var(--muted);font-size:10.5px;margin-top:5px;font-variant-numeric:tabular-nums}
.mi-side{grid-area:side;display:flex;flex-direction:column;gap:12px;min-width:0}
.mi-side .lv-h{margin-bottom:11px}
.mi-need{border-color:color-mix(in srgb,var(--warn) 55%,var(--line));background:linear-gradient(180deg,color-mix(in srgb,var(--warn-soft) 70%,var(--surface)),var(--surface) 140px)}
.mi-need.quiet{border-color:var(--line);background:var(--surface)}
.mi-need>.lv-h{color:var(--warn-ink)}.mi-need.quiet>.lv-h{color:var(--muted)}
.mi-q+.mi-q,.mi-q+.mi-flash{margin-top:12px;padding-top:12px;border-top:1px solid var(--line)}
.mi-q-top{display:flex;gap:10px;align-items:flex-start}
.mi-q-body{min-width:0;flex:1}
.mi-q-t{display:block;text-align:left;border:0;background:none;padding:0;font:inherit;font-weight:650;font-size:13.5px;line-height:1.35;color:var(--text);cursor:pointer}
.mi-q-t:hover{color:var(--accent)}
.mi-q-m{color:var(--muted);font-size:12px;margin-top:2px}
.mi-cmd{display:block;margin-top:5px;font:11.5px/1.45 var(--mono);background:var(--sunken);border:1px solid var(--line);border-radius:6px;padding:5px 8px;color:var(--text);white-space:pre-wrap;overflow-wrap:anywhere}
.mi-q-acts{display:flex;flex-wrap:wrap;gap:6px;margin-top:10px}
.mi-more{margin:8px 0 -4px -10px}
.mi-flash{display:flex;align-items:center;gap:7px;margin-top:10px;font-size:12.5px;font-weight:600;border-radius:8px;padding:7px 10px;animation:rise .2s ease-out}
.mi-flash.ok{background:var(--good-soft);color:var(--good-ink)}.mi-flash.bad{background:var(--crit-soft);color:var(--crit-ink)}
.mi-heads>.lv-h{color:var(--crit-ink)}
.mi-hu{line-height:1.45}.mi-hu+.mi-hu{margin-top:12px;padding-top:12px;border-top:1px solid var(--line)}
.mi-hu b{font-weight:650}.mi-hu-d{margin-top:3px;font-size:12.5px}
.mi-hu .mi-q-acts{padding-left:0}
.mi-learn{grid-area:learn}.mi-learn>.lv-h{color:var(--ctx)}.mi-open{width:26px;height:26px;font-size:14px;margin:-3px -6px 0 0;color:var(--muted)}
.mi-learn ul{list-style:none;margin:0;padding:0;display:grid;grid-template-columns:repeat(auto-fill,minmax(330px,1fr));gap:10px 28px;color:var(--text-2);line-height:1.45}
.mi-learn .lv-ctx{display:inline-flex;vertical-align:baseline;margin-right:1px}
.mi-planning{display:block}.mi-planning .lv-card{display:flex;gap:14px;align-items:center;padding:20px}
.mi-spin{width:18px;height:18px;border-radius:50%;border:2px solid var(--line-2);border-top-color:var(--accent);animation:spin .9s linear infinite;flex:none;display:inline-block}
.mi-spin.sm{width:12px;height:12px;border-width:1.5px}
.mi-empty{display:block;padding-top:8vh}
.mi-set{max-width:680px;margin:0 auto;padding:26px 28px 22px}
.mi-set p{margin:6px 0 16px;line-height:1.5}
.mi-gt,.mi-gr{width:100%;border:1px solid var(--line-2);border-radius:9px;background:var(--surface-2);color:var(--text);padding:10px 12px;font:inherit;outline:none;resize:vertical}
.mi-gt{font-size:15px;line-height:1.45;min-height:84px}
.mi-gt:focus,.mi-gr:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.mi-gr{font-family:var(--mono);font-size:12.5px;padding:8px 10px}
.mi-glab{display:grid;grid-template-columns:auto 1fr;gap:10px;align-items:center;margin-top:12px;color:var(--muted);font-size:12.5px}
.mi-gfoot{display:flex;align-items:center;gap:14px;justify-content:space-between;margin-top:16px}
.mi-gmsg{font-size:12.5px;line-height:1.4}.mi-gmsg.bad{color:var(--crit-ink)}
.mi-newgoal{margin-left:10px;font-size:11.5px;text-transform:none;letter-spacing:0}
.mi-gl2{display:block;margin-top:14px;color:var(--muted);font-size:12.5px;font-weight:600}.mi-gl2 .muted{font-weight:400}
.mi-gwho{display:inline-flex;flex-wrap:wrap;gap:2px;margin-top:6px;padding:3px;border:1px solid var(--line);border-radius:999px;background:var(--surface-2)}
.mi-gw{border:0;background:none;border-radius:999px;padding:5px 12px;font:inherit;font-size:12.5px;font-weight:600;color:var(--text-2);cursor:pointer;display:inline-flex;gap:6px;align-items:center}
.mi-gw:hover{color:var(--text)}.mi-gw.on{background:var(--surface);color:var(--accent);box-shadow:var(--shadow-1)}
.mi-gw .n{font-size:11px;color:var(--muted);font-weight:500}
.mi-gpanel{margin-top:10px}
.mi-gp{margin:8px 0 0!important;font-size:12.5px}
.mi-gos{font-size:13px;min-height:96px;margin-top:6px}
.mi-gmodel{display:grid;grid-template-columns:1fr 1fr;gap:0 14px}
.mi-gmodel .mi-glab{grid-template-columns:auto 1fr}
.mi-gagents{display:flex;flex-direction:column;gap:4px;margin-top:6px;max-height:260px;overflow:auto}
.mi-ga{display:flex;align-items:center;gap:8px;padding:6px 9px;border:1px solid var(--line);border-radius:9px;cursor:pointer;min-width:0}
.mi-ga.on{border-color:var(--accent);background:var(--accent-soft)}
.mi-gat{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0;font-size:12px}
.mi-err{display:flex;flex-direction:column;gap:8px;align-items:flex-start;max-width:560px}
.mi-loading{display:block}.mi-skel{height:28px;border-radius:8px;background:var(--surface-3);width:55%;margin-bottom:12px;animation:pulse 1.4s ease-in-out infinite}.mi-skel.short{width:30%;height:14px}
@media (max-width:1180px){
  .mi-wrap{grid-template-columns:minmax(0,1fr);grid-template-rows:none;grid-template-areas:"head" "side" "cols" "pulse" "learn"}
  .mi-side{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));align-items:start}
}
@media (max-width:900px){.mi-cols{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media (max-width:560px){
  .mi-wrap{padding:14px 14px 24px;gap:14px}
  .mi-cols{grid-template-columns:minmax(0,1fr)}
  .mi-goal{font-size:18px}.mi-pct b{font-size:24px}
  .mi-side{grid-template-columns:minmax(0,1fr)}
  .mi-learn ul{grid-template-columns:minmax(0,1fr)}
  .mi-pulse .lv-bars{gap:1px}
  .mi-set{padding:18px}
}
`);
