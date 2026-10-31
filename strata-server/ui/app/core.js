// The command center's building blocks: elements, icons, the API, number
// formats, and the overlays (toasts, drawers, dialogs, menus, tooltips).
// Text from the server goes in with textContent, never as markup.

export const SVGNS = "http://www.w3.org/2000/svg";

/** An element: h("div", {class: "card", onclick: f}, child, "text", [more]). */
export function h(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  setAttrs(e, attrs);
  append(e, kids);
  return e;
}

/** An SVG element, with the same attributes as `h`. */
export function s(tag, attrs, ...kids) {
  const e = document.createElementNS(SVGNS, tag);
  setAttrs(e, attrs);
  append(e, kids);
  return e;
}

function setAttrs(e, attrs) {
  if (!attrs) return;
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") e.setAttribute("class", v);
    else if (k === "text") e.textContent = v;
    else if (k === "style" && typeof v === "object") Object.assign(e.style, v);
    else if (k === "dataset") Object.assign(e.dataset, v);
    else if (k.startsWith("on") && typeof v === "function") e.addEventListener(k.slice(2), v);
    else if (k === "value" && "value" in e) e.value = v;
    else if (k === "checked" || k === "disabled" || k === "selected") e[k] = !!v;
    else e.setAttribute(k, v === true ? "" : v);
  }
}

function append(e, kids) {
  for (const k of kids.flat(Infinity)) {
    if (k == null || k === false) continue;
    e.append(k instanceof Node ? k : document.createTextNode(String(k)));
  }
}

export const $ = (sel, root = document) => root.querySelector(sel);
export const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
export const clear = (e, ...kids) => { e.replaceChildren(); append(e, kids); return e; };

