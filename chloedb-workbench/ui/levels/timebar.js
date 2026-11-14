// Time travel (SPEC.md, "Fourth build", row 8): a scrubber above Mission and
// Map. Its ticks are the snapshots Workbench kept of its state database
// (every 10 minutes and at each goal change, for a day) over the events per
// minute; a click or a drag picks a moment, and the screen is mounted again
// on what `GET /api/time?at=` answers for it: what the fleet knew then, with
// each worktree as it was. "Live" goes back.
//
// A screen uses it by wrapping its mount:
//   export const mount = timeTravel(mountMission, { screen: "mission" });
// The wrapped screen gets a ctx whose api answers its GETs from the moment
// picked (goals, a goal, questions, rules, the map, agents) and refuses any
// change, and whose state is the fleet then; the app's live updates wait
// until Live.
import { h, useStyle, toMs, fmtClock, fmtDur, repoOf, onEscape, tooltip, ctxChip } from "./common.js";

const enc = encodeURIComponent;
const RANGES = [["1 h", 3600e3], ["3 h", 3 * 3600e3], ["12 h", 12 * 3600e3], ["24 h", 24 * 3600e3]];
const RANGE_KEY = "wb.time.range";
const shortSha = (s) => (s ? String(s).slice(0, 7) : "");

function store(k, v) {
  try { if (v === undefined) return localStorage.getItem(k); if (v === null) localStorage.removeItem(k); else localStorage.setItem(k, v); } catch {}
  return null;
}

/** Answers a screen's GET from a moment's data, as the live routes would. */
function pastApi(ctx, past) {
  const when = fmtClock(past.at_ms);
  const m = past.mission || {};
  return async (path, opts = {}) => {
    const method = (opts.method || "GET").toUpperCase();
    if (method !== "GET") throw Object.assign(new Error(`You are looking at ${when}. Go back to Live to change anything.`), { status: 409 });
    const u = new URL(path, location.origin);
    const p = u.pathname;
    if (p === "/api/goals") return { goals: m.goals || [] };
    const g = p.match(/^\/api\/goals\/([^/]+)$/);
    if (g) {
      const id = decodeURIComponent(g[1]);
      if (m.details && m.details[id]) return m.details[id];
      if (m.detail && m.detail.goal && m.detail.goal.id === id) return m.detail;
      const goal = (m.goals || []).find((x) => x.id === id);
      if (goal) return { goal, outcomes: [], agents: [], open_questions: 0, learned: [], heads_up: [], pulse: [], missing: [] };
      throw Object.assign(new Error(`There was no goal ${id} at ${when}.`), { status: 404 });
    }
    if (p === "/api/questions") {
      const st = u.searchParams.get("state");
      return { questions: (m.questions || []).filter((q) => !st || q.state === st) };
    }
    if (p === "/api/rules") return { rules: m.rules || [] };
    if (p === "/api/outcomes") return { outcomes: (m.detail && m.detail.outcomes) || [] };
    if (p === "/api/map") {
      if (past.map) return past.map;
      throw Object.assign(new Error((past.missing || [])[0] || `The map of ${when} was not kept.`), { status: 404 });
    }
    if (p === "/api/agents") return { agents: past.agents || [] };
    const a = p.match(/^\/api\/agents\/([^/]+)$/);
    if (a) {
      const one = (past.agents || []).find((x) => x.id === decodeURIComponent(a[1]));
      if (one) return { agent: one };
    }
    // A story, a diff, a recording: what an agent did is the same then and
    // now, as far as it had got; but its rows are today's.
    if (/^\/api\/agents\/[^/]+\/(story|events)$/.test(p)) throw Object.assign(new Error(`Not kept as of ${when}.`), { status: 404 });
    return ctx.api(path, opts);
  };
}

