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
    else if (k === "style" && typeof v === "object") for (const [p, x] of Object.entries(v)) { if (p.startsWith("--")) e.style.setProperty(p, x); else e.style[p] = x; }
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
  sliders: '<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>',
  undo: '<path d="M9 14 4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 0 10h-3"/>',
  redo: '<path d="m15 14 5-5-5-5"/><path d="M20 9H9a5 5 0 0 0 0 10h3"/>',
  reset: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/>',
  route: '<circle cx="6" cy="19" r="3"/><circle cx="18" cy="5" r="3"/><path d="M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15"/>',
  rollout: '<path d="M3 12h4l3 7 4-14 3 7h4"/>',
  autoscale: '<path d="M7 20V6M3.5 9.5 7 6l3.5 3.5"/><path d="M17 4v14M13.5 14.5 17 18l3.5-3.5"/>',
  cloud: '<path d="M7 18h10.5a4 4 0 0 0 .6-7.95A6 6 0 0 0 6.3 9.1 4.5 4.5 0 0 0 7 18z"/>',
  cluster: '<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>',
  timeline: '<path d="M3 12h18"/><circle cx="7" cy="12" r="2"/><circle cx="14" cy="12" r="2"/><path d="M7 5v3M14 16v3M19 5v4"/>',
  bookmark: '<path d="M6 3h12v18l-6-4-6 4z"/>',
  rocket: '<path d="M5 15c-1.5 1-2 4-2 6 2 0 5-.5 6-2"/><path d="M9 18 6 15c1-4 4-9 11-12 0 0 1 1 1 1-3 7-8 10-12 11z"/><circle cx="14.5" cy="9.5" r="1.5"/>',
  pin: '<path d="M12 17v5M9 3h6l-1 5 3 3v2H7v-2l3-3z"/>',
  share: '<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>',
  note: '<path d="M4 4h16v12l-4 4H4z"/><path d="M16 20v-4h4M8 9h8M8 13h5"/>',
  labs: '<path d="M9 3h6M10 3v6l-5 9a2 2 0 0 0 1.7 3h10.6a2 2 0 0 0 1.7-3l-5-9V3"/><path d="M7.5 15h9"/>',
  node: '<rect x="3" y="4" width="18" height="7" rx="1.5"/><rect x="3" y="13" width="18" height="7" rx="1.5"/><path d="M7 7.5h.01M7 16.5h.01"/>',
  shard: '<path d="M21 16V8a2 2 0 0 0-1-1.7l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.7l7 4a2 2 0 0 0 2 0l7-4a2 2 0 0 0 1-1.7z"/><path d="M3.3 7 12 12l8.7-5M12 22V12"/>',
  key: '<circle cx="8" cy="15" r="4"/><path d="m10.8 12.2 9.2-9.2M16 7l3 3M18 5l2 2"/>',
  feedback: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/><path d="M12 7v4M12 14h.01"/>',
  camera: '<path d="M4 8h3l2-3h6l2 3h3a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V9a1 1 0 0 1 1-1z"/><circle cx="12" cy="13.5" r="3.5"/>',
  fbbox: '<rect x="4" y="5" width="16" height="14" rx="1.5"/>',
  fbarrow: '<path d="M5 19 19 5M10 5h9v9"/>',
  fbtext: '<path d="M5 6V4h14v2M12 4v16M9 20h6"/>',
  fbredact: '<rect x="3" y="8" width="18" height="8" rx="1" fill="currentColor"/>',
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
  get(k, d) { try { const v = localStorage.getItem("chloedb.cc." + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem("chloedb.cc." + k, JSON.stringify(v)); } catch { /* private mode */ } },
  del(k) { try { localStorage.removeItem("chloedb.cc." + k); } catch { /* private mode */ } },
};

// ---------------------------------------------------------------- events
const handlers = new Map();
export const bus = {
  on(ev, f) { if (!handlers.has(ev)) handlers.set(ev, new Set()); handlers.get(ev).add(f); return () => handlers.get(ev).delete(f); },
  emit(ev, x) { for (const f of handlers.get(ev) || []) { try { f(x); } catch (e) { console.error(e); } } },
};

// ---------------------------------------------------------------- narrow screens
/**
 * The breakpoints, the same numbers as the CSS: below NARROW (900 px, a
 * tablet or a phone on its side) the sidebar is a drawer behind a menu
 * button, the range and Share sit in a menu, and drawers, dialogs and the
 * palette are full-screen sheets; below PHONE (560 px, a phone) one column.
 * In CSS: `@media (max-width: 900px)` and `@media (max-width: 560px)`.
 * `narrow` on the bus fires when the width crosses either one (with
 * isNarrow()); a view that lays out in JS redraws then.
 */
