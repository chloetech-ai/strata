// Story (SPEC.md, "Levels", one agent): the agent read at four levels, from
// why it exists down to its terminal: Intent, Story (a chapter per turn),
// Steps (its tool calls grouped, each opening to its events) and Terminal
// (the last lines, and the way to the live one). Beside them: its déjà vus,
// what ChloeCtx put in its context (and left out), and a box to steer it at
// the level you are reading. `]` goes a level deeper (toward the terminal),
// `[` a level back up, as the top bar's level switch does.
import {
  h, useStyle, toMs, fmtDur, fmtClock, avatar, ctxChip, pill, routeParam, mergeAgents, inlineCode,
  fmtTokens, fmtSpan, fmtDay, keyNav, rerender, later,
} from "./common.js";
import { testChip } from "./test-chip.js";

const enc = encodeURIComponent;
const LEVELS = [
  ["intent", "Intent", "why it exists"],
  ["story", "Story", "a chapter per turn"],
  ["steps", "Steps", "tool calls, grouped"],
  ["terminal", "Terminal", "the raw session"],
];
const KIND_TONE = { read: "lane-1", search: "lane-1", edit: "good", run: "warn", memory: "ctx", agents: "lane-4", web: "lane-5", other: "muted" };
const STEER_HINT = {
  intent: "Change what it is for…",
  story: "Redirect its next chapter…",
  steps: "Tell it about this step…",
  terminal: "Say it in words, not keystrokes…",
};