// ---------------------------------------------------------------- icons
// 24-unit outline icons, drawn with the current color.
const ICONS = {
  overview: '<rect x="3" y="3" width="7" height="9" rx="1.5"/><rect x="14" y="3" width="7" height="5" rx="1.5"/><rect x="14" y="12" width="7" height="9" rx="1.5"/><rect x="3" y="16" width="7" height="5" rx="1.5"/>',
  topology: '<circle cx="12" cy="5" r="2.5"/><circle cx="5" cy="19" r="2.5"/><circle cx="12" cy="19" r="2.5"/><circle cx="19" cy="19" r="2.5"/><path d="M12 7.5v9M10.5 7 6 16.8M13.5 7l4.5 9.8"/>',
  perf: '<polyline points="22 12 18 12 15 21 9 3 6 12 2 12"/>',
  queries: '<path d="M4 6h16M4 12h10M4 18h7"/><circle cx="18" cy="16" r="3"/><path d="m20.2 18.2 1.8 1.8"/>',
  logs: '<path d="M6 3h9l4 4v14H6z"/><path d="M15 3v4h4M9 12h7M9 16h7M9 8h3"/>',
  alerts: '<path d="M6 8a6 6 0 1 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/><path d="M10.3 21a1.9 1.9 0 0 0 3.4 0"/>',
  access: '<circle cx="8" cy="15" r="4"/><path d="m10.8 12.2 9.2-9.2M16 7l3 3M18 5l2 2"/>',
  system: '<rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M9 1v3M15 1v3M9 20v3M15 20v3M20 9h3M20 14h3M1 9h3M1 14h3"/>',
  sql: '<polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/>',
  builder: '<rect x="3" y="3" width="8" height="8" rx="1.5"/><rect x="13" y="13" width="8" height="8" rx="1.5"/><path d="M7 11v4a2 2 0 0 0 2 2h4M17 13V9a2 2 0 0 0-2-2h-4"/>',
  schema: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M9 9v11"/>',
  import: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17 8 12 3 7 8"/><path d="M12 3v12"/>',
  download: '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><path d="M12 15V3"/>',
  streams: '<path d="M2 6c2.5 0 2.5 2 5 2s2.5-2 5-2 2.5 2 5 2 2.5-2 5-2M2 12c2.5 0 2.5 2 5 2s2.5-2 5-2 2.5 2 5 2 2.5-2 5-2M2 18c2.5 0 2.5 2 5 2s2.5-2 5-2 2.5 2 5 2 2.5-2 5-2"/>',
  relevance: '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1.5"/>',
  recsys: '<path d="M12 3l2.6 5.3 5.9.9-4.3 4.1 1 5.8L12 16.4 6.8 19.1l1-5.8L3.5 9.2l5.9-.9z"/>',
  sid: '<path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18"/>',
  autocomplete: '<path d="M4 7V5h16v2M9 19h6M12 5v14"/>',
  media: '<rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="8.5" cy="8.5" r="1.5"/><path d="m21 15-5-5L5 21"/>',
  graph: '<circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><path d="m8.6 13.5 6.8 4M15.4 6.5l-6.8 4"/>',
  agent: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/><path d="M8 9h8M8 13h5"/>',
  classic: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5M12 7v5l3 3"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  x: '<path d="M18 6 6 18M6 6l12 12"/>',
  check: '<polyline points="20 6 9 17 4 12"/>',
  warn: '<path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 16v-4M12 8h.01"/>',
  okc: '<circle cx="12" cy="12" r="9"/><polyline points="16 9 10.5 15 8 12.5"/>',
  bad: '<circle cx="12" cy="12" r="9"/><path d="m15 9-6 6M9 9l6 6"/>',
  play: '<polygon points="6 4 20 12 6 20 6 4"/>',
  pause: '<rect x="6" y="4" width="4" height="16" rx="1"/><rect x="14" y="4" width="4" height="16" rx="1"/>',
  refresh: '<path d="M21 12a9 9 0 0 1-15.4 6.4L3 16M3 12a9 9 0 0 1 15.4-6.4L21 8"/><path d="M21 3v5h-5M3 21v-5h5"/>',
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>',
  moon: '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>',
  left: '<polyline points="15 18 9 12 15 6"/>',
  right: '<polyline points="9 18 15 12 9 6"/>',
  down: '<polyline points="6 9 12 15 18 9"/>',
  up: '<polyline points="18 15 12 9 6 15"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  minus: '<path d="M5 12h14"/>',
  trash: '<path d="M3 6h18M8 6V4h8v2M6 6l1 15h10l1-15M10 11v6M14 11v6"/>',
  copy: '<rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"/>',
  ext: '<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6M15 3h6v6M10 14 21 3"/>',
  filter: '<polygon points="22 3 2 3 10 12.5 10 19 14 21 14 12.5 22 3"/>',
  zin: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5M11 8v6M8 11h6"/>',
  zout: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5M8 11h6"/>',
  fit: '<path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M3 16v3a2 2 0 0 0 2 2h3M16 21h3a2 2 0 0 0 2-2v-3"/>',
  stop: '<circle cx="12" cy="12" r="9"/><rect x="9" y="9" width="6" height="6" rx="1"/>',
  clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
  db: '<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v14c0 1.7 3.6 3 8 3s8-1.3 8-3V5M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/>',
  box: '<path d="M21 16V8a2 2 0 0 0-1-1.7l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.7l7 4a2 2 0 0 0 2 0l7-4a2 2 0 0 0 1-1.7z"/><path d="M3.3 7 12 12l8.7-5M12 22V12"/>',
  cpu: '<rect x="5" y="5" width="14" height="14" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M9 2v3M15 2v3M9 19v3M15 19v3M19 9h3M19 15h3M2 9h3M2 15h3"/>',
  disk: '<path d="M22 12H2M5.5 5h13L22 12v6a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-6z"/><path d="M6 16h.01M10 16h.01"/>',
  mem: '<rect x="2" y="7" width="20" height="10" rx="1.5"/><path d="M6 11v2M10 11v2M14 11v2M18 11v2M6 17v3M18 17v3"/>',
  lock: '<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>',
  unlock: '<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 7.8-1.2"/>',
  user: '<circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0 1 16 0"/>',
  edit: '<path d="M12 20h9M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/>',
  grip: '<circle cx="9" cy="6" r="1"/><circle cx="15" cy="6" r="1"/><circle cx="9" cy="12" r="1"/><circle cx="15" cy="12" r="1"/><circle cx="9" cy="18" r="1"/><circle cx="15" cy="18" r="1"/>',
  bolt: '<polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"/>',
  layers: '<polygon points="12 2 2 7 12 12 22 7 12 2"/><polyline points="2 17 12 22 22 17"/><polyline points="2 12 12 17 22 12"/>',
  menu: '<path d="M3 6h18M3 12h18M3 18h18"/>',
  dots: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  shield: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>',
  flame: '<path d="M12 2c1 4 6 6 6 12a6 6 0 0 1-12 0c0-3 2-5 3-6 0 2 1 3 2 3 0-4 1-6 1-9z"/>',
  wrench: '<path d="M14.7 6.3a4 4 0 0 0 5 5L21 13l-8 8-3.5-3.5 8-8M14.7 6.3 13 4.6a4 4 0 0 0-5.6 5.6l1.7 1.7"/>',
  gauge: '<path d="M12 14l4-4"/><path d="M3.3 19a10 10 0 1 1 17.4 0"/>',
  table: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M3 9h18M3 15h18M9 3v18M15 3v18"/>',
  link: '<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>',
  sparkle: '<path d="M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5 18 18M6 18l2.5-2.5M15.5 8.5 18 6"/>',
  keyboard: '<rect x="2" y="6" width="20" height="12" rx="2"/><path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M7 14h10"/>',
  eye: '<path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/>',
  route: '<circle cx="6" cy="19" r="3"/><circle cx="18" cy="5" r="3"/><path d="M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15"/>',
};