export const NARROW = 900, PHONE = 560;
const mq = (px) => { try { return matchMedia(`(max-width: ${px}px)`); } catch { return null; } };
const narrowMq = mq(NARROW), phoneMq = mq(PHONE);
/** True below 900 px. */
export const isNarrow = () => !!narrowMq?.matches;
/** True below 560 px (a phone held upright). */
export const isPhone = () => !!phoneMq?.matches;
for (const q of [narrowMq, phoneMq]) q?.addEventListener?.("change", () => bus.emit("narrow", isNarrow()));

// ---------------------------------------------------------------- the API
// The hub (chloedb-server's crate::hub): at /c/<name>, the page is the
// command center of that connection of this server's book, and its calls
// go to /c/<name>/v1/..., which the server forwards with that cluster's key.
const hubMatch = location.pathname.match(/^\/c\/([a-z0-9][a-z0-9-]{0,39})\/?$/);
/** The hub connection this page works on, or null on the server's own cluster. */
export const connectionName = hubMatch ? hubMatch[1] : null;
const BASE = connectionName ? "/c/" + connectionName : "";
/** A path of the server as this page calls it: through the hub on a connection. */
export const at = (p) => (BASE && typeof p === "string" && /^\/(v1\/|livez|readyz|metrics)/.test(p) ? BASE + p : p);

// The token (or API key) is kept for the tab only, under the key the
// query console reads too.
let token = new URLSearchParams(location.search).get("token") || (() => { try { return sessionStorage.getItem("chloedb.token"); } catch { return null; } })();
if (token) { try { sessionStorage.setItem("chloedb.token", token); } catch { /* private mode */ } }
if (new URLSearchParams(location.search).has("token")) { try { history.replaceState(null, "", location.pathname + location.hash); } catch { /* old browser */ } }
export function setToken(t) { token = t || null; try { if (t) sessionStorage.setItem("chloedb.token", t); else sessionStorage.removeItem("chloedb.token"); } catch { /* private mode */ } bus.emit("token"); }
export const hasToken = () => !!token;
/** The Authorization header, for requests `api` cannot make (an upload with progress). */
export const authHeader = () => (token ? { authorization: "Bearer " + token } : {});

/**
 * A failed call. `status` is the HTTP status, 0 when nothing answered (a
 * network failure or a timeout). `answered` is false when the answer did
 * not come from ChloeDB (nothing answered, or a proxy's page that is not
 * JSON): the server counts as not answering then.
 */
export class ApiError extends Error {
  constructor(status, message, body, answered = true) { super(message); this.status = status; this.body = body; this.answered = answered; }
}

// Whether the server answers: every call of `api` says. A call answered by
// ChloeDB (any status, its JSON) means yes; a network failure, a timeout or
// an answer that is not ChloeDB's JSON (an ingress's 502 page) means no.
const conn = { ok: true, since_ms: null, status: null, message: null, path: null, last_ok_ms: null, last_fail_ms: null };
/** `{ ok, since_ms, status, message, path, last_ok_ms }`: whether the server answers, and since when it does not. */
export const connection = () => ({ ...conn });
function answered() {
  conn.last_ok_ms = Date.now();
  if (conn.ok) return;
  Object.assign(conn, { ok: true, since_ms: null, status: null, message: null, path: null });
  bus.emit("connection", connection());
}
// An app's own request (/v1/apps/<name>/...) that fails says nothing of the
// server: the server forwards it and answers for an app that is down, so a
// request lost on its way (an app page's load cut off, a blocked request)
// is that page's to say, not the whole command center's (APC-27). If the
// server itself is gone, its own polls say so.
const APP_PATH = /^\/v1\/apps\/[^/]+\/./;
function notAnswering(status, message, path) {
  if (APP_PATH.test(String(path || "").split("?")[0])) return;
  const now = Date.now();
  conn.last_fail_ms = now;
  const was = conn.ok;
  Object.assign(conn, { ok: false, since_ms: was ? now : conn.since_ms, status, message, path });
  bus.emit("connection", connection());
}
/** What an HTTP status means when it did not come from ChloeDB. */
function proxyWhy(status) {
  return status === 502 ? "502 from the ingress or proxy" : status === 503 ? "503 from the ingress or proxy" : status === 504 ? "504: the ingress or proxy gave up waiting"
    : status >= 500 ? `${status} that is not ChloeDB's` : `an answer that is not ChloeDB's (HTTP ${status})`;
}
// Header values are Latin-1: anything else in a reason goes percent-encoded.
const headerSafe = (t) => String(t).replace(/[\r\n]+/g, " ").replace(/[^\x20-\x7e]/g, (c) => encodeURIComponent(c)).slice(0, 2000);

/**
 * Calls the server: api("/v1/x"), api("/v1/x", {body}), api(p, {method: "DELETE"}).
 * On a hub connection's page the call goes to that cluster; `hub: true`
 * calls the hub itself (its book of connections).
 * opts: {method, body, type, signal, text, blob, reason, timeout, hub}. `reason`
 * goes in the `x-strata-reason` header (the audit entry keeps it); `timeout`
 * (ms) gives up and counts as not answering. Every call says it comes from
 * the command center (`x-strata-client`), so the server leaves the UI's own
 * calls out of the routes and queries it shows.
 */
