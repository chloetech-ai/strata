// Shared pieces of the level screens (SPEC.md, "Levels (third build)").
// Each screen is an ES module in this folder exporting
//   mount(root, ctx) -> { update(state), unmount() }
// where ctx = { api(path, opts), state: {agents, events}, go(hash),
// openWhy(args), openRewind(args), zoom(agentId) }.
// No build step and no dependencies: plain DOM, styles injected once per
// module with useStyle(), colours only from the theme's tokens (style.css).

export function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k === "class") el.className = v;
    else if (k === "style" && typeof v === "object") for (const [p, x] of Object.entries(v)) { if (p.startsWith("--")) el.style.setProperty(p, x); else el.style[p] = x; }
    else if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v);
    else if (k === "html") el.innerHTML = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid == null || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

const styled = new Set();
/** Adds a module's CSS to the page once. */
export function useStyle(id, css) {
  if (styled.has(id)) return;
  styled.add(id);
  document.head.append(h("style", { "data-level": id }, css));
}

export const toMs = (t) => (t == null ? null : typeof t === "number" ? t : Date.parse(t));

export function fmtDur(ms) {
  if (ms == null || !isFinite(ms)) return "";
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s} s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min`;
  if (m < 48 * 60) return `${Math.floor(m / 60)} h ${m % 60} min`;
  return `${Math.round(m / 1440)} days`;
}

export function fmtClock(t) {
  const ms = toMs(t);
  return ms ? new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) : "";
}

export function fmtAgo(t) {
  const ms = toMs(t);
  return ms ? fmtDur(Date.now() - ms) + " ago" : "";
}

/** A stable hue per agent name, so an agent keeps its colour on every screen. */
const HUES = [222, 266, 176, 330, 40, 150, 12, 200];
function hashHue(a) {
  const key = (a && (a.name || a.id)) || "?";
  let x = 0;
  for (const c of key) x = (x * 31 + c.charCodeAt(0)) >>> 0;
  return HUES[x % HUES.length];
}
const fleetHues = new Map();
export function agentHue(a) {
  return (a && fleetHues.get(a.id)) ?? hashHue(a);
}
/** Makes the agents seen together tell apart: oldest first, each keeps its
 *  own hue unless an older one has one within 20°, and then takes the middle
 *  of the widest free gap (never the reds, which mean trouble). Any screen
 *  may call it with the fleet; the colours stay the same everywhere. */
export function setFleet(agents) {
  const list = [...(agents || [])].sort((x, y) => (toMs(x.created) || 0) - (toMs(y.created) || 0) || String(x.id).localeCompare(String(y.id)));
  const dist = (p, q) => { const d = Math.abs(p - q) % 360; return Math.min(d, 360 - d); };
  const taken = [];
  fleetHues.clear();
  for (const a of list) {
    let hue = hashHue(a);
    if (taken.some((t) => dist(t, hue) < 20)) {
      let best = hue, gap = -1;
      for (let c = 30; c <= 340; c += 5) {
        const g = Math.min(...taken.map((t) => dist(t, c)));
        if (g > gap) { gap = g; best = c; }
      }
      hue = best;
    }
    taken.push(hue);
    fleetHues.set(a.id, hue);
  }
}
export function agentColor(a) { return `hsl(${agentHue(a)} 80% 68%)`; }

/** The square avatar with the agent's initial; `you` for the user. */
export function avatar(a, size = 24) {
  if (a === "you") return h("span", { class: "lv-av lv-you", style: { width: size + "px", height: size + "px" } }, "you");
  const name = (a && (a.name || a.id)) || "?";
  return h("span", { class: "lv-av", title: name, style: { width: size + "px", height: size + "px", background: agentColor(a) } }, name[0].toUpperCase());
}

/** Avatar plus name, clickable to the agent's story. */
export function agentChip(a, ctx) {
  const name = (a && (a.name || a.id)) || "?";
  return h("button", { class: "lv-agent", onclick: (e) => { e.stopPropagation(); ctx && ctx.go(`#/story/${a.id}`); } }, avatar(a, 18), name);
}

/** The ◆ chip that marks something ChloeCtx remembered or decided. */
export function ctxChip(text, title) { return h("span", { class: "lv-ctx", title: title || "from ChloeCtx" }, text); }

export function pill(text, tone = "mut") { return h("span", { class: `lv-pill lv-${tone}` }, text); }

export function bar(pct, tone = "accent") {
  return h("div", { class: "lv-bar" }, h("b", { style: { width: Math.max(0, Math.min(100, pct || 0)) + "%", background: `var(--${tone})` } }));
}

export function empty(text) { return h("div", { class: "lv-empty" }, text); }

useStyle("common", `
.lv-av{display:inline-grid;place-items:center;border-radius:7px;font-weight:var(--w-title);font-size:var(--fs-xs);color:var(--av-ink);flex:none}
.lv-you{background:var(--text);color:var(--bg);font-size:9px}
.lv-agent{display:inline-flex;align-items:center;gap:6px;border:0;background:none;color:var(--text-2);font:inherit;cursor:pointer;padding:0}
.lv-agent:hover{color:var(--text)}
.lv-ctx{display:inline-flex;align-items:center;gap:5px;font-size:11px;font-weight:600;color:var(--ctx)}
.lv-ctx::before{content:"";width:7px;height:7px;border-radius:2px;background:currentColor;transform:rotate(45deg);flex:none}
.lv-pill{display:inline-flex;align-items:center;border-radius:999px;padding:2px 9px;font-size:11px;font-weight:600;white-space:nowrap}
.lv-mut{background:var(--surface-3);color:var(--text-2)} .lv-good{background:var(--good-soft);color:var(--good-ink)}
.lv-warn{background:var(--warn-soft);color:var(--warn-ink)} .lv-crit{background:var(--crit-soft);color:var(--crit-ink)}
.lv-acc{background:var(--accent-soft);color:var(--accent-text)}
.lv-bar{height:6px;border-radius:999px;background:var(--surface-3);overflow:hidden}.lv-bar>b{display:block;height:100%;border-radius:999px}
.lv-empty{color:var(--muted);padding:24px;text-align:center}
`);

// ---------------------------------------------------------------- shared helpers
// (added with Mission, Story and Decide; generic, for every level screen)

/** A route's parameter: ctx.params[name] when the shell gives it, else the
 *  hash's second segment (`#/story/<agent>` → agent). */