export function icon(name, cls = "") {
  const e = document.createElementNS(SVGNS, "svg");
  e.setAttribute("viewBox", "0 0 24 24");
  e.setAttribute("class", "i " + cls);
  e.setAttribute("aria-hidden", "true");
  e.innerHTML = ICONS[name] || ICONS.info;
  return e;
}

// ---------------------------------------------------------------- storage
export const store = {
  get(k, d) { try { const v = localStorage.getItem("strata.cc." + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem("strata.cc." + k, JSON.stringify(v)); } catch { /* private mode */ } },
  del(k) { try { localStorage.removeItem("strata.cc." + k); } catch { /* private mode */ } },
};

// ---------------------------------------------------------------- events
const handlers = new Map();
export const bus = {
  on(ev, f) { if (!handlers.has(ev)) handlers.set(ev, new Set()); handlers.get(ev).add(f); return () => handlers.get(ev).delete(f); },
  emit(ev, x) { for (const f of handlers.get(ev) || []) { try { f(x); } catch (e) { console.error(e); } } },
};

// ---------------------------------------------------------------- the API
// The token (or API key) is kept for the tab only, under the key the
// query console reads too.
let token = new URLSearchParams(location.search).get("token") || (() => { try { return sessionStorage.getItem("strata.token"); } catch { return null; } })();
if (token) { try { sessionStorage.setItem("strata.token", token); } catch { /* private mode */ } }
if (new URLSearchParams(location.search).has("token")) { try { history.replaceState(null, "", location.pathname + location.hash); } catch { /* old browser */ } }
export function setToken(t) { token = t || null; try { if (t) sessionStorage.setItem("strata.token", t); else sessionStorage.removeItem("strata.token"); } catch { /* private mode */ } bus.emit("token"); }
export const hasToken = () => !!token;

export class ApiError extends Error {
  constructor(status, message, body) { super(message); this.status = status; this.body = body; }
}

/** Calls the server: api("/v1/x"), api("/v1/x", {body}), api(p, {method: "DELETE"}). */
export async function api(path, opts = {}) {
  const init = { method: opts.method || (opts.body === undefined ? "GET" : "POST"), headers: {} };
  if (opts.body !== undefined) {
    if (opts.body instanceof Blob || opts.body instanceof ArrayBuffer || typeof opts.body === "string") { init.body = opts.body; if (opts.type) init.headers["content-type"] = opts.type; }
    else { init.headers["content-type"] = "application/json"; init.body = JSON.stringify(opts.body); }
  }
  if (token) init.headers.authorization = "Bearer " + token;
  if (opts.signal) init.signal = opts.signal;
  const r = await fetch(path, init);
  if (opts.text) { const t = await r.text(); if (!r.ok) throw new ApiError(r.status, t || "HTTP " + r.status); return t; }
  if (opts.blob) { if (!r.ok) throw new ApiError(r.status, "HTTP " + r.status); return r; }
  let j; try { j = await r.json(); } catch { j = {}; }
  if (r.status === 401) bus.emit("unauthorized", path);
  if (!r.ok) throw new ApiError(r.status, j.error || ("HTTP " + r.status), j);
  return j;
}

// ---------------------------------------------------------------- formats
export const fmt = {
  n(v, d = 0) { if (v == null || Number.isNaN(v)) return "–"; return Number(v).toLocaleString(undefined, { maximumFractionDigits: d, minimumFractionDigits: 0 }); },
  /** 1234 → 1.2k */
  si(v, d = 1) {
    if (v == null || Number.isNaN(v)) return "–";
    const a = Math.abs(v);
    if (a >= 1e9) return (v / 1e9).toFixed(d).replace(/\.0+$/, "") + "B";
    if (a >= 1e6) return (v / 1e6).toFixed(d).replace(/\.0+$/, "") + "M";
    if (a >= 1e4) return (v / 1e3).toFixed(d).replace(/\.0+$/, "") + "k";
    if (a >= 100) return v.toFixed(0);
    if (a >= 10) return v.toFixed(1).replace(/\.0$/, "");
    if (a === 0) return "0";
    return v.toFixed(2).replace(/\.?0+$/, "");
  },
  ms(v) {
    if (v == null || Number.isNaN(v)) return "–";
    if (v === 0) return "0";
    if (v >= 60000) return (v / 60000).toFixed(1) + " min";
    if (v >= 1000) return (v / 1000).toFixed(v >= 10000 ? 0 : 2) + " s";
    if (v >= 100) return v.toFixed(0) + " ms";
    if (v >= 10) return v.toFixed(1) + " ms";
    if (v >= 1) return v.toFixed(2) + " ms";
    return (v * 1000).toFixed(0) + " µs";
  },
  bytes(v) {
    if (v == null || Number.isNaN(v)) return "–";
    const u = ["B", "KB", "MB", "GB", "TB", "PB"]; let i = 0; let x = Math.abs(v);
    while (x >= 1024 && i < u.length - 1) { x /= 1024; i++; }
    return (v < 0 ? "-" : "") + (i === 0 ? x.toFixed(0) : x.toFixed(x >= 100 ? 0 : 1)) + " " + u[i];
  },
  pct(v, d = 1) { if (v == null || Number.isNaN(v)) return "–"; return (v * 100).toFixed(d).replace(/\.0+$/, "") + "%"; },
  rate(v) { return v == null ? "–" : fmt.si(v) + "/s"; },
  dur(secs) {
    if (secs == null) return "–";
    const d = Math.floor(secs / 86400), hr = Math.floor(secs % 86400 / 3600), m = Math.floor(secs % 3600 / 60), sec = Math.floor(secs % 60);
    if (d) return `${d}d ${hr}h`; if (hr) return `${hr}h ${m}m`; if (m) return `${m}m ${sec}s`; return `${sec}s`;
  },
  ago(ms) {
    if (!ms) return "never";
    const s = (Date.now() - ms) / 1000;
    if (s < 0) return "in " + fmt.dur(-s);
    if (s < 5) return "just now"; if (s < 60) return Math.floor(s) + " s ago"; if (s < 3600) return Math.floor(s / 60) + " min ago";
    if (s < 86400) return Math.floor(s / 3600) + " h ago"; return Math.floor(s / 86400) + " d ago";
  },
  time(ms, sec = true) { if (!ms) return "–"; const d = new Date(ms); return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: sec ? "2-digit" : undefined, hour12: false }); },
  datetime(ms) { if (!ms) return "–"; return new Date(ms).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }); },
  plural(n, w, ws) { return `${fmt.n(n)} ${n === 1 ? w : (ws || w + "s")}`; },
};