// A GET a key limited to tables was refused ("... is not for a key limited
// to tables"): its path (without the query) is not asked again for that key,
// so that a page polling it does not send a refused request a poll. The
// refusal is thrown again, as the server gave it.
const refusedFor = { token: null, paths: new Map() };
const routeOf = (path) => String(path).split("?")[0];
/** The refusal of a route this key was refused as a key limited to tables, or null. */
export function refusedRoute(path) {
  if (refusedFor.token !== token) return null;
  return refusedFor.paths.get(routeOf(path)) || null;
}

// The key a 401 answered (null: no key), until another is given: the
// polls hold meanwhile (`every`), so a tab without a valid key does not
// ask the server every few seconds, nor fill its audit log with refusals.
let refusedKey;
/** Whether the server refused the key in use (or wants one): the views say "Sign in", the polls wait. */
export const signInNeeded = () => refusedKey !== undefined && refusedKey === token;

// The server's own views (statements, logs, index sizes, keys) are read by
// an admin, or an ops or propose key: whoami says so (`may.server_views`).
// Another key's page does not ask for them, to be refused (and audited) a poll.
const SERVER_VIEWS = /^\/v1\/(admin\/|queries|logs|profile|index)/;
function noServerViews(path) {
  const w = ctx.whoami;
  if (!w || w.root || w.may?.server_views !== false || !SERVER_VIEWS.test(routeOf(path))) return null;
  return `${routeOf(path)} is one of the server's views: it needs an admin key (or an ops or propose key), and "${w.name || "this key"}" has ${w.scope}`;
}

export function api(path, opts = {}) {
  // LIV-14: cards that ask the same thing at once share one request; each
  // gets its own copy of the answer.
  if (opts.method || opts.body !== undefined || opts.blob || opts.signal || opts.reason) return answer(path, opts);
  const key = `${opts.hub ? "hub " : ""}${opts.text ? "text " : ""}${token || ""} ${path}`;
  const was = inflight.get(key);
  if (was) return was.then((j) => (j && typeof j === "object" ? structuredClone(j) : j));
  const p = answer(path, opts).finally(() => inflight.delete(key));
  inflight.set(key, p);
  return p;
}
const inflight = new Map();
async function answer(path, opts) {
  try { return await request(path, opts); }
  catch (e) {
    // An agent's key says why it changes the server (the audit log keeps
    // it): asked once, then the request is sent again with it.
    if (e?.body?.error_code !== "reason_required" || opts.reason || opts.blob) throw e;
    // Several at once (Acknowledge all) share one question.
    asking ||= confirm({ title: "Say why", body: `This key is an agent's (${ctx.whoami?.name || "a key"}${roleOf() ? `, ${roleOf()}` : ""}): the server keeps a reason with each change it makes, in the audit log.`, ok: "Send", reason: "Why: a sentence, kept in the audit log" }).finally(() => { asking = null; });
    const ok = await asking;
    if (!ok) throw e;
    return request(path, { ...opts, reason: ok.reason });
  }
}
let asking = null;