export function routeParam(ctx, name) {
  if (ctx && ctx.params && ctx.params[name]) return ctx.params[name];
  const parts = (location.hash || "").replace(/^#\/?/, "").split(/[/?]/);
  return parts[1] ? decodeURIComponent(parts[1]) : null;
}

/** The repository a screen is about: ctx.params.repo, else the one most of
 *  the running agents work in. */
export function repoOf(ctx) {
  if (ctx && ctx.params && ctx.params.repo) return ctx.params.repo;
  const n = new Map();
  for (const a of (ctx && ctx.state && ctx.state.agents) || []) if (a.repo && a.running !== false) n.set(a.repo, (n.get(a.repo) || 0) + 1);
  let best = null, max = 0;
  for (const [r, c] of n) if (c > max) [best, max] = [r, c];
  return best || "";
}

/** Agents by id from several lists (fetched rows, the stream's): the row
 *  whose state or narration is newer wins; on a tie, the later list. */
export function mergeAgents(...lists) {
  const stamp = (a) => Math.max(toMs(a.state_since) || 0, toMs(a.narration_ts) || 0);
  const out = new Map();
  for (const list of lists) for (const a of list || []) {
    const had = out.get(a.id);
    if (!had || stamp(a) >= stamp(had)) out.set(a.id, a);
  }
  return out;
}

/** Text with `code` spans, as nodes. */
export function inlineCode(text) {
  return String(text ?? "").split(/(`[^`]+`)/).filter(Boolean).map((p) => (p.length > 2 && p[0] === "`" && p.at(-1) === "`" ? h("code", { class: "lv-code" }, p.slice(1, -1)) : p));
}

/** JSON a row keeps as TEXT (options, precedent), or already parsed. */
export function parseJSON(v, fallback = null) {
  if (v == null || v === "") return fallback;
  if (typeof v !== "string") return v;
  try { return JSON.parse(v); } catch { return fallback; }
}

/** A question's precedent as records to show. The server keeps what
 *  `chloectx ask --json` printed ({text, ids, agents: {id: Agent}, missing,
 *  rules: [{id, text, option}]}): the notes are the text's "- <date> <kind>:
 *  <text>" lines, in the order of `ids`. Rules of the repository that speak
 *  for an option come first, as yours. Each record is {id, kind, text, ts,
 *  agent, by}; `by` is "you", the agent's name, or null (a session before
 *  Workbench). */
export function precedentOf(q) {
  const p = parseJSON(q && q.precedent, null);
  if (!p || typeof p !== "object") return { records: [], ids: [], text: "", missing: [], pending: true };
  if (Array.isArray(p.records)) return { ids: [], missing: [], ...p };
  const ids = Array.isArray(p.ids) ? p.ids : [];
  const agents = p.agents || {};
  const records = [];
  const seen = new Set();
  for (const r of p.rules || []) {
    if (seen.has(r.id)) continue;
    seen.add(r.id);
    records.push({ id: r.id, kind: "rule", text: r.text, ts: null, agent: null, by: "you", option: r.option });
  }
  let i = 0;
  for (const line of String(p.text || "").split("\n")) {
    const m = line.match(/^- (\d{4}-\d{2}-\d{2}) ([\w-]+): (.*)$/);
    if (!m) continue;
    const id = ids[i++] || null;
    const agent = (id && agents[id]) || null;
    records.push({ id, kind: m[2], text: m[3].trim(), ts: m[1], agent, by: agent ? agent.name : null });
  }
  return { records, ids, text: p.text || "", missing: p.missing || [], pending: false };
}

/** 950, 41k, 1.2M. */
export function fmtTokens(n) {
  if (n == null || !isFinite(n)) return "";
  if (n < 1000) return String(n);
  if (n < 1e6) return `${n < 1e4 ? (n / 1000).toFixed(1).replace(/\.0$/, "") : Math.round(n / 1000)}k`;
  return `${(n / 1e6).toFixed(1).replace(/\.0$/, "")}M`;
}

/** A short span: 0.4 s, 11 s, 2 m 40 s, 1 h 5 m. */
export function fmtSpan(ms) {
  if (ms == null || !isFinite(ms)) return "";
  if (ms < 1000) return `${(ms / 1000).toFixed(1)} s`;
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s} s`;
  if (s < 3600) return s % 60 ? `${Math.floor(s / 60)} m ${s % 60} s` : `${s / 60} min`;
  return `${Math.floor(s / 3600)} h ${Math.round((s % 3600) / 60)} m`;
}

/** A day as people say it: today, yesterday, 9 Nov. */
export function fmtDay(t) {
  const ms = toMs(t);
  if (!ms) return "";
  const d = new Date(ms);
  const days = Math.round((new Date(new Date().toDateString()) - new Date(d.toDateString())) / 864e5);
  if (days === 0) return "today";
  if (days === 1) return "yesterday";
  return d.toLocaleDateString([], { day: "numeric", month: "short" });
}

/** Vertical bars for a series (a pulse, activity per minute); `tone(v, i)`
 *  names a theme token per bar, `title(v, i)` its tooltip. */
export function barChart(values, { height = 64, tone = () => "accent", title = () => null } = {}) {
  const max = Math.max(1, ...values);
  return h("div", { class: "lv-bars", style: { height: height + "px" } }, values.map((v, i) =>
    h("i", { title: title(v, i), style: { height: (v ? Math.max(6, (v / max) * 100) : 3) + "%", background: `var(--${tone(v, i)})`, opacity: v ? 1 : 0.4 } })));
}

const typing = (el) => el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName));

/** A screen's keyboard: `j`/`k` move the focus through the visible elements
 *  matching `items`, in document order; Enter or Space on one that is not a
 *  button clicks it. `onKey(e)` sees other keys first and returns true when
 *  it used one. Keys typed into a field, or with a modifier, are left alone.
 *  Returns the function that removes it. */
export function keyNav(root, { items = "[data-nav]", onKey } = {}) {
  const handler = (e) => {
    if (!root.isConnected || e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey || typing(e.target)) return;
    if (onKey && onKey(e)) { e.preventDefault(); return; }
    if (e.key === "j" || e.key === "k") {
      const list = [...root.querySelectorAll(items)].filter((x) => x.offsetParent !== null);
      if (!list.length) return;
      e.preventDefault();
      const cur = document.activeElement && document.activeElement.closest ? document.activeElement.closest(items) : null;
      const at = list.indexOf(cur);
      const next = at < 0 ? list[0] : list[Math.max(0, Math.min(list.length - 1, at + (e.key === "j" ? 1 : -1)))];
      next.focus({ preventScroll: true });
      next.scrollIntoView({ block: "nearest" });
    } else if ((e.key === "Enter" || e.key === " ") && root.contains(e.target) && e.target.matches(items) && !/^(BUTTON|A|LABEL)$/.test(e.target.tagName)) {
      e.preventDefault();
      e.target.click();
    }
  };
  document.addEventListener("keydown", handler);
  return () => document.removeEventListener("keydown", handler);
}

/** Replaces `host`'s children with what `fn` builds, keeping the focus on
 *  the element with the same data-key. */
export function rerender(host, fn) {
  const had = document.activeElement && host.contains(document.activeElement) ? document.activeElement.dataset.key : null;
  host.replaceChildren(...[fn()].flat(Infinity).filter((x) => x != null && x !== false));
  if (had) { const el = host.querySelector(`[data-key="${CSS.escape(had)}"]`); if (el) el.focus({ preventScroll: true }); }
}