export function timeTravel(mountScreen, { screen = "" } = {}) {
  return function mount(root, ctx) {
    const S = {
      repo: repoOf(ctx), line: null, err: null, at: null, past: null, loading: false, range: Number(store(RANGE_KEY)) || 3 * 3600e3,
      live: ctx.state, hover: null, drag: false, open: false,
    };
    const frame = h("div", { class: "tt-frame" });
    const bar = h("div", { class: "tt-bar", role: "group", "aria-label": "Time travel" });
    const host = h("div", { class: "tt-host" });
    frame.append(bar, host);
    root.append(frame);
    const tip = tooltip();
    let view = null, alive = true, dropEsc = null, req = 0, timer = null;

    function mountView() {
      try { view?.unmount(); } catch (e) { console.error(e); }
      host.replaceChildren();
      const c = S.past
        ? { ...ctx, api: pastApi(ctx, S.past), state: { agents: S.past.agents || [], events: [] }, params: { ...(ctx.params || {}), repo: S.past.repo || S.repo }, past: { at_ms: S.past.at_ms } }
        : ctx;
      view = mountScreen(host, c);
    }

    async function loadLine() {
      // Sixth build: no repository yet is the screen's picker, not an error here.
      if (!S.repo) { S.line = null; S.err = "No repository yet: open one (Map, Library or search) to travel in its time."; if (alive) draw(); return; }
      try {
        S.line = await ctx.api(`/api/time${S.repo ? `?repo=${enc(S.repo)}` : ""}`);
        S.err = null;
      } catch (e) {
        S.line = null;
        S.err = e.status === 404 ? "This server has no time travel." : e.message || String(e);
      }
      if (alive) draw();
    }

    async function go(at) {
      if (at == null) {
        S.at = null; S.past = null; S.loading = false;
        dropEsc?.(); dropEsc = null;
        mountView(); draw();
        return;
      }
      const line = S.line;
      const now = Date.now();
      // A minute is as fine as anyone reads a clock.
      at = Math.round(Math.min(now, Math.max(line ? line.from_ms : now - 864e5, at)) / 60e3) * 60e3;
      if (now - at < 30e3) return go(null);
      S.at = at; S.loading = true; draw();
      const n = ++req;
      try {
        const past = await ctx.api(`/api/time?at=${Math.round(at)}${S.repo ? `&repo=${enc(S.repo)}` : ""}`);
        if (n !== req || !alive) return;
        if (past.available === false) { S.err = past.why || "Nothing was kept for then."; S.at = null; S.past = null; S.loading = false; draw(); return; }
        S.past = past; S.loading = false;
        if (!dropEsc) dropEsc = onEscape(() => go(null));
        mountView(); draw();
      } catch (e) {
        if (n !== req) return;
        S.loading = false; S.err = e.message || String(e); draw();
      }
    }

    // ------------------------------------------------------------- the strip
    function span() {
      const now = Date.now();
      const from = Math.max(S.line ? S.line.from_ms : now - S.range, now - S.range);
      return { from: Math.min(from, S.at != null ? S.at - S.range * 0.1 : from), to: now };
    }

    function draw() {
      if (!alive) return;
      bar.classList.toggle("past", S.at != null);
      if (!S.line) {
        bar.replaceChildren(h("div", { class: "tt-row" }, h("span", { class: "tt-k" }, clockIcon(), "Time"), h("span", { class: "tt-off" }, S.err || "Loading the timeline…")));
        return;
      }
      if (S.line.available === false) {
        bar.replaceChildren(h("div", { class: "tt-row" }, h("span", { class: "tt-k" }, clockIcon(), "Time"), h("span", { class: "tt-off" }, S.line.why || "Time travel is not available.")));
        return;
      }
      const { from, to } = span();
      const x = (t) => ((t - from) / (to - from)) * 100;
      const snaps = (S.line.snapshots || []).filter((s) => s.ts_ms >= from && s.ts_ms <= to);
      const evs = (S.line.events || []).filter(([t]) => t >= from && t <= to);
      // Events in buckets of a width that keeps about 120 bars.
      const step = Math.max(60e3, Math.ceil((to - from) / 120 / 60e3) * 60e3);
      const buckets = new Map();
      for (const [t, c] of evs) { const b = Math.floor(t / step) * step; buckets.set(b, (buckets.get(b) || 0) + c); }
      const max = Math.max(1, ...buckets.values());
      const hours = [];
      const hr = to - from > 6 * 3600e3 ? 3 * 3600e3 : to - from > 2 * 3600e3 ? 3600e3 : 30 * 60e3;
      for (let t = Math.ceil(from / hr) * hr; t < to; t += hr) hours.push(t);

      const track = h("div", { class: "tt-track", tabindex: "0", role: "slider", "aria-label": "Moment to look at", "aria-valuemin": from, "aria-valuemax": to, "aria-valuenow": S.at ?? to, "aria-valuetext": S.at != null ? `as of ${fmtClock(S.at)}` : "live" },
        h("div", { class: "tt-hist" }, [...buckets].map(([t, c]) => h("i", { style: { left: x(t) + "%", width: Math.max(0.35, (step / (to - from)) * 100 - 0.12) + "%", height: Math.max(8, (c / max) * 100) + "%" } }))),
        h("div", { class: "tt-snaps" }, snaps.map((s) => h("b", { class: "tt-snap" + (s.reason === "goal" ? " goal" : s.reason === "manual" ? " man" : ""), style: { left: x(s.ts_ms) + "%" } }))),
        h("div", { class: "tt-hours" }, hours.map((t) => h("span", { style: { left: x(t) + "%" } }, fmtClock(t)))),
        S.at != null ? h("div", { class: "tt-shade", style: { left: x(S.at) + "%" } }) : null,
        h("div", { class: "tt-handle" + (S.at == null ? " live" : ""), style: { left: (S.at != null ? x(S.at) : 100) + "%" } }, h("span", {}, S.at != null ? fmtClock(S.at) : "now")));
      const pick = (e) => {
        const r = track.getBoundingClientRect();
        return from + Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)) * (to - from);
      };
      const near = (t) => snaps.reduce((b, s) => (!b || Math.abs(s.ts_ms - t) < Math.abs(b.ts_ms - t) ? s : b), null);
      track.addEventListener("pointerdown", (e) => { S.drag = true; track.setPointerCapture(e.pointerId); moveHandle(track, x(pick(e)), pick(e)); });
      track.addEventListener("pointermove", (e) => {
        const t = pick(e);
        if (S.drag) { moveHandle(track, x(t), t); return; }
        const s = near(t);
        const close = s && Math.abs(x(s.ts_ms) - x(t)) < 0.8;
        tip.show([h("b", {}, fmtClock(t)), h("div", { class: "tt-tipsub" }, close ? (s.detail ? `snapshot: ${s.detail}` : "snapshot") : `${fmtDur(Date.now() - t)} ago`)], e.clientX, e.clientY);
      });
      track.addEventListener("pointerleave", () => tip.hide());
      track.addEventListener("pointerup", (e) => { if (!S.drag) return; S.drag = false; tip.hide(); go(pick(e)); });
      track.addEventListener("keydown", (e) => {
        const all = S.line.snapshots || [];
        const cur = S.at ?? Date.now();
        if (e.key === "ArrowLeft") { e.preventDefault(); const s = [...all].reverse().find((s) => s.ts_ms < cur - 1000); if (s) go(s.ts_ms); }
        else if (e.key === "ArrowRight") { e.preventDefault(); const s = all.find((s) => s.ts_ms > cur + 1000); go(s ? s.ts_ms : null); }
        else if (e.key === "End" || e.key === "l") { e.preventDefault(); go(null); }
      });

      const kept = (S.line.snapshots || []).length;
      const ranges = h("div", { class: "tt-ranges", role: "group", "aria-label": "How far back" }, RANGES.map(([label, ms]) =>
        h("button", { class: "tt-rng" + (S.range === ms ? " on" : ""), "aria-pressed": String(S.range === ms), onclick: () => { S.range = ms; store(RANGE_KEY, String(ms)); draw(); } }, label)));
      const live = h("button", { class: "tt-live" + (S.at == null ? " on" : ""), onclick: () => go(null), title: S.at == null ? "Showing now" : "Back to now (Esc)" }, h("i"), "Live");
      const row = h("div", { class: "tt-row" },
        h("span", { class: "tt-k", title: `${kept} snapshots kept, the oldest ${fmtDur(Date.now() - (S.line.from_ms || Date.now()))} ago` }, clockIcon(), "Time"),
        track, ranges, live);
      bar.replaceChildren(...[row, S.at != null ? banner() : null].filter(Boolean));
    }

    function moveHandle(track, pct, t) {
      const hd = track.querySelector(".tt-handle");
      hd.style.left = pct + "%"; hd.classList.remove("live");
      hd.firstChild.textContent = fmtClock(t);
    }

    function banner() {
      const p = S.past;
      const at = S.at;
      const s = p && p.snapshot;
      const off = s ? Math.round((s.ts_ms - at) / 60e3) : 0;
      const heads = (p && p.heads) || [];
      return h("div", { class: "tt-banner", role: "status" },
        h("span", { class: "tt-asof" }, "As of ", h("b", {}, fmtClock(at))),
        S.loading ? h("span", { class: "tt-dim" }, "opening the snapshot…") : [
          h("span", { class: "tt-dim" }, screen === "map" ? "each worktree as it was" : "what the fleet knew then"),
          s ? h("span", { class: "tt-meta", title: s.detail || "" }, `snapshot ${fmtClock(s.ts_ms)}${off ? ` (${Math.abs(off)} min ${off > 0 ? "after" : "before"})` : ""}${s.detail ? ` · ${s.detail}` : ""}`) : null,
          heads.length ? h("span", { class: "tt-heads" }, heads.slice(0, 5).map((x) => h("span", { class: "tt-head", title: `${x.branch || ""} HEAD ${x.head || "?"}${x.dirty ? `, its uncommitted changes kept as ${x.tree}` : ""}` }, x.name, " ", h("code", {}, shortSha(x.dirty ? x.tree : x.head)), x.dirty ? h("i", { title: "with uncommitted changes" }, "+") : null)), heads.length > 5 ? h("span", { class: "tt-dim" }, `+${heads.length - 5}`) : null) : null,
          (screen !== "map" && p && p.mission && p.mission.detail && (p.mission.detail.learned || []).length) ? ctxChip(`${p.mission.detail.learned.length} records known then`, "ChloeCtx's records up to that moment") : null,
        ],
        h("span", { class: "tt-sp" }),
        h("span", { class: "tt-ro" }, "read-only"),
        h("button", { class: "btn sm tt-back", onclick: () => go(null) }, "Back to live"));
    }

    // ------------------------------------------------------------- life
    // A link from another screen (Trace's "Mission then") rides as
    // `#/mission?at=<ms>`: open on that moment once the timeline is known.
    const linked = (() => {
      const i = location.hash.indexOf("?");
      const at = Number(new URLSearchParams(i >= 0 ? location.hash.slice(i + 1) : "").get("at"));
      return Number.isFinite(at) && at > 0 ? at : null;
    })();
    mountView();
    draw();
    loadLine().then(() => { if (alive && linked != null) go(linked); });
    timer = setInterval(() => { if (S.at == null) loadLine(); else draw(); }, 60e3);

    return {
      update(state) {
        S.live = state;
        if (S.at == null) view?.update?.(state);
      },
      unmount() {
        alive = false;
        clearInterval(timer);
        dropEsc?.();
        tip.remove();
        try { view?.unmount(); } catch (e) { console.error(e); }
        frame.remove();
      },
    };
  };
}