async function request(path, opts) {
  const get = !opts.method && opts.body === undefined;
  const held = get && !opts.hub ? refusedRoute(path) || noServerViews(path) : null;
  if (held) throw new ApiError(403, held, { error: held, limited: !!ctx.whoami?.tables, local: true });
  const init = { method: opts.method || (opts.body === undefined ? "GET" : "POST"), headers: { "x-strata-client": "command-center" } };
  if (opts.body !== undefined) {
    if (opts.body instanceof Blob || opts.body instanceof ArrayBuffer || typeof opts.body === "string") { init.body = opts.body; if (opts.type) init.headers["content-type"] = opts.type; }
    else { init.headers["content-type"] = "application/json"; init.body = JSON.stringify(opts.body); }
  }
  if (token) init.headers.authorization = "Bearer " + token;
  if (opts.reason) init.headers["x-chloedb-reason"] = headerSafe(opts.reason);
  let timer = null, timedOut = false;
  if (opts.timeout) {
    const ac = new AbortController();
    opts.signal?.addEventListener("abort", () => ac.abort());
    timer = setTimeout(() => { timedOut = true; ac.abort(); }, opts.timeout);
    init.signal = ac.signal;
  } else if (opts.signal) init.signal = opts.signal;
  let r;
  try { r = await fetch(opts.hub ? path : at(path), init); }
  catch (e) {
    clearTimeout(timer);
    if (opts.signal?.aborted && !timedOut) throw e; // the caller cancelled it
    const why = timedOut ? `no answer in ${fmt.ms(opts.timeout)}` : "no answer: the network failed or the server is not running";
    notAnswering(0, why, path);
    throw new ApiError(0, why, null, false);
  }
  clearTimeout(timer);
  if (r.status === 401 && !opts.hub) refusedKey = token;
  if (opts.blob) {
    if (r.ok) { answered(); return r; }
    const t = await r.text().catch(() => "");
    const j = parseJson(t);
    if (j === undefined && r.status >= 500) { notAnswering(r.status, proxyWhy(r.status), path); throw new ApiError(r.status, proxyWhy(r.status), null, false); }
    answered();
    if (r.status === 401) bus.emit("unauthorized", path);
    throw new ApiError(r.status, j?.error || "HTTP " + r.status, j || {});
  }
  const t = await r.text().catch(() => null);
  if (t == null) { notAnswering(r.status || 0, "the answer was cut off", path); throw new ApiError(r.status || 0, "the answer was cut off", null, false); }
  if (opts.text) {
    // A page that is not ChloeDB's error: a proxy answered for it.
    if (!r.ok && r.status >= 500 && parseJson(t) === undefined) { notAnswering(r.status, proxyWhy(r.status), path); throw new ApiError(r.status, proxyWhy(r.status), null, false); }
    answered();
    if (r.status === 401) bus.emit("unauthorized", path);
    if (!r.ok) throw new ApiError(r.status, parseJson(t)?.error || t || "HTTP " + r.status, parseJson(t) || {});
    return t;
  }
  let j = t.trim() === "" ? {} : parseJson(t);
  // ChloeDB always answers JSON here (its errors say {"error": ...}). Anything
  // else came from something in front of it.
  if (j === undefined || (r.status >= 500 && !isStrataError(j))) {
    notAnswering(r.status, proxyWhy(r.status), path);
    throw new ApiError(r.status, proxyWhy(r.status), null, false);
  }
  answered();
  if (r.status === 401) bus.emit("unauthorized", path);
  // A route this key's scope or its tables do not reach is not asked again for it.
  if (r.status === 403 && get && !opts.hub && typeof j?.error === "string" && (j.error.includes("not for a key limited to tables") || j.error_code === "scope_refused")) {
    if (refusedFor.token !== token) { refusedFor.token = token; refusedFor.paths = new Map(); }
    refusedFor.paths.set(routeOf(path), j.error);
    j.limited = !!ctx.whoami?.tables || j.error.includes("not for a key limited to tables");
  }
  if (!r.ok) throw new ApiError(r.status, j?.error || ("HTTP " + r.status), j);
  return j;
}
function parseJson(t) { try { return JSON.parse(t); } catch { return undefined; } }
const isStrataError = (j) => !!j && typeof j === "object" && (typeof j.error === "string" || typeof j.status === "string");

// ---------------------------------------------------------------- shared context
// data.js fills this (who is signed in, the cluster, the time range, pause),
// so that the helpers here read it without importing data.js.
const ctx = { whoami: null, cluster: null, paused: false, rangeParams: () => ({}) };
/** For data.js: provide({whoami, cluster, paused, rangeParams}). */
export function provide(x) { Object.assign(ctx, x); }
/** Whether the live view is paused (data.js `setPaused`). */
export const isPaused = () => ctx.paused;

// ---------------------------------------------------------------- scope
const RANK = { feed: 1, read: 2, write: 3, admin: 4 };
// The routes as the server lists them (GET /v1/catalog: each one's scope,
// tier, and whether the ops and propose roles may call it), for a key that
// acts on the server by its role rather than its scope.
let ROUTES = null;
/** For data.js: the catalog's routes ([{method, path, scope, tier, roles}]). */
export function provideRoutes(list) { ROUTES = list ? new Map(list.map((r) => [`${r.method} ${r.path}`, r])) : null; }
/** A key for operations' role (whoami's scope "ops" or "propose"), or null. */
export const roleOf = (w = ctx.whoami) => (w && !RANK[w.scope] && w.may?.server_views ? w.scope : null);
/** What a key may do to the data: its scope, or for a role key what whoami's `may.data` says (read). */
const dataScope = (w) => (RANK[w.scope] ? w.scope : w.may?.data || "read");
/**
 * Whether the signed-in key may do this: can("feed" | "read" | "write" |
 * "admin"), and, for an action on the server, its route ("POST
 * /v1/alerts/:rule/ack"): the server's catalog then says which scope it
 * needs and whether an ops or propose key may call it (the bounded,
 * tier 1 actions for ops; the server's views for both). Unknown (not
 * signed in, or whoami failed) can read only. Views hide or disable what
 * they cannot do, with `needs(scope, route)` as the tooltip.
 */