/** fn, called once `ms` after the last call. */
export function later(fn, ms) {
  let t = null;
  const f = (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
  f.cancel = () => clearTimeout(t);
  return f;
}

useStyle("common-helpers", `
.lv-screen{height:100%;overflow:auto;overscroll-behavior:contain}
.lv-code{font-family:var(--mono);font-size:.88em;background:var(--surface-3);border-radius:4px;padding:1px 4px}
.lv-h{margin:0 0 10px;font-size:11.5px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--muted)}
.lv-h .n{font-weight:500;margin-left:3px}
.lv-card{background:var(--surface);border:var(--hair) solid var(--line);border-radius:var(--r);padding:14px;box-shadow:var(--shadow-1)}
.lv-bars{display:flex;align-items:flex-end;gap:3px}.lv-bars>i{flex:1;min-width:2px;border-radius:2px 2px 0 0}
.lv-vio{background:color-mix(in srgb,var(--ctx) 14%,transparent);color:var(--ctx)}
`);

// ---------------------------------------------------------------------------
// More shared pieces: the page frame, cards, a tooltip, Esc handling, short
// inline formatting, numbers.

/** The token the app keeps (for URLs that cannot carry a header). */
export function token(ctx) {
  if (ctx && ctx.token) return ctx.token;
  try { return sessionStorage.getItem("chloedb-workbench-token") || "dev"; } catch { return "dev"; }
}

/** An agent's colour at an alpha, for tints. */
export function agentTint(a, alpha) { return `hsl(${agentHue(a)} 80% 68% / ${alpha})`; }

/** `**bold**` and `code` in a sentence, as nodes; everything else is text. */
export function md(text) {
  const out = [];
  const re = /\*\*([^*]+)\*\*|`([^`]+)`/g;
  const s = String(text ?? "");
  let at = 0, m;
  while ((m = re.exec(s))) {
    if (m.index > at) out.push(s.slice(at, m.index));
    out.push(m[1] != null ? h("b", null, m[1]) : h("code", { class: "lv-code" }, m[2]));
    at = re.lastIndex;
  }
  if (at < s.length) out.push(s.slice(at));
  return out;
}

/** 1.2M, 312k, 940. */
export function fmtNum(n) {
  if (n == null || !isFinite(n)) return "";
  const a = Math.abs(n);
  if (a >= 1e6) return (n / 1e6).toFixed(a >= 1e7 ? 0 : 1).replace(/\.0$/, "") + "M";
  if (a >= 1e3) return Math.round(n / 1e3) + "k";
  return String(Math.round(n));
}

/** Calls fn at most once per `wait` ms, the last call's arguments winning. */
export function throttle(fn, wait) {
  let t = null, last = 0, args;
  const run = () => { t = null; last = Date.now(); fn(...args); };
  const call = (...a) => { args = a; if (t) return; t = setTimeout(run, Math.max(0, wait - (Date.now() - last))); };
  call.cancel = () => { clearTimeout(t); t = null; };
  return call;
}

/** Esc closes the newest open panel or bubble first. Returns its remover. */
const escStack = [];
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || !escStack.length) return;
  e.preventDefault();
  e.stopPropagation();
  escStack[escStack.length - 1]();
}, true);
export function onEscape(close) {
  escStack.push(close);
  return () => { const i = escStack.lastIndexOf(close); if (i >= 0) escStack.splice(i, 1); };
}

/** One floating tooltip: show(content, x, y) near a point, hide(), remove(). */
export function tooltip() {
  const el = h("div", { class: "lv-tip", role: "tooltip", hidden: true });
  document.body.append(el);
  return {
    show(content, x, y) {
      el.replaceChildren(...[content].flat());
      el.hidden = false;
      const r = el.getBoundingClientRect();
      const left = Math.min(window.innerWidth - r.width - 8, Math.max(8, x + 14));
      const top = y + 18 + r.height > window.innerHeight - 8 ? y - r.height - 12 : y + 18;
      el.style.left = left + "px";
      el.style.top = Math.max(8, top) + "px";
    },
    hide() { el.hidden = true; },
    remove() { el.remove(); },
  };
}

/** A card with an optional small uppercase heading and a right-hand aside. */
export function card(title, aside, ...kids) {
  return h("section", { class: "lv-card" },
    title || aside ? h("div", { class: "lv-card-head" }, title ? h("h2", { class: "lv-h2" }, title) : null, aside) : null,
    ...kids);
}

useStyle("common-2", `
.lv-screen{position:absolute;inset:0;color:var(--text);font-size:13px;line-height:1.45}
.lv-screen.aw,.lv-screen.fl,.lv-screen.mp,.lv-screen.pv{padding:20px 24px 28px}
.lv-head{display:flex;align-items:baseline;gap:6px 14px;flex-wrap:wrap}
.lv-h1{margin:0;font:var(--w-title) var(--fs-lg)/var(--lh-lg) var(--sans);letter-spacing:var(--track-md)}
.lv-sub{color:var(--muted)}
.lv-h2{margin:0;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2);text-transform:uppercase;letter-spacing:var(--track-label)}
.lv-card{background:var(--surface);border:var(--hair) solid var(--line);border-radius:var(--r);padding:14px 16px;box-shadow:var(--shadow-1);min-width:0}
.lv-card-head{display:flex;align-items:center;justify-content:space-between;gap:10px;margin-bottom:10px}
.lv-code{font-family:var(--mono);font-size:.92em;background:var(--surface-3);border-radius:4px;padding:0 4px}
.lv-tip{position:fixed;z-index:80;pointer-events:none;max-width:340px;background:var(--surface);color:var(--text);border:1px solid var(--line-2);border-radius:9px;padding:8px 10px;font-size:12px;line-height:1.45;box-shadow:var(--shadow-2)}
.lv-tip[hidden]{display:none}
.lv-link{border:0;background:none;padding:0;color:var(--accent-text);cursor:pointer;font:inherit;font-weight:var(--w-mid);text-decoration:none}
.lv-link:hover{text-decoration:underline}
button.lv-ctx{border:0;background:none;padding:0;cursor:pointer;font-family:inherit}
@media (max-width:640px){.lv-screen.aw,.lv-screen.fl,.lv-screen.mp,.lv-screen.pv{padding:14px}.lv-h1{font-size:19px}}
`);

// ---------------------------------------------------------------- goal pieces
// The shared pieces of the goal pages (JOURNEYS.md, "Shared pieces"): the
// page frame, the stepper, buttons with their key, labels, panes, banks of
// tiles, state dots, the question card, and the dense pieces (churnBar,
// blastCompact, turnStrip, meter, testStrip). Review and Debug (goals/
// review.js, goals/debug.js) build on them. Styles use the tokens only.

const SVGNS = "http://www.w3.org/2000/svg";
const PATHS = {
  search: "M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14zM20 20l-4-4",
  bell: "M6 8a6 6 0 1 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.9 1.9 0 0 0 3.4 0",
  more: "M5 12h.01M12 12h.01M19 12h.01",
  chev: "M9 6l6 6-6 6",
  down: "M6 9l6 6 6-6",
  term: "M4 5h16v14H4zM7.5 9.5l3 2.5-3 2.5M12.5 15h4",
  check: "M5 12.5l4.5 4.5L19 7.5",
  plus: "M12 5v14M5 12h14",
  folder: "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
  spark: "M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5L18 18M6 18l2.5-2.5M15.5 8.5L18 6",
  rewind: "M3 12a9 9 0 1 0 2.6-6.4M3 4v5h5M12 8v4.5l3 1.8",
  branch: "M6 3v12M18 9a3 3 0 1 0 0-.01M6 21a3 3 0 1 0 0-.01M18 12c0 4-6 3-11.5 7",
  ship: "M4 15l8 5 8-5M4 10l8 5 8-5-8-5z",
  why: "M9.3 9.2a2.8 2.8 0 0 1 5.4 1c0 1.9-2.7 2.4-2.7 4.1M12 17.6h.01M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18z",
  new: "M12 3l2.5 5.5L20 9l-4 4 1 6-5-3-5 3 1-6-4-4 5.5-.5z",
  map: "M3 3h18v18H3zM11 3v18M11 12h10M16 12v9",
  stop: "M7 7h10v10H7z",
  warn: "M12 3.5l9.5 16.5h-19zM12 10v4.5M12 17.4h.01",
  pause: "M8 5v14M16 5v14",
  play: "M7 5l12 7-12 7z",
  eye: "M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12zM12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z",
  x: "M6 6l12 12M18 6L6 18",
  back: "M15 6l-6 6 6 6",
  home: "M4 11l8-7 8 7M6 9.5V20h12V9.5",
  grid: "M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z",
  kbd: "M3 7h18v10H3zM7 11h.01M11 11h.01M15 11h.01M8 14h8",
  bug: "M8 9h8v6a4 4 0 0 1-8 0zM12 9V6M9 5l1.5 1.5M15 5l-1.5 1.5M4 12h4M16 12h4M5 18l3-1.5M19 18l-3-1.5",
  clock: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 7.5V12l3 2",
  book: "M5 4h10a3 3 0 0 1 3 3v13H8a3 3 0 0 1-3-3zM5 17a3 3 0 0 1 3-3h10",
  gear: "M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6zM12 2.5v3M12 18.5v3M4.2 6.2l2.1 2.1M17.7 15.7l2.1 2.1M2.5 12h3M18.5 12h3M4.2 17.8l2.1-2.1M17.7 8.3l2.1-2.1",
  send: "M4 12l16-8-6 16-2.5-6.5z",
  agents: "M9 8a3 3 0 1 0 0-.01M3.5 19a5.5 5.5 0 0 1 11 0M16 6a3 3 0 1 1 1.5 5.6M17.5 14a5 5 0 0 1 3 5",
};

/** A line icon (24×24 paths, 1.5 stroke): `icon("search")`, `icon("chev", "i sm")`. */
export function icon(name, cls = "i") {
  const s = document.createElementNS(SVGNS, "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("class", cls);
  s.setAttribute("aria-hidden", "true");
  const p = document.createElementNS(SVGNS, "path");
  p.setAttribute("d", PATHS[name] || PATHS.chev);
  s.append(p);
  return s;
}

/** A keycap: `kc("⌘↵")`. */
export function kc(key) { return h("kbd", { class: "kc" }, key); }

/** A small-caps mono label, with an optional right-hand aside: `label("Needs you", h("a", …))`. */
export function label(text, ...aside) {
  return h("div", { class: "lbl" }, h("span", {}, text), aside.length ? h("span", { class: "sp" }) : null, ...aside);
}

/** A button, or a link that looks like one, with its key shown:
 *  `btn("Merge into main", { primary: true, kbd: "⌘↵", onclick })`. */
export function btn(text, { primary, ghost, danger, sm, lg, href, onclick, kbd, icon: ic, cls, disabled, title, type = "button" } = {}) {
  const kids = [ic ? icon(ic, "i sm") : null, text, kbd ? kc(kbd) : null];
  const c = ["btn", primary && "primary", ghost && "ghost", danger && "danger", sm && "sm", lg && "lg", cls].filter(Boolean).join(" ");
  return href ? h("a", { class: c, href, onclick, title }, kids) : h("button", { class: c, type, onclick, disabled: disabled || null, title }, kids);
}

/** A goal page: a header (kicker, title, one sentence, an aside), the body,
 *  and an optional inspector (the frosted side panel, --insp-w wide on a
 *  desktop; under the body at 1024px and below). `steps` puts a stepper
 *  above the title. */
export function page({ title, sub, aside, steps, wide, cls, inspector, kicker } = {}, ...body) {
  const main = h("div", { class: "gp-main" },
    steps ? stepper(steps) : null,
    title || sub || aside ? h("header", { class: "gp-head" },
      h("div", { class: "gp-ht" }, kicker ? h("div", { class: "gp-kick" }, kicker) : null, title ? h("h1", { class: "gp-title" }, title) : null, sub ? h("p", { class: "gp-sub" }, sub) : null),
      aside || null) : null,
    ...body);
  return h("div", { class: `gp${wide ? " wide" : ""}${inspector ? " with-insp" : ""}${cls ? " " + cls : ""}` },
    main, inspector ? h("aside", { class: "gp-insp", "aria-label": "Details" }, inspector) : null);
}

/** The stepper: `{ list: [{label, href, state: "done"|"now"|"next"}], after: {label, href} }`.
 *  On a phone it reads "Step 2 of 4 · What to build" over a bar of segments. */
export function stepper({ list, after } = {}) {
  const at = Math.max(0, list.findIndex((s) => s.state === "now"));
  return h("nav", { class: "gp-steps", "aria-label": "Steps" },
    h("div", { class: "gp-sl" }, list.map((s, i) => [
      i ? h("span", { class: "gp-sep" }, "/") : null,
      h("a", { class: `gp-step ${s.state || "next"}`, href: s.href || null, "aria-current": s.state === "now" ? "step" : null },
        h("span", { class: "gp-n" }, s.state === "done" ? icon("check", "i sm") : String(i + 1).padStart(2, "0")), s.label),
    ]), after ? h("a", { class: "gp-after", href: after.href }, after.label, icon("chev", "i sm")) : null),
    h("div", { class: "gp-mstep" }, h("span", {}, h("b", {}, `Step ${at + 1} of ${list.length}`), ` · ${list[at] ? list[at].label : ""}`),
      h("span", { class: "gp-mseg" }, list.map((s) => h("i", { class: s.state || "next" })))));
}

/** A flat pane on a hairline, with a label header: `pane("Agents", aside, ...kids)`.
 *  Put padded content in `h("div", {class: "bd"}, …)`. */
export function pane(title, aside, ...kids) {
  return h("section", { class: "pane" }, title ? h("div", { class: "hd" }, aside ? label(title, aside) : label(title)) : null, ...kids);
}

/** Tiles separated by hairlines: `bank(3, tile, tile, tile)` (one column on a
 *  phone unless the bank has the class `keep`). */
export function bank(cols, ...tiles) {
  return h("div", { class: "bank", style: { "--cols": String(cols) } }, ...tiles);
}

/** A state dot and its word: group is needs|working|ready|finished|failed. */
export const GROUP_WORDS = { needs: "needs you", working: "working", idle: "idle", ready: "ready", finished: "finished", failed: "failed" };
export function stateDot(group, text) {
  return h("span", { class: `st ${group}` }, h("i", {}), text || GROUP_WORDS[group] || group);
}

/** A short time since: "now", "4 min", "2 h", "3 d". */
export function since(t) {
  const ms = toMs(t);
  if (!ms) return "";
  const m = Math.max(0, Math.round((Date.now() - ms) / 60000));
  return m < 1 ? "now" : m < 60 ? `${m} min` : m < 48 * 60 ? `${Math.round(m / 60)} h` : `${Math.round(m / 1440)} d`;
}

/** The options of a question as [{label, n, no}], whatever form the server sent. */
export function optionsOf(q) {
  const raw = parseJSON(q && q.options, []) || [];
  return raw.map((o, i) => (typeof o === "string" ? { label: o, n: i + 1 } : { ...o, label: o.label || o.text || "", n: o.n || i + 1 }));
}

/** What a wide option would write, from Claude Code's own label: the rule,
 *  the file it lands in, and whether it outlives the session. `null` for a
 *  plain yes or no. Shown on the grant sheet before anything is typed. */
export function grantOf(q, o, agent) {
  if (!o || !o.grants) return null;
  const d = parseJSON(q.detail, {}) || {};
  const tool = d.tool || "Bash";
  const l = o.label || "";
  const where = `${(agent && (agent.cwd || agent.worktree)) || "the agent's folder"}/.claude/settings.local.json`;
  const m = l.match(/don.?t ask again for\s+(.+?)(?:\s+commands?)?(?:\s+in\s+.+)?$/i);
  if (/auto mode|auto-accept|bypass/i.test(l)) return { rule: "auto mode", where: "this session only", lasting: false, says: o.grants };
  if (/during this session|all edits|allow all/i.test(l)) return { rule: tool === "Bash" ? "every command" : "every edit", where: "this session only", lasting: false, says: o.grants };
  if (m) {
    const prefix = m[1].replace(/[`'"“”]/g, "").replace(/\s*[-:*]+$/, "").trim();
    return { rule: tool === "Bash" ? `Bash(${prefix}:*)` : `${tool}(${prefix})`, prefix, where, lasting: true, says: o.grants };
  }
  return { rule: l, where, lasting: true, says: o.grants };
}

