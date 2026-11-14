// Flow (SPEC.md, "Levels", Flow): where the fleet's time went in a window,
// from `GET /api/flow`. Three numbers (waiting on you, redo, tokens), a bar
// per agent split six ways and scaled to its own time, a timeline of each
// agent's stretches, and the coach: suggestions with Apply (POST
// /api/flow/apply), ChloeCtx's report's own folded in.
import {
  h, useStyle, avatar, ctxChip, pill, empty, fmtDur, fmtClock, fmtNum, toMs, tooltip, repoOf, throttle, card, md, setFleet,
} from "./common.js";

export const KINDS = [
  { k: "model", label: "model", color: "var(--lane-1)", note: "the model thinking and writing" },
  { k: "tools", label: "tools", color: "var(--lane-3)", note: "tools running" },
  { k: "tests", label: "tests", color: "var(--lane-4)", note: "a test runner" },
  { k: "waiting", label: "waiting on you", color: "var(--warn)", note: "a question or a permission prompt" },
  { k: "redo", label: "redo", color: "var(--crit)", note: "a tool call failing with an error seen before" },
  { k: "idle", label: "idle", color: "var(--line-2)", note: "turn over, waiting for a prompt" },
];
const KIND = Object.fromEntries(KINDS.map((x) => [x.k, x]));
const WINDOWS = [{ label: "15 min", m: 15 }, { label: "1 h", m: 60 }, { label: "4 h", m: 240 }, { label: "today", m: "today" }];
const nameOf = (a) => (a && (a.name || a.id)) || "?";
const pct = (x, total) => (total ? Math.round((100 * x) / total) : 0);

// Which report suggestions a coach suggestion already says.
const COVERS = { allow: "permission", fix: "error_loop", capsule: "reread" };