export const debounce = (f, ms = 200) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => f(...a), ms); }; };
export const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
export async function copy(text, what = "Copied") {
  try { await navigator.clipboard.writeText(text); toast(what, "ok"); }
  catch { const t = h("textarea", { style: { position: "fixed", opacity: 0 } }); t.value = text; document.body.append(t); t.select(); document.execCommand("copy"); t.remove(); toast(what, "ok"); }
}

// ---------------------------------------------------------------- SQL
const KW = new Set("SELECT DISTINCT FROM WHERE AND OR NOT IN IS NULL EXISTS BETWEEN ORDER BY GROUP HAVING WITH AS JOIN LEFT RIGHT INNER OUTER CROSS ON LIMIT OFFSET DESC ASC INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE DROP ALTER INDEX PRIMARY KEY REFERENCES DEFAULT UNIQUE CASE WHEN THEN ELSE END UNION ALL ANY LIKE TRUE FALSE PER RANK USING EXPLAIN OVER PARTITION FILTER RECURSIVE IF".split(" "));
const FNS = new Set("MATCH COUNT SUM AVG MIN MAX BM25 KNN NEAR DISTANCE SCORE MINMAX RRF COALESCE LOWER UPPER LENGTH ROUND ABS NOW REACHABLE PATH NEIGHBORS PAGERANK DECAY BOOST MAXSIM DOT COSINE FUZZY PREFIX SID_AT ENCODE RANKED RERANK EMBED HIGHLIGHT SNIPPET TERMS ARRAY_LENGTH CAST DATE_TRUNC AT_LEAST".split(" "));
/** SQL as spans for a <pre class="code">. */
export function highlightSql(sql) {
  const out = [];
  const re = /(--[^\n]*)|('(?:[^']|'')*')|("(?:[^"]|"")*")|(\b\d+(?:\.\d+)?(?:e[+-]?\d+)?\b)|([A-Za-z_][A-Za-z0-9_]*)|(:[A-Za-z_]\w*|\?)|([<>=!~]=?|\|\||[-+*/%])/g;
  let last = 0, m;
  while ((m = re.exec(sql))) {
    if (m.index > last) out.push(document.createTextNode(sql.slice(last, m.index)));
    let cls = null;
    if (m[1]) cls = "tk-cmt"; else if (m[2]) cls = "tk-str"; else if (m[3]) cls = null; else if (m[4]) cls = "tk-num";
    else if (m[5]) { const u = m[5].toUpperCase(); cls = KW.has(u) ? "tk-kw" : FNS.has(u) && sql[re.lastIndex] === "(" ? "tk-fn" : null; }
    else if (m[6]) cls = "tk-op"; else if (m[7]) cls = "tk-p";
    out.push(cls ? h("span", { class: cls, text: m[0] }) : document.createTextNode(m[0]));
    last = re.lastIndex;
  }
  out.push(document.createTextNode(sql.slice(last)));
  return out;
}
export const sqlBlock = (sql, attrs = {}) => h("pre", { class: "code", ...attrs }, highlightSql(sql || ""));