const LONG = 25;
/** A question as JOURNEYS.md and the UX critique want it, the one card used
 *  everywhere (Start, Continue, the drawer, the agent page, the phone):
 *  - what will run, in full (over 25 lines: the first 25 and "Show all N
 *    lines", and the options wait until it is shown);
 *  - Claude's summary below it, marked as not checked;
 *  - Claude Code's options in Claude Code's order with its own labels and
 *    numbers, all equal weight; an option that grants more than this one
 *    call says so on a second line, sits apart, and goes to `onGrant` (the
 *    grant sheet) rather than `onAnswer`;
 *  - keys, while the card has the focus: a digit picks that option, Enter
 *    sends it, Esc drops the pick. Digits outside a card do nothing.
 *  `q` is a row of /api/questions (or /api/needs' `question`). */
export function questionCard(q, { agent, onAnswer, onGrant, onText, big = false, agentLink = true } = {}) {
  const d = parseJSON(q.detail, {}) || {};
  const name = (agent && agent.name) || q.agent_name || "agent";
  const opts = optionsOf(q);
  const cmd = String(d.command || d.diff || "");
  const lines = cmd ? cmd.split("\n") : [];
  const long = lines.length > LONG;
  const title = q.kind === "permission" ? (d.menu && d.menu.question) || q.text : q.text;
  const kv = [["Tool", d.tool], ["Writes", d.rel || d.file]].filter(([, v]) => v);
  let typed = null, picked = -1, shown = !long;
  const pre = cmd ? h("pre", { class: "qc-cmd" }, long ? lines.slice(0, LONG).join("\n") : cmd) : null;
  const reason = long ? h("div", { class: "qc-wait" }, `Read all ${lines.length} lines to answer.`) : null;
  const buttons = opts.map((o, i) => h("button", {
    class: `qc-opt${o.grants ? " wide" : ""}${o.no ? " no" : ""}`, type: "button", "data-n": o.n, "data-i": i, disabled: long || null,
    onclick: () => (o.grants && onGrant ? onGrant(i, o) : onAnswer && onAnswer(i, o)),
  }, kc(String(o.n)), h("span", { class: "ol" }, h("span", {}, o.label), o.grants ? h("small", {}, `${o.grants[0].toUpperCase()}${o.grants.slice(1)}.${onGrant ? " Opens a check first." : ""}`) : null)));
  const showAll = long ? h("button", { class: "lv-link qc-more", type: "button", onclick: () => {
    pre.textContent = cmd; shown = true; showAll.remove(); reason.remove(); buttons.forEach((b) => { b.disabled = false; });
  } }, `Show all ${lines.length} lines`) : null;
  const pick = (i) => { picked = i; buttons.forEach((b, k) => b.classList.toggle("sel", k === i)); };
  const card = h("div", { class: `qc${big ? " big" : ""}`, "data-q": q.id, tabindex: "0", "aria-label": `${name}: ${title || "a question"}`,
    onkeydown: (e) => {
      if (e.target !== card && e.target.closest("input,textarea")) return;
      if (/^[1-9]$/.test(e.key) && !e.metaKey && !e.ctrlKey && !e.altKey) {
        const i = opts.findIndex((o) => String(o.n) === e.key);
        if (i >= 0 && shown) { pick(i); e.preventDefault(); e.stopPropagation(); }
      } else if (e.key === "Enter" && picked >= 0 && e.target === card) {
        e.preventDefault(); e.stopPropagation(); buttons[picked].click();
      } else if (e.key === "Escape" && picked >= 0) {
        e.preventDefault(); e.stopPropagation(); pick(-1);
      }
    } },
    h("div", { class: "qc-hd" }, agent ? avatar(agent, 18) : null, h("b", {}, name), h("span", {}, q.kind === "ask" ? "asks you" : "waiting for you"),
      h("span", { class: "t num" }, since(q.created_ms || q.created)),
      agentLink && q.agent ? h("a", { class: "lv-link qc-open", href: `#/a/${q.agent}` }, "Open agent") : null),
    h("div", { class: "qc-ask" }, title || "Claude Code is asking"),
    cmd ? h("div", { class: "qc-cmdh" }, h("span", {}, d.tool || "Command"), h("span", { class: "num" }, `${lines.length} line${lines.length === 1 ? "" : "s"}, ${long ? "the first 25 shown" : "shown in full"}`)) : null,
    pre, showAll,
    q.context && !cmd ? h("p", { class: "qc-what" }, q.context) : null,
    d.description ? h("p", { class: "qc-what" }, h("span", { class: "qc-prov" }, "Claude's summary, not checked: "), d.description) : null,
    big && kv.length ? h("dl", { class: "kv" }, kv.map(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])) : null,
    opts.length ? h("div", { class: "qc-opts", role: "group", "aria-label": "Claude Code's options" }, reason, buttons)
      : q.kind === "permission" ? h("div", { class: "qc-wait" }, "Workbench has not read Claude Code's options for this prompt. ",
        q.agent ? h("a", { class: "lv-link", href: `#/a/${q.agent}/terminal` }, "Answer it in its terminal") : "Answer it in its terminal.") : null,
    q.kind === "ask" || onText ? h("form", { class: "qc-type", onsubmit: (e) => { e.preventDefault(); const v = typed.value.trim(); if (v && onText) onText(v); } },
      typed = h("input", { class: "field", placeholder: q.kind === "ask" ? "Answer in your words" : "Or tell it what to do instead", "aria-label": "Answer in your words" })) : null,
    q.kind === "permission" && opts.length ? h("div", { class: "qc-foot" }, "The numbers are Claude Code's own. Pick one with its key and Enter, or click it. Workbench types it, and only while this prompt is still open.") : null);
  return card;
}