export function mount(root, ctx) {
  const S = { win: 60, data: null, err: null, loading: false, applied: new Set(), busy: new Set(), fixOpen: null, fixText: "", errs: new Map(), hidden: new Set() };
  try { const w = localStorage.getItem("wb-flow-window"); if (w) S.win = w === "today" ? "today" : Number(w) || 60; } catch {}
  const tip = tooltip();
  const el = h("div", { class: "lv-screen fl" });
  root.append(el);
  let alive = true;

  const minutes = () => (S.win === "today" ? Math.max(1, Math.ceil((Date.now() - new Date().setHours(0, 0, 0, 0)) / 60000)) : S.win);

  async function load() {
    S.loading = true;
    try {
      const repo = repoOf(ctx);
      const q = new URLSearchParams({ minutes: String(minutes()) });
      if (repo) q.set("repo", repo);
      S.data = await ctx.api(`/api/flow?${q}`);
      S.err = null;
      for (const s of S.data.suggestions || []) if (s.applied) S.applied.add(s.id);
      setFleet([...(ctx.state?.agents || []), ...(S.data.agents || []).map((r) => r.agent)].filter((a, i, all) => all.findIndex((b) => b.id === a.id) === i));
    } catch (e) { S.err = e.message || String(e); }
    S.loading = false;
    if (alive) render();
  }
  const refresh = throttle(load, 15000);

  // --------------------------------------------------------------- render
  function render() {
    tip.hide();
    const head = h("header", { class: "lv-head fl-head" },
      h("h1", { class: "lv-h1" }, "Flow"),
      h("span", { class: "lv-sub" }, `where the ${S.win === "today" ? "day" : "last " + windowLabel()} went · from Workbench's events and ChloeCtx's report, nothing sent anywhere`),
      h("div", { class: "fl-win", role: "radiogroup", "aria-label": "Window" }, WINDOWS.map((w) => h("button", {
        role: "radio", "aria-checked": S.win === w.m ? "true" : "false", class: S.win === w.m ? "on" : "",
        onclick: () => { S.win = w.m; try { localStorage.setItem("wb-flow-window", String(w.m)); } catch {} load(); render(); },
        onkeydown: (e) => {
          const i = WINDOWS.indexOf(w), d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
          if (d) { e.preventDefault(); const n = WINDOWS[(i + d + WINDOWS.length) % WINDOWS.length]; S.win = n.m; load(); render(); requestAnimationFrame(() => el.querySelector(".fl-win .on")?.focus()); }
        },
      }, w.label))),
      S.loading && S.data ? h("span", { class: "spin", "aria-label": "Loading" }) : null);
    if (!S.data) { el.replaceChildren(head, S.err ? empty(`Flow could not load: ${S.err}`) : empty("Adding up where the time went…")); return; }
    const d = S.data;
    el.replaceChildren(...[head,
      S.err ? h("div", { class: "err" }, S.err) : null,
      h("div", { class: "fl-grid" },
        h("div", { class: "fl-main" }, bigNumbers(d), perAgent(d), timeline(d)),
        coach(d))].filter(Boolean));
  }
  function windowLabel() { const w = WINDOWS.find((x) => x.m === S.win); return S.win === 60 ? "60 minutes" : S.win === 15 ? "15 minutes" : S.win === 240 ? "4 hours" : w ? w.label : ""; }

  function totalsOf(o) { return KINDS.reduce((s, x) => s + (o[x.k + "_ms"] || 0), 0); }

  function bigNumbers(d) {
    const t = d.totals || {};
    const all = totalsOf(t);
    const waitingAgents = (d.agents || []).filter((r) => r.waiting_ms > 0).length;
    const tok = d.tokens || {};
    const tokens = (tok.input || 0) + (tok.output || 0) + (tok.cache_read || 0);
    return h("section", { class: "lv-card fl-big" },
      h("div", { class: "fl-nums" },
        h("div", { class: "fl-num" }, h("div", { class: "fl-n fl-warn" }, `${pct(t.waiting_ms, all)}%`),
          h("div", { class: "fl-l" }, "of the fleet's time was ", h("b", null, "waiting on you")),
          h("div", { class: "fl-s" }, `${fmtDur(t.waiting_ms)} across ${waitingAgents} agent${waitingAgents === 1 ? "" : "s"}`)),
        h("div", { class: "fl-num" }, h("div", { class: "fl-n fl-crit" }, `${pct(t.redo_ms, all)}%`),
          h("div", { class: "fl-l" }, "was ", h("b", null, "redo"), ": the same error, again"),
          h("div", { class: "fl-s" }, `${fmtDur(t.redo_ms)} of failed calls seen before`)),
        h("div", { class: "fl-num" }, h("div", { class: "fl-n fl-acc" }, fmtNum(tokens)),
          h("div", { class: "fl-l" }, h("b", null, "tokens"), ` in ${S.win === "today" ? "the day" : windowLabel()}`),
          h("div", { class: "fl-s" }, `${fmtNum(tok.input)} in · ${fmtNum(tok.output)} out · ${fmtNum(tok.cache_read)} from cache`))),
      h("div", { class: "fl-fleet", role: "img", "aria-label": KINDS.map((x) => `${x.label} ${pct(t[x.k + "_ms"], all)}%`).join(", ") },
        stack(t, all, (x) => [h("b", null, `Fleet · ${x.label}`), h("div", null, `${fmtDur(t[x.k + "_ms"])}, ${pct(t[x.k + "_ms"], all)}% of ${fmtDur(all)}`), h("div", { class: "lv-sub" }, x.note)])));
  }

  // One stacked bar; each part sized by its share, with a tooltip.
  function stack(o, total, tipFor, extraClass = "") {
    return h("div", { class: "fl-stack " + extraClass },
      KINDS.filter((x) => o[x.k + "_ms"] > 0).map((x) => {
        const b = h("b", { class: `fl-seg k-${x.k}`, style: `flex:${o[x.k + "_ms"]} 1 0;background:${x.color}` });
        b.addEventListener("mousemove", (e) => tip.show(tipFor(x), e.clientX, e.clientY));
        b.addEventListener("mouseleave", () => tip.hide());
        return b;
      }));
  }

  function legend(t) {
    return h("div", { class: "fl-legend" }, KINDS.map((x) => h("span", { class: "fl-key", title: x.note },
      h("i", { class: `k-${x.k}`, style: `background:${x.color}` }), x.label, t ? h("span", { class: "fl-key-v" }, fmtDur(t[x.k + "_ms"])) : null)));
  }

  function perAgent(d) {
    const rows = [...(d.agents || [])].sort((a, b) => totalsOf(b) - totalsOf(a));
    const max = Math.max(1, ...rows.map(totalsOf));
    return h("section", { class: "lv-card" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Per agent"), legend(d.totals)),
      rows.length ? h("div", { class: "fl-rows" }, rows.map((r) => {
        const tot = totalsOf(r);
        const breakdown = () => [h("div", { class: "fl-tt" }, avatar(r.agent, 16), h("b", null, nameOf(r.agent)), h("span", { class: "lv-sub" }, fmtDur(tot))),
          h("table", { class: "fl-tbl" }, KINDS.filter((x) => r[x.k + "_ms"]).map((x) => h("tr", null,
            h("td", null, h("i", { class: `fl-dot k-${x.k}`, style: `background:${x.color}` }), x.label), h("td", null, fmtDur(r[x.k + "_ms"])), h("td", null, `${pct(r[x.k + "_ms"], tot)}%`))))];
        const row = h("div", { class: "fl-row", tabindex: "0", "aria-label": `${nameOf(r.agent)}: ${KINDS.map((x) => `${x.label} ${fmtDur(r[x.k + "_ms"]) || "0 s"}`).join(", ")}` },
          avatar(r.agent, 24),
          h("button", { class: "fl-name", onclick: () => ctx.go(`#/story/${r.agent.id}`), title: `${nameOf(r.agent)}'s story` }, nameOf(r.agent)),
          h("div", { class: "fl-track" }, h("div", { style: `width:${Math.max(1.5, (100 * tot) / max)}%` },
            stack(r, tot, (x) => [h("div", { class: "fl-tt" }, avatar(r.agent, 16), h("b", null, `${nameOf(r.agent)} · ${x.label}`)),
              h("div", null, `${fmtDur(r[x.k + "_ms"])}, ${pct(r[x.k + "_ms"], tot)}% of its ${fmtDur(tot)}`)]))),
          h("span", { class: "fl-tot" }, fmtDur(tot)));
        row.addEventListener("focus", () => { const b = row.getBoundingClientRect(); tip.show(breakdown(), b.left + 140, b.bottom - 8); });
        row.addEventListener("blur", () => tip.hide());
        return row;
      })) : h("div", { class: "lv-sub" }, "No agent ran in this window."));
  }

  function timeline(d) {
    const from = toMs(d.window?.from_ms), to = toMs(d.window?.to_ms);
    const span = Math.max(1, to - from);
    const rows = [...(d.agents || [])].sort((a, b) => toMs(a.agent.created) - toMs(b.agent.created) || totalsOf(b) - totalsOf(a));
    const x = (t) => (100 * (t - from)) / span;
    // Ticks every 5, 10, 15, 30 or 60 minutes, so there are about five.
    const step = [5, 10, 15, 30, 60, 120].map((m) => m * 60000).find((s) => span / s <= 6) || 240 * 60000;
    const ticks = [];
    for (let t = Math.ceil(from / step) * step; t < to - step * 0.3; t += step) ticks.push(t);
    const cross = h("div", { class: "fl-cross", hidden: true });
    const lanes = h("div", { class: "fl-lanes" },
      h("div", { class: "fl-gridw", "aria-hidden": "true" }, ticks.map((t) => h("i", { style: `left:${x(t)}%` }))),
      rows.map((r) => {
        const segs = r.segments || [];
        const track = h("div", { class: "fl-lt", tabindex: "0", role: "group", "aria-label": `${nameOf(r.agent)}'s timeline: ${segs.length} stretches. Arrows walk them, Enter rewinds one.` },
          segs.map(([s, e, k], i) => h("i", {
            class: `fl-ls k-${k}`, "data-i": i,
            style: `left:${x(Math.max(s, from))}%;width:${Math.max(0.15, x(Math.min(e, to)) - x(Math.max(s, from)))}%;background:${(KIND[k] || KIND.idle).color}`,
          })));
        const show = (i, px, py) => {
          const [s, e, k] = segs[i];
          track.querySelectorAll(".fl-ls.on").forEach((n) => n.classList.remove("on"));
          track.querySelector(`[data-i="${i}"]`)?.classList.add("on");
          tip.show([h("div", { class: "fl-tt" }, avatar(r.agent, 16), h("b", null, nameOf(r.agent)), h("span", { class: "fl-tk", style: `background:${(KIND[k] || KIND.idle).color}` }), (KIND[k] || { label: k }).label),
            h("div", null, `${fmtClock(s)} – ${fmtClock(e)} · ${fmtDur(e - s)}`),
            h("div", { class: "lv-sub" }, "Click to rewind its terminal here")], px, py);
        };
        let cur = -1;
        track.addEventListener("mousemove", (e) => {
          const t = e.target.closest(".fl-ls");
          if (t) show(+t.dataset.i, e.clientX, e.clientY); else { tip.hide(); track.querySelectorAll(".fl-ls.on").forEach((n) => n.classList.remove("on")); }
        });
        track.addEventListener("mouseleave", () => { tip.hide(); track.querySelectorAll(".fl-ls.on").forEach((n) => n.classList.remove("on")); });
        track.addEventListener("click", (e) => { const t = e.target.closest(".fl-ls"); if (t) rewind(r.agent, segs[+t.dataset.i]); });
        track.addEventListener("keydown", (e) => {
          if (!segs.length) return;
          if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
            e.preventDefault();
            cur = cur < 0 ? (e.key === "ArrowRight" ? 0 : segs.length - 1) : Math.max(0, Math.min(segs.length - 1, cur + (e.key === "ArrowRight" ? 1 : -1)));
            const n = track.querySelector(`[data-i="${cur}"]`).getBoundingClientRect();
            show(cur, n.left + n.width / 2, n.bottom);
          } else if (e.key === "Enter" && cur >= 0) rewind(r.agent, segs[cur]);
        });
        track.addEventListener("blur", () => { tip.hide(); cur = -1; track.querySelectorAll(".fl-ls.on").forEach((n) => n.classList.remove("on")); });
        return h("div", { class: "fl-lane" }, h("span", { class: "fl-ln" }, avatar(r.agent, 16), nameOf(r.agent)), track);
      }),
      cross);
    lanes.addEventListener("mousemove", (e) => {
      const lt = lanes.querySelector(".fl-lt");
      if (!lt) return;
      const a = lt.getBoundingClientRect(), b = lanes.getBoundingClientRect();
      if (e.clientX < a.left || e.clientX > a.right) { cross.hidden = true; return; }
      cross.hidden = false;
      cross.style.left = e.clientX - b.left + "px";
      cross.dataset.t = fmtClock(from + ((e.clientX - a.left) / a.width) * span);
    });
    lanes.addEventListener("mouseleave", () => { cross.hidden = true; });
    const axis = h("div", { class: "fl-axis" }, h("span", { class: "fl-ax-pad" }),
      h("div", { class: "fl-ax" }, h("span", { style: "left:0" }, fmtClock(from)), ticks.filter((t) => x(t) > 8 && x(t) < 92).map((t) => h("span", { style: `left:${x(t)}%`, class: "mid" }, fmtClock(t))), h("span", { style: "right:0", class: "end" }, "now")));
    return h("section", { class: "lv-card fl-tl" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Timeline"), h("span", { class: "lv-sub" }, "hover a stretch for its time · click to rewind it")),
      rows.length ? [lanes, axis] : h("div", { class: "lv-sub" }, "Nothing in this window."));
  }

  function rewind(agent, seg) {
    const [s, e] = seg;
    const st = toMs(agent.started_ms ?? agent.created);
    if (!st) return;
    const to = e - st, from = Math.max(0, Math.max(s, e - 120000) - st);
    tip.hide();
    ctx.openRewind({ agent: agent.id, from_ms: from, to_ms: to });
  }

  // ----------------------------------------------------------------- coach
  function actionLabel(s) {
    const a = s.action || {};
    if (a.label) return a.label;
    switch (a.kind) {
      case "allow": return "Allow for this repo";
      case "fix": return "Save as a fix";
      case "sequence": return a.path ? `Sequence ${a.path}` : "Run them in sequence";
      default: return "Apply";
    }
  }

  async function apply(s, text) {
    S.busy.add(s.id); S.errs.delete(s.id); render();
    try {
      const body = { id: s.id, repo: S.data?.repo || repoOf(ctx), minutes: S.data?.window?.minutes || minutes() };
      if (text != null) body.text = text;
      await ctx.api("/api/flow/apply", { method: "POST", body });
      S.applied.add(s.id);
      if (S.fixOpen === s.id) { S.fixOpen = null; S.fixText = ""; }
    } catch (e) { S.errs.set(s.id, e.message || String(e)); }
    S.busy.delete(s.id);
    render();
  }

  function coach(d) {
    // The server folds the report's own suggestions in (`from: "chloectx"`,
    // no action); they go under the report's heading. When it does not, the
    // report's are read from `report` itself.
    const all = d.suggestions || [];
    const sugg = all.filter((s) => s.from !== "chloectx");
    const kinds = new Set(sugg.map((s) => s.action?.kind).filter(Boolean));
    const said = sugg.map((s) => `${s.title} ${s.detail}`).join(" ");
    const folded = all.filter((s) => s.from === "chloectx").map((s) => ({ kind: s.kind, text: s.title, saves: s.saves, seconds: s.saves_ms != null ? s.saves_ms / 1000 : null, harness: s.harness || null }));
    const extra = (folded.length ? folded : d.report?.suggestions || []).filter((r) => ![...kinds].some((k) => COVERS[k] === r.kind) && !said.includes(String(r.text).slice(0, 40)));
    // A report's advice that one of ours covers keeps its harness link: it
    // goes beside ours (ours applies to this repo's agents, the link drafts
    // the harness change for every session).
    const hxOf = (kind) => (folded.find((r) => COVERS[kind] === r.kind && r.harness && r.harness.href) || {}).harness || null;
    const hxLink = (hx) => h("a", { class: "btn sm fl-hx", href: hx.href, title: hx.allow && hx.allow.length ? `Opens Optimize the harness › Permissions with ${hx.allow.join(", ")} in a draft` : "Opens Optimize the harness › Permissions" }, hx.label || "Make this a harness change");
    const items = sugg.map((s, i) => {
      const a = s.action, done = S.applied.has(s.id), busy = S.busy.has(s.id);
      // Only an allow and a fix have something to switch on (SPEC.md, Flow).
      const can = a && (a.kind === "allow" || a.kind === "fix");
      let act = null;
      if (done) act = pill(a?.kind === "allow" ? "Allowed for agents started from now" : "Applied", "good");
      else if (can && a.kind === "fix" && S.fixOpen === s.id) {
        const ta = h("textarea", { class: "fl-fix", rows: "2", placeholder: "What fixes it, in a sentence the fleet will get on that error", "aria-label": "The fix" }, S.fixText);
        import("./complete.js").then((m) => m.attachComplete(ta, { field: "steer", ctx })).catch(() => {}); // Fifth build: autocomplete
        ta.addEventListener("input", () => { S.fixText = ta.value; });
        ta.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && ta.value.trim()) apply(s, ta.value.trim()); if (e.key === "Escape") { e.stopPropagation(); S.fixOpen = null; render(); } });
        requestAnimationFrame(() => ta.focus());
        act = h("div", { class: "fl-fixw" }, a.error ? h("code", { class: "fl-err" }, a.error) : null, ta,
          h("div", { class: "fl-acts" }, h("button", { class: "btn sm primary", disabled: busy ? true : null, onclick: () => ta.value.trim() && apply(s, ta.value.trim()) }, busy ? "Saving…" : "Save the fix"),
            h("button", { class: "btn sm", onclick: () => { S.fixOpen = null; render(); } }, "Cancel")));
      } else if (can) {
        act = h("button", { class: "btn sm primary", disabled: busy ? true : null, onclick: () => (a.kind === "fix" ? (S.fixOpen = s.id, render()) : apply(s)) }, busy ? "Applying…" : actionLabel(s));
      } else act = h("span", { class: "fl-how" }, a?.kind === "sequence" ? "By hand: give the file to one agent, or start the next when the first is done" : a?.kind === "capsule" ? "By hand: nothing to switch on here" : "By hand: there is nothing to switch on");
      return h("li", { class: "fl-sg" + (done ? " done" : "") },
        h("span", { class: "fl-sn" }, done ? "✓" : String(i + 1)),
        h("div", { class: "fl-sb" },
          h("div", { class: "fl-st" }, md(s.title)),
          s.detail ? h("div", { class: "fl-sd" }, md(s.detail)) : null,
          h("div", { class: "fl-acts" }, act, a && !done && hxOf(a.kind) ? hxLink(hxOf(a.kind)) : null, s.saves && s.saves_ms !== 0 && !/~0 s\b/.test(s.saves) && !(S.fixOpen === s.id) ? h("span", { class: "fl-saves" }, /^(saves|bears)\b/.test(s.saves) ? s.saves : `saves ${s.saves}`) : null),
          S.errs.has(s.id) ? h("div", { class: "err" }, S.errs.get(s.id)) : null));
    });
    return h("aside", { class: "lv-card fl-coach" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2 fl-acc" }, `Coach · ${sugg.length + extra.length} suggestion${sugg.length + extra.length === 1 ? "" : "s"}`)),
      items.length ? h("ol", { class: "fl-sl" }, items) : h("div", { class: "lv-sub" }, "Nothing to suggest in this window: no repeated approvals, errors or rereads."),
      extra.length ? h("div", { class: "fl-rep" },
        h("div", { class: "fl-rep-h" }, ctxChip("From ChloeCtx's report of the day"), d.report ? h("span", { class: "lv-sub" }, `${d.report.turns} turns · ${d.report.tool_calls} tool calls · ${d.report.compactions} compactions`) : null),
        h("ul", { class: "fl-rl" }, extra.map((r) => h("li", null, h("div", { class: "fl-sd" }, md(r.text)),
          // ChloeCtx says to edit settings.json by hand; here it is a harness draft.
          r.harness && r.harness.href ? hxLink(r.harness) : null,
          r.seconds ? h("span", { class: "fl-saves" }, `bears on ~${fmtDur(r.seconds * 1000)}`) : null)))) : null,
      h("p", { class: "fl-foot" }, "Each suggestion is a query over the session records; applying one changes only what agents started from now on get."));
  }

  load();
  render();
  return {
    update(state) { if (state) ctx.state = { ...ctx.state, ...state }; refresh(); },
    unmount() { alive = false; refresh.cancel(); tip.remove(); el.remove(); },
  };
}