function clockIcon() {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 16 16"); s.setAttribute("class", "tt-ic"); s.setAttribute("aria-hidden", "true");
  s.innerHTML = '<circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M8 4.6V8l2.4 1.6" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>';
  return s;
}

useStyle("timebar", `
.tt-frame{position:absolute;inset:0;display:flex;flex-direction:column}
.tt-host{position:relative;flex:1;min-height:0}
.tt-bar{flex:none;border-bottom:1px solid var(--line);background:var(--surface);padding:7px 22px 6px}
.tt-bar.past{background:color-mix(in srgb,var(--warn) 7%,var(--surface));border-bottom-color:color-mix(in srgb,var(--warn) 45%,var(--line))}
.tt-row{display:flex;align-items:center;gap:14px;min-height:34px}
.tt-k{display:inline-flex;align-items:center;gap:6px;font-size:11.5px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);flex:none}
.tt-ic{width:15px;height:15px}
.tt-off{color:var(--muted);font-size:12.5px}
.tt-track{position:relative;flex:1;min-width:120px;height:34px;cursor:pointer;touch-action:none;border-radius:6px;outline:none}
.tt-track:focus-visible{box-shadow:0 0 0 3px var(--focus)}
.tt-hist{position:absolute;left:0;right:0;top:3px;height:17px}
.tt-hist>i{position:absolute;bottom:0;background:var(--accent);opacity:.32;border-radius:1.5px 1.5px 0 0}
.tt-snaps{position:absolute;left:0;right:0;top:21px;height:6px;border-top:1px solid var(--line-2)}
.tt-snap{position:absolute;top:-1px;width:1.5px;height:6px;margin-left:-.75px;background:var(--text-2);opacity:.55}
.tt-snap.goal{width:7px;height:7px;margin-left:-3.5px;top:-4px;border-radius:2px;transform:rotate(45deg);background:var(--accent);opacity:1}
.tt-snap.man{background:var(--good);opacity:1;height:8px}
.tt-hours{position:absolute;left:0;right:0;top:25px;height:10px;font-size:10px;color:var(--muted);font-variant-numeric:tabular-nums}
.tt-hours>span{position:absolute;transform:translateX(-50%);white-space:nowrap}
.tt-shade{position:absolute;top:0;bottom:12px;right:0;background:repeating-linear-gradient(135deg,color-mix(in srgb,var(--warn) 16%,transparent) 0 4px,transparent 4px 8px);border-left:0;pointer-events:none;border-radius:0 4px 4px 0}
.tt-handle{position:absolute;top:-2px;bottom:10px;width:2px;margin-left:-1px;background:var(--warn-line);pointer-events:none}
.tt-handle.live{background:var(--good)}
.tt-handle>span{position:absolute;top:-1px;left:50%;transform:translate(-50%,-100%);display:none}
.tt-bar.past .tt-handle::after{content:"";position:absolute;top:-3px;left:-5px;width:12px;height:12px;border-radius:50%;background:var(--warn-line);box-shadow:0 0 0 3px var(--surface)}
.tt-handle.live::after{content:"";position:absolute;top:-3px;left:-4px;width:10px;height:10px;border-radius:50%;background:var(--good);box-shadow:0 0 0 3px var(--surface)}
.tt-ranges{display:flex;gap:2px;background:var(--surface-3);border-radius:8px;padding:2px;flex:none}
.tt-rng{border:0;background:none;color:var(--muted);font:inherit;font-size:11.5px;font-weight:600;padding:3px 8px;border-radius:6px;cursor:pointer}
.tt-rng.on{background:var(--surface);color:var(--text);box-shadow:var(--shadow-1)}
.tt-live{display:inline-flex;align-items:center;gap:7px;height:28px;padding:0 12px;border-radius:999px;border:1px solid var(--line-2);background:var(--surface);color:var(--text-2);font:inherit;font-size:12.5px;font-weight:650;cursor:pointer;flex:none}
.tt-live>i{width:7px;height:7px;border-radius:50%;background:var(--muted)}
.tt-live.on{border-color:color-mix(in srgb,var(--good) 55%,var(--line));color:var(--good-ink);background:var(--good-soft)}
.tt-live.on>i{background:var(--good);box-shadow:0 0 0 0 var(--good);animation:tt-pulse 1.8s ease-out infinite}
.tt-live:not(.on):hover{border-color:var(--good);color:var(--text)}
@keyframes tt-pulse{0%{box-shadow:0 0 0 0 color-mix(in srgb,var(--good) 60%,transparent)}100%{box-shadow:0 0 0 7px transparent}}
.tt-banner{display:flex;align-items:center;flex-wrap:wrap;gap:6px 12px;margin:5px 0 2px;font-size:12.5px;color:var(--text-2)}
.tt-asof{font-size:13.5px;color:var(--warn-ink);font-weight:600}
.tt-asof b{font-size:15px;font-weight:750;font-variant-numeric:tabular-nums}
.tt-dim{color:var(--muted)}
.tt-meta{color:var(--text-2);font-variant-numeric:tabular-nums}
.tt-heads{display:inline-flex;flex-wrap:wrap;gap:5px}
.tt-head{display:inline-flex;align-items:center;gap:3px;border:1px solid var(--line);background:var(--surface);border-radius:6px;padding:1px 7px;font-size:11.5px;color:var(--text-2)}
.tt-head code{font-family:var(--mono);font-size:11px;color:var(--text)}
.tt-head i{font-style:normal;color:var(--warn-ink);font-weight:700}
.tt-sp{flex:1}
.tt-ro{font-size:11px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--warn-ink);border:1px solid color-mix(in srgb,var(--warn) 50%,var(--line));border-radius:999px;padding:1px 8px}
.tt-tipsub{color:var(--muted);font-size:11.5px}
@media (max-width:760px){.tt-bar{padding:6px 14px}.tt-ranges{display:none}.tt-heads{display:none}}
`);