// ---------------------------------------------------------------- dense pieces (JOURNEYS.md, "Scale rules")

/** +/− as one short bar (rule 6): exact numbers in the title on a desktop, printed beside it on a phone. */
export function churnBar(add, del, max = 600) {
  const w = (n) => Math.max(n ? 3 : 0, Math.round(Math.sqrt((n || 0) / max) * 64));
  return h("span", { class: "churn", title: `+${add || 0} −${del || 0} lines` },
    h("i", { class: "a", style: { width: w(add) + "px" } }), h("i", { class: "d", style: { width: w(del) + "px" } }),
    h("span", { class: "cn num" }, `+${add || 0} −${del || 0}`));
}

/** Churn as five squares: the share added in green, removed in red (Review's pieces table). */
export function churnSquares(add, del) {
  const t = (add || 0) + (del || 0);
  const a = t ? Math.round(((add || 0) / t) * 5) : 0;
  return h("span", { class: "ds", title: `+${add || 0} −${del || 0} lines` }, [0, 1, 2, 3, 4].map((i) => h("i", { class: t ? (i < a ? "a" : "d") : null })));
}

/** HIGH / MED / LOW in mono small caps. */
export function riskLabel(risk) {
  const r = String(risk || "low").toLowerCase();
  return h("span", { class: `rk ${r}` }, r === "medium" ? "MED" : r.toUpperCase());
}