export function mount(root, ctx) {
  const id = routeParam(ctx, "agent");
  const S = {
    id, story: null, error: null, outcome: null, goal: null, rules: [], stream: (ctx.state && ctx.state.agents) || [],
    level: "story", chapter: null, all: false, open: new Set(), events: new Map(), peek: null, peekAt: 0,
    talk: "", sent: null, sending: false, about: true, seen: "",
  };
  const el = h("div", { class: "lv-screen sy" });
  root.append(el);
  const render = () => rerender(el, view);
  const renderSoon = later(render, 60);
  const reloadSoon = later(() => load(), 1500);
  const peekSoon = later(() => loadPeek(true), 1200);
  let alive = true;

  const agentOf = () => {
    const m = mergeAgents(S.story ? [S.story.agent] : [], S.stream);
    return m.get(S.id) || null;
  };

  // ------------------------------------------------------------- data
  async function load() {
    if (!S.id) { S.error = null; render(); return; }
    try {
      const st = await ctx.api(`/api/agents/${enc(S.id)}/story`);
      S.story = st;
      S.error = null;
      if (S.chapter == null || !st.chapters.some((c) => c.turn === S.chapter)) S.chapter = (st.chapters.find((c) => c.state === "now") || st.chapters.at(-1) || {}).turn ?? null;
      const a = st.agent || {};
      const extra = [];
      if (a.goal) extra.push(ctx.api(`/api/goals/${enc(a.goal)}`).then((g) => { S.goal = g.goal; S.outcome = (g.outcomes || []).find((o) => o.id === a.outcome) || null; }).catch(() => {}));
      if (a.repo) extra.push(ctx.api(`/api/rules?repo=${enc(a.repo)}`).then((r) => { S.rules = r.rules || []; }).catch(() => {}));
      if (alive) render();
      await Promise.all(extra);
      if (alive) render();
      if (S.level === "terminal" || !S.peek) loadPeek();
    } catch (e) {
      S.error = e.status === 404 ? `There is no agent ${S.id}: pick one above.` : e.message || String(e);
      if (alive) render();
    }
  }

  async function loadEvents(step) {
    const key = step.events.join(",");
    if (S.events.has(key) || !step.events.length) return;
    S.events.set(key, null);
    const lo = Math.min(...step.events), hi = Math.max(...step.events);
    try {
      const { events } = await ctx.api(`/api/agents/${enc(S.id)}/events?after=${lo - 1}&limit=${Math.min(500, hi - lo + 1)}`);
      const want = new Set(step.events);
      S.events.set(key, events.filter((e) => want.has(e.seq)));
    } catch (e) { S.events.set(key, { error: e.message }); }
    if (alive) render();
  }

  // The last half minute of its terminal, as text.
  async function loadPeek(force) {
    const a = agentOf();
    if (!a || (!force && Date.now() - S.peekAt < 5000)) return;
    S.peekAt = Date.now();
    const started = a.started_ms || toMs(a.created) || Date.now();
    const end = (toMs(a.ended) || Date.now()) - started;
    try {
      const r = await ctx.api(`/api/agents/${enc(S.id)}/recording?from_ms=${Math.max(0, Math.round(end - 30000))}`);
      S.peek = { lines: screenLines((r.before || "") + (r.events || []).filter((e) => e[1] === "o").map((e) => e[2]).join("")), end };
    } catch (e) { S.peek = { error: e.status === 404 ? "No recording of its terminal." : e.message, end }; }
    if (alive) render();
  }

  // ------------------------------------------------------------- levels
  function setLevel(name, { focus = true } = {}) {
    S.level = name;
    const st = S.story;
    if (st && name === "story" && S.chapter == null) S.chapter = (st.chapters.at(-1) || {}).turn ?? null;
    if (st && name === "steps") {
      const ch = st.chapters.find((c) => c.turn === S.chapter) || st.chapters.at(-1);
      const step = ch && (ch.steps.find((s) => s.failed) || ch.steps.at(-1));
      if (step) { const k = stepKey(ch, step); S.open.add(k); loadEvents(step); }
    }
    if (name === "terminal") loadPeek();
    render();
    if (!focus) return;
    const sec = el.querySelector(`[data-level="${name}"]`);
    if (!sec) return;
    const target = name === "story" ? sec.querySelector(".sy-ch.on") : name === "steps" ? sec.querySelector(".sy-step.open") || sec.querySelector(".sy-step") : name === "terminal" ? sec.querySelector(".sy-live") : sec;
    (target || sec).focus({ preventScroll: true });
    sec.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }
  const move = (d) => {
    const i = LEVELS.findIndex(([n]) => n === S.level);
    const j = i + d;
    if (j >= LEVELS.length) { ctx.zoom(S.id); return; }   // below the terminal: the live one
    if (j < 0) { ctx.go("#/mission"); return; }            // above its intent: the fleet's
    setLevel(LEVELS[j][0]);
  };

  // ------------------------------------------------------------- view
  // Sixth build: which agent's story, picked at the top (Dev needed three
  // clicks and a detour through Terminals to switch). Running agents first,
  // newest first, then those that ended in the last day.
  function agentsBar() {
    const all = [...mergeAgents(S.stream, S.story ? [S.story.agent] : []).values()];
    const day = Date.now() - 24 * 3600e3;
    const live = all.filter((x) => x.running !== false && x.state !== "exited").sort((x, y) => (toMs(y.created) || 0) - (toMs(x.created) || 0));
    const gone = all.filter((x) => !(x.running !== false && x.state !== "exited") && ((toMs(x.ended) || toMs(x.state_since) || 0) > day || x.id === S.id)).sort((x, y) => (toMs(y.created) || 0) - (toMs(x.created) || 0));
    if (!live.length && !gone.length) return h("nav", { class: "sy-pick", "aria-label": "Agent" }, h("span", { class: "sy-pick-k" }, "Story of"), h("span", { class: "muted" }, "no agent yet: start one with New agent (n), and its story is told here."));
    const chip = (x) => h("button", {
      class: "sy-pick-a" + (x.id === S.id ? " on" : "") + (x.state === "exited" || x.running === false ? " gone" : ""), "data-key": `pick:${x.id}`,
      "aria-current": x.id === S.id ? "page" : null, title: `${x.name}: ${x.state}${x.task ? ` · ${String(x.task).slice(0, 120)}` : ""}`,
      onclick: () => { if (x.id !== S.id) ctx.go(`#/story/${x.id}`); },
    }, avatar(x, 18), h("b", {}, x.name), h("span", { class: `sy-pick-s s-${x.state}` }, x.state === "waiting" ? "needs you" : x.state));
    return h("nav", { class: "sy-pick", "aria-label": "Whose story" }, h("span", { class: "sy-pick-k" }, "Story of"),
      live.map(chip), gone.length ? h("span", { class: "sy-pick-sep", "aria-hidden": "true" }) : null, gone.slice(0, 6).map(chip));
  }

  function view() {
    if (!S.id) return h("div", { class: "sy-wrap" }, agentsBar(), h("div", { class: "lv-card sy-err" }, h("b", {}, "Pick an agent above to read its story."), h("div", { class: "muted" }, "Its intent, a chapter per turn, the steps of each, and its terminal.")));
    if (S.error && !S.story) return h("div", { class: "sy-wrap" }, agentsBar(), h("div", { class: "lv-card sy-err" }, h("b", {}, "This story could not load."), h("div", { class: "muted" }, S.error),
      h("div", { class: "sy-row" }, h("button", { class: "btn sm", "data-key": "retry", onclick: () => load() }, "Try again"), h("button", { class: "btn sm ghost", "data-key": "back", onclick: () => ctx.go("#/mission") }, "Mission"))));
    if (!S.story) return h("div", { class: "sy-wrap" }, agentsBar(), h("div", { class: "sy-main" }, h("div", { class: "sy-skel" }), h("div", { class: "sy-skel short" }), h("div", { class: "sy-skel tall" })));
    const a = agentOf();
    // Embedded in the agent page (JOURNEYS.md, Progress): no agent picker,
    // no second header with level pills, no terminal of its own.
    const embed = !!(ctx.params && ctx.params.embed);
    return h("div", { class: `sy-wrap${embed ? " embed" : ""}` }, embed ? null : agentsBar(),
      h("div", { class: "sy-main" }, embed ? null : header(a), intentLevel(a), storyLevel(a), stepsLevel(), embed ? null : terminalLevel(a)),
      h("aside", { class: "sy-side" }, dejaVu(a), knewCard(), embed ? null : talkCard(a)));
  }

  function header(a) {
    const st = S.story, o = S.outcome;
    const tok = st.tokens ? (st.tokens.input || 0) + (st.tokens.output || 0) : null;
    const first = st.chapters[0];
    const by = first && first.prompt ? first.prompt.by : null;
    const status = statusText(a);
    return h("header", { class: "sy-head" },
      S.goal ? h("button", { class: "sy-crumb", "data-key": "crumb", onclick: () => ctx.go("#/mission") }, "← ", S.goal.text) : null,
      h("div", { class: "sy-title" },
        avatar(a, 38),
        h("div", { class: "sy-tt" },
          h("h1", {}, a.name, o ? h("span", {}, ` · ${o.title}`) : null),
          h("div", { class: "sy-sub" }, [
            a.branch ? h("span", {}, a.cwd && a.cwd !== a.repo ? "worktree " : "branch ", h("code", {}, a.branch)) : null,
            h("span", {}, `started ${fmtClock(a.created || a.started_ms)}${by ? ` by ${by}` : ""}`),
            h("span", {}, fmtDur((toMs(a.ended) || Date.now()) - (a.started_ms || toMs(a.created)))),
            tok ? h("span", { title: `${(st.tokens.input || 0).toLocaleString()} in, ${(st.tokens.output || 0).toLocaleString()} out, ${(st.tokens.cache_read || 0).toLocaleString()} read from cache` }, `${fmtTokens(tok)} tokens`) : null,
            a.model ? h("span", {}, a.model) : null,
          ].filter(Boolean))),
        h("div", { class: "sy-hr" },
          pill(status[0], status[1]),
          h("div", { class: "sy-levels", role: "tablist", "aria-label": "Level" },
            LEVELS.map(([n, label]) => h("button", { role: "tab", "aria-selected": S.level === n ? "true" : "false", class: S.level === n ? "on" : null, "data-key": `lvl:${n}`, onclick: () => setLevel(n) }, label)),
            h("span", { class: "sy-keys", title: "] goes a level deeper, toward the terminal; [ a level back up" }, h("kbd", {}, "["), h("kbd", {}, "]"))))));
  }

  function statusText(a) {
    const o = S.outcome;
    const pct = o && o.state !== "done" && o.progress ? ` · ${o.progress}%` : "";
    if (!a) return ["unknown", "mut"];
    if (a.state === "waiting") return [`waiting for you${pct}`, "warn"];
    if (a.state === "exited") return [a.exit_code ? `exited ${a.exit_code}` : "finished", a.exit_code ? "crit" : "good"];
    if (a.state === "idle") return [`turn over${pct}`, "mut"];
    if (o && o.state === "done") return ["outcome done", "good"];
    const now = S.story.chapters.find((c) => c.state === "now");
    const kind = now && now.steps.at(-1) && now.steps.at(-1).kind;
    return [`${{ run: /test/.test(now?.steps.at(-1)?.label || "") ? "testing" : "running", edit: "building", read: "exploring", search: "exploring", memory: "recalling", agents: "coordinating", web: "researching" }[kind] || "working"}${pct}`, "acc"];
  }

  function level(name, body, extra) {
    const [, label, sub] = LEVELS.find(([n]) => n === name);
    return h("section", { class: `sy-lv${S.level === name ? " on" : ""}`, "data-level": name, tabindex: -1, "aria-label": label },
      h("button", { class: "sy-lab", "data-key": `lab:${name}`, onclick: () => setLevel(name), title: `Read it at the ${label} level` }, label, h("small", {}, sub), extra || null),
      h("div", { class: "sy-body" }, body));
  }

  function intentLevel(a) {
    const st = S.story, o = S.outcome;
    // The task once (Maya saw it three times): Intent is a one-line
    // summary; the whole task is the agent page's header, or, on the
    // story's own page, folded under the summary. A narration that is only
    // "spawned: <the task>" says "Starting…" instead.
    const embed = !!(ctx.params && ctx.params.embed);
    const summary = String(st.intent || a.task || "").replace(/\s+/g, " ").trim();
    const otherTask = !embed && a.task && a.task.replace(/\s+/g, " ").trim() !== summary;
    let now = a.narration || a.waiting_for || a.last_event || "";
    if (/^spawned\b[^:]*:/.test(now)) now = a.state === "starting" ? "Starting…" : "";
    return level("intent", [
      h("div", { class: "sy-intent", title: summary }, inlineCode(summary)),
      now ? h("div", { class: "sy-now" },
        a.state === "working" ? h("i", { class: "sy-live-dot" }) : h("i", { class: `sy-dot s-${a.state}` }),
        h("span", {}, inlineCode(now),
          a.narration_ts ? h("span", { class: "muted" }, ` · ${fmtDur(Date.now() - toMs(a.narration_ts))} ago`) : null)) : null,
      o ? h("div", { class: "sy-meta" },
        h("span", {}, "outcome ", h("b", {}, o.title), S.goal ? ` of ${S.goal.text.split(":")[0]}` : ""),
        // The batch's state: how many of its parts are merged, and done.
        S.goal && S.goal.parts && S.goal.parts.total > 1
          ? h("span", { class: S.goal.parts.done ? "sy-gdone" : null }, S.goal.parts.done ? `goal done: ${S.goal.parts.words}` : S.goal.parts.words) : null) : null,
      otherTask ? h("details", { class: "sy-task" }, h("summary", { class: "lv-link" }, "Show the whole task"), h("p", {}, a.task)) : null,
    ]);
  }

  function storyLevel(a) {
    const chs = S.story.chapters;
    // A turn whose prompt is the task says so rather than quoting it again.
    const flat = (t) => String(t || "").replace(/\s+/g, " ").trim();
    const task = flat(a && a.task);
    if (!chs.length) return level("story", h("div", { class: "muted" }, "No turn yet: its first prompt has not been answered."));
    return level("story", h("div", { class: "sy-chs", style: { "--n": Math.min(chs.length, 4) } }, chs.map((c) => {
      // Now: accent. A turn of recalling and reading, no edit or command: a
      // decision (ChloeCtx's violet). Else done: good.
      const quiet = !c.steps.some((s) => s.kind === "edit" || s.kind === "run");
      const tone = c.state === "now" ? "accent" : quiet && c.steps.some((s) => s.kind === "memory") ? "ctx" : "good";
      const dur = c.state === "now" ? "now" : fmtDur((c.ended_ms || 0) - (c.started_ms || 0));
      return h("button", {
        class: `sy-ch${S.chapter === c.turn ? " on" : ""}${c.state === "now" ? " now" : ""}`, style: { "--tone": `var(--${tone})` }, "data-nav": "", "data-key": `ch:${c.turn}`,
        "aria-pressed": S.chapter === c.turn ? "true" : "false", onclick: () => { S.chapter = c.turn; S.all = false; S.level = "story"; render(); },
      },
        h("div", { class: "sy-ch-h" }, h("b", {}, `${c.turn} · ${c.title}`), h("span", {}, dur)),
        h("div", { class: "sy-ch-s" }, inlineCode(c.summary || "")),
        h("div", { class: "sy-ch-f" }, h("span", { class: "sy-by" }, c.prompt?.by === "you" ? "you" : c.prompt?.by || "", ":\u00a0"), task && flat(c.prompt?.text) === task ? h("span", { class: "sy-pr" }, "the task") : h("span", { class: "sy-pr" }, `“${c.prompt?.text || ""}”`)));
    })));
  }

  const stepKey = (c, s) => `${c.turn}:${s.started_ms}:${s.label}`;

  function stepsLevel() {
    const chs = S.story.chapters;
    const sel = chs.find((c) => c.turn === S.chapter) || chs.at(-1);
    const shown = S.all ? chs : sel ? [sel] : [];
    const total = chs.reduce((n, c) => n + c.steps.length, 0);
    const extra = h("span", { class: "sy-lab-x" }, S.all ? `${total} steps` : sel ? `chapter ${sel.turn} of ${chs.length}` : "");
    return level("steps", [
      h("div", { class: "sy-steps-h" },
        h("span", { class: "muted" }, S.all ? `All ${chs.length} chapters, ${total} steps` : sel ? [h("b", {}, `${sel.turn} · ${sel.title}`), ` · ${sel.steps.length} steps`] : ""),
        h("button", { class: "btn sm ghost", "data-key": "all", onclick: () => { S.all = !S.all; render(); } }, S.all ? "Only this chapter" : `All ${total} steps`)),
      shown.map((c) => [
        S.all ? h("div", { class: "sy-steps-ch" }, `${c.turn} · ${c.title}`) : null,
        c.steps.length ? c.steps.map((s) => step(c, s)) : h("div", { class: "muted sy-nostep" }, "No tool calls in this turn."),
      ]),
    ], extra);
  }

  function step(c, s) {
    const k = stepKey(c, s);
    const open = S.open.has(k);
    const running = s.running != null ? !!s.running && c.state === "now" : s.ms == null && c.state === "now";
    const toggle = () => { if (open) S.open.delete(k); else { S.open.add(k); loadEvents(s); } S.level = "steps"; render(); };
    return h("div", { class: `sy-stepw${open ? " open" : ""}` },
      h("button", { class: `sy-step${open ? " open" : ""}`, "data-nav": "", "data-key": `st:${k}`, "aria-expanded": open ? "true" : "false", onclick: toggle },
        h("span", { class: "sy-caret" }, open ? "▾" : "▸"),
        h("i", { class: "sy-kind", style: { background: `var(--${KIND_TONE[s.kind] || "muted"})` }, title: s.kind }),
        h("b", {}, s.label),
        h("span", { class: "sy-det" }, inlineCode(s.detail || "")),
        s.failed ? pill(`${s.failed} failed`, "crit") : null,
        s.kind === "run" ? testChip(ctx, S.id, s) : null,
        h("span", { class: `sy-ms${running ? " run" : ""}` }, running ? "running" : s.ms == null ? "—" : fmtSpan(s.ms))),
      open ? eventsOf(s) : null);
  }

  function eventsOf(s) {
    const evs = S.events.get(s.events.join(","));
    const a = agentOf();
    const started = a ? a.started_ms || toMs(a.created) : 0;
    if (evs === undefined || evs === null) return h("div", { class: "sy-evs muted" }, "Loading its events…");
    if (evs.error) return h("div", { class: "sy-evs bad" }, evs.error);
    if (!evs.length) return h("div", { class: "sy-evs muted" }, "Its events are gone from the log.");
    return h("ol", { class: "sy-evs" }, evs.map((e) => {
      const t = toMs(e.ts);
      const failed = /^failed|error/i.test(e.summary || "");
      return h("li", { class: failed ? "bad" : null },
        h("span", { class: "sy-et" }, new Date(t).toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" })),
        h("code", { class: "sy-tool" }, (e.tool || e.kind || "").replace(/^mcp__(\w+)__/, "$1·")),
        h("span", { class: "sy-es" }, e.summary || e.kind),
        started ? h("button", { class: "btn sm ghost sy-rw", "data-key": `rw:${e.seq}`, title: "Rewind its terminal to this moment", onclick: () => ctx.openRewind({ agent: S.id, from_ms: Math.max(0, t - started - 15000), to_ms: t - started + 3000 }) }, "▶ rewind") : null);
    }));
  }

  function terminalLevel(a) {
    const p = S.peek;
    const started = a.started_ms || toMs(a.created) || 0;
    return level("terminal", h("div", { class: "sy-term" },
      p && p.lines && p.lines.length ? h("div", { class: "sy-lines" }, p.lines.map((l) => h("div", {}, l.length ? l.map(([t, c]) => (c ? h("span", { class: c }, t) : t)) : " ")))
        : h("div", { class: "sy-lines muted" }, p && p.error ? p.error : p ? "Nothing on its screen in the last 30 s." : "Reading its terminal…"),
      h("div", { class: "sy-term-f" },
        h("button", { class: "sy-live", "data-nav": "", "data-key": "live", onclick: () => ctx.zoom(S.id) }, "↳ open the live terminal ", h("kbd", {}, "↵")),
        p && p.end ? h("button", { class: "sy-live", "data-key": "rw-last", onclick: () => ctx.openRewind({ agent: S.id, from_ms: Math.max(0, p.end - 60000), to_ms: p.end }) }, "▶ rewind the last minute") : null,
        h("span", { class: "sy-age" }, S.peekAt ? `as of ${fmtClock(S.peekAt)}` : ""))), null);
  }

  // ------------------------------------------------------------- side
  function dejaVu(a) {
    const ds = S.story.deja_vu || [];
    if (!ds.length) return null;
    const started = a.started_ms || toMs(a.created) || 0;
    return h("section", { class: "lv-card sy-deja" },
      h("h2", { class: "lv-h" }, "Déjà vu", ds.length > 1 ? h("span", { class: "n" }, `· ${ds.length}`) : null),
      ds.map((d, i) => i > 0 ? h("div", { class: "sy-dv more" },
        h("div", { class: "sy-dv-t" }, h("b", {}, `Also${d.when ? `, ${fmtDay(d.when)}` : ""}: `), inlineCode(d.text)),
        d.shown_ms && started ? h("button", { class: "btn sm ghost icon", "data-key": `dv:${i}`, title: `Rewind to when it was shown, at ${fmtClock(d.shown_ms)}`, "aria-label": "Rewind that moment", onclick: () => ctx.openRewind({ agent: S.id, from_ms: Math.max(0, d.shown_ms - started - 20000), to_ms: d.shown_ms - started + 25000 }) }, "▶") : null) : h("div", { class: "sy-dv" },
        h("b", {}, i === 0 ? "This was solved before." : `Also solved before${d.when ? `, ${fmtDay(d.when)}` : ""}.`),
        h("div", { class: "sy-dv-t" }, inlineCode(d.text)),
        h("div", { class: "sy-row" },
          d.shown_ms ? pill(`shown to ${a.name} at ${fmtClock(d.shown_ms)}`, "good") : null,
          d.when && i === 0 ? h("span", { class: "muted sy-when" }, `first fixed ${fmtDay(d.when)}`) : null,
          d.shown_ms && started ? h("button", { class: "btn sm", "data-key": `dv:${i}`, title: "Play its terminal around the moment it was shown the fix", onclick: () => ctx.openRewind({ agent: S.id, from_ms: Math.max(0, d.shown_ms - started - 20000), to_ms: d.shown_ms - started + 25000 }) }, "Rewind that moment ▶") : null))));
  }

  function knewCard() {
    const ks = S.story.knew || [];
    const rules = new Set(S.rules.map((r) => r.ctx_id));
    const skipped = (k) => String(k.decision || "").startsWith("skip");
    const kept = ks.filter((k) => !skipped(k)).sort((x, y) => (y.score || 0) - (x.score || 0));
    const out = ks.filter(skipped);
    const b = S.story.budget;
    const rec = (k) => {
      const kind = rules.has(k.id) ? "rule" : k.kind;
      const meta = [kind, k.by ? `${k.by}${k.ts ? `, ${fmtDay(k.ts)}` : ""}` : null, k.turn ? `turn ${k.turn}` : null].filter(Boolean).join(" · ");
      return h("li", { class: `sy-mem${skipped(k) ? " out" : ""}${k.deja_vu ? " dv" : ""}`, title: k.id },
        h("div", { class: "sy-mem-t" }, inlineCode(k.text)),
        h("div", { class: "sy-mem-m" }, meta,
          skipped(k) ? h("span", { class: "sy-left" }, ` · left out (${String(k.decision).replace(/^skip:\s*/, "")})`) : k.score != null ? h("span", { class: "sy-score" }, ` · ${k.score.toFixed(2)}`) : null,
          k.deja_vu ? h("span", { class: "sy-dvtag" }, "déjà vu") : null));
    };
    return h("section", { class: "lv-card sy-knew" },
      h("h2", { class: "lv-h" }, "What it knew"),
      h("div", { class: "muted sy-knew-s" }, "recalled by ChloeCtx into its context · receipts"),
      ks.length ? h("ul", {}, kept.map(rec), out.map(rec)) : h("div", { class: "muted" }, "ChloeCtx gave it nothing: no receipts for its session."),
      ks.length ? h("div", { class: "sy-knew-f muted" }, b ? `${b.shown ?? kept.length} of ${b.candidates} candidates · ${(b.tokens || 0).toLocaleString()} tokens of ${(b.limit || 0).toLocaleString()}` : `${kept.length} in its context · ${out.length} left out`) : null);
  }

  function aboutText() {
    const ch = S.story.chapters.find((c) => c.turn === S.chapter);
    if (S.level === "story" && ch) return `chapter ${ch.turn}, ${ch.title}`;
    if (S.level === "steps" && ch) {
      const open = ch.steps.filter((s) => S.open.has(stepKey(ch, s)));
      if (open.length) return `the step “${open.at(-1).label}”`;
      return `chapter ${ch.turn}, ${ch.title}`;
    }
    return null;
  }

  function talkCard(a) {
    // (The chapter you read is not sent with it: your words go as written.)
    const ta = h("textarea", { class: "sy-ta", rows: 2, "data-key": "talk", placeholder: STEER_HINT[S.level], "aria-label": `Steer ${a.name}` });
    import("./complete.js").then((m) => m.attachComplete(ta, { field: "steer", ctx, agent: S.id, repo: a.repo })).catch(() => {}); // Fifth build: autocomplete
    ta.value = S.talk;
    ta.addEventListener("input", () => { S.talk = ta.value; const b = ta.parentNode && ta.parentNode.querySelector('[data-key="send"]'); if (b) b.disabled = S.sending || !S.talk.trim(); });
    const send = async () => {
      const text = ta.value.trim();
      if (!text || S.sending) return;
      S.sending = true; S.sent = null; render();
      // Sixth build: a steer always says what became of it (the server's
      // message: typed, queued and why), and an answer that never comes
      // says so too.
      try {
        const r = await Promise.race([
          ctx.api(`/api/agents/${enc(S.id)}/steer`, { method: "POST", body: { text } }),
          new Promise((_, no) => setTimeout(() => no(new Error(`Workbench did not answer in 15 s; ${a.name} may not have it. Check its terminal.`)), 15000)),
        ]);
        S.talk = "";
        const msg = r && r.message ? r.message : r && r.delivered === "typed" ? `Typed into ${a.name}'s prompt.` : `Sent. ${a.name} gets it at its next tool call.`;
        S.sent = { ok: true, warn: r && r.delivered === "queued", text: msg };
      } catch (e) { S.sent = { ok: false, text: `Not sent: ${e.message}` }; }
      S.sending = false; render();
      setTimeout(() => { S.sent = null; if (alive) render(); }, S.sent && (S.sent.ok === false || S.sent.warn) ? 15000 : 6000);
    };
    ta.addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); } if (e.key === "Escape") ta.blur(); });
    return h("section", { class: "lv-card sy-talk" },
      h("h2", { class: "lv-h" }, `Tell ${a.name}`),
      // Your words go exactly as written: no hidden "(About chapter …)" in front.
      h("div", { class: "sy-tabox" }, ta, h("button", { class: "btn sm primary", "data-key": "send", disabled: S.sending || !S.talk.trim(), onclick: send }, S.sending ? "Sending…" : "Send")),
      S.sent ? h("div", { class: `sy-sent ${S.sent.ok ? (S.sent.warn ? "warn" : "ok") : "bad"}`, role: "status" }, S.sent.ok && !S.sent.warn ? "✓ " : "", S.sent.text)
        : h("div", { class: "muted sy-hint" }, "Goes in as a steer, not a keystroke: a working agent gets it at its next tool call, an idle one as its next prompt."));
  }

  // ------------------------------------------------------------- life
  const offKeys = keyNav(el, {
    onKey: (e) => {
      // [ and ] come through the shell's levelKey (below), as on every screen.
      if (e.key === "t" && S.story) { setLevel("terminal"); return true; }
      return false;
    },
  });
  // A click in a level makes it the level being read (for the steer box).
  el.addEventListener("focusin", (e) => {
    const sec = e.target.closest && e.target.closest("[data-level]");
    if (sec && sec.dataset.level !== S.level) { S.level = sec.dataset.level; S.about = true; renderSoon(); }
  });
  render();
  load();

  return {
    /** The top bar's [ (-1, up toward Intent) and ] (+1, down toward the
     *  terminal) move through this agent's own levels first. */
    levelKey(d) { move(d); return true; },
    update(state) {
      if (!state) return;
      S.stream = state.agents || S.stream;
      const a = (S.stream || []).find((x) => x.id === S.id);
      const sig = a ? `${a.state}:${a.last_event}:${a.outcome}` : "";
      if (sig !== S.seen) { if (S.seen) { reloadSoon(); if (S.level === "terminal") peekSoon(); } S.seen = sig; }
      renderSoon();
    },
    unmount() {
      alive = false;
      offKeys(); renderSoon.cancel(); reloadSoon.cancel(); peekSoon.cancel();
      el.remove();
    },
  };
}