// ---------------------------------------------------------------- overlays
let toastBox;
export function toast(msg, kind = "info", ms = 3800) {
  if (!toastBox) { toastBox = h("div", { class: "toasts", role: "status", "aria-live": "polite" }); document.body.append(toastBox); }
  const ic = kind === "bad" ? "bad" : kind === "ok" ? "okc" : "info";
  const t = h("div", { class: "toast " + kind }, icon(ic), h("div", { class: "grow" }, msg));
  toastBox.append(t);
  setTimeout(() => { t.style.transition = "opacity .3s"; t.style.opacity = "0"; setTimeout(() => t.remove(), 300); }, ms);
}

const layers = [];
function onKey(e) {
  if (e.key === "Escape" && layers.length) { e.preventDefault(); e.stopPropagation(); layers[layers.length - 1](); }
}
document.addEventListener("keydown", onKey, true);
function layer(nodes, close) {
  const prev = document.activeElement;
  document.body.append(...nodes);
  let done = false;
  const f = () => { if (done) return; done = true; nodes.forEach((n) => n.remove()); const i = layers.indexOf(f); if (i >= 0) layers.splice(i, 1); close?.(); prev?.focus?.(); };
  layers.push(f);
  return f;
}

/** A panel from the right: drawer({title, sub, body, foot, wide}). */
export function drawer({ title, sub, body, foot, wide, onClose }) {
  const scrim = h("div", { class: "scrim" });
  const closeBtn = h("button", { class: "btn ghost icon", "aria-label": "Close", title: "Close (Esc)" }, icon("x"));
  const bd = h("div", { class: "bd" }, body);
  const d = h("aside", { class: "drawer" + (wide ? " wide" : ""), role: "dialog", "aria-modal": "true", "aria-label": typeof title === "string" ? title : "Details" },
    h("div", { class: "hd" }, h("div", { class: "grow" }, h("h2", {}, title), sub ? h("div", { class: "small muted" }, sub) : null), closeBtn),
    bd, foot ? h("div", { class: "ft" }, foot) : null);
  const close = layer([scrim, d], onClose);
  scrim.onclick = close; closeBtn.onclick = close;
  setTimeout(() => closeBtn.focus(), 0);
  return { close, el: d, body: bd };
}