/** A gauge tile: a label, a big mono number, a scale of ticks, the top 3 names. */
function gauge(ring) {
  const ticks = Math.max(1, Math.min(24, ring.of || 24));
  const on = ring.of ? Math.round((Math.min(ring.n, ring.of) / ring.of) * ticks) : Math.min(ticks, ring.n || 0);
  const names = ring.names || [];
  return h("div", { class: "gauge" },
    label(ring.label),
    h("div", { class: "v" }, h("b", { class: "num" }, String(ring.n)), ring.unit ? h("span", {}, ring.unit) : null),
    h("div", { class: "ticks" }, Array.from({ length: ticks }, (_, i) => h("i", { class: i < on ? "on" : null }))),
    names.length ? h("div", { class: "names" }, names.slice(0, 3).map((n, i) => [i ? ", " : "", ring.code ? h("code", {}, n) : n]),
      names.length > 3 ? h("button", { class: "lv-link", type: "button", onclick: ring.onMore || null }, ` +${names.length - 3} more`) : null) : null);
}

/** The blast radius (rules 2, 3): a one-line verdict, three gauges
 *  (functions, callers, public names), then only the exceptions.
 *  `{ verdict, tone: good|warn|crit, rings: [{n, label, names, code, of, unit}],
 *     exceptions: [{tone: crit|warn|ctx|info, text, sub}], tests, actions: () => nodes, title }` */
export function blastCompact({ verdict, tone = "warn", rings = [], exceptions = [], tests, actions, title = "Blast radius" } = {}) {
  const n = exceptions.length + (tests ? 1 : 0);
  const phone = matchMedia("(max-width: 640px)").matches;
  return h("section", { class: "blast-c" },
    title ? label(title) : null,
    h("div", { class: `pane verdict ${tone}` }, h("span", { class: "g" }), h("p", {}, verdict)),
    actions ? h("div", { class: "blast-act" }, actions()) : null,
    rings.length ? bank(rings.length, ...rings.map(gauge)) : null,
    n ? h("details", { class: "pane blast-ex", open: !phone || null },
      h("summary", { class: "hd" }, label("Exceptions", h("span", { class: "num" }, String(n))), h("span", { class: "bx-o", "aria-hidden": "true" }, icon("chev", "i sm"))),
      exceptions.map((e) => h("div", { class: `ex ${e.tone || "info"}` }, h("span", { class: "gl" }), h("div", {}, h("b", {}, e.text), e.sub ? h("span", {}, e.sub) : null))),
      tests ? h("div", { class: "ex ok" }, h("span", { class: "gl" }), h("div", {}, h("b", {}, tests))) : null) : null);
}

/** A session as a strip (rule 9): one tick per turn, coloured by kind
 *  (read|edit|run|fail|you|ctx), split into named phases [{name, from, to}]
 *  (turns counted from 1). `marks` are the turns that stand out, `notes[n]`
 *  their sentence, `href(n)` the link a picked turn offers. A tap names the
 *  turn: nothing is hover-only. */
export function turnStrip(turns, phases = [], marks = [], notes = {}, { href, onPick } = {}) {
  const cap = h("div", { class: "ts-cap" }, "Tap a tick to see its turn.");
  const say = (i, k) => {
    cap.replaceChildren(h("b", {}, `Turn ${i + 1}`), ` · ${k}`, notes[i + 1] ? ` · ${notes[i + 1]}` : "", href ? [" ", h("a", { class: "lv-link", href: href(i + 1) }, "Open")] : "");
    if (onPick) onPick(i + 1, k);
  };
  return h("div", { class: "tstrip" },
    h("div", { class: "ts-x" }, h("div", { class: "ts-in", style: { "--n": String(turns.length) } },
      phases.length ? h("div", { class: "ts-ph" }, phases.map((p) => h("span", { style: { flex: String(p.to - p.from + 1) } }, p.name))) : null,
      h("div", { class: "ts-ticks" }, turns.map((k, i) => h("i", { class: `k-${k}${marks.includes(i + 1) ? " mark" : ""}`, title: `turn ${i + 1}: ${k}`, onclick: () => say(i, k) }))))),
    cap,
    h("div", { class: "ts-legend" }, [["read", "read (short)"], ["ctx", "memory (half)"], ["run", "ran (tall)"], ["edit", "edit (full)"], ["fail", "failed (capped)"], ["you", "you"]].map(([k, l]) => h("span", {}, h("i", { class: `k-${k}` }), l))));
}

/** "41% full" (rule 12). */
export function meter(pct, text) {
  const p = Math.max(0, Math.min(100, Math.round(pct || 0)));
  return h("div", { class: `meter${p >= 85 ? " hot" : ""}` }, h("span", { class: "mb" }, h("b", { style: { width: p + "%" } })), h("span", {}, h("b", { class: "num" }, `${p}% full`), text ? ` · ${text}` : null));
}

/** The last runs as squares: a string of p (pass), f (fail), s (skipped or none), oldest first. */
export function testStrip(runs) {
  return h("span", { class: "tstr", title: "the last runs, oldest first" }, String(runs || "").split("").map((r) => h("i", { class: r === "p" ? "p" : r === "f" ? "f" : "s" })));
}