useStyle("flow", `
.fl{display:flex;flex-direction:column;gap:16px}
.fl .lv-card-head{flex-wrap:wrap}
.fl-head .fl-win{margin-left:auto}
.fl-win{display:flex;gap:2px;padding:3px;border-radius:999px;background:var(--sunken);border:1px solid var(--line);align-self:center}
.fl-win button{border:0;background:none;border-radius:999px;padding:4px 12px;font:600 12.5px var(--sans);color:var(--muted);cursor:pointer}
.fl-win button:hover{color:var(--text)}
.fl-win button.on{background:var(--accent-soft);color:var(--accent);box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--accent) 35%,transparent)}
.fl-grid{display:grid;grid-template-columns:minmax(0,1fr) 470px;gap:18px;align-items:start}
.fl-main{display:flex;flex-direction:column;gap:14px;min-width:0}
.fl-big{padding:20px 22px 18px}
.fl-nums{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:20px}
.fl-n{font-size:46px;font-weight:750;letter-spacing:-.025em;line-height:1.05;font-variant-numeric:tabular-nums}
.fl-warn{color:var(--warn)}.fl-crit{color:var(--crit)}.fl-acc{color:var(--accent)}
.fl-l{margin-top:6px;color:var(--text-2);font-size:13.5px}.fl-l b{color:var(--text)}
.fl-s{margin-top:2px;color:var(--muted);font-size:12px}
.fl-fleet{margin-top:16px}
.fl-stack{display:flex;gap:2px;height:14px;width:100%;border-radius:5px;overflow:hidden}
.fl-fleet .fl-stack{height:10px}
.fl-seg{display:block;height:100%;min-width:2px}
.fl-seg:hover{filter:brightness(1.12)}
.k-idle{background-image:repeating-linear-gradient(135deg,transparent 0 3px,color-mix(in srgb,var(--muted) 35%,transparent) 3px 4.5px)!important}
.fl-legend{display:flex;flex-wrap:wrap;gap:4px 14px;justify-content:flex-end}
.fl-key{display:inline-flex;align-items:center;gap:6px;color:var(--text-2);font-size:12.5px;white-space:nowrap}
.fl-key i{width:10px;height:10px;border-radius:3px;display:inline-block}
.fl-key-v{color:var(--muted);font-size:11.5px;font-variant-numeric:tabular-nums}
.fl-rows{display:flex;flex-direction:column;gap:4px;margin-top:4px}
.fl-row{display:grid;grid-template-columns:24px 104px minmax(0,1fr) 60px;align-items:center;gap:10px;padding:5px 4px;border-radius:8px;outline:none}
.fl-row:hover,.fl-row:focus-visible{background:var(--surface-2)}
.fl-row:focus-visible{box-shadow:0 0 0 2px var(--focus)}
.fl-name{border:0;background:none;padding:0;color:var(--text-2);font:inherit;font-size:13.5px;text-align:left;cursor:pointer;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.fl-name:hover{color:var(--text);text-decoration:underline}
.fl-track{min-width:0}
.fl-row .fl-stack{height:16px}
.fl-tot{font:12px var(--mono);color:var(--muted);text-align:right}
.fl-tt{display:flex;align-items:center;gap:6px;margin-bottom:3px}
.fl-tk{width:9px;height:9px;border-radius:3px;display:inline-block;margin-left:4px}
.fl-tbl{border-collapse:collapse;margin-top:4px;font-size:12px}.fl-tbl td{padding:1px 10px 1px 0;font-variant-numeric:tabular-nums}.fl-tbl td:first-child{display:flex;align-items:center;gap:6px}
.fl-dot{width:8px;height:8px;border-radius:2px;display:inline-block}
.fl-tl{--lab:110px;--lg:10px}
.fl-lanes{position:relative;display:flex;flex-direction:column;gap:9px;padding:4px 0}
.fl-lane{display:grid;grid-template-columns:var(--lab) minmax(0,1fr);align-items:center;gap:var(--lg)}
.fl-ln{display:flex;align-items:center;gap:7px;color:var(--text-2);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;font-size:12.5px}
.fl-lt{position:relative;height:16px;border-radius:4px;background:var(--sunken);outline:none;cursor:pointer}
.fl-lt:focus-visible{box-shadow:0 0 0 2px var(--focus)}
.fl-ls{position:absolute;top:0;bottom:0;border-right:1.5px solid var(--surface);border-radius:2px}
.fl-ls.on{top:-3px;bottom:-3px;box-shadow:0 0 0 2px var(--text);z-index:1;border-radius:3px}
.fl-gridw{position:absolute;top:-2px;bottom:-2px;left:calc(var(--lab) + var(--lg));right:0;pointer-events:none;z-index:1}
.fl-gridw i{position:absolute;top:0;bottom:0;width:0;border-left:1px dashed color-mix(in srgb,var(--text-2) 30%,transparent)}
.fl-cross{position:absolute;top:-4px;bottom:-4px;width:1px;background:var(--text-2);pointer-events:none}
.fl-cross::after{content:attr(data-t);position:absolute;top:-16px;left:4px;font:600 10.5px var(--sans);color:var(--text-2);white-space:nowrap}
.fl-axis{display:grid;grid-template-columns:var(--lab) minmax(0,1fr);gap:var(--lg);margin-top:6px}
.fl-ax{position:relative;height:16px;color:var(--muted);font-size:11.5px}
.fl-ax span{position:absolute;top:0;white-space:nowrap}.fl-ax span.mid{transform:translateX(-50%)}
.fl-coach{padding:18px 20px}
.fl-rl .fl-hx{margin:6px 8px 0 0}
.fl-sl{list-style:none;margin:0;padding:0}
.fl-sg{display:flex;gap:12px;padding:14px 0;border-top:1px solid var(--line)}
.fl-sg:first-child{border-top:0;padding-top:4px}
.fl-sn{width:26px;height:26px;border-radius:50%;background:var(--surface-3);display:grid;place-items:center;font-weight:700;flex:none;font-size:12.5px}
.fl-sg.done .fl-sn{background:var(--good-soft);color:var(--good-ink)}
.fl-sb{min-width:0;flex:1}
.fl-st{font-weight:650;font-size:14px}
.fl-sd{margin-top:3px;color:var(--text-2)}
.fl-acts{display:flex;align-items:center;flex-wrap:wrap;gap:8px;margin-top:9px}
.fl-saves{display:inline-flex;align-items:center;border-radius:999px;padding:2px 9px;font-size:11.5px;font-weight:650;background:var(--good-soft);color:var(--good-ink);white-space:nowrap}
.fl-how{color:var(--muted);font-size:12px}
.fl-fixw{display:flex;flex-direction:column;gap:6px;width:100%}
.fl-fix{width:100%;border:1px solid var(--line-2);border-radius:8px;background:var(--sunken);padding:7px 9px;resize:vertical;outline:none}
.fl-fix:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.fl-err{font:11.5px var(--mono);color:var(--crit-ink);background:var(--crit-soft);border-radius:6px;padding:4px 7px;overflow-wrap:anywhere}
.fl-fixw .fl-acts{margin-top:0}
.fl-rep{margin-top:6px;padding-top:12px;border-top:1px solid var(--line)}
.fl-rep-h{display:flex;flex-direction:column;gap:2px}
.fl-rl{margin:8px 0 0;padding:0 0 0 2px;list-style:none;display:flex;flex-direction:column;gap:10px}
.fl-rl li{display:flex;flex-direction:column;align-items:flex-start;gap:5px;padding-left:12px;border-left:2px solid color-mix(in srgb,var(--ctx) 45%,transparent)}
.fl-rl .fl-saves{background:var(--surface-3);color:var(--text-2)}
.fl-foot{margin:14px 0 0;color:var(--muted);font-size:12px}
@media (max-width:1240px){.fl-grid{grid-template-columns:minmax(0,1fr) 380px}.fl-n{font-size:38px}}
@media (max-width:980px){.fl-grid{grid-template-columns:1fr}}
@media (max-width:640px){.fl-nums{grid-template-columns:1fr;gap:14px}.fl-n{font-size:34px}.fl-row{grid-template-columns:22px 76px minmax(0,1fr) 48px;gap:7px}.fl-tl{--lab:76px;--lg:7px}.fl-head .fl-win{margin-left:0}.fl-legend{justify-content:flex-start}}
`);