/** A dialog in the middle: modal({title, body, foot, wide}). */
export function modal({ title, body, foot, wide, onClose }) {
  const scrim = h("div", { class: "scrim" });
  const m = h("div", { class: "modal" + (wide ? " wide" : ""), role: "dialog", "aria-modal": "true" },
    h("div", { class: "hd" }, h("h2", {}, title)), h("div", { class: "bd" }, body), foot ? h("div", { class: "ft" }, foot) : null);
  const close = layer([scrim, m], onClose);
  scrim.onclick = close;
  setTimeout(() => (m.querySelector("input,textarea,select,button.primary") || m).focus?.(), 0);
  return { close, el: m };
}

/** Asks before doing something: resolves true or false. */
export function confirm({ title, body, ok = "Confirm", danger = false }) {
  return new Promise((resolve) => {
    let answered = false;
    const yes = h("button", { class: "btn " + (danger ? "danger solid" : "primary") }, ok);
    const no = h("button", { class: "btn" }, "Cancel");
    const m = modal({ title, body: typeof body === "string" ? h("p", { style: { margin: 0 }, class: "dim" }, body) : body, foot: [no, yes], onClose: () => { if (!answered) resolve(false); } });
    yes.onclick = () => { answered = true; m.close(); resolve(true); };
    no.onclick = () => { answered = true; m.close(); resolve(false); };
    setTimeout(() => yes.focus(), 0);
  });
}

/** A menu under an element: menu(anchor, [{label, icon, onclick, danger} | "-"]). */
export function menu(anchor, items) {
  const r = anchor.getBoundingClientRect();
  const m = h("div", { class: "menu", role: "menu" }, items.map((it) => it === "-" ? h("hr") :
    h("button", { role: "menuitem", class: it.danger ? "danger" : "", onclick: () => { close(); it.onclick?.(); } }, it.icon ? icon(it.icon) : null, it.label)));
  const scrim = h("div", { style: { position: "fixed", inset: 0, zIndex: 84 } });
  const close = layer([scrim, m]);
  scrim.onclick = close;
  const w = 200;
  m.style.left = Math.min(r.left, innerWidth - w - 8) + "px";
  m.style.top = (r.bottom + 4) + "px";
  setTimeout(() => m.querySelector("button")?.focus(), 0);
  return close;
}

let tipEl;
/** Shows the chart tooltip at a client point; `content` is nodes. */
export function showTip(x, y, content) {
  if (!tipEl) { tipEl = h("div", { class: "tip", role: "tooltip" }); document.body.append(tipEl); }
  clear(tipEl, content);
  tipEl.style.display = "block";
  const w = tipEl.offsetWidth, ht = tipEl.offsetHeight;
  let left = x + 14, top = y + 14;
  if (left + w > innerWidth - 8) left = x - w - 14;
  if (top + ht > innerHeight - 8) top = y - ht - 14;
  tipEl.style.left = Math.max(8, left) + "px";
  tipEl.style.top = Math.max(8, top) + "px";
}
export function hideTip() { if (tipEl) tipEl.style.display = "none"; }
/** A tooltip on hover and focus: tipOn(el, () => nodes). */
export function tipOn(e, content) {
  e.addEventListener("pointermove", (ev) => showTip(ev.clientX, ev.clientY, typeof content === "function" ? content() : content));
  e.addEventListener("pointerleave", hideTip);
  e.addEventListener("focus", () => { const r = e.getBoundingClientRect(); showTip(r.right, r.top, typeof content === "function" ? content() : content); });
  e.addEventListener("blur", hideTip);
  return e;
}