export function can(scope, route = null) {
  const w = ctx.whoami;
  if (!w) return scope === "read";
  if (w.root) return true;
  const r = route ? ROUTES?.get(route) : null;
  const need = r?.scope || scope;
  // A key limited to tables is never the server's admin: its admin is its
  // tables' (CREATE, ALTER, DROP).
  if (w.tables && (RANK[need] || RANK.admin) >= RANK.admin) return false;
  if ((RANK[dataScope(w)] || 0) >= (RANK[need] || RANK.admin)) return true;
  const role = roleOf(w);
  return !!(role && r?.roles?.[role]);
}
/** Whether an action's request needs a reason from this key (an agent's: whoami's `may.reason_required`). */
export const reasonNeeded = () => !!ctx.whoami?.may?.reason_required;
/** The tooltip of an action the key cannot take: "needs an admin key", or for a bounded action "needs a write or an ops key". */
export function needs(scope, route = null) {
  const r = route ? ROUTES?.get(route) : null;
  const need = r?.scope || scope;
  if (ctx.whoami?.tables && need === "admin") return "needs an admin key of the server (this key is limited to tables)";
  const a = need === "admin" ? "an admin" : `a ${need}`;
  const ops = route && (r ? r.roles?.ops : false);
  const role = roleOf();
  if (role === "propose" && ops) return `needs ${a} or an ops key (this key is propose: it files change requests)`;
  if (ops) return `needs ${a} or an ops key`;
  if (role) return `needs ${a} key (this key is ${role}: it files a change request instead)`;
  return `needs ${a} key`;
}
/** Whether the signed-in key is limited to tables (whoami's `tables`). */
export const limited = () => !!ctx.whoami?.tables;
/** A key's grants in words: "swarm_* admin, * read" (whoami's or a key's `tables`). */
export const grantsText = (tables) => Object.entries(tables || {}).map(([t, s]) => `${t} ${s}`).join(", ");
/** What the header says of a key: its scope, or for a key limited to tables its grants (and its preset). */
export function keyLabel(w) {
  if (!w) return "";
  if (!w.tables) return w.scope;
  return (w.preset ? w.preset + ": " : "") + grantsText(w.tables);
}
/** Why a page shows nothing for an error `e`: "this key can't see …" for a key limited to tables, else the error. */
export function cannotSee(e, what) {
  if (e?.status === 403 && (e.body?.limited || limited())) return `This key can't see ${what}: it is limited to tables (${grantsText(ctx.whoami?.tables)}).`;
  return e?.message || String(e);
}
/** Disables a button the key cannot use, and says why: gate(btn, "admin"). Returns the button. */
export function gate(btn, scope, route = null) {
  if (!can(scope, route)) { btn.disabled = true; btn.title = (btn.title ? btn.title + ": " : "") + needs(scope, route); btn.classList.add("gated"); }
  return btn;
}