// The text of a terminal's output, as lines of [text, class] runs: colours
// (SGR) kept as theme classes, other escapes dropped, a lone \r rewrites the
// line. Its last 9 lines with something on them.
const FG = { 31: "t-red", 32: "t-grn", 33: "t-yel", 34: "t-blu", 35: "t-vio", 36: "t-cya", 90: "t-dim", 91: "t-red", 92: "t-grn", 93: "t-yel", 94: "t-blu", 95: "t-vio", 96: "t-cya" };
export function screenLines(s) {
  s = s.replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "").replace(/\x1b[()][A-Z0-9]/g, "");
  const lines = [[]];
  let fg = null, bold = false, dim = false;
  const re = /\x1b\[([0-9;?]*)([A-Za-z])|\r\n|\n|\r|[^\x1b\r\n]+|\x1b/g;
  let m;
  while ((m = re.exec(s))) {
    const t = m[0];
    if (m[2] === "m") {
      for (const p of (m[1] || "0").split(";").map(Number)) {
        if (p === 0) { fg = null; bold = false; dim = false; } else if (p === 1) bold = true; else if (p === 2) dim = true; else if (p === 22) { bold = false; dim = false; } else if (p === 39) fg = null; else if (FG[p]) fg = FG[p];
      }
    } else if (m[2] === "J" && m[1] === "2") { lines.length = 0; lines.push([]); }
    else if (m[2]) continue;
    else if (t === "\n" || t === "\r\n") lines.push([]);
    else if (t === "\r") lines[lines.length - 1] = [];
    else if (t !== "\x1b") lines[lines.length - 1].push([t, [fg || (dim ? "t-dim" : null), bold ? "t-b" : null].filter(Boolean).join(" ")]);
  }
  return lines.filter((l) => l.some(([t]) => t.trim())).slice(-9);
}