// ---------------------------------------------------------------- widgets
/** A segmented control: seg([[value, label]], value, onChange). */
export function seg(options, value, onChange, attrs = {}) {
  const box = h("div", { class: "seg", role: "group", ...attrs });
  const set = (v) => { value = v; for (const b of box.children) b.setAttribute("aria-pressed", String(b.dataset.v === String(v))); };
  for (const [v, label, title] of options) box.append(h("button", { type: "button", dataset: { v: String(v) }, title, onclick: () => { set(v); onChange(v); } }, label));
  set(value);
  box.set = set;
  return box;
}

export function toggle(checked, onChange, label) {
  const inp = h("input", { type: "checkbox", checked, onchange: () => onChange(inp.checked) });
  const sw = h("label", { class: "switch", title: label }, inp, h("span"));
  return label ? h("label", { class: "row nw", style: { gap: "8px", cursor: "pointer" } }, sw, h("span", { class: "small dim" }, label)) : sw;
}

export function pill(state, text) {
  const ic = { ok: "okc", good: "okc", firing: "bad", down: "bad", failing: "bad", critical: "bad", bad: "bad", pending: "clock", degraded: "warn", warn: "warn", warning: "warn", disabled: "minus", info: "info", serious: "warn" }[state] || "info";
  return h("span", { class: "pill " + state }, icon(ic), text ?? state);
}

export function empty(ic, title, sub, action) {
  return h("div", { class: "empty" }, icon(ic), h("b", {}, title), sub ? h("div", { class: "small" }, sub) : null, action || null);
}

export function card({ title, sub, tools, body, foot, cls = "", flush = false, attrs = {} }) {
  return h("section", { class: "card " + cls, ...attrs },
    title || tools ? h("div", { class: "hd" }, title ? h("h3", {}, title) : null, sub ? h("span", { class: "sub" }, sub) : null, tools ? h("div", { class: "tools" }, tools) : null) : null,
    h("div", { class: "bd" + (flush ? " flush" : "") }, body), foot ? h("div", { class: "ft" }, foot) : null);
}

/**
 * A sortable table. columns: [{key, label, num, render(row), sort(row), width, cls}];
 * opts: {rows, sortKey, desc, onRow(row, tr), rowClass(row), empty, compact, max}.
 * Returns an element with .update(rows).
 */
export function table(columns, opts = {}) {
  let rows = opts.rows || [];
  let sortKey = opts.sortKey ?? null, desc = opts.desc ?? true;
  const thead = h("thead"), tbody = h("tbody");
  const t = h("table", { class: "t" + (opts.compact ? " compact" : "") }, thead, tbody);
  const wrap = h("div", { class: "tbl-wrap", style: opts.maxHeight ? { maxHeight: opts.maxHeight } : null }, t);
  const val = (c, r) => c.sort ? c.sort(r) : r[c.key];
  function head() {
    clear(thead, h("tr", {}, columns.map((c) => {
      const sortable = c.sortable !== false && c.key;
      const th = h("th", { class: (c.num ? "num " : "") + (sortable ? "sort" : ""), style: c.width ? { width: c.width } : null, title: c.title },
        c.label, sortable && sortKey === c.key ? h("span", { class: "arrow" }, desc ? "▼" : "▲") : null);
      if (sortable) th.onclick = () => { if (sortKey === c.key) desc = !desc; else { sortKey = c.key; desc = !!c.num; } head(); body(); };
      return th;
    })));
  }
  function body() {
    let rs = rows.slice();
    if (sortKey) {
      const c = columns.find((x) => x.key === sortKey);
      if (c) rs.sort((a, b) => { const x = val(c, a), y = val(c, b); const r = x == null ? 1 : y == null ? -1 : typeof x === "number" ? x - y : String(x).localeCompare(String(y)); return desc ? -r : r; });
    }
    if (opts.max) rs = rs.slice(0, opts.max);
    if (!rs.length) { clear(tbody, h("tr", {}, h("td", { colspan: columns.length }, opts.empty || empty("info", "Nothing here yet")))); return; }
    clear(tbody, rs.map((r) => {
      const tr = h("tr", { class: (opts.onRow ? "click " : "") + (opts.rowClass ? opts.rowClass(r) : "") },
        columns.map((c) => h("td", { class: (c.num ? "num " : "") + (c.cls || "") }, c.render ? c.render(r) : r[c.key] == null ? "–" : String(r[c.key]))));
      if (opts.onRow) { tr.onclick = (e) => { if (e.target.closest("button,a,input")) return; opts.onRow(r, tr); }; tr.tabIndex = 0; tr.onkeydown = (e) => { if (e.key === "Enter") opts.onRow(r, tr); }; }
      return tr;
    }));
  }
  head(); body();
  wrap.update = (r) => { rows = r || []; body(); };
  wrap.rows = () => rows;
  return wrap;
}