// ---------------------------------------------------------------- copies
/** The shard and replica of a copy's URL in a /v1/cluster answer, or null. */
export function findCopy(url, cluster = ctx.cluster) {
  if (!url || !cluster?.shards) return null;
  const u = String(url).replace(/\/+$/, "");
  for (let i = 0; i < cluster.shards.length; i++) {
    const sh = cluster.shards[i];
    const reps = sh.replicas || [];
    for (let j = 0; j < reps.length; j++) if (String(reps[j].url).replace(/\/+$/, "") === u) return { shard: sh.shard ?? i, copy: j, sh, rep: reps[j] };
  }
  return null;
}
const hostPort = (url) => String(url).replace(/^[a-z]+:\/\//, "").replace(/\/.*$/, "");
/**
 * The short name of a copy, as the cluster names it from its address
 * ("s1-0" for a pod the operator runs, "a0", "127.0.0.1:8004"); host:port
 * for a URL the cluster does not list.
 */
export function copyName(url, cluster = ctx.cluster) {
  if (!url) return "–";
  const f = findCopy(url, cluster);
  if (!f) return hostPort(url);
  // The server's own texts (fleet health, events) say the same name; the node name goes in the title.
  return f.rep.short || `s${f.shard}-${f.copy}`;
}
/** The long form, for a tooltip: "s1-0: shard 1, copy 0, primary · node … · http://…". */
export function copyTitle(url, cluster = ctx.cluster) {
  if (!url) return "";
  const f = findCopy(url, cluster);
  if (!f) return String(url);
  const r = f.rep;
  const role = r.role === "unknown" ? `not answering${r.last_role ? ` (was ${r.last_role})` : ""}` : r.role;
  return [`${copyName(url, cluster)}: shard ${f.shard}${role ? ", " + role : ""}`, r.name && r.name !== r.node ? `server ${r.name}` : null, r.node ? `node ${r.node}` : null, r.zone ? `zone ${r.zone}` : null, String(url)].filter(Boolean).join(" · ");
}

// ---------------------------------------------------------------- links
/**
 * A link into a view that keeps the global time range: link("logs", {node: "s1-0"})
 * → "#/logs?node=s1-0&range=6h" (or &from=…&to=… for a fixed window).
 * `view` may hold a path ("alert/disk-full"); null and "" params are left out;
 * from/to given in params win over the global range.
 */
export function link(view, params = {}) {
  const q = new URLSearchParams();
  const own = Object.entries(params || {}).filter(([, v]) => v != null && v !== "");
  for (const [k, v] of own) q.set(k, String(v));
  if (!q.has("from") && !q.has("to") && !q.has("range")) for (const [k, v] of Object.entries(ctx.rangeParams() || {})) if (v != null) q.set(k, String(v));
  const qs = q.toString();
  return "#/" + String(view).replace(/^#?\/?/, "") + (qs ? "?" + qs : "");
}

/**
 * A link to Logs with its search: logsLink({ server, shard, level, request,
 * text }, params). `server` is a copy's URL (written as its short name), a
 * short name or a node name; `level: "warn"` finds warn and worse. The global
 * range goes along (`params.from`/`to` win), as with `link`.
 * logsLink({ server: url, level: "warn" }) -> "#/logs?q=server:s1-0 level>=warn&range=6h".
 */
export function logsLink({ server, shard, level, request, text } = {}, params = {}) {
  const sv = server && /^https?:\/\//.test(server) ? copyName(server) : server;
  const q = [sv ? `server:${sv}` : null, shard != null && shard !== "" ? `shard:${shard}` : null, level ? `level>=${level}` : null, request ? `request:${request}` : null, text || null].filter(Boolean).join(" ");
  return link("logs", { ...params, q: q || null });
}

// ---------------------------------------------------------------- times
/** How long ago: "just now", "12 s ago", "4 min ago", "3 h ago", "2 d ago" ("in 5 min" ahead). */
export const ago = (ms) => fmt.ago(ms);
const sameDay = (a, b) => a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
/** An absolute local time: "19:42:15" today, "Nov 2 19:42" another day; "–" when unknown. */
export function when(ms, { sec = true } = {}) {
  if (ms == null || !Number.isFinite(+ms) || +ms <= 0) return "–";
  const d = new Date(+ms);
  const t = d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: sec ? "2-digit" : undefined, hour12: false });
  if (sameDay(d, new Date())) return t;
  return d.toLocaleDateString("en-US", { month: "short", day: "numeric", year: d.getFullYear() === new Date().getFullYear() ? undefined : "numeric" }) + " " + t.slice(0, 5);
}
/** The full time with its zone, for a title: "Mon, Nov 2, 2026, 19:42:15 GMT+1 (Europe/Madrid)". */
export function whenTitle(ms) {
  if (ms == null || !Number.isFinite(+ms) || +ms <= 0) return "";
  const d = new Date(+ms);
  let zone = ""; try { zone = Intl.DateTimeFormat().resolvedOptions().timeZone; } catch { /* old browser */ }
  return d.toLocaleString("en-US", { weekday: "short", month: "short", day: "numeric", year: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false, timeZoneName: "short" }) + (zone ? ` (${zone})` : "");
}
/** A <time> element showing when(ms), with the full time and zone (and how long ago) in its title. */
export function timeEl(ms, opts = {}) {
  return h("time", { datetime: ms ? new Date(+ms).toISOString() : null, title: ms ? `${whenTitle(ms)} · ${fmt.ago(ms)}` : null, class: opts.cls || null }, when(ms, opts));
}
/** A window as text: "19:12–20:12" today, "Nov 1 19:12–20:12", "Nov 1 22:00 – Nov 2 08:00". */
export function span(from, to) {
  if (!from || !to) return "–";
  const a = new Date(+from), b = new Date(+to);
  const hm = (d) => d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false });
  const day = (d) => d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
  if (sameDay(a, b)) return (sameDay(a, new Date()) ? "" : day(a) + " ") + hm(a) + "–" + hm(b);
  return `${day(a)} ${hm(a)} – ${day(b)} ${hm(b)}`;
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
  /**
   * A value of the data (a cell, a chart's point, a Sheets cell's sum): the
   * one number format of every grid and chart, so a number reads the same in
   * each. Grouped digits, 2 decimals from 1,000 up and up to 6 below, and an
   * exponent outside 1e-4 to 1e15, where grouped digits stop being read.
   */
  num(v) {
    if (v == null || v === "" || Number.isNaN(Number(v))) return "–";
    v = Number(v);
    if (!Number.isFinite(v)) return String(v);
    const a = Math.abs(v);
    if (a !== 0 && (a >= 1e15 || a < 1e-4)) return v.toExponential(4).replace(/\.?0+e/, "e");
    return v.toLocaleString(undefined, { maximumFractionDigits: a >= 1000 ? 2 : 6 });
  },
  /** A chart axis's ticks: about `n` round values (1, 2, 2.5 or 5 times a power of ten) from lo to hi. */
  ticks(lo, hi, n = 4) {
    if (!(hi > lo)) hi = lo + 1;
    const raw = (hi - lo) / Math.max(1, n), mag = 10 ** Math.floor(Math.log10(raw)), f = raw / mag;
    const step = (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * mag;
    const out = [];
    for (let t = Math.floor(lo / step) * step; t <= Math.ceil(hi / step) * step + step / 2; t += step) out.push(+t.toPrecision(12));
    return out;
  },
  /** A tick's label: short (1.5M, 250k), as round as the ticks are. */
  tick(v) {
    const a = Math.abs(v);
    const short = (x, u) => +x.toPrecision(3) + u;
    return a >= 1e12 ? short(v / 1e12, "T") : a >= 1e9 ? short(v / 1e9, "B") : a >= 1e6 ? short(v / 1e6, "M") : a >= 1e4 ? short(v / 1e3, "k") : fmt.num(+v.toPrecision(6));
  },
  /** How long until a disk fills, from days: minutes or hours under a day, as the server's texts say it. */
  fills(days) {
    if (days == null || !Number.isFinite(days)) return "–";
    if (days < 1 / 24) return `${Math.max(1, Math.round(days * 1440))} min`;
    if (days < 1) return `${Math.round(days * 24)} h`;
    return days < 10 ? `${(Math.round(days * 10) / 10).toString()} days` : `${Math.round(days)} days`;
  },
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

/**
 * Asks before doing something. Focus starts on Cancel, so Enter alone never
 * confirms. `typed: "audit_orders"` keeps OK disabled until that text is
 * typed; `reason: true` (or a placeholder string) asks why, required, and the
 * answer goes in the audit entry (pass it to `api` as `opts.reason`).
 * Resolves `false`, or `{ reason }` (truthy; reason is null when not asked).
 */
export function confirm({ title, body, ok = "Confirm", danger = false, typed = null, reason = false }) {
  return new Promise((resolve) => {
    let answered = false;
    const yes = h("button", { class: "btn " + (danger ? "danger solid" : "primary") }, ok);
    const no = h("button", { class: "btn" }, "Cancel");
    const typedIn = typed != null ? h("input", { class: "input", autocomplete: "off", spellcheck: "false", "aria-label": `Type ${typed} to confirm`, style: { width: "100%" } }) : null;
    const reasonIn = reason ? h("textarea", { class: "input", rows: 2, placeholder: typeof reason === "string" ? reason : "Why: kept in the audit log with the change", "aria-label": "Reason", style: { width: "100%", resize: "vertical" } }) : null;
    const ready = () => (!typedIn || typedIn.value.trim() === String(typed)) && (!reasonIn || reasonIn.value.trim().length > 0);
    const sync = () => { yes.disabled = !ready(); };
    const parts = [
      typeof body === "string" ? h("p", { style: { margin: 0 }, class: "dim" }, body) : body,
      typedIn ? h("label", { class: "confirm-field" }, h("span", { class: "small dim" }, "Type ", h("b", { class: "mono" }, String(typed)), " to confirm"), typedIn) : null,
      reasonIn ? h("label", { class: "confirm-field" }, h("span", { class: "small dim" }, "Reason (required)"), reasonIn) : null,
    ];
    const m = modal({ title, body: parts, foot: [no, yes], onClose: () => { if (!answered) resolve(false); } });
    const go = () => { if (!ready()) return; answered = true; const why = reasonIn ? reasonIn.value.trim() : null; m.close(); resolve({ reason: why }); };
    yes.onclick = go;
    no.onclick = () => { answered = true; m.close(); resolve(false); };
    for (const i of [typedIn, reasonIn]) if (i) { i.oninput = sync; i.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey && ready()) { e.preventDefault(); go(); } }; }
    sync();
    // Cancel has the focus; with a field to fill, the first field has it.
    setTimeout(() => (typedIn || reasonIn || no).focus(), 0);
  });
}