useStyle("story", `
.sy-pick{grid-column:1/-1;display:flex;align-items:center;gap:6px;flex-wrap:wrap;min-width:0}
.sy-pick-k{color:var(--muted);font-size:11.5px;font-weight:650;text-transform:uppercase;letter-spacing:.06em;margin-right:4px}
.sy-pick-a{display:inline-flex;align-items:center;gap:6px;height:30px;padding:0 10px 0 5px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);font:inherit;font-size:12.5px;cursor:pointer}
.sy-pick-a:hover{border-color:var(--line-2);color:var(--text)}
.sy-pick-a.on{border-color:var(--accent);background:var(--accent-soft);color:var(--text)}
.sy-pick-a b{font-weight:650;color:var(--text)}
.sy-pick-a.gone{opacity:.7}
.sy-pick-s{font-size:11px;color:var(--muted)}
.sy-pick-s.s-waiting{color:var(--warn-ink);font-weight:600}.sy-pick-s.s-working{color:var(--good-ink)}
.sy-pick-sep{width:1px;height:18px;background:var(--line-2);margin:0 4px}
.sy-wrap{display:grid;grid-template-columns:minmax(0,1fr) 368px;gap:22px;padding:18px 22px 32px;max-width:1880px;margin:0 auto;align-items:start}
.sy-main{min-width:0}
.sy-head{padding-bottom:6px}
.sy-crumb{border:0;background:none;padding:0;margin-bottom:10px;color:var(--muted);font:inherit;font-size:12.5px;cursor:pointer;max-width:100%;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.sy-crumb:hover{color:var(--text)}
.sy-title{display:flex;align-items:center;gap:12px}
.sy-title>.lv-av{border-radius:10px;font-size:15px}
.sy-tt{min-width:0;flex:1}
.sy-tt h1{margin:0;font-size:19px;font-weight:680;letter-spacing:-.01em;line-height:1.25}
.sy-tt h1 span{font-weight:600}
.sy-sub{display:flex;flex-wrap:wrap;color:var(--muted);margin-top:3px;font-size:13px}
.sy-sub>span+span::before{content:"·";margin:0 7px}
.sy-sub code{font-family:var(--mono);font-size:12px;color:var(--text-2)}
.sy-hr{display:flex;align-items:center;gap:12px;flex:none}
.sy-levels{display:flex;align-items:center;gap:2px;background:var(--sunken);border:1px solid var(--line);border-radius:999px;padding:3px}
.sy-levels button{border:0;background:none;color:var(--muted);font:inherit;font-size:12px;font-weight:600;padding:3px 11px;border-radius:999px;cursor:pointer}
.sy-levels button:hover{color:var(--text)}
.sy-levels button.on{background:var(--accent-soft);color:var(--accent);box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--accent) 35%,transparent)}
.sy-keys{display:inline-flex;gap:3px;margin:0 4px 0 6px}
.sy-lv{display:grid;grid-template-columns:118px minmax(0,1fr);gap:16px;align-items:start;padding:15px 0 15px;border-top:1px dashed var(--line);position:relative;outline:none}
.sy-lv::before{content:"";position:absolute;left:-12px;top:15px;bottom:15px;width:3px;border-radius:3px;background:transparent;transition:background .15s}
.sy-lv.on::before{background:var(--accent)}
.sy-lab{border:0;background:none;padding:3px 0 0;text-align:left;font:inherit;font-size:11px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);cursor:pointer;align-self:start}
.sy-lv.on .sy-lab{color:var(--accent)}
.sy-lab small{display:block;font-weight:500;text-transform:none;letter-spacing:0;font-size:11.5px;margin-top:3px;color:var(--muted)}
.sy-lab-x{display:block;font-weight:500;text-transform:none;letter-spacing:0;font-size:11.5px;margin-top:6px;color:var(--muted)}
.sy-body{min-width:0}
.sy-intent{font-size:16.5px;font-weight:620;line-height:1.4;letter-spacing:-.005em;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.sy-task{margin-top:6px;font-size:12.5px;color:var(--text-2)}.sy-task>summary{cursor:pointer;list-style:none}.sy-task>summary::-webkit-details-marker{display:none}.sy-task p{margin:4px 0 0;white-space:pre-wrap;overflow-wrap:anywhere}
.sy-now{display:flex;align-items:baseline;gap:7px;margin-top:8px;color:var(--text-2)}
.sy-live-dot,.sy-dot{width:7px;height:7px;border-radius:50%;flex:none;background:var(--good);align-self:center}
.sy-live-dot{animation:sy-live 1.8s ease-out infinite}
@keyframes sy-live{0%{box-shadow:0 0 0 0 color-mix(in srgb,var(--good) 60%,transparent)}100%{box-shadow:0 0 0 6px transparent}}
.sy-dot.s-waiting{background:var(--warn)}.sy-dot.s-idle,.sy-dot.s-exited{background:var(--muted)}
.sy-meta{display:flex;flex-wrap:wrap;gap:4px 16px;margin-top:6px;color:var(--muted);font-size:12.5px}
.sy-meta b{color:var(--text-2);font-weight:600}
.sy-gdone{color:var(--good);font-weight:600}
.sy-chs{display:grid;grid-template-columns:repeat(var(--n),minmax(0,1fr));gap:10px}
.sy-ch{display:flex;flex-direction:column;text-align:left;font:inherit;color:inherit;background:var(--surface);border:1px solid var(--line);border-top:3px solid var(--tone);border-radius:10px;padding:10px 12px 11px;cursor:pointer;min-width:0;box-shadow:var(--shadow-1);transition:border-color .12s,box-shadow .12s}
.sy-ch:hover{border-color:var(--line-2);border-top-color:var(--tone)}
.sy-ch.on{border-color:color-mix(in srgb,var(--accent) 55%,var(--line));border-top-color:var(--tone);box-shadow:0 0 0 3px var(--focus)}
.sy-ch-h{display:flex;justify-content:space-between;gap:8px;align-items:baseline}
.sy-ch-h b{font-size:13.5px;font-weight:650}
.sy-ch-h span{color:var(--muted);font-size:12px;white-space:nowrap;font-variant-numeric:tabular-nums}
.sy-ch.now .sy-ch-h span{color:var(--accent);font-weight:600}
.sy-ch-s{color:var(--text-2);margin-top:6px;line-height:1.45;flex:1}
.sy-ch-f{display:flex;gap:0;margin-top:9px;font-size:11.5px;color:var(--muted);min-width:0;white-space:nowrap}
.sy-by{font-weight:600;flex:none}.sy-pr{overflow:hidden;text-overflow:ellipsis;min-width:0}
.sy-steps-h{display:flex;justify-content:space-between;align-items:center;gap:10px;margin:-4px 0 8px}
.sy-steps-h b{color:var(--text);font-weight:600}
.sy-steps-ch{font-size:11px;font-weight:700;letter-spacing:.05em;text-transform:uppercase;color:var(--muted);margin:12px 0 6px}
.sy-stepw{border:1px solid var(--line);border-radius:9px;background:var(--surface);margin-bottom:6px;overflow:hidden}
.sy-stepw.open{border-color:var(--line-2)}
.sy-step{display:flex;align-items:center;gap:9px;width:100%;padding:8px 12px;border:0;background:none;font:inherit;color:inherit;text-align:left;cursor:pointer;min-width:0}
.sy-step:hover{background:var(--surface-2)}
.sy-caret{color:var(--muted);font-size:11px;width:9px;flex:none}
.sy-kind{width:8px;height:8px;border-radius:2px;flex:none}
.sy-step b{font-weight:620;white-space:nowrap;flex:none}
.sy-det{flex:1;min-width:0;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;margin-left:4px}
.sy-ms{font:12px var(--mono);color:var(--muted);white-space:nowrap;flex:none;font-variant-numeric:tabular-nums}
.sy-ms.run{color:var(--accent);font-family:var(--sans);font-weight:600}
.sy-ms.run::before{content:"";display:inline-block;width:6px;height:6px;border-radius:50%;background:currentColor;margin-right:6px;vertical-align:middle;animation:pulse 1.2s ease-in-out infinite}
.sy-evs{list-style:none;margin:0;padding:4px 12px 8px 39px;border-top:1px solid var(--line);background:var(--surface-2);font-size:12.5px}
.sy-evs li{display:flex;align-items:center;gap:10px;padding:3px 0;min-width:0}
.sy-evs li.bad .sy-es{color:var(--crit-ink)}
.sy-evs.muted,.sy-evs.bad{padding:8px 12px 8px 39px}.sy-evs.bad{color:var(--crit-ink)}
.sy-et{color:var(--muted);font-variant-numeric:tabular-nums;width:82px;flex:none;font-size:12px}
.sy-tool{font:11px var(--mono);color:var(--text-2);background:var(--surface-3);border-radius:4px;padding:1px 6px;flex:none;max-width:200px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.sy-es{flex:1;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;color:var(--text-2)}
.sy-rw{height:22px;padding:0 7px;font-size:11.5px;opacity:0;transition:opacity .1s}
.sy-evs li:hover .sy-rw,.sy-rw:focus-visible{opacity:1}
.sy-nostep{padding:6px 0}
.sy-term{background:var(--term-bg);border:1px solid var(--term-line);border-radius:10px;padding:11px 14px 8px;font:12px/1.6 var(--mono);color:var(--term-fg);overflow:hidden}
.sy-lines>div{white-space:pre;overflow:hidden;text-overflow:ellipsis}
.sy-lines.muted{color:var(--muted);font-family:var(--sans)}
.t-red{color:var(--crit-ink)}.t-grn{color:var(--good-ink)}.t-yel{color:var(--warn-ink)}.t-blu{color:var(--accent)}.t-vio{color:var(--ctx)}.t-cya{color:var(--lane-3)}.t-dim{color:var(--muted)}.t-b{font-weight:700}
.sy-term-f{display:flex;align-items:center;gap:14px;margin-top:6px;padding-top:6px;border-top:1px solid var(--term-line);flex-wrap:wrap}
.sy-live{border:0;background:none;padding:2px 0;color:var(--muted);font:inherit;cursor:pointer;display:inline-flex;align-items:center;gap:6px}
.sy-live:hover{color:var(--accent)}
.sy-live kbd{height:16px;min-width:16px;font-size:10px}
.sy-age{margin-left:auto;color:var(--muted);font-family:var(--sans);font-size:11.5px}
.sy-side{display:flex;flex-direction:column;gap:12px;min-width:0}
.sy-side .lv-h{margin-bottom:10px}
.sy-deja{border-color:color-mix(in srgb,var(--warn) 45%,var(--line));background:linear-gradient(180deg,var(--warn-soft),var(--surface) 75%)}
.sy-deja>.lv-h{color:var(--warn-ink)}
.sy-dv+.sy-dv{margin-top:12px;padding-top:12px;border-top:1px solid color-mix(in srgb,var(--warn) 25%,var(--line))}
.sy-dv b{font-weight:650}
.sy-dv.more{display:flex;gap:8px;align-items:flex-start}.sy-dv.more .sy-dv-t{margin:0;flex:1;font-size:12.5px}.sy-dv.more .btn{flex:none;margin-top:-3px}
.sy-dv-t{color:var(--text-2);margin-top:4px;line-height:1.5}
.sy-row{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-top:10px}
.sy-when{font-size:12px}
.sy-knew>.lv-h{color:var(--ctx);margin-bottom:2px}
.sy-knew-s{font-size:12.5px;margin-bottom:12px}
.sy-knew ul{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:11px}
.sy-mem{border-left:2px solid var(--ctx);padding:1px 0 1px 11px}
.sy-mem-t{font-weight:620;line-height:1.4;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.sy-mem-m{color:var(--muted);font-size:12.5px;margin-top:2px}
.sy-score{font-variant-numeric:tabular-nums}
.sy-mem.out{border-left-color:var(--line-2)}.sy-mem.out .sy-mem-t{color:var(--muted);font-weight:560}
.sy-left{color:var(--text-2)}
.sy-dvtag{display:inline-flex;margin-left:7px;font-size:10.5px;font-weight:700;color:var(--warn-ink);background:var(--warn-soft);border-radius:999px;padding:0 7px;vertical-align:1px}
.sy-knew-f{margin-top:13px;padding-top:10px;border-top:1px solid var(--line);font-size:12.5px;font-variant-numeric:tabular-nums}
.sy-about{display:flex;align-items:center;justify-content:space-between;gap:8px;background:var(--accent-soft);color:var(--accent);border-radius:7px;padding:4px 6px 4px 10px;font-size:12px;margin-bottom:8px}
.sy-about b{font-weight:600}
.sy-x{border:0;background:none;color:inherit;font-size:15px;line-height:1;cursor:pointer;padding:0 4px;border-radius:4px}
.sy-x:hover{background:color-mix(in srgb,var(--accent) 15%,transparent)}
.sy-tabox{display:flex;gap:8px;align-items:flex-end;border:1px solid var(--line-2);border-radius:9px;background:var(--surface-2);padding:7px 7px 7px 11px}
.sy-tabox:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.sy-ta{flex:1;border:0;background:none;resize:none;outline:none;color:var(--text);font:inherit;line-height:1.45;min-height:40px;padding:2px 0}
.sy-hint{font-size:12.5px;margin-top:8px;line-height:1.45}
.sy-sent{font-size:12.5px;font-weight:600;margin-top:8px;border-radius:7px;padding:6px 10px}
.sy-sent.ok{background:var(--good-soft);color:var(--good-ink)}.sy-sent.bad{background:var(--crit-soft);color:var(--crit-ink)}.sy-sent.warn{background:var(--warn-soft);color:var(--warn-ink)}
.sy-err{max-width:560px;display:flex;flex-direction:column;gap:8px;align-items:flex-start}
.sy-skel{height:30px;width:50%;border-radius:8px;background:var(--surface-3);margin:6px 0 14px;animation:pulse 1.4s ease-in-out infinite}.sy-skel.short{height:16px;width:30%}.sy-skel.tall{height:140px;width:100%}
@media (max-width:1180px){.sy-wrap{grid-template-columns:minmax(0,1fr)}.sy-side{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));align-items:start}}
@media (max-width:900px){.sy-chs{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media (max-width:640px){
  .sy-wrap{padding:14px 14px 24px}
  .sy-title{flex-wrap:wrap}.sy-hr{flex-direction:row;align-items:center;flex-wrap:wrap;width:100%;justify-content:space-between}
  .sy-keys{display:none}
  .sy-lv{grid-template-columns:minmax(0,1fr);gap:8px}
  .sy-lab small{display:inline;margin-left:8px}
  .sy-lab-x{display:none}
  .sy-chs{grid-template-columns:minmax(0,1fr)}
  .sy-det{display:none}
  .sy-side{grid-template-columns:minmax(0,1fr)}
  .sy-et{width:auto}
  .sy-rw{opacity:1}
}
`);