useStyle("goal-pieces", `
.gp{display:grid;grid-template-columns:minmax(0,1fr);min-height:100%}
.gp.with-insp{grid-template-columns:minmax(0,1fr) var(--insp-w)}
.gp-main{padding:var(--s-6) var(--s-7) 64px;min-width:0;max-width:var(--page-w);width:100%}
.gp.wide .gp-main{max-width:none}
.gp-insp{background:var(--glass-in);-webkit-backdrop-filter:var(--glass-blur);backdrop-filter:var(--glass-blur);border-left:var(--hair) solid var(--line);padding:var(--s-5) var(--s-5) var(--s-6);display:flex;flex-direction:column;gap:var(--s-6);min-width:0}
.gp-head{display:flex;align-items:flex-end;gap:var(--s-4);flex-wrap:wrap;margin-bottom:var(--s-6)}
.gp-ht{flex:1;min-width:0}
.gp-kick{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2);margin-bottom:var(--s-2)}
.gp-title{margin:0;font:var(--w-title) var(--fs-xl)/var(--lh-xl) var(--sans);letter-spacing:var(--track-xl)}
.gp-sub{margin:var(--s-1) 0 0;font-size:var(--fs-md);line-height:var(--lh-md);color:var(--text-2);max-width:68ch}
.gp-sec{margin-top:var(--s-6)}.gp-sec>.lbl{margin-bottom:var(--s-3)}
.gp-steps{margin-bottom:var(--s-5)}
.gp-sl{display:flex;align-items:center;gap:var(--s-1);flex-wrap:wrap;font:var(--w-text) var(--fs-sm)/var(--lh-sm) var(--mono);color:var(--text-2)}
.gp-step{display:inline-flex;align-items:center;gap:var(--s-2);padding:var(--s-1) var(--s-2);border-radius:var(--r-xs);color:var(--text-2);text-decoration:none}
.gp-step .gp-n{color:var(--text-3)}
.gp-step.now{background:var(--line);color:var(--text);font-weight:var(--w-mid)}
.gp-step.now .gp-n{color:var(--accent-text)}
.gp-step.done .gp-n{color:var(--good)}
.gp-sep{color:var(--text-3)}
.gp-after{margin-left:auto;display:inline-flex;align-items:center;gap:var(--s-1);color:var(--accent-text);text-decoration:none}
.gp-mstep{display:none}
.lbl{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2);display:flex;align-items:center;gap:var(--s-2);min-width:0}
.lbl .sp{flex:1}
.lbl a,.lbl .lv-link,.lbl button{color:var(--accent-text);letter-spacing:0;text-transform:none;font-family:var(--sans);font-size:var(--fs-sm)}
.pane{background:var(--surface);border:var(--hair) solid var(--line);border-radius:var(--r);min-width:0}
.pane>.hd{padding:var(--s-2) var(--s-4);border-bottom:var(--hair) solid var(--line);min-height:36px;display:flex;align-items:center}
.pane>.hd>.lbl{flex:1}
.pane>.bd{padding:var(--s-3) var(--s-4)}
.bank{display:grid;grid-template-columns:repeat(var(--cols,3),minmax(0,1fr));gap:var(--hair);background:var(--line);border:var(--hair) solid var(--line);border-radius:var(--r);overflow:hidden}
.bank>*{background:var(--surface);min-width:0}
.st{display:inline-flex;align-items:center;gap:6px;font-size:var(--fs-sm);color:var(--text-2);white-space:nowrap}
.st i{width:7px;height:7px;border-radius:50%;background:var(--text-3);flex:none}
.st.needs{color:var(--accent-text);font-weight:var(--w-mid)}.st.needs i{background:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}
.st.working i{background:var(--good)}
.st.ready i{background:transparent;border:1.5px solid var(--good)}
.st.idle i{background:transparent;border:1.5px solid var(--text-3)}
.st.failed{color:var(--crit-ink)}.st.failed i{background:var(--crit)}
.kv{display:grid;grid-template-columns:96px minmax(0,1fr);gap:var(--s-2) var(--s-4);margin:var(--s-3) 0 0}
.kv dt{font:var(--w-mid) var(--fs-xs)/var(--lh-body) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2)}
.kv dd{margin:0;min-width:0;overflow-wrap:anywhere}
.field{display:block;width:100%;height:var(--ctl-h-lg);padding:0 var(--s-3);border-radius:var(--r-sm);border:var(--hair) solid var(--line-2);background:var(--surface);color:var(--text);font:var(--w-text) var(--fs-body) var(--sans);outline:none}
textarea.field{height:auto;min-height:96px;padding:var(--s-2) var(--s-3);line-height:var(--lh-body);resize:vertical}
.field::placeholder{color:var(--text-3)}
.field:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}
.qc{background:var(--surface);border-radius:var(--r);box-shadow:var(--shadow-2);overflow:hidden;min-width:0}
.qc-hd{display:flex;align-items:center;gap:var(--s-2);padding:var(--s-3) var(--s-4) 0;font-size:var(--fs-sm);color:var(--text-2)}
.qc-hd b{color:var(--text);font-size:var(--fs-body);font-weight:var(--w-title)}
.qc-hd .t{margin-left:auto;font-family:var(--mono)}
.qc-open{font-size:var(--fs-sm)}
.qc-ask{padding:6px var(--s-4) 0;font-weight:var(--w-title);font-size:var(--fs-md);line-height:var(--lh-md);letter-spacing:var(--track-md)}
.qc-cmd{margin:var(--s-3) var(--s-4) 0;padding:10px var(--s-3);border-radius:var(--r-sm);background:var(--well);font:var(--w-text) var(--fs-sm)/var(--lh-sm) var(--mono);white-space:pre-wrap;overflow-wrap:break-word;word-break:normal;overflow-x:auto}
.qc-what{margin:0;padding:var(--s-2) var(--s-4) 0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.qc .kv{padding:0 var(--s-4)}
.qc-opts{margin-top:var(--s-3);border-top:var(--hair) solid var(--line);display:flex;flex-direction:column}
.qc-opt{display:flex;align-items:center;gap:10px;min-height:40px;padding:var(--s-2) var(--s-4);border:0;border-bottom:var(--hair) solid var(--line);background:none;color:var(--text);text-align:left;cursor:pointer;font-size:var(--fs-body)}
.qc-opt:last-child{border-bottom:0}
.qc-opt:hover:not(:disabled){background:var(--surface-2)}
.qc-opt:disabled{color:var(--text-3);cursor:not-allowed}
.qc-opt .ol{display:flex;flex-direction:column;min-width:0}
.qc-opt small{color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.qc-opt.wide{border-top:var(--hair) solid var(--line-2);margin-top:var(--s-2)}
.qc-opt:has(+ .qc-opt.wide){border-bottom:0}
.qc-opt.sel{box-shadow:inset 0 0 0 1.5px var(--accent);background:var(--accent-soft)}
.qc-opt.sel .kc{border-color:var(--accent-line);color:var(--accent-text)}
.qc:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
.qc.focus{box-shadow:0 0 0 2px var(--accent),var(--shadow-2)}
.qc-cmdh{display:flex;justify-content:space-between;gap:var(--s-2);margin:var(--s-3) var(--s-4) 0;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:.04em;text-transform:uppercase;color:var(--text-2)}
.qc-cmdh+.qc-cmd{margin-top:var(--s-1)}
.qc-more{margin:var(--s-2) var(--s-4) 0}
.qc-wait{padding:var(--s-2) var(--s-4);font-size:var(--fs-sm);color:var(--warn-ink);border-bottom:var(--hair) solid var(--line)}
.qc-prov{color:var(--text-2);font-style:italic}
.qc-type{padding:var(--s-3) var(--s-4) 0}.qc-type .field{margin:0}
.qc-foot{padding:var(--s-3) var(--s-4);font-size:var(--fs-xs);line-height:var(--lh-xs);color:var(--text-3)}
.qc-opts:last-child,.qc-type:last-child{padding-bottom:var(--s-3)}
.qc.big{box-shadow:none;border:var(--hair) solid var(--line)}
.qc.big .qc-ask{font-size:var(--fs-lg);line-height:var(--lh-lg);letter-spacing:var(--track-xl)}
.qc.big .qc-cmd{font-size:var(--fs-body);line-height:var(--lh-body)}
.qc.big .qc-opt{min-height:48px;font-size:var(--fs-md)}
.churn{display:inline-flex;align-items:center;gap:1px;white-space:nowrap}
.churn i{display:inline-block;height:6px;border-radius:1px}.churn i.a{background:var(--good)}.churn i.d{background:var(--crit)}
.churn .cn{margin-left:6px;font-size:var(--fs-xs);color:var(--text-2)}
.ds{display:inline-flex;gap:2px}.ds i{width:8px;height:8px;border-radius:2px;background:var(--line-2)}.ds i.a{background:var(--good)}.ds i.d{background:var(--crit)}
.rk{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:.06em;color:var(--text-2)}
.rk.high{color:var(--crit-ink)}.rk.medium{color:var(--warn-ink)}
.blast-c{display:flex;flex-direction:column;gap:var(--s-3)}
.verdict{display:flex;gap:var(--s-3);align-items:flex-start;padding:var(--s-4) 18px}
.verdict .g{width:22px;height:22px;border-radius:50%;border:1.5px solid var(--warn);display:grid;place-items:center;flex:none;margin-top:1px}
.verdict .g::after{content:"";width:8px;height:8px;border-radius:50%;background:var(--warn)}
.verdict.good .g{border-color:var(--good)}.verdict.good .g::after{background:var(--good)}
.verdict.crit .g{border-color:var(--crit)}.verdict.crit .g::after{background:var(--crit)}
.verdict p{margin:0;font-size:var(--fs-md);line-height:var(--lh-md);font-weight:var(--w-mid);letter-spacing:var(--track-md)}
.blast-act{display:flex;gap:var(--s-2);flex-wrap:wrap}
.gauge{padding:14px var(--s-4) var(--s-4)}
.gauge .v{display:flex;align-items:baseline;gap:var(--s-2);margin-top:var(--s-2)}
.gauge .v b{font:var(--w-mid) var(--fs-xl)/var(--lh-xl) var(--mono);letter-spacing:-.04em}
.gauge .v span{color:var(--text-2)}
.ticks{display:flex;gap:3px;margin-top:var(--s-3);height:12px;align-items:flex-end}
.ticks i{flex:1;height:6px;background:var(--line-2);border-radius:1px}.ticks i.on{background:var(--text-2);height:12px}
.gauge .names{margin-top:10px;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm);overflow-wrap:anywhere}
.blast-ex summary{list-style:none;cursor:pointer}.blast-ex summary::-webkit-details-marker{display:none}
.blast-ex summary .bx-o{margin-left:var(--s-3);display:inline-flex;align-items:center;gap:4px;color:var(--text-2);font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase}
.blast-ex summary .bx-o::before{content:"Show"}.blast-ex[open] summary .bx-o::before{content:"Hide"}
.blast-ex summary .bx-o svg{transform:rotate(90deg);transition:transform .15s}.blast-ex[open] summary .bx-o svg{transform:rotate(-90deg)}
.ex{display:grid;grid-template-columns:16px minmax(0,1fr);gap:10px;align-items:start;padding:10px var(--s-4);border-bottom:var(--hair) solid var(--line)}
.ex:last-child{border-bottom:0}
.ex .gl{width:8px;height:8px;border-radius:50%;margin:6px 4px 0;background:var(--text-3)}
.ex.crit .gl{background:var(--crit)}.ex.warn .gl{background:var(--warn)}.ex.ok .gl{background:var(--good)}.ex.ctx .gl{background:var(--ctx);border-radius:2px;transform:rotate(45deg)}
.ex b{font-weight:var(--w-mid);display:block}
.ex div>span{display:block;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm);margin-top:2px}
.tstrip{display:flex;flex-direction:column;gap:var(--s-1);min-width:0}
.ts-x{overflow-x:auto;max-width:100%}
.ts-in{min-width:min(100%,calc(var(--n) * 3px))}
.ts-ph{display:flex;gap:2px;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2);text-transform:uppercase;letter-spacing:.04em}
.ts-ph span{border-bottom:1.5px solid var(--line-2);padding-bottom:3px;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ts-ticks{display:flex;gap:1px;height:34px;align-items:stretch;margin-top:var(--s-1)}
.ts-ticks i{flex:1;min-width:2px;border-radius:1px;opacity:.9;cursor:pointer;align-self:flex-end;height:100%}
.ts-ticks i.k-read{height:40%}.ts-ticks i.k-ctx{height:55%}.ts-ticks i.k-run{height:75%}.ts-ticks i.k-fail{box-shadow:inset 0 3px 0 var(--text)}
.ts-ticks i.mark{outline:1.5px solid var(--text);outline-offset:1px;opacity:1;z-index:1}
.k-read{background:var(--lane-1)}.k-edit{background:var(--good)}.k-run{background:var(--warn)}.k-fail{background:var(--crit)}.k-you{background:var(--text)}.k-ctx{background:var(--ctx)}
.ts-cap{font-size:var(--fs-sm);color:var(--text-2);min-height:var(--lh-sm)}
.ts-legend{display:flex;gap:var(--s-3);flex-wrap:wrap;font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2)}
.ts-legend span{display:inline-flex;align-items:center;gap:5px}.ts-legend i{width:9px;height:9px;border-radius:2px;display:inline-block}
.meter{display:flex;align-items:center;gap:var(--s-2);font-size:var(--fs-sm);color:var(--text-2)}
.meter .mb{width:110px;height:6px;border-radius:3px;background:var(--line-2);overflow:hidden;flex:none}
.meter .mb b{display:block;height:100%;background:var(--accent)}
.meter.hot .mb b{background:var(--warn)}
.meter>span>b{color:var(--text);font-weight:var(--w-mid)}
.tstr{display:inline-flex;gap:2px;vertical-align:middle}.tstr i{width:6px;height:12px;border-radius:1px;background:var(--good)}.tstr i.f{background:var(--crit)}.tstr i.s{background:var(--line-2)}
@media (max-width:1024px){
  .gp.with-insp{grid-template-columns:minmax(0,1fr)}
  .gp-insp{border-left:0;border-top:var(--hair) solid var(--line);background:var(--surface-2)}
  .gp-main{padding:var(--s-5) var(--s-5) 48px}
}
@media (max-width:640px){
  .gp-main{padding:var(--s-4) var(--s-4) var(--s-6)}
  .gp-insp{padding:var(--s-4)}
  .gp-title{font-size:var(--fs-lg);line-height:var(--lh-lg)}
  .gp-sub{font-size:var(--fs-body);line-height:var(--lh-body)}
  .gp-head{margin-bottom:var(--s-4)}
  .gp-sl{display:none}
  .gp-mstep{display:flex;flex-direction:column;gap:6px;font-size:var(--fs-sm);color:var(--text-2)}
  .gp-mstep b{color:var(--text)}
  .gp-mseg{display:flex;gap:3px}.gp-mseg i{flex:1;height:3px;border-radius:2px;background:var(--line-2)}.gp-mseg i.done,.gp-mseg i.now{background:var(--accent)}
  .bank{grid-template-columns:minmax(0,1fr)}
  .bank.keep{grid-template-columns:repeat(var(--cols,3),minmax(0,1fr))}
  .kv{grid-template-columns:minmax(0,1fr);gap:2px}
  .kv dd{margin-bottom:var(--s-2)}
  .field{height:48px;font-size:16px}
  .qc-opt{min-height:var(--touch);font-size:var(--fs-md)}
  .gauge .v b{font-size:var(--fs-lg)}
}
@media (pointer:coarse){.qc-opt,.gp-step{min-height:var(--touch)}.field{font-size:16px}}
`);