/** A small bar of a share (0..1), for a table cell. */
export function shareBar(v, color = "var(--s1)") {
  return h("div", { class: "row nw", style: { gap: "8px" } }, h("div", { class: "barbox" }, h("div", { class: "bar", style: { width: clamp(v * 100, 0, 100) + "%", background: color } })), h("span", { class: "small dim num" }, fmt.pct(v)));
}

/** Makes children of `list` reorderable by dragging; onOrder(newIndices). */
export function sortable(list, onOrder) {
  let src = null;
  list.addEventListener("dragstart", (e) => { src = e.target.closest("[draggable=true]"); if (!src) return; src.classList.add("dragging-src"); e.dataTransfer.effectAllowed = "move"; e.dataTransfer.setData("text/plain", ""); });
  list.addEventListener("dragend", () => { src?.classList.remove("dragging-src"); src = null; onOrder?.([...list.children]); });
  list.addEventListener("dragover", (e) => {
    if (!src) return; e.preventDefault();
    const over = e.target.closest("[draggable=true]");
    if (!over || over === src || over.parentNode !== list) return;
    const r = over.getBoundingClientRect();
    const after = (e.clientY - r.top) > r.height / 2;
    list.insertBefore(src, after ? over.nextSibling : over);
  });
}

/** A file drop area: dropzone({label, accept, onFiles}). */
export function dropzone({ label, sub, accept, multiple = true, onFiles }) {
  const inp = h("input", { type: "file", accept, multiple, style: { display: "none" }, onchange: () => { if (inp.files.length) onFiles([...inp.files]); inp.value = ""; } });
  const z = h("div", { class: "dropzone", tabIndex: 0, role: "button" }, icon("import", "lg"), h("div", { style: { fontWeight: 600, marginTop: "6px" } }, label), sub ? h("div", { class: "small" }, sub) : null, inp);
  z.onclick = () => inp.click();
  z.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); inp.click(); } };
  z.ondragover = (e) => { e.preventDefault(); z.classList.add("over"); };
  z.ondragleave = () => z.classList.remove("over");
  z.ondrop = (e) => { e.preventDefault(); z.classList.remove("over"); if (e.dataTransfer.files.length) onFiles([...e.dataTransfer.files]); };
  return z;
}

/** Repeats `f` every `ms` while the page is visible; returns a stopper. */
export function every(ms, f, now = true) {
  let stopped = false, timer = null, running = false;
  const tick = async () => {
    if (stopped) return;
    if (!document.hidden && !running) { running = true; try { await f(); } catch (e) { console.warn(e); } running = false; }
    if (!stopped) timer = setTimeout(tick, ms);
  };
  if (now) tick(); else timer = setTimeout(tick, ms);
  return () => { stopped = true; clearTimeout(timer); };
}

/** What an error says, for a view that could not load. */
export function failure(e, what) {
  if (e instanceof ApiError && e.status === 403) return empty("lock", `${what} needs an admin key`, e.message, h("button", { class: "btn sm", onclick: () => bus.emit("unauthorized") }, icon("access"), "Use another key"));
  if (e instanceof ApiError && e.status === 401) return empty("lock", "Sign in to see " + what.toLowerCase(), "This server needs a token or an API key.", h("button", { class: "btn sm primary", onclick: () => bus.emit("unauthorized") }, "Enter a key"));
  return h("div", { class: "error" }, `${what}: ${e.message}`);
}