/** A menu under an element: menu(anchor, [{label, icon, onclick, danger} | "-"]). */
export function menu(anchor, items) {
  const r = anchor.getBoundingClientRect();
  const m = h("div", { class: "menu", role: "menu" }, items.map((it) => it === "-" ? h("hr") :
    h("button", { role: "menuitem", class: it.danger ? "danger" : "", onclick: () => { close(); it.onclick?.(); } }, it.color ? h("span", { class: "conn-dot", style: { background: it.color } }) : it.icon ? icon(it.icon) : null, it.label)));
  const scrim = h("div", { style: { position: "fixed", inset: 0, zIndex: 84 } });
  const close = layer([scrim, m]);
  scrim.onclick = close;
  const w = 200;
  m.style.left = Math.min(r.left, innerWidth - w - 8) + "px";
  // Near the bottom of the window it opens upward.
  const ht = m.offsetHeight || items.length * 34 + 8;
  if (r.bottom + 4 + ht > innerHeight - 8 && r.top - 4 - ht >= 8) m.style.top = (r.top - 4 - ht) + "px";
  else m.style.top = Math.max(8, Math.min(r.bottom + 4, innerHeight - 8 - ht)) + "px";
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
let tapArmed = false;
/** After a tap showed the tooltip: the next tap anywhere, or a scroll, hides it. */
export function tapTip() {
  if (tapArmed) return;
  tapArmed = true;
  const off = () => { tapArmed = false; hideTip(); removeEventListener("pointerdown", off, true); removeEventListener("scroll", off, true); };
  setTimeout(() => { addEventListener("pointerdown", off, true); addEventListener("scroll", off, true); }, 0);
}
/** A tooltip on hover and focus: tipOn(el, () => nodes). */
export function tipOn(e, content) {
  e.addEventListener("pointermove", (ev) => { if (ev.pointerType === "mouse") showTip(ev.clientX, ev.clientY, typeof content === "function" ? content() : content); });
  // A tap shows it too (a phone has no hover); the next tap anywhere hides it.
  e.addEventListener("pointerdown", (ev) => { if (ev.pointerType !== "mouse") { showTip(ev.clientX, ev.clientY - 40, typeof content === "function" ? content() : content); tapTip(); } });
  e.addEventListener("pointerleave", (ev) => { if (ev.pointerType === "mouse") hideTip(); });
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

// ---------------------------------------------------------------- attention
/**
 * LIV-14: whether anybody is at the page. A tab nobody touched (no pointer,
 * key, wheel or scroll) for IDLE_MS polls IDLE_SLOW times less often; a
 * touch, a key or the tab shown again asks at once what is due, and polls at
 * the full rate again. A hidden tab polls nothing.
 */
export const IDLE_MS = 30e3, IDLE_SLOW = 5;
let lastInput = Date.now(), dozing = false;
const wakers = new Set();
/** True once nobody touched the page for IDLE_MS. */
export function isIdle() {
  if (!dozing && Date.now() - lastInput > IDLE_MS) { dozing = true; bus.emit("idle", true); }
  return dozing;
}
function wake(shown) {
  lastInput = Date.now();
  if (!dozing && !shown) return;
  if (dozing) { dozing = false; bus.emit("idle", false); }
  for (const f of [...wakers]) f();
}
for (const ev of ["pointerdown", "pointermove", "keydown", "wheel", "scroll", "touchstart"]) document.addEventListener(ev, () => wake(false), { capture: true, passive: true });
document.addEventListener("visibilitychange", () => { if (!document.hidden) wake(true); });

/**
 * Repeats `f` every `ms` while the page is visible; returns a stopper. While
 * the command center is paused (`isPaused()`), ticks after the first are
 * skipped, so a view does not redraw; `{always: true}` keeps polling (for a
 * change under way the person is waiting on), at the full rate even while
 * nobody is at the page unless `{idle: "slow"}`. Other polls run
 * IDLE_SLOW times less often then (isIdle()), and at once when someone is
 * back and they are due.
 */
export function every(ms, f, now = true, opts = {}) {
  let stopped = false, timer = null, running = false, first = true, ran = 0;
  const slows = !opts.always || opts.idle === "slow";
  const tick = async () => {
    if (stopped) return;
    const resting = slows && isIdle() && Date.now() - ran < ms * IDLE_SLOW - ms / 2;
    // Paused, or without a valid key, a poll runs once (a view learns why it is empty) and then waits.
    if (!document.hidden && !running && !resting && (first || opts.always || (!ctx.paused && !signInNeeded()))) { first = false; running = true; ran = Date.now(); try { await f(); } catch (e) { console.warn(e); } running = false; }
    if (!stopped) timer = setTimeout(tick, ms);
  };
  // Someone is back (or the tab shown again): what is due is asked now.
  const waker = () => { if (stopped || running || first || Date.now() - ran < ms) return; clearTimeout(timer); tick(); };
  wakers.add(waker);
  if (now) tick(); else timer = setTimeout(tick, ms);
  return () => { stopped = true; clearTimeout(timer); wakers.delete(waker); };
}

/** What an error says, for a view that could not load. */
export function failure(e, what) {
  if (e instanceof ApiError && e.status === 403 && (e.body?.limited || limited())) return empty("lock", `This key can't see ${what.toLowerCase()}`, cannotSee(e, what.toLowerCase()), h("button", { class: "btn sm", onclick: () => bus.emit("unauthorized") }, icon("access"), "Use another key"));
  if (e instanceof ApiError && e.status === 403) return empty("lock", `${what} needs an admin key`, e.message, h("button", { class: "btn sm", onclick: () => bus.emit("unauthorized") }, icon("access"), "Use another key"));
  if (e instanceof ApiError && e.status === 401) return empty("lock", "Sign in to see " + what.toLowerCase(), "This server needs a token or an API key.", h("button", { class: "btn sm primary", onclick: () => bus.emit("unauthorized") }, "Enter a key"));
  return h("div", { class: "error" }, `${what}: ${e.message}`);
}
