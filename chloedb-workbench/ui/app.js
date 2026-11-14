// ChloeDB Workbench: the page. One module, no build step. State comes from
// /api/stream (server-sent events); terminals are WebSockets to
// /api/agents/:id/pty drawn with xterm.js (window.Terminal, loaded by
// index.html from the pinned CDN URL). SPEC.md is the contract; JOURNEYS.md
// says what each page is for. The pages are modules: ui/goals/ (Start, New
// project, Continue, the inbox, the agent page's parts, More; the shell:
// top bar, tab bar, the drawer and the switcher) mounted into #goal-view by
// the router (ui/goals/router.js); the agent page, All terminals and the
// Decisions timeline are views of this file.
import { resolve } from "/goals/router.js";
import * as Shell from "/goals/shell.js";
import * as AG from "/goals/agent.js";
import { kc } from "/levels/common.js";

const TOKEN_KEY = "chloedb-workbench-token";
const REPO_KEY = "chloedb-workbench-repo";
const REPOS_KEY = "chloedb-workbench-repos";
const MODEL_KEY = "chloedb-workbench-model";
const PREVIEW_LIMIT = 6; // past this many running agents, tiles show their last event instead of a terminal
const DEFAULT_SIZE = [120, 32]; // the PTY's size at spawn (SPEC.md)
const PREVIEW_RATIO = 0.5; // a tile's terminal box: height over width (120 x 32 at 11px fits)
const MONO = 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace';
const ENC = new TextEncoder();

const $ = (id) => document.getElementById(id);

const S = {
  token: null,
  agents: new Map(), // id -> Agent
  tiles: new Map(), // id -> Tile
  order: [], // running agents' ids, in grid order
  health: null,
  zoomed: null,
  selected: null,
  prefix: false,
  sizes: new Map(), // id -> [cols, rows], the PTY's size as this page last set it
  pendingZoom: null,
  conn: "connecting",
};

// ------------------------------------------------------------- helpers

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null || v === false) continue;
      if (k === "class") el.className = v;
      else if (k === "text") el.textContent = v;
      else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v === true ? "" : v);
    }
  }
  for (const kid of kids.flat()) if (kid != null && kid !== false) el.append(kid);
  return el;
}

function icon(name, cls = "i sm") {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", cls);
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

function parse(text) {
  try { return JSON.parse(text); } catch { return null; }
}

function store(kind, key, value) {
  try {
    const s = kind === "session" ? sessionStorage : localStorage;
    if (value === undefined) return s.getItem(key);
    if (value === null) s.removeItem(key);
    else s.setItem(key, value);
  } catch { /* storage blocked: keep going without it */ }
  return null;
}

function toMs(t) {
  if (t == null || t === "") return null;
  if (typeof t === "number") return t < 1e12 ? t * 1000 : t;
  const ms = Date.parse(t);
  return Number.isNaN(ms) ? null : ms;
}

function fmtDur(ms) {
  if (ms == null || ms < 0) ms = 0;
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const hr = Math.floor(m / 60);
  if (hr < 24) return m % 60 ? `${hr}h ${m % 60}m` : `${hr}h`;
  const d = Math.floor(hr / 24);
  return hr % 24 ? `${d}d ${hr % 24}h` : `${d}d`;
}

function fmtClock(t) {
  const ms = toMs(t);
  if (ms == null) return "";
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
}

function fmtWhen(t) {
  const ms = toMs(t);
  if (ms == null) return "";
  const d = new Date(ms);
  const today = new Date().toDateString() === d.toDateString();
  return today ? fmtClock(ms) : d.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false });
}

function since(el, t) {
  const ms = toMs(t);
  if (ms == null) { el.removeAttribute("data-since"); el.textContent = ""; return; }
  el.dataset.since = String(ms);
  el.textContent = fmtDur(Date.now() - ms);
}

function tickTimes() {
  const now = Date.now();
  for (const el of document.querySelectorAll("[data-since]")) {
    const t = fmtDur(now - Number(el.dataset.since));
    if (el.textContent !== t) el.textContent = t;
  }
}

function cap(s) { return s ? s[0].toUpperCase() + s.slice(1) : s; }

function nameOf(a) { return (a && (a.name || a.id)) || "?"; }

function shortPath(p) {
  if (!p) return "";
  return p.replace(/^\/(home|Users)\/[^/]+/, "~");
}

const STATE_LABEL = { starting: "starting", working: "working", waiting: "waiting for you", idle: "idle", exited: "exited" };

function chip(el, a) {
  el.className = "chip";
  el.dataset.state = a.state || "starting";
  let label = STATE_LABEL[a.state] || a.state || "starting";
  if (a.state === "exited" && a.exit_code != null && a.exit_code !== 0) {
    label = `exited ${a.exit_code}`;
    el.classList.add("bad");
  }
  el.textContent = label;
}

function toast(text, bad = false) {
  const t = h("div", { class: bad ? "toast bad" : "toast", role: bad ? "alert" : "status", text });
  $("toasts").append(t);
  setTimeout(() => t.remove(), bad ? 6000 : 3500);
}

function isDark() {
  const forced = document.documentElement.dataset.theme;
  if (forced) return forced === "dark";
  return matchMedia("(prefers-color-scheme: dark)").matches;
}

function termTheme() {
  const cs = getComputedStyle(document.documentElement);
  const bg = cs.getPropertyValue("--term-bg").trim();
  const fg = cs.getPropertyValue("--term-fg").trim();
  if (isDark()) {
    return {
      background: bg || "#0f1114", foreground: fg || "#d8dde4", cursor: "#d8dde4", cursorAccent: bg || "#0f1114",
      selectionBackground: "rgba(122,162,255,.32)",
      black: "#1d2128", red: "#f07178", green: "#8fd18b", yellow: "#e6c07b", blue: "#7aa2ff", magenta: "#c3a6ff", cyan: "#76d0e0", white: "#c7ccd4",
      brightBlack: "#5c6370", brightRed: "#ff8b92", brightGreen: "#a9e6a5", brightYellow: "#f5d58f", brightBlue: "#9ab8ff", brightMagenta: "#d6c1ff", brightCyan: "#94e2ef", brightWhite: "#f2f4f7",
    };
  }
  return {
    background: bg || "#fbfbfc", foreground: fg || "#1f2328", cursor: "#1f2328", cursorAccent: bg || "#fbfbfc",
    selectionBackground: "rgba(47,91,211,.22)",
    black: "#24292f", red: "#c62f37", green: "#1a7f37", yellow: "#8a5a00", blue: "#0b5cd5", magenta: "#8250df", cyan: "#1b7c83", white: "#6e7781",
    brightBlack: "#57606a", brightRed: "#a40e26", brightGreen: "#116329", brightYellow: "#6c4400", brightBlue: "#218bff", brightMagenta: "#a475f9", brightCyan: "#3192aa", brightWhite: "#8c959f",
  };
}

// ----------------------------------------------------------------- token

function readToken() {
  const url = new URL(location.href);
  const t = url.searchParams.get("token");
  if (t) {
    store("session", TOKEN_KEY, t);
    url.searchParams.delete("token");
    history.replaceState(history.state, "", url.pathname + url.search + url.hash);
    return t;
  }
  return store("session", TOKEN_KEY);
}

function gate(title, text) {
  $("gate-title").textContent = title;
  if (text) $("gate-text").textContent = text;
  $("gate").hidden = false;
  setTimeout(() => $("gate-input").focus(), 0);
}

$("gate-form").addEventListener("submit", (e) => {
  e.preventDefault();
  let v = $("gate-input").value.trim();
  if (!v) return;
  try {
    const u = new URL(v);
    v = u.searchParams.get("token") || v;
  } catch { /* a bare token */ }
  S.token = v;
  store("session", TOKEN_KEY, v);
  $("gate").hidden = true;
  $("gate-input").value = "";
  start();
});

// ------------------------------------------------------------------- api

async function api(path, { method = "GET", body } = {}) {
  const headers = { Authorization: `Bearer ${S.token}` };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  let r;
  try {
    r = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  } catch {
    throw Object.assign(new Error("The Workbench server did not answer."), { status: 0 });
  }
  const j = parse(await r.text()) || {};
  if (!r.ok) {
    if (r.status === 401) gate("The token was refused", "This page's token is not the server's. Open the URL that chloedb-workbench serve printed, or paste it here.");
    throw Object.assign(new Error(j.error || `${r.status} ${r.statusText}`), { status: r.status, code: j.error_code });
  }
  return j;
}

// ---------------------------------------------------------------- agents

function norm(a) {
  if (a.running == null) a.running = a.state !== "exited";
  if (!Array.isArray(a.children)) a.children = [];
  return a;
}

function upsert(a) {
  if (!a || !a.id) return;
  norm(a);
  const prev = S.agents.get(a.id);
  S.agents.set(a.id, a);
  if (prev && prev.state !== a.state && a.state === "waiting" && S.zoomed !== a.id) {
    S.tiles.get(a.id)?.flash();
  }
  if (!prev && a.running) followLive(a);
  // What needs you and the projects' groups follow the agents' states.
  if (!prev || prev.state !== a.state || prev.waiting_for !== a.waiting_for || prev.running !== a.running) { needsSoon(); projectsSoon(); }
  schedule();
}

// A restart (SPEC.md, "Sixth build: E"): a view of an exited agent follows
// the agent that now runs under its name, in its repository.
function liveId(id) {
  const a = S.agents.get(id);
  if (!a || a.running) return id;
  const next = [...S.agents.values()].filter((x) => x.running && x.id !== id && x.repo === a.repo && (x.restarted_from === id || (a.name && x.name === a.name)))
    .sort((x, y) => (toMs(y.created) || 0) - (toMs(x.created) || 0))[0];
  return next ? next.id : id;
}
function followLive(a) {
  if (!a.restarted_from) return; // only a restart moves a view that is open
  if (S.zoomed && S.zoomed === a.restarted_from) replaceHash(location.hash.replace(`/a/${a.restarted_from}`, `/a/${a.id}`));
  else if (G.r && G.r.agent === a.restarted_from) replaceHash(location.hash.replace(a.restarted_from, a.id));
}

/** "Restart with the same task" (the switcher's Finished rows, the agent
 *  page): POST /api/agents/:id/restart, then open the agent that runs now. */
async function restartAgent(id) {
  const a = S.agents.get(id);
  if (!a || a.running) return null;
  try {
    const r = await api(`/api/agents/${encodeURIComponent(id)}/restart`, { method: "POST", body: {} });
    if (r && r.agent) {
      upsert(r.agent);
      toast(r.resumed ? `Restarted ${nameOf(r.agent)}: it continues its session` : `Restarted ${nameOf(r.agent)} with its task (no session to continue)`);
      if (S.zoomed === id || !S.zoomed) go(`#/a/${r.agent.id}`);
      return r.agent;
    }
  } catch (e) {
    toast(`Could not restart it: ${e.message}`, true);
  }
  return null;
}

async function loadAll() {
  try {
    const { agents = [] } = await api("/api/agents?all=1");
    // The stream is newer than this answer for the agents it already sent.
    for (const a of agents) if (!S.agents.has(a.id) || !norm(a).running) upsert(a);
  } catch (e) {
    if (e.status !== 401) console.warn("agents:", e.message);
  }
  // An agent page opened by its address before the agent was known.
  if (G.r && G.r.page === "agent" && !S.zoomed && S.agents.has(G.r.agent)) route(location.hash);
}

// ---------------------------------------------------------------- stream

let es = null;
let esTries = 0;
let esTimer = 0;

function setConn(state) {
  S.conn = state;
  $("conn").dataset.state = state;
  $("conn-text").textContent = { live: "live", connecting: "connecting", reconnecting: "reconnecting", offline: "offline" }[state];
}

/** Settles once the stream's first snapshot of the agents is in. */
let gotSnapshot;
const firstSnapshot = new Promise((r) => { gotSnapshot = r; });

function connectStream() {
  clearTimeout(esTimer);
  es?.close();
  setConn(esTries ? "reconnecting" : "connecting");
  es = new EventSource(`/api/stream?token=${encodeURIComponent(S.token)}`);
  es.addEventListener("open", () => { esTries = 0; setConn("live"); });
  es.addEventListener("snapshot", (e) => {
    const d = parse(e.data) || {};
    const list = d.agents || [];
    const ids = new Set(list.map((a) => a.id));
    // Running agents the snapshot leaves out ended while we were away; drop
    // them, and loadAll brings them back as exited.
    for (const [id, a] of S.agents) if (a.running && !ids.has(id)) S.agents.delete(id);
    for (const a of list) upsert(a);
    setConn("live");
    gotSnapshot();
    loadAll();
  });
  es.addEventListener("agent", (e) => {
    const d = parse(e.data);
    upsert(d && d.agent ? d.agent : d);
  });
  es.addEventListener("event", (e) => {
    const d = parse(e.data);
    const ev = d && d.event ? d.event : d;
    if (ev) {
      L.recent.push(ev);
      if (L.recent.length > 600) L.recent.splice(0, L.recent.length - 500);
      onEvent(ev);
    }
  });
  es.onerror = () => {
    if (es.readyState === EventSource.CLOSED) {
      setConn("offline");
      esTries++;
      // Find out why before trying again: a refused token opens the gate.
      api("/api/agents").then(
        () => { esTimer = setTimeout(connectStream, Math.min(1000 * 2 ** esTries, 15000)); },
        (err) => { if (err.status !== 401) esTimer = setTimeout(connectStream, Math.min(1000 * 2 ** esTries, 15000)); },
      );
    } else {
      setConn("reconnecting");
    }
  };
}

// ------------------------------------------------------------ pty socket

class PtySocket {
  constructor(id, handlers) {
    this.id = id;
    this.h = handlers;
    this.closed = false;
    this.tries = 0;
    this.ws = null;
    this.connect();
  }

  connect() {
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/api/agents/${encodeURIComponent(this.id)}/pty?token=${encodeURIComponent(S.token)}`);
    ws.binaryType = "arraybuffer";
    let first = true;
    ws.onopen = () => { this.tries = 0; this.h.onOpen?.(this); };
    ws.onmessage = (e) => {
      if (first) { first = false; this.h.onReset?.(); } // the scrollback is replayed on every attach
      this.h.onData(typeof e.data === "string" ? e.data : new Uint8Array(e.data));
    };
    ws.onclose = () => {
      if (this.ws === ws) this.ws = null;
      if (this.closed) return;
      const a = S.agents.get(this.id);
      if (!a || !a.running) { this.h.onClose?.("ended"); return; }
      this.tries++;
      this.h.onClose?.("retry");
      this.timer = setTimeout(() => { if (!this.closed) this.connect(); }, Math.min(400 * 2 ** this.tries, 10000));
    };
    this.ws = ws;
  }

  get open() { return this.ws?.readyState === WebSocket.OPEN; }

  send(data) {
    if (!this.open) return false;
    this.ws.send(typeof data === "string" ? ENC.encode(data) : data);
    return true;
  }

  resize(cols, rows) {
    if (this.open) this.ws.send(JSON.stringify({ resize: [cols, rows] }));
  }

  close() {
    this.closed = true;
    clearTimeout(this.timer);
    try { this.ws?.close(); } catch { /* already closed */ }
  }
}

function sizeOf(id) { return S.sizes.get(id) || DEFAULT_SIZE; }

function setSize(id, cols, rows) {
  const [c, r] = sizeOf(id);
  if (c === cols && r === rows) return;
  S.sizes.set(id, [cols, rows]);
  S.tiles.get(id)?.preview?.resize(cols, rows);
}

// ---------------------------------------------------------------- tiles

class Preview {
  constructor(id, host) {
    this.id = id;
    this.host = host;
    this.inner = h("div", { class: "pv" });
    host.replaceChildren(this.inner);
    const [cols, rows] = sizeOf(id);
    this.term = new window.Terminal({
      cols, rows, fontSize: 11, lineHeight: 1.0, fontFamily: MONO, disableStdin: true, cursorBlink: false,
      cursorStyle: "block", scrollback: 0, theme: termTheme(), allowTransparency: false,
    });
    this.term.open(this.inner);
    if (this.term.textarea) { this.term.textarea.tabIndex = -1; this.term.textarea.setAttribute("aria-hidden", "true"); }
    this.ro = new ResizeObserver(() => this.queue());
    this.ro.observe(host);
    this.term.onCursorMove(() => this.queue());
    this.sock = new PtySocket(id, {
      onReset: () => this.term.reset(),
      onData: (d) => this.term.write(d),
    });
    requestAnimationFrame(() => this.scale());
  }

  resize(cols, rows) {
    try { this.term.resize(cols, rows); } catch { /* disposed */ }
    requestAnimationFrame(() => this.scale());
  }

  // The tile's box keeps one shape whatever the PTY's size; when the screen
  // is taller than the box (after a zoom made the PTY bigger), the box
  // follows the cursor, where the agent is writing.
  scale() {
    const scr = this.inner.querySelector(".xterm-screen");
    if (!scr) return;
    const w = scr.offsetWidth, ht = scr.offsetHeight;
    const room = this.host.clientWidth - 12;
    if (!w || !ht || room <= 0) return;
    const s = Math.min(room / w, 1);
    const box = Math.round(room * PREVIEW_RATIO);
    const cell = ht / this.term.rows;
    const cursor = this.term.buffer.active.cursorY;
    const shift = Math.max(0, Math.min(ht * s - box, (cursor + 2) * cell * s - box));
    this.inner.style.transform = `translateY(${-Math.round(shift)}px) scale(${s})`;
    this.host.style.aspectRatio = "auto";
    this.host.style.height = `${box + 10}px`;
  }

  queue() {
    if (this.raf) return;
    this.raf = requestAnimationFrame(() => { this.raf = 0; this.scale(); });
  }

  retheme() { this.term.options.theme = termTheme(); }

  dispose() {
    cancelAnimationFrame(this.raf);
    this.sock.close();
    this.ro.disconnect();
    this.term.dispose();
    this.host.replaceChildren();
    this.host.style.height = "";
    this.host.style.aspectRatio = "";
  }
}

class Tile {
  constructor(id) {
    this.id = id;
    this.preview = null;
    this.idx = h("span", { class: "idx" });
    this.nm = h("span", { class: "nm" });
    this.chip = h("span", { class: "chip" });
    this.br = h("span", { class: "br" }, icon("branch"), h("span"));
    this.model = h("span", { class: "model" });
    this.kids = h("span", { class: "kids" });
    this.since = h("span", { class: "since" });
    this.task = h("p", { class: "task" });
    this.pv = h("div", { class: "preview" });
    this.footIcon = h("span");
    this.last = h("span", { class: "last" });
    this.foot = h("div", { class: "tile-foot" }, this.footIcon, this.last);
    this.el = h("article", { class: "tile", tabindex: "0", "data-id": id },
      h("div", { class: "tile-head" }, this.idx, this.nm, h("span", { class: "sep" }), this.chip),
      h("div", { class: "meta" }, this.br, this.model, this.kids, this.since),
      this.task, this.pv, this.foot);
    this.el.addEventListener("click", () => go(`#/a/${this.id}/terminal`));
    this.el.addEventListener("focus", () => select(this.id, { focus: false }));
    this.key = "";
  }

  update(a, n, withPreview) {
    const key = [a.name, a.state, a.state_since, a.branch, a.task, a.last_event, a.waiting_for, a.model, a.children.length, n, withPreview].join("\u0001");
    if (key === this.key) return;
    this.key = key;
    this.el.dataset.state = a.state || "starting";
    this.el.setAttribute("aria-label", `${nameOf(a)}, ${STATE_LABEL[a.state] || a.state}`);
    this.idx.textContent = n <= 9 ? String(n) : "";
    this.nm.textContent = nameOf(a);
    this.nm.title = a.cwd || "";
    chip(this.chip, a);
    this.br.lastChild.textContent = a.branch || "no branch";
    this.br.title = a.cwd ? `${a.branch || ""} in ${a.cwd}` : a.branch || "";
    this.model.textContent = a.model || "";
    this.model.hidden = !a.model;
    this.kids.textContent = a.children.length ? `${a.children.length} spawned` : "";
    this.kids.hidden = !a.children.length;
    since(this.since, a.state_since);
    this.task.textContent = a.task || "No task: started at a prompt";
    this.task.classList.toggle("none", !a.task);
    this.task.title = a.task || "";
    const waiting = a.state === "waiting";
    this.foot.classList.toggle("ask", waiting);
    this.footIcon.replaceChildren(icon(waiting ? "bell" : a.state === "idle" ? "turn" : "act"));
    this.last.textContent = cap(waiting ? a.waiting_for || "Waiting for you" : a.last_event || (a.state === "starting" ? "Starting Claude Code…" : "No events yet"));
    this.last.title = this.last.textContent;
    this.setPreview(withPreview);
  }

  setPreview(on) {
    on = on && !!window.Terminal;
    if (on && !this.preview) this.preview = new Preview(this.id, this.pv);
    if (!on && this.preview) { this.preview.dispose(); this.preview = null; }
    this.pv.hidden = !on;
  }

  flash() {
    this.el.classList.remove("flash");
    void this.el.offsetWidth;
    this.el.classList.add("flash");
  }

  dispose() {
    this.setPreview(false);
    this.el.remove();
  }
}

// --------------------------------------------------------------- render

let raf = 0;
function schedule() {
  if (!raf) raf = requestAnimationFrame(() => { raf = 0; render(); });
}

const byCreated = (a, b) => (toMs(a.created) || 0) - (toMs(b.created) || 0) || a.id.localeCompare(b.id);
const bySince = (a, b) => (toMs(a.state_since) || 0) - (toMs(b.state_since) || 0);

function render() {
  const all = [...S.agents.values()];
  const running = all.filter((a) => a.running).sort(byCreated);
  const exited = all.filter((a) => !a.running).sort((a, b) => (toMs(b.ended || b.state_since) || 0) - (toMs(a.ended || a.state_since) || 0));
  S.order = running.map((a) => a.id);
  if (S.selected && !S.agents.has(S.selected)) S.selected = null;

  Shell.update(); // the one count of what needs you, the agents, the title
  if (!$("grid-view").hidden) { renderGrid(running); renderExited(exited); }
  if (S.zoomed) renderZoom();
  if (G.view) { try { G.view.update && G.view.update(levelState()); } catch (err) { console.error(err); } }
  for (const v of Z.views) { try { v && v.update && v.update(levelState()); } catch (err) { console.error(err); } }
}

function renderGrid(running) {
  const grid = $("grid");
  const previews = running.length <= PREVIEW_LIMIT;
  grid.classList.toggle("compact", !previews);
  const live = new Set(running.map((a) => a.id));
  for (const [id, t] of S.tiles) if (!live.has(id)) { t.dispose(); S.tiles.delete(id); }
  running.forEach((a, i) => {
    let t = S.tiles.get(a.id);
    if (!t) { t = new Tile(a.id); S.tiles.set(a.id, t); }
    if (grid.children[i] !== t.el) grid.insertBefore(t.el, grid.children[i] || null);
    t.el.classList.toggle("sel", S.selected === a.id);
    t.update(a, i + 1, previews);
  });
  $("empty").hidden = running.length > 0;
}

function renderExited(list) {
  $("fold").hidden = !list.length;
  $("fold-n").textContent = list.length ? `(${list.length})` : "";
  const rows = list.slice(0, 200).map((a) => {
    const c = h("span");
    chip(c, a);
    return h("button", { class: "xrow", title: `Open ${nameOf(a)}`, onclick: () => go(`#/a/${a.id}`) },
      h("span", { class: "nm", text: nameOf(a) }), c,
      h("span", { class: "t", text: a.task || "No task" }),
      h("span", { class: "br", text: a.branch || "" }),
      h("span", { class: "when", text: fmtWhen(a.ended || a.state_since) }));
  });
  $("exited").replaceChildren(...rows);
}

// ---------------------------------------------------------------- select

function select(id, { focus = true } = {}) {
  if (!id) return;
  if (S.selected !== id) {
    S.selected = id;
    for (const [tid, t] of S.tiles) t.el.classList.toggle("sel", tid === id);
  }
  if (focus && !S.zoomed) {
    const t = S.tiles.get(id);
    if (t && document.activeElement !== t.el) t.el.focus({ preventScroll: true });
    t?.el.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }
}

function move(dir) {
  const ids = S.order;
  if (!ids.length) return;
  if (S.zoomed) {
    const i = ids.indexOf(S.zoomed);
    const step = dir === "left" || dir === "up" ? -1 : 1;
    const next = ids[((i < 0 ? 0 : i + step) + ids.length) % ids.length];
    replaceHash(`#/a/${next}${Z.tab === "progress" ? "" : "/" + (Z.tab === "term" ? "terminal" : Z.tab)}`);
    return;
  }
  const cur = S.tiles.get(S.selected);
  if (!cur) { select(ids[0]); return; }
  const r0 = cur.el.getBoundingClientRect();
  const cx = r0.left + r0.width / 2, cy = r0.top + r0.height / 2;
  let best = null, bestD = Infinity;
  for (const id of ids) {
    if (id === S.selected) continue;
    const r = S.tiles.get(id).el.getBoundingClientRect();
    const x = r.left + r.width / 2, y = r.top + r.height / 2;
    const dx = x - cx, dy = y - cy;
    const ok = { left: dx < -4 && Math.abs(dy) < r0.height / 2, right: dx > 4 && Math.abs(dy) < r0.height / 2, up: dy < -4, down: dy > 4 }[dir];
    if (!ok) continue;
    const d = dir === "up" || dir === "down" ? Math.abs(dy) * 1 + Math.abs(dx) * 2 : Math.abs(dx);
    if (d < bestD) { bestD = d; best = id; }
  }
  // Left and right run on into the row before or after, as reading order does.
  if (!best && (dir === "left" || dir === "right")) {
    const i = ids.indexOf(S.selected) + (dir === "left" ? -1 : 1);
    if (i >= 0 && i < ids.length) best = ids[i];
  }
  if (best) select(best);
}

// ------------------------------------------------------------------ zoom

// The agent page (JOURNEYS.md, "The agent page"): #/a/<id>[/<tab>]. Its
// header, question and tabs come from ui/goals/agent.js; the terminal, the
// diff and the events are this file's. `tab` is progress, changes, preview,
// term or events; `typing` is the phone's Type (read-only until then).
const Z = { id: null, term: null, fit: null, sock: null, ro: null, events: [], live: [], loaded: false, factsKey: "", tab: "progress", views: [], hosted: {}, typing: false, headKey: "" };
const PHONE = matchMedia("(max-width: 640px)");

function zoom(id, { tab = "progress" } = {}) {
  const a = S.agents.get(id);
  if (!a) return;
  S.selected = id;
  if (Z.id !== id || $("zoom-view").hidden) {
    closeZoomTerm();
    unmountTabs();
    S.zoomed = id;
    Z.typing = false;
    Z.factsKey = Z.headKey = "";
    $("zoom-view").hidden = false;
    openZoomTerm(id);
    loadEvents(id);
    loadChanges(id, { quiet: true }); // its count shows on the tab
  }
  setTab(tab, { focus: tab === "changes" || (tab === "term" && !PHONE.matches) });
  render();
}

/** Leaves the agent page (the router opens what comes next). */
function unzoom() {
  if (!S.zoomed) return;
  closeZoomTerm();
  unmountTabs();
  S.zoomed = null;
  Z.typing = false;
  $("zoom-view").hidden = true;
}

function unmountTabs() {
  for (const v of Z.views) { try { v && v.unmount && v.unmount(); } catch (err) { console.error(err); } }
  Z.views = [];
  Z.hosted = {};
  $("ag-progress").replaceChildren();
  $("ag-preview").replaceChildren();
}

/** Progress (the story, embedded) and Preview mount the first time they show. */
function hostTab(kind) {
  if (Z.hosted[kind] || !S.zoomed) return;
  const id = S.zoomed;
  Z.hosted[kind] = true;
  AG.mountTab(kind, $(kind === "preview" ? "ag-preview" : "ag-progress"), levelCtx(id)).then((v) => { if (S.zoomed === id) Z.views.push(v); else v?.unmount?.(); })
    .catch((err) => { $(kind === "preview" ? "ag-preview" : "ag-progress").replaceChildren(h("div", { class: "lv-loading bad", text: `This did not load: ${err.message || err}` })); });
}

/** The phone's Type: the terminal takes keys only after it. */
function setTyping(on) {
  Z.typing = !!on;
  const a = S.agents.get(S.zoomed);
  if (Z.term) Z.term.options.disableStdin = !a?.running || (PHONE.matches && !Z.typing);
  $("keybar").hidden = PHONE.matches ? !Z.typing : true;
  if (Z.typing) Z.term?.focus(); else if (PHONE.matches) Z.term?.blur();
  drawTermNote();
  requestAnimationFrame(() => { try { Z.fit?.fit(); } catch { /* hidden */ } });
}
function drawTermNote() {
  const a = S.agents.get(S.zoomed);
  if (!a) return;
  $("termnote").replaceChildren(...AG.termNote(a, { phone: PHONE.matches, typing: Z.typing, onType: () => setTyping(true), onDone: () => setTyping(false) }));
}
PHONE.addEventListener("change", () => { if (S.zoomed) setTyping(false); });

function overlay(text) {
  const o = $("z-overlay");
  o.hidden = !text;
  o.textContent = text || "";
}

function openZoomTerm(id) {
  const host = $("z-term");
  host.replaceChildren();
  overlay(null);
  Z.id = id;
  Z.events = [];
  Z.live = [];
  Z.loaded = false;
  if (!window.Terminal) {
    overlay("The terminal library did not load (cdn.jsdelivr.net).");
    return;
  }
  const a = S.agents.get(id);
  const term = new window.Terminal({
    fontFamily: MONO, fontSize: 13, lineHeight: 1.0, cursorBlink: true, scrollback: 10000,
    theme: termTheme(), macOptionIsMeta: true, allowProposedApi: false, disableStdin: !a.running || (PHONE.matches && !Z.typing),
  });
  const fit = new window.FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);
  Z.term = term;
  Z.fit = fit;
  const doFit = () => { if (Z.term === term && host.clientWidth > 0) { try { fit.fit(); } catch { /* not laid out yet */ } } };
  doFit();
  Z.sock = new PtySocket(id, {
    onOpen: (s) => {
      overlay(null);
      if (S.agents.get(id)?.running) { s.resize(term.cols, term.rows); setSize(id, term.cols, term.rows); }
    },
    onReset: () => term.reset(),
    onData: (d) => term.write(d),
    onClose: (why) => {
      if (why === "ended") {
        term.options.disableStdin = true;
        term.write("\x1b[?25l"); // no cursor in a terminal nobody can type into
        overlay("The session has ended. Its scrollback is above.");
      }
      else overlay("Reconnecting to the terminal…");
    },
  });
  term.onData((d) => Z.sock?.send(d));
  term.onBinary((d) => Z.sock?.send(Uint8Array.from(d, (c) => c.charCodeAt(0) & 255)));
  term.onResize(({ cols, rows }) => {
    if (!S.agents.get(id)?.running) return;
    Z.sock?.resize(cols, rows);
    setSize(id, cols, rows);
  });
  let fitRaf = 0;
  Z.ro = new ResizeObserver(() => { cancelAnimationFrame(fitRaf); fitRaf = requestAnimationFrame(doFit); });
  Z.ro.observe(host);
  if (a.running && !PHONE.matches) setTimeout(() => { if (Z.term === term && $("z-body").dataset.tab === "term") term.focus(); }, 0);
}

function closeZoomTerm() {
  Z.ro?.disconnect();
  Z.sock?.close();
  Z.term?.dispose();
  Z.id = Z.term = Z.fit = Z.sock = Z.ro = null;
  $("z-term").replaceChildren();
}

function renderZoom() {
  const a = S.agents.get(S.zoomed);
  if (!a) return;
  const proj = P.list.find((p) => p.path === a.repo);
  const ready = proj && (proj.ready || []).find((r) => r.id === a.id);
  const files = C.id === a.id && C.data ? (C.data.files || []).length : 0;
  const mine = N.items.filter((it) => it.agent && it.agent.id === a.id);
  const group = Shell.groupOf(a);
  const merged = (proj && proj.merged && proj.merged[a.id]) || null;
  const headKey = JSON.stringify([a.name, a.state, a.state_since, a.task, a.branch, a.model, a.running, a.read_only, group, proj && proj.project, ready, Z.tab, files, mine.map((it) => it.id), merged]);
  if (headKey !== Z.headKey) {
    Z.headKey = headKey;
    $("ag-head").replaceChildren(AG.head(a, {
      group, project: proj && proj.project, merged,
      stat: ready ? { files: ready.files, added: ready.added, removed: ready.removed } : null,
      ship: proj ? `#/p/${proj.project}/ship/${a.id}` : null,
      onStop: () => confirmStop(a.id), onRestart: () => restartAgent(a.id), onTell: () => tellSheet(a),
    }));
    // The question it asks comes first (JOURNEYS.md): the one question card.
    $("ag-q").replaceChildren(...mine.map((it) => Shell.needCard(it, { big: true })));
    $("ag-tabs").replaceChildren(...AG.tabs(a, Z.tab === "term" ? "terminal" : Z.tab, { n: files }));
    drawTermNote();
  }
  if (Z.term) Z.term.options.disableStdin = !a.running || (PHONE.matches && !Z.typing);

  const key = JSON.stringify([a.task, a.repo, a.cwd, a.branch, a.model, a.parent, a.children, a.pid, a.session_id, a.created, a.ended, a.exit_code, a.depth, a.read_only, a.restarted_from, a.resumed]);
  if (key === Z.factsKey) return;
  Z.factsKey = key;
  const link = (id) => {
    const o = S.agents.get(id);
    return h("a", { href: `#/a/${id}`, title: o?.task || id, text: o ? nameOf(o) : id });
  };
  const rows = [];
  const fact = (k, v, title) => { if (v == null || v === "" || (Array.isArray(v) && !v.length)) return; rows.push(h("dt", { text: k }), h("dd", { title: title || (typeof v === "string" ? v : null) }, v)); };
  fact("repo", shortPath(a.repo), a.repo);
  if (a.cwd && a.cwd !== a.repo) fact("worktree", shortPath(a.cwd), a.cwd);
  fact("branch", a.branch);
  fact("model", a.model);
  if (a.read_only) fact("mode", "read-only", "No Edit or Write; Bash only for commands that read");
  if (a.restarted_from) fact("restart of", link(a.restarted_from), a.resumed ? `continuing session ${a.resumed}` : "started again with its task");
  if (a.parent) fact("spawned by", link(a.parent));
  if (a.children.length) fact("spawned", a.children.flatMap((id, i) => (i ? [", ", link(id)] : [link(id)])));
  fact("pid", a.pid != null ? String(a.pid) : null);
  fact("session", a.session_id);
  fact("started", fmtWhen(a.created));
  if (!a.running) fact("ended", fmtWhen(a.ended) + (a.exit_code != null ? `, exit ${a.exit_code}` : ""));
  Z.servers?.destroy();
  Z.servers = LC ? LC.serversBox(api, a.id) : null;
  $("z-facts").replaceChildren(...[
    h("p", { class: "task-full", text: a.task || "No task: started at a prompt." }),
    h("dl", null, rows),
    Z.servers ? Z.servers.el : null].filter(Boolean));
}

// ---------------------------------------------------------------- events

const KIND = {
  spawned: ["spawned", "c-sys"],
  SessionStart: ["session", "c-sys"],
  UserPromptSubmit: ["prompt", "c-prompt"],
  PreToolUse: ["tool", "c-tool"],
  PostToolUse: ["result", "c-tool"],
  Notification: ["asks", "c-ask"],
  Stop: ["done", "c-stop"],
  SubagentStop: ["subagent", "c-sys"],
  SessionEnd: ["ended", "c-end"],
  input: ["input", "c-prompt"],
  exited: ["exited", "c-end"],
};

function evRow(ev) {
  const [label, cls] = KIND[ev.kind] || [ev.kind || "event", "c-sys"];
  const s = h("span", { class: "s" });
  let text = (ev.summary || "").replace(/\/(home|Users)\/[^/\s]+\//g, "~/");
  // The server's summaries read "Bash: cargo test", "Bash done: cargo test",
  // "prompt: …": the label and the tool's chip already say the first part.
  if (ev.tool) {
    s.append(h("code", { text: ev.tool }));
    if (text.startsWith(ev.tool)) text = text.slice(ev.tool.length).replace(/^\s*(done\b)?\s*:?\s*/, "");
  } else if (ev.kind === "UserPromptSubmit") {
    text = text.replace(/^prompt:\s*/, "");
  }
  s.append(document.createTextNode(ev.tool ? text : cap(text)));
  return h("li", { class: `ev ${cls}`, title: `${ev.kind}${ev.tool ? ` ${ev.tool}` : ""} at ${fmtWhen(ev.ts)} (seq ${ev.seq})` },
    h("span", { class: "ts", text: fmtClock(ev.ts) }), h("span", { class: "k", text: label }), s);
}

function renderEvents() {
  const ol = $("z-events");
  $("ev-n").textContent = Z.events.length ? String(Z.events.length) : "";
  if (!Z.events.length) {
    ol.replaceChildren(h("li", { class: "ev-empty", text: Z.loaded ? "No events yet." : "Loading…" }));
    return;
  }
  ol.replaceChildren(...Z.events.map(evRow));
  ol.scrollTop = ol.scrollHeight;
}

async function loadEvents(id) {
  renderEvents();
  const got = [];
  let after = 0;
  try {
    for (let page = 0; page < 25; page++) {
      const { events = [] } = await api(`/api/agents/${encodeURIComponent(id)}/events?after=${after}&limit=200`);
      if (Z.id !== id) return;
      got.push(...events);
      if (events.length < 200) break;
      after = events[events.length - 1].seq;
    }
  } catch (e) {
    if (Z.id !== id) return;
    toast(`Events: ${e.message}`, true);
  }
  const seen = new Set(got.map((e) => e.seq));
  for (const e of Z.live) if (!seen.has(e.seq)) got.push(e);
  got.sort((a, b) => a.seq - b.seq);
  Z.events = got.slice(-1500);
  Z.live = [];
  Z.loaded = true;
  renderEvents();
}

function onEvent(ev) {
  trailOnEvent(ev);
  if (!Z.id || ev.agent !== Z.id) return;
  if (!Z.loaded) { Z.live.push(ev); return; }
  if (Z.events.length && Z.events[Z.events.length - 1].seq >= ev.seq && Z.events.some((e) => e.seq === ev.seq)) return;
  const ol = $("z-events");
  const atBottom = ol.scrollHeight - ol.scrollTop - ol.clientHeight < 40;
  if (!Z.events.length) ol.replaceChildren();
  Z.events.push(ev);
  ol.append(evRow(ev));
  $("ev-n").textContent = String(Z.events.length);
  if (atBottom) ol.scrollTop = ol.scrollHeight;
}

function setTab(tab, { focus = false } = {}) {
  Z.tab = tab;
  Z.headKey = "";
  $("z-body").dataset.tab = tab;
  if (tab === "progress" || tab === "preview") hostTab(tab);
  if (tab !== "term" && Z.typing) setTyping(false);
  $("keybar").hidden = !(PHONE.matches && Z.typing && tab === "term");
  if (tab === "term") requestAnimationFrame(() => { try { Z.fit?.fit(); } catch { /* hidden */ } if (focus && !PHONE.matches) Z.term?.focus(); });
  else if (Z.term?.textarea === document.activeElement) Z.term.blur();
  if (tab === "changes" && S.zoomed) {
    if (C.id !== S.zoomed || (!C.data && !C.err) || C.stale) loadChanges(S.zoomed, { fresh: C.stale });
    else if (C.want) renderChanges();
    if (focus) {
      requestAnimationFrame(() => {
        if (C.rows.length) setCursor(C.cursor >= 0 ? C.cursor : Math.max(0, C.rows.findIndex((r) => r.kind === "+")));
        else $("ch-scroll").focus({ preventScroll: true });
      });
    }
  }
}

/** "Tell X…", the one way to talk to an agent (the story's second box is
 *  gone): your words exactly as written, through POST /steer, which never
 *  types into an open 1/2/3 menu. An agent at its prompt gets them typed
 *  as its next instruction; a working one, or one showing a menu, at its
 *  next tool call. The sheet closes on Send and says which. */
function tellSheet(a) {
  const asking = N.items.some((i) => i.agent && i.agent.id === a.id && i.kind !== "alert") || a.state === "waiting";
  const box = h("textarea", { class: "field", rows: 3, placeholder: `What should ${nameOf(a)} do next? Your words go exactly as written.`, "aria-label": `Tell ${nameOf(a)}` });
  const sendBtn = h("button", { class: "btn primary", type: "button", disabled: true }, "Send");
  const how = asking
    ? h("div", { class: "callout warn tell-q" }, icon("warn", "i"), h("div", null, h("b", { text: `${nameOf(a)} is asking you something now.` }),
      h("p", { text: "Nothing is typed into that prompt, where a key would pick an option: your words are queued, and it gets them at its next tool call, once the prompt is answered. Answer its question on the page first." })))
    : h("p", { class: "muted", text: a.state === "idle" ? "Typed into its prompt as its next instruction." : "It is working: it gets this at its next tool call, or as its next prompt when this turn ends." });
  let busy = false;
  // "From now on…", "always", "never": also a rule for every agent of the
  // project, if you leave it ticked (Dev: his "from now on" became nothing).
  const ruleTick = h("input", { type: "checkbox", checked: true });
  const ruleRow = h("label", { class: "tell-rule", hidden: true }, ruleTick, " ", h("span", {}, h("b", {}, "Also make it a rule"), " Every agent of this project is told it, from its next start."));
  const standing = (t) => /^(from now on|always|never|by default|going forward)\b/i.test(t.trim());
  const send = async (done) => {
    const text = box.value.trim();
    if (!text || busy) { box.focus(); return; }
    busy = true; sendBtn.disabled = true; sendBtn.textContent = "Sending…";
    try {
      const r = await api(`/api/agents/${encodeURIComponent(a.id)}/steer`, { method: "POST", body: { text } });
      let ruled = "";
      if (standing(text) && ruleTick.checked) {
        try { await api("/api/rules", { method: "POST", body: { repo: a.repo, text } }); ruled = " It is a rule now too: More › Rules."; }
        catch (e) { ruled = ` The rule was not made: ${e.message}.`; }
      }
      done();
      noteActive();
      toast((r && r.message ? r.message : r && r.delivered === "typed" ? `Typed into ${nameOf(a)}'s prompt.` : `Queued: ${nameOf(a)} gets it at its next tool call.`) + ruled);
    } catch (e) { toast(`Not sent: ${e.message}`, true); busy = false; sendBtn.disabled = false; sendBtn.textContent = "Send"; }
  };
  sendBtn.addEventListener("click", () => send(s.close));
  box.addEventListener("input", () => { sendBtn.disabled = !box.value.trim() || busy; ruleRow.hidden = !standing(box.value); });
  const s = Shell.sheet({ kicker: nameOf(a), title: `Tell ${nameOf(a)}`, body: [how, box, ruleRow, h("p", { class: "muted tell-k" }, kc("⌘↵"), " sends · ", kc("Esc"), " closes without sending")],
    actions: (done) => [h("button", { class: "btn", type: "button", onclick: done }, "Don't send"), sendBtn] });
  box.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); send(s.close); } });
  requestAnimationFrame(() => box.focus());
}

// The keys under the terminal on a phone: the digits insert a digit and
// need Send (UX critique, issue 5); Send is Enter.
const KEYBAR = { esc: "\x1b", tab: "\t", stab: "\x1b[Z", up: "\x1b[A", down: "\x1b[B", cc: "\x03", enter: "\r", 1: "1", 2: "2", 3: "3" };
$("keybar").addEventListener("pointerdown", (e) => e.preventDefault()); // keep the terminal's focus
$("keybar").addEventListener("click", (e) => {
  const k = e.target.closest("button")?.dataset.k;
  if (k && Z.sock && Z.typing) Z.sock.send(KEYBAR[k]);
});

// --------------------------------------------------------------- dialogs

const openDialog = () => document.querySelector("dialog[open]");

for (const d of document.querySelectorAll("dialog")) {
  d.addEventListener("click", (e) => {
    if (e.target.closest("[data-close]") || e.target === d) d.close();
  });
  d.addEventListener("close", () => {
    if (S.zoomed && S.agents.get(S.zoomed)?.running && Z.tab === "term" && !PHONE.matches) Z.term?.focus();
    else if (S.selected) S.tiles.get(S.selected)?.el.focus({ preventScroll: true });
  });
}

function showHelp() {
  if (openDialog() || Shell.sheetOpen()) return;
  Shell.keysSheet();
}

function recentRepos() {
  const list = parse(store("local", REPOS_KEY) || "[]");
  return Array.isArray(list) ? list.filter((x) => typeof x === "string") : [];
}

// Sixth build, E: the servers agents start and the stop choice (ui/levels/lifecycle.js).
let LC = null;
import("/levels/lifecycle.js").then((m) => { LC = m; }).catch(() => {});
// Fifth build: autocomplete and intent checks for Ask why (ui/levels/complete.js).
import("/levels/complete.js").then((m) => { const c = { api, go }; m.attachComplete($("ask-q"), { field: "why", ctx: c, repo: () => D.repo || "" }); }).catch(() => {});

let confirmFor = null;
function confirmStop(id) {
  const a = S.agents.get(id);
  if (!a || !a.running || openDialog()) return;
  confirmFor = id;
  $("confirm-title").textContent = `Stop ${nameOf(a)}?`;
  $("confirm-text").textContent = `Its Claude Code session ends: Workbench sends it SIGHUP, then SIGKILL after 5 s. ${a.cwd && a.cwd !== a.repo ? "Its worktree and branch are left as they are." : ""}`.trim();
  stopServers = { choice: () => undefined };
  if (LC) LC.stopChoice(api, id, $("confirm-servers")).then((c) => { if (confirmFor === id) stopServers = c; });
  else $("confirm-servers").hidden = true;
  $("confirm-dialog").showModal();
  $("confirm-yes").focus();
}
let stopServers = { choice: () => undefined };

async function doStop() {
  const id = confirmFor;
  confirmFor = null;
  $("confirm-dialog").close();
  if (!id) return;
  const servers = stopServers.choice();
  try {
    await api(`/api/agents/${encodeURIComponent(id)}/stop`, { method: "POST", body: servers === undefined ? {} : { servers } });
    toast(`Stopping ${nameOf(S.agents.get(id))}`);
  } catch (e) {
    toast(`Could not stop it: ${e.message}`, true);
  }
}
$("confirm-yes").addEventListener("click", doStop);
$("confirm-no").addEventListener("click", () => { confirmFor = null; $("confirm-dialog").close(); });
$("confirm-dialog").addEventListener("keydown", (e) => {
  if (e.key === "y" || e.key === "Y") { e.preventDefault(); doStop(); }
  else if (e.key === "n" || e.key === "N") { e.preventDefault(); confirmFor = null; $("confirm-dialog").close(); }
});

// -------------------------------------------------------- decision trail
// SPEC.md, "Decision trail": the Changes tab (the agent's diff), the
// decision card (a side sheet), Rewind (the agent's terminal played back at
// the moment of the edit) and the Decisions screen with Ask why.

const DEC_REPO_KEY = "chloedb-workbench-dec-repo";
const SHEET_WIDE_KEY = "chloedb-workbench-sheet-wide";
const EDIT_TOOLS = new Set(["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"]);
const enc = encodeURIComponent;
const ID_RE = /(\b(?=[0-9a-z]{0,7}[a-z])[0-9a-z]{4,8}:\d+\b)/i;

// Answers kept for the page's life; an agent's are dropped when it edits.
const cache = { diff: new Map(), why: new Map(), dec: new Map() };

function cached(map, key, load, fresh) {
  if (fresh || !map.has(key)) {
    const p = load();
    map.set(key, p);
    p.catch(() => { if (map.get(key) === p) map.delete(key); });
  }
  return map.get(key);
}
const getDiff = (id, fresh) => cached(cache.diff, id, () => api(`/api/agents/${enc(id)}/diff`), fresh);
// SPEC.md, "Sixth build: Ship and Trace tell the truth": a line clicked
// from a commit is read and blamed as that commit has it (commit=).
// A line of the main checkout (Library, Search) has no agent: it is asked of
// the project (git blame, and the agents' sessions that touched the file).
const whoOf = (agent) => (agent ? `agent=${enc(agent)}` : `repo=${enc(pickRepo() || currentProject()?.path || "")}`);
const getWhy = (agent, path, line, commit) => cached(cache.why, `${agent || ""}\u0001${path}\u0001${line}\u0001${commit || ""}`, () => api(`/api/why?${whoOf(agent)}&path=${enc(path)}&line=${line}${commit ? `&commit=${enc(commit)}` : ""}`));
const getDecisions = (repo, fresh) => cached(cache.dec, repo, () => api(`/api/decisions?repo=${enc(repo)}&limit=200`), fresh);

function forgetAgent(id) {
  cache.diff.delete(id);
  for (const k of cache.why.keys()) if (k.startsWith(`${id}\u0001`)) cache.why.delete(k);
}

function hm(ms) { return new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }); }
function whenPhrase(ms) {
  const d = new Date(ms);
  if (d.toDateString() === new Date().toDateString()) return `at ${hm(ms)}`;
  if (d.toDateString() === new Date(Date.now() - 864e5).toDateString()) return `yesterday at ${hm(ms)}`;
  return `on ${d.toLocaleDateString([], { month: "short", day: "numeric" })} at ${hm(ms)}`;
}
function fmtFull(ms) { return new Date(ms).toLocaleString([], { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit" }); }
function agoText(ms) { const d = Date.now() - ms; return d < 45_000 ? "just now" : `${fmtDur(d)} ago`; }
function mmss(ms) { const s = Math.max(0, Math.round(ms / 1000)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`; }
const nrm = (s) => String(s || "").replace(/\s+/g, " ").trim().toLowerCase();
const short = (sha, n = 7) => (sha ? String(sha).slice(0, n) : "");
const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

// Text with record ids (5be0c7d2:12) set as code.
function withIds(text) {
  return String(text ?? "").split(ID_RE).map((part, i) => (i % 2 ? h("code", { class: "rid", text: part }) : part));
}

function liveAgent(a) {
  if (!a) return null;
  return S.agents.get(a.id) || a;
}

function ancestry(a) {
  const out = [];
  let p = a?.parent;
  while (p && out.length < 16) {
    const pa = S.agents.get(p);
    if (!pa) break;
    out.push(pa);
    p = pa.parent;
  }
  return out;
}

function goAgent(a, opts) {
  if (!a) return;
  if (!S.agents.has(a.id)) upsert({ ...a });
  const t = opts && opts.tab;
  go(`#/a/${a.id}${t ? "/" + (t === "term" ? "terminal" : t) : ""}`);
}

function agentChip(a, { self = false, keepSheet = false } = {}) {
  const live = liveAgent(a);
  if (!live) return h("span", { class: "ag you", title: "You" }, icon("user"), h("span", { class: "nm", text: "you" }));
  return h("button", {
    class: `ag${self ? " self" : ""}`, "data-state": live.state || "exited", type: "button",
    title: `${nameOf(live)}, ${STATE_LABEL[live.state] || live.state || ""}: zoom it\n${live.task || ""}`,
    onclick: (e) => { e.stopPropagation(); if (!keepSheet) closeSheet({ focus: false }); goAgent(live); },
  }, h("span", { class: "dot" }), h("span", { class: "nm", text: nameOf(live) }));
}

function chain(agent, lineage) {
  const parents = (lineage && lineage.length ? lineage : ancestry(agent)).slice().reverse();
  const items = [agentChip(null)];
  for (const p of parents) items.push(h("span", { class: "arr", "aria-hidden": "true", text: "→" }), agentChip(p));
  if (agent) items.push(h("span", { class: "arr", "aria-hidden": "true", text: "→" }), agentChip(agent, { self: true }));
  return h("div", { class: "chain", role: "list", "aria-label": "Who started whom" }, items);
}

function copyText(text, what) {
  const done = () => toast(`Copied ${what}`);
  try {
    if (navigator.clipboard?.writeText) { navigator.clipboard.writeText(text).then(done, () => toast("The browser refused the copy", true)); return; }
  } catch { /* fall through */ }
  toast("The browser refused the copy", true);
}

function section(title, ...kids) {
  return h("section", { class: "sec" }, h("h3", { class: "sec-h", text: title }), ...kids);
}

function skel(n = 3) {
  return h("div", { class: "skel" }, Array.from({ length: n }, (_, i) => h("i", { style: `width:${[92, 76, 84, 60, 70][i % 5]}%` })));
}

// ------------------------------------------------------------- Changes

const C = { id: null, data: null, err: null, req: 0, rows: [], cursor: -1, files: [], collapsed: new Set(), want: null, timer: 0, stale: false };

function changesVisible() { return S.zoomed && $("z-body").dataset.tab === "changes" && !$("zoom-view").hidden; }

// SPEC.md, "Fourth build" (use case 7): the Blast radius summary above the
// diff, ui/levels/blast-summary.js, mounted once; it reads when it shows.
let blastSummary = null;
function showBlastSummary(id, fresh) {
  blastSummary ||= import("/levels/blast-summary.js").then((m) => {
    const host = h("div", { class: "ch-blast" });
    $("ch-scroll").before(host);
    return m.mount(host, { api, go });
  }).catch((err) => { console.error(err); return null; });
  blastSummary.then((s) => s && s.show(id, { fresh }));
}

async function loadChanges(id, { fresh = false, quiet = false } = {}) {
  showBlastSummary(id, fresh);
  const req = ++C.req;
  if (C.id !== id) { C.id = id; C.data = null; C.err = null; C.cursor = -1; C.collapsed = new Set(); }
  if (!quiet && !C.data) renderChanges();
  try {
    const d = await getDiff(id, fresh);
    if (req !== C.req || C.id !== id) return;
    C.data = d;
    C.err = null;
    C.stale = false;
  } catch (e) {
    if (req !== C.req || C.id !== id) return;
    C.err = e;
  }
  renderChanges();
}

function fileCounts(f) {
  let add = 0, del = 0;
  for (const hk of f.hunks || []) for (const l of hk.lines || []) { if (l.kind === "+") add++; else if (l.kind === "-") del++; }
  return [add, del];
}

const STATUS = { added: ["A", "added"], modified: ["M", "modified"], deleted: ["D", "deleted"], renamed: ["R", "renamed"] };
function statusOf(s) {
  const k = String(s || "modified").toLowerCase();
  return STATUS[k] || Object.values(STATUS).find(([l]) => l === k[0]?.toUpperCase()) || ["M", k];
}

function renderChanges() {
  const box = $("ch-scroll");
  const badge = document.querySelector("#ag-tabs .n") || { textContent: "" };
  const sum = $("ch-sum");
  const cur = C.rows[C.cursor];
  const keep = cur ? { path: cur.path, n: cur.n, old: cur.old, kind: cur.kind } : null;
  const hadFocus = box.contains(document.activeElement);
  const top = box.scrollTop;
  C.rows = [];
  C.files = [];
  if (C.err && !C.data) {
    badge.textContent = "";
    sum.textContent = "";
    const notYet = C.err.status === 404 && /route/i.test(C.err.message);
    box.replaceChildren(h("div", { class: "ch-empty bad" }, icon("warn", "i"), h("div", null,
      h("b", { text: notYet ? "This server has no diff route yet." : "The diff could not be read." }),
      h("p", { text: C.err.message }),
      h("button", { class: "btn sm", type: "button", onclick: () => loadChanges(C.id, { fresh: true }) }, "Try again"))));
    return;
  }
  if (!C.data) {
    badge.textContent = "";
    sum.textContent = "Reading the diff…";
    box.replaceChildren(h("div", { class: "ch-load" }, skel(6)));
    return;
  }
  const files = C.data.files || [];
  let add = 0, del = 0;
  for (const f of files) { const [a, d] = fileCounts(f); add += a; del += d; }
  badge.textContent = files.length ? String(files.length) : "";
  sum.replaceChildren(...[
    h("b", { text: plural(files.length, "file") }),
    files.length ? h("span", { class: "n-add", text: `+${add}` }) : null,
    files.length ? h("span", { class: "n-del", text: `−${del}` }) : null,
    h("span", { class: "since" }, "from ", h("code", { title: C.data.base || "", text: short(C.data.base) || "its base" })),
    C.stale ? h("span", { class: "stale", text: "· changed since, refreshing" }) : null].filter(Boolean));
  if (!files.length) {
    const a = S.agents.get(C.id);
    box.replaceChildren(h("div", { class: "ch-empty" }, icon("check", "i"), h("div", null,
      h("b", { text: "No changes yet" }),
      h("p", { text: `${a ? nameOf(a) : "This agent"} has not changed anything since ${short(C.data.base) || "its base"}. Its edits show here as it makes them, each line with a Why?` }))));
    return;
  }
  const frag = document.createDocumentFragment();
  files.forEach((f, fi) => frag.append(fileBlock(f, fi)));
  box.replaceChildren(frag);
  box.scrollTop = top;
  // Back where the cursor was, or where a card asked to go.
  const fromCard = !!C.want;
  const want = C.want || keep;
  C.want = null;
  if (want) {
    const i = C.rows.findIndex((r) => r.path === want.path && r.kind === want.kind && (r.kind === "-" ? r.old === want.old : r.n === want.n));
    if (i >= 0) setCursor(i, { focus: !fromCard && hadFocus, center: fromCard });
  }
  if (T.line && T.line.agent === C.id) markLine(T.line, T.lineState);
}

function fileBlock(f, fi) {
  const [code, label] = statusOf(f.status);
  const [add, del] = fileCounts(f);
  const path = f.path || f.old_path || "?";
  const slash = path.lastIndexOf("/");
  const collapsed = C.collapsed.has(path);
  const head = h("button", { class: "cf-head", type: "button", "aria-expanded": String(!collapsed), title: collapsed ? "Show this file" : "Fold this file" },
    icon("chev", "i sm tw"),
    h("span", { class: `fst fst-${code}`, title: label, text: code }),
    h("span", { class: "path" }, slash >= 0 ? h("span", { class: "dir", text: path.slice(0, slash + 1) }) : null, h("b", { text: path.slice(slash + 1) })),
    f.old_path && f.old_path !== path ? h("span", { class: "from", text: `from ${f.old_path}` }) : null,
    f.untracked ? h("span", { class: "ut", title: "Not yet added to git", text: "untracked" }) : null,
    h("span", { class: "cnt" }, add ? h("span", { class: "n-add", text: `+${add}` }) : null, del ? h("span", { class: "n-del", text: `−${del}` }) : null));
  const sec = h("section", { class: `cf${collapsed ? " folded" : ""}`, "data-path": path }, head);
  head.addEventListener("click", () => {
    if (C.collapsed.has(path)) C.collapsed.delete(path); else C.collapsed.add(path);
    renderChanges();
  });
  C.files.push({ path, first: C.rows.length });
  const body = h("div", { class: "cf-body" });
  const inner = h("div", { class: "cf-inner" });
  body.append(inner);
  sec.append(body);
  if (f.binary || f.too_large || !(f.hunks || []).length) {
    inner.append(h("div", { class: "cf-note", text: f.binary ? "A binary file: not shown." : f.too_large ? "Too large to show here." : code === "D" ? "Deleted." : code === "R" ? "Renamed, with no change to its text." : "No lines to show." }));
    return sec;
  }
  let shown = 0;
  for (const hk of f.hunks) {
    let o = hk.old_start || 0, n = hk.new_start || 0;
    const lines = hk.lines || [];
    const oc = lines.filter((l) => l.kind !== "+").length, nc = lines.filter((l) => l.kind !== "-").length;
    inner.append(h("div", { class: "hk", text: `@@ -${o}${oc !== 1 ? `,${oc}` : ""} +${n}${nc !== 1 ? `,${nc}` : ""} @@` }));
    if (o === 0 && oc) o = 1;
    if (n === 0 && nc) n = 1;
    for (const l of lines) {
      if (shown++ > 4000) break;
      const kind = l.kind === "+" || l.kind === "-" ? l.kind : " ";
      const oldN = kind === "+" ? null : (l.old_line ?? o);
      const newN = kind === "-" ? null : (l.new_line ?? n);
      if (kind !== "+") o = (oldN ?? o) + 1;
      if (kind !== "-") n = (newN ?? n) + 1;
      const i = C.rows.length;
      const cls = kind === "+" ? "add" : kind === "-" ? "del" : "ctx";
      const el = h("div", { class: `dl ${cls}`, "data-i": String(i), tabindex: "-1" },
        h("span", { class: "gut" }, h("span", { class: "o", text: oldN ?? "" }), h("span", { class: "n", text: newN ?? "" })),
        h("span", { class: "m", "aria-hidden": "true", text: kind === " " ? "" : kind === "-" ? "−" : "+" }),
        h("span", { class: "t", text: l.text ?? "" }));
      if (!collapsed) C.rows.push({ el, kind, path, n: newN, old: oldN, text: l.text ?? "", file: fi });
      inner.append(el);
    }
    if (shown > 4000) { inner.append(h("div", { class: "cf-note", text: "The rest of this file's diff is not shown." })); break; }
  }
  return sec;
}

// One Why? button for the whole diff (sixth build): it sits on the line
// under the pointer, else the cursor's line, and only on added lines.
const whyBtn = h("button", { class: "why", type: "button", tabindex: "-1", title: "Who decided this line, when, and why (y, or Alt-click the line)" }, "Why?");
function placeWhy(el) {
  const target = el && el.classList.contains("add") ? el : C.rows[C.cursor]?.el;
  if (target && target.classList.contains("add")) { if (whyBtn.parentNode !== target) target.append(whyBtn); }
  else whyBtn.remove();
}
$("ch-scroll").addEventListener("mouseover", (e) => { const row = e.target.closest?.(".dl"); if (row) placeWhy(row); });
$("ch-scroll").addEventListener("mouseleave", () => placeWhy(null));

function setCursor(i, { focus = true, center = false } = {}) {
  if (!C.rows.length) return;
  i = Math.max(0, Math.min(C.rows.length - 1, i));
  C.rows[C.cursor]?.el.classList.remove("cur");
  C.cursor = i;
  const r = C.rows[i];
  r.el.classList.add("cur");
  placeWhy(r.el);
  if (focus) r.el.focus({ preventScroll: true });
  r.el.scrollIntoView({ block: center ? "center" : "nearest" });
}

function rowWhy(i) {
  const r = C.rows[i];
  if (!r) return;
  if (r.kind !== "+") {
    toast(r.kind === "-" ? "A removed line has no trail: only lines the agent added do." : `${nameOf(S.agents.get(C.id))} did not write this line: it is unchanged from the base.`);
    return;
  }
  openLineCard(C.id, r.path, r.n, { text: r.text });
}

$("ch-scroll").addEventListener("mousedown", (e) => { if (e.target.closest(".why")) e.preventDefault(); });
$("ch-scroll").addEventListener("click", (e) => {
  const row = e.target.closest(".dl");
  if (!row) return;
  const i = C.rows.findIndex((r) => r.el === row);
  if (i < 0) return;
  setCursor(i);
  if (e.target.closest(".why") || e.altKey) { e.preventDefault(); rowWhy(i); }
});
$("ch-scroll").addEventListener("focus", (e) => {
  if (e.target !== $("ch-scroll") || !C.rows.length) return;
  setCursor(C.cursor >= 0 ? C.cursor : Math.max(0, C.rows.findIndex((r) => r.kind === "+")));
});
$("ch-refresh").addEventListener("click", () => { if (C.id) { forgetAgent(C.id); loadChanges(C.id, { fresh: true }); } });

function changesKey(e) {
  const k = e.key;
  const step = (d) => { setCursor((C.cursor < 0 ? (d > 0 ? -1 : C.rows.length) : C.cursor) + d); return true; };
  if (k === "j" || k === "ArrowDown") return step(1);
  if (k === "k" || k === "ArrowUp") return step(-1);
  if (k === "PageDown" || k === "PageUp") return step(k === "PageDown" ? 20 : -20);
  if (k === "]" || k === "[") {
    const cur = C.rows[C.cursor]?.file ?? -1;
    const list = C.files.filter((f) => C.rows[f.first] && C.rows[f.first].path === f.path);
    const idx = list.findIndex((f) => C.rows[f.first].file === cur);
    const next = list[(idx + (k === "]" ? 1 : -1) + list.length) % list.length];
    if (next) setCursor(next.first, { center: false });
    return true;
  }
  if ((k === "y" || k === "Enter") && C.cursor >= 0) { rowWhy(C.cursor); return true; }
  if (k === "g") return setCursor(0), true;
  if (k === "G") return setCursor(C.rows.length - 1), true;
  return false;
}

function rowOf(t) {
  if (!t || C.id !== t.agent) return null;
  return C.rows.find((r) => r.kind === "+" && r.path === t.path && r.n === t.line) || null;
}

// The line a card is about: pinned while the card is open, pending while a
// Rewind plays before the edit, and a flash at the moment it is written.
function markLine(t, state) {
  for (const el of document.querySelectorAll(".dl.pin, .dl.ghost")) el.classList.remove("pin", "ghost");
  const line = $("sheet").querySelector(".sh-line");
  if (line) line.classList.toggle("ghost", state === "pending");
  if (!t) return;
  const r = rowOf(t);
  if (!r) return;
  r.el.classList.add("pin");
  if (state === "pending") r.el.classList.add("ghost");
}

function flashLine(t) {
  const line = $("sheet").querySelector(".sh-line");
  for (const el of [line, rowOf(t)?.el].filter(Boolean)) {
    el.classList.remove("ghost", "flash");
    void el.offsetWidth;
    el.classList.add("flash");
  }
  const r = rowOf(t);
  if (r && changesVisible()) r.el.scrollIntoView({ block: "nearest", behavior: "smooth" });
}

// -------------------------------------------------------------- the sheet

const T = { open: false, kind: null, key: null, req: 0, player: null, stack: [], line: null, lineState: null, reopen: null, card: null };

function sheetLayout() {
  const wide = document.body.classList.contains("sheet-wide");
  $("sh-wide").setAttribute("aria-pressed", String(wide));
  requestAnimationFrame(() => { try { Z.fit?.fit(); } catch { /* hidden */ } T.player?.scale(); if (D.open) drawLinks(); });
}

function showSheet() {
  const sh = $("sheet");
  if (sh.hidden) {
    sh.hidden = false;
    document.body.classList.add("sheet-open");
    if (store("local", SHEET_WIDE_KEY) === "1") document.body.classList.add("sheet-wide");
    sheetLayout();
  }
  T.open = true;
}

function closeSheet({ focus = true } = {}) {
  if (!T.open) return;
  T.req++;
  T.player?.dispose();
  T.player = null;
  T.open = false;
  T.kind = T.key = T.reopen = T.card = null;
  T.stack = [];
  T.line = null;
  T.lineState = null;
  markLine(null);
  $("sheet").hidden = true;
  $("sh-body").replaceChildren();
  document.body.classList.remove("sheet-open", "sheet-wide");
  sheetLayout();
  for (const el of document.querySelectorAll(".dc.sel")) el.classList.remove("sel");
  if (!focus) return;
  if (changesVisible() && C.rows[C.cursor]) C.rows[C.cursor].el.focus({ preventScroll: true });
  else if (D.open) (document.querySelector(".dc.cur .dc-open") || $("tl"))?.focus?.({ preventScroll: true });
}

$("sh-close").addEventListener("click", () => closeSheet());
$("sh-wide").addEventListener("click", () => toggleWide());
$("sh-back").addEventListener("click", () => sheetBack());

function toggleWide() {
  const on = document.body.classList.toggle("sheet-wide");
  store("local", SHEET_WIDE_KEY, on ? "1" : null);
  sheetLayout();
}

function sheetBack() {
  const prev = T.stack.pop();
  if (prev) prev(true);
}

function sheetHead(kind, loc, copy) {
  $("sh-kind").textContent = kind;
  const l = $("sh-loc");
  l.replaceChildren();
  if (loc) {
    l.append(h("span", { text: loc, title: loc }));
    if (copy) l.append(h("button", { class: "btn ghost icon xs", type: "button", title: `Copy ${copy}`, "aria-label": `Copy ${copy}`, onclick: () => copyText(loc, copy) }, icon("copy")));
  }
  $("sh-back").hidden = !T.stack.length;
}

function beginCard(kind, key, reopen, { push = false, back = false } = {}) {
  if (!back) {
    if (push && T.open && T.reopen && T.key !== key) T.stack.push(T.reopen);
    else if (!push) T.stack = [];
  }
  T.kind = kind;
  T.key = key;
  T.reopen = reopen;
  T.player?.dispose();
  T.player = null;
  T.line = null;
  T.lineState = null;
  markLine(null);
  showSheet();
  $("sh-body").scrollTop = 0;
  return ++T.req;
}

// --------------------------------------------------------- the line card

async function openLineCard(agentId, path, line, { text, commit, push = false, back = false } = {}) {
  const key = `line\u0001${agentId}\u0001${path}\u0001${line}\u0001${commit || ""}`;
  if (T.open && T.key === key && !back) return;
  const req = beginCard("line", key, (b) => openLineCard(agentId, path, line, { text, commit, back: b }), { push, back });
  T.line = { agent: agentId, path, line };
  markLine(T.line);
  sheetHead("Decision trail", `${path}:${line}`, "path:line");
  $("sh-body").replaceChildren(
    lineBlock(line, text),
    h("div", { class: "tracing" }, h("span", { class: "spin", "aria-hidden": "true" }), "Tracing this line through ChloeCtx's records and Workbench's sessions…"),
    skel(4));
  try {
    const card = await getWhy(agentId, path, line, commit);
    if (req !== T.req) return;
    T.card = card;
    renderLineCard(card, agentId);
  } catch (e) {
    if (req !== T.req) return;
    $("sh-body").replaceChildren(lineBlock(line, text), h("div", { class: "callout bad" }, icon("warn", "i"), h("div", null,
      h("b", { text: e.status === 404 && /route/i.test(e.message) ? "This server cannot answer Why? yet." : "The trail could not be read." }),
      h("p", { text: e.message }),
      h("button", { class: "btn sm", type: "button", onclick: () => { cache.why.delete(`${agentId}\u0001${path}\u0001${line}\u0001${commit || ""}`); openLineCard(agentId, path, line, { text, commit, back: true }); } }, "Try again"))));
  }
}

function rewindLead(a, rw) {
  const lead = Math.round(((rw.edit_ms ?? rw.to_ms - 3000) - rw.from_ms) / 1000);
  return lead < 1 ? `${nameOf(a)}'s terminal, from the start of its session` : `${nameOf(a)}'s terminal, ${lead} s before the edit`;
}

function lineBlock(n, text) {
  return h("div", { class: "sh-line" }, h("span", { class: "ln", text: n != null ? String(n) : "" }), h("code", { text: text ?? "" }));
}

function headline(card) {
  const a = liveAgent(card.who?.agent);
  const editMs = toMs(card.when?.edit_ts);
  const commit = card.where?.commit;
  const p = card.who?.prompt;
  const out = [];
  // "Wrote this" only on the evidence of an edit (ChloeCtx's or Workbench's
  // tool call); else what is known: the commit, and whose branch it is on.
  if (a && card.when) {
    out.push(h("b", { class: "hn", text: nameOf(a) }), " wrote this");
    if (editMs) out.push(` ${whenPhrase(editMs)}`);
    else if (commit?.date) out.push(`, committed ${whenPhrase(toMs(commit.date))}`);
    if (p?.by) out.push(", on ", p.by === "you" ? "your" : h("b", { class: "hn", text: `${p.by}'s` }), " prompt");
    if (card.found === "chloectx") {
      const n = (card.knew || []).length;
      out.push(n ? `, after recalling ${plural(n, "note")}` : ", with nothing recalled from memory");
    }
    out.push(".");
  } else if (a && commit) {
    out.push("This line came in with ", h("code", { text: short(commit.sha) }), ` ${whenPhrase(toMs(commit.date))}`, commit.subject ? `, “${commit.subject}”` : "", ", on ", h("b", { class: "hn", text: nameOf(a) }), "'s branch. No recorded edit shows who typed it.");
  } else if (a) {
    out.push(h("b", { class: "hn", text: nameOf(a) }), "'s worktree has this line, but no recorded edit shows who wrote it.");
  } else if (commit) {
    out.push("This line came from commit ", h("code", { text: short(commit.sha) }), ` ${whenPhrase(toMs(commit.date))}`, commit.subject ? `, “${commit.subject}”` : "", ". No agent's session is on record for it.");
  } else {
    out.push("Nothing on record says who wrote this line.");
  }
  return h("h2", { class: "headline" }, out);
}

function partialNote(card) {
  const miss = (card.missing || []).filter((m) => m && !(card.found === "chloectx" && !card.where?.commit && /not committed/i.test(m)));
  if (!miss.length) return null;
  const from = { chloectx: "ChloeCtx's records", git: "git alone", workbench: "Workbench's rows" }[card.found] || card.found;
  const full = card.found === "chloectx";
  const list = h("ul", null, miss.map((m) => h("li", null, withIds(m))));
  return h("div", { class: `callout ${full ? "soft" : "info"}` }, icon("info", "i"), h("div", null,
    h("b", { text: full ? `From ${from}. ${miss.length === 1 ? "One thing" : `${miss.length} things`} could not be found:` : `A partial answer, from ${from}. Not found:` }),
    list));
}

function promptQuote(p, agent) {
  if (!p || !p.text) return null;
  const by = p.by === "you" ? "You" : p.by;
  const q = h("blockquote", { class: "prompt clamp" }, p.text);
  const more = h("button", { class: "linkish", type: "button", hidden: true, onclick: () => { q.classList.toggle("clamp"); more.textContent = q.classList.contains("clamp") ? "Show all" : "Show less"; } }, "Show all");
  requestAnimationFrame(() => { if (q.scrollHeight > q.clientHeight + 2) more.hidden = false; });
  const ms = toMs(p.ts);
  return h("div", { class: "pq" },
    h("div", { class: "pq-by" }, icon(p.by === "you" ? "user" : "turn"), h("span", null, h("b", { text: by }), ` prompted ${agent ? nameOf(agent) : "it"}`, ms ? ` ${whenPhrase(ms)}` : "")),
    q, more);
}

function facts(rows) {
  const dl = h("dl", { class: "kv" });
  for (const [k, v, title] of rows) {
    if (v == null || v === "" || (Array.isArray(v) && !v.length)) continue;
    dl.append(h("dt", { text: k }), h("dd", { title: title || (typeof v === "string" ? v : null) }, v));
  }
  return dl;
}

function sessShort(s) { return s ? h("code", { title: s, text: String(s).slice(0, 8) }) : null; }

function renderLineCard(card, agentId) {
  const a = liveAgent(card.who?.agent);
  const editMs = toMs(card.when?.edit_ts);
  const commit = card.where?.commit;
  const body = [];
  if (card.line) sheetHead("Decision trail", `${card.line.path}:${card.line.line}`, "path:line");
  body.push(lineBlock(card.line?.line, card.line?.text));
  body.push(headline(card));
  body.push(partialNote(card));

  // Who.
  if (a || card.who?.prompt) body.push(section("Who", chain(a, (card.who?.lineage || []).map(liveAgent)), promptQuote(card.who?.prompt, a)));

  // Rewind, the moment itself.
  if (card.rewind && a) {
    const host = h("div", { class: "rw" });
    body.push(h("section", { class: "sec rw-sec" }, h("div", { class: "sec-hrow" }, h("h3", { class: "sec-h", text: "Rewind" }),
      h("span", { class: "sec-sub", text: rewindLead(a, card.rewind) })), host));
    const t = { ...T.line };
    T.player = new Rewind(host, {
      agent: card.rewind.agent || a.id, from: card.rewind.from_ms, to: card.rewind.to_ms, edit: card.rewind.edit_ms, name: nameOf(a),
      onWrite: (state) => {
        if (state === "flash") { T.lineState = "written"; flashLine(t); return; }
        T.lineState = state;
        markLine(t, state);
      },
    });
  }

  // When and where.
  const turn = card.when?.turn;
  const when = facts([
    ["edited", editMs ? h("span", null, fmtFull(editMs), h("span", { class: "muted", text: ` · ${agoText(editMs)}` })) : null, card.when?.how ? `How: ${card.when.how}` : null],
    ["turn", card.when ? h("span", null, turn != null ? `${turn} of session ` : "session ", sessShort(card.when.session)) : null],
    ["commit", commit ? h("span", null, h("code", { title: commit.sha, text: short(commit.sha) }), commit.subject ? ` ${commit.subject}` : "", commit.date ? h("span", { class: "muted", text: ` · ${whenPhrase(toMs(commit.date))}` }) : null) : h("span", { class: "muted", text: "not committed yet" })],
  ]);
  if (!card.when) when.prepend(h("dt", { text: "edited" }), h("dd", { class: "muted", text: "not on record" }));
  const showIn = card.line && a && !(S.zoomed === a.id && changesVisible())
    ? h("button", { class: "linkish", type: "button", onclick: () => showInChanges(a, card.line.path, card.line.line) }, `Show in ${nameOf(a)}'s changes →`)
    : null;
  const where = facts([
    ["repo", shortPath(card.where?.repo), card.where?.repo],
    ["worktree", card.where?.worktree && card.where.worktree !== card.where.repo ? shortPath(card.where.worktree) : null, card.where?.worktree],
    ["branch", card.where?.branch ? h("span", null, icon("branch"), ` ${card.where.branch}`) : null],
    ["line", card.line ? h("span", null, h("code", { text: `${card.line.path}:${card.line.line}` })) : null],
  ]);
  body.push(h("div", { class: "two-col" }, section("When", when), section("Where", where, showIn)));

  // Why: the notes of that turn.
  const notes = card.why || [];
  body.push(section("Why", notes.length
    ? [card.why_from === "session" ? h("p", { class: "muted", text: `Notes ChloeCtx kept from ${a ? nameOf(a) : "the agent"}'s session between the prompt and the commit. No edit record ties them to this very line.` }) : null,
      h("ol", { class: "notes" }, notes.map((n) => noteEl(n, card)))]
    : h("p", { class: "empty-s", text: card.found === "chloectx" ? "No decision, preference or fact was distilled from that turn." : a ? `ChloeCtx kept no decision, preference or fact from ${nameOf(a)}'s session before this line's commit.` : "No agent's session is on record for this line, so there are no notes of its turn." })));

  // What it knew.
  const knew = card.knew || [];
  body.push(section(`What it knew${knew.length ? ` · ${knew.length}` : ""}`, knew.length
    ? h("ol", { class: "recs" }, knew.map((k) => recordEl(k, card)))
    : h("p", { class: "empty-s", text: card.found === "chloectx" ? "Memory had put nothing in its context before the edit." : "Unknown: the edit's receipts are not available." })));

  // Worth a look.
  const worth = card.worth_a_look || [];
  if (worth.length) body.push(section("Worth a look", h("ul", { class: "worth" }, worth.map((w) => h("li", null, icon("warn"), h("span", null, withIds(typeof w === "string" ? w : JSON.stringify(w))))))));

  body.push(h("p", { class: "src" }, `Answered from ${{ chloectx: "ChloeCtx's blame, Workbench's rows and git", workbench: "Workbench's rows and git", git: "git" }[card.found] || card.found}.`));
  $("sh-body").replaceChildren(...body.filter(Boolean));
  enrichFromDecisions(card);
}

function noteEl(n, card) {
  const ms = toMs(n.ts);
  return h("li", { class: `note k-${n.kind || "decision"}`, "data-text": nrm(n.text), "data-id": n.id || "" },
    h("div", { class: "note-top" }, h("span", { class: "tag", text: n.kind || "note" }), ms ? h("span", { class: "muted", text: hm(ms) }) : null, h("span", { class: "note-x" })),
    h("blockquote", null, n.text || ""));
}

function nowOf(k) {
  const now = String(k.now || "current");
  if (/^superseded/.test(now)) return { state: "superseded", by: (now.match(/by\s+(\S+)/) || [])[1] || null };
  if (/^deleted/.test(now)) return { state: "deleted" };
  return { state: "current" };
}

function recordEl(k, card) {
  const st = nowOf(k);
  const score = typeof k.score === "number" ? k.score : null;
  const flags = [];
  if (st.state === "superseded") {
    flags.push(h("button", { class: "flag warn", type: "button", title: st.by ? `Superseded since, by ${st.by}: open it on the Decisions screen` : "Superseded since", "data-by": st.by || "", onclick: () => st.by && openDecisions({ repo: card.where?.repo, focus: st.by, open: true }) },
      icon("warn"), h("span", { class: "fl-t" }, "superseded", st.by ? " by " : "", st.by ? h("code", { text: st.by }) : null)));
  }
  if (st.state === "deleted") flags.push(h("span", { class: "flag crit" }, icon("x"), "deleted since"));
  if (k.untrusted) flags.push(h("span", { class: "flag crit", title: "It came from a tool's output (the web, or another tool), not from you or the code" }, icon("warn"), "untrusted source"));
  return h("li", { class: `rec ${st.state}${k.untrusted ? " untrusted" : ""}`, "data-id": k.id || "" },
    h("div", { class: "rec-top" },
      h("span", { class: "tag", text: k.kind || "record" }),
      k.id ? h("code", { class: "rid", text: k.id }) : null,
      h("span", { class: "sep" }),
      score != null ? h("span", { class: "score", title: `Score ${score.toFixed(3)}` }, h("i", { style: `--w:${Math.max(4, Math.min(100, score * 100)).toFixed(0)}%` }), score.toFixed(2)) : null,
      k.decision ? h("span", { class: "rdec", title: "What the recall did with it", text: k.decision }) : null),
    h("p", { class: "rec-text" }, k.text || h("span", { class: "muted", text: "(its text is gone)" })),
    flags.length ? h("div", { class: "flags" }, flags) : null);
}

// With the repository's decisions at hand, a superseded record names what
// replaced it, and a note of this very turn that replaced it says so.
async function enrichFromDecisions(card) {
  const repo = card.where?.repo;
  if (!repo || !(card.knew || []).some((k) => nowOf(k).state === "superseded")) return;
  const req = T.req;
  let list;
  try { list = (await getDecisions(repo)).decisions || []; } catch { return; }
  if (req !== T.req) return;
  const byId = new Map(list.map((d) => [d.id, d]));
  for (const li of $("sh-body").querySelectorAll(".rec.superseded")) {
    const btn = li.querySelector(".flag.warn");
    const by = btn?.dataset.by && byId.get(btn.dataset.by);
    if (!by) continue;
    const note = [...$("sh-body").querySelectorAll(".note")].find((n) => (by.id && n.dataset.id === by.id) || n.dataset.text === nrm(by.text));
    const who = by.agent ? nameOf(liveAgent(by.agent)) : "another session";
    li.append(h("p", { class: "rec-by" }, h("span", { class: "muted", text: note ? "Overridden in this very turn: " : `Since, ${who} decided: ` }), h("span", { text: `“${by.text}”` })));
    if (note) {
      note.classList.add("overrides");
      note.querySelector(".note-x").replaceChildren(h("span", { class: "flag good" }, icon("check"), "overrides what it recalled"));
    }
  }
}

function showInChanges(a, path, line) {
  C.want = { path, n: line, kind: "+" };
  if (S.zoomed === a.id && Z.tab === "changes") { if (C.data && C.id === a.id) renderChanges(); return; }
  goAgent(a, { tab: "changes" });
}

// ------------------------------------------------------------- Rewind

class Rewind {
  constructor(host, o) {
    this.o = o;
    this.host = host;
    this.speed = 1;
    this.playing = false;
    this.started = false;
    this.written = null;
    this.idx = 0;
    this.disposed = false;
    this.from = o.from;
    this.to = o.to;
    this.edit = o.edit ?? o.to - 3000;
    this.inner = h("div", { class: "rw-inner" });
    this.scr = h("div", { class: "rw-scr" }, this.inner);
    this.poster = h("button", { class: "rw-poster", type: "button", disabled: true, onclick: () => this.play() },
      h("span", { class: "rw-pbtn" }, icon("play", "i")),
      h("span", { class: "rw-ptext" }, h("b", { text: "Rewind" }), h("span", { text: `Watch ${o.name} write this line` })),
      h("kbd", { text: "r" }));
    this.note = h("div", { class: "rw-note", hidden: true });
    this.stage = h("div", { class: "rw-stage loading" }, this.scr, this.poster, this.note);
    this.btn = h("button", { class: "btn ghost icon sm rw-play", type: "button", title: "Play (Space)", "aria-label": "Play", disabled: true, onclick: () => this.toggle() }, icon("play"));
    this.time = h("span", { class: "rw-time", text: "0:00 / 0:00" });
    this.fill = h("span", { class: "rw-fill" });
    this.thumb = h("span", { class: "rw-thumb" });
    this.mark = h("span", { class: "rw-mark", title: "The edit" }, h("span", { class: "rw-mlabel", text: "edit" }));
    this.track = h("div", { class: "rw-track", role: "slider", tabindex: "0", "aria-label": "Position in the recording", "aria-valuemin": "0" }, h("span", { class: "rw-rail" }), this.fill, this.mark, this.thumb);
    this.speeds = h("div", { class: "seg", role: "group", "aria-label": "Speed" }, [1, 2, 4].map((x) => h("button", { type: "button", "data-x": String(x), "aria-pressed": String(x === 1), title: `${x}× (${x})`, onclick: () => this.setSpeed(x) }, `${x}×`)));
    this.bar = h("div", { class: "rw-bar" }, this.btn, this.time, this.track, this.speeds);
    this.keys = h("div", { class: "rw-keys" }, h("kbd", { text: "r" }), " play from the start ", h("kbd", { text: "Space" }), " pause ", h("kbd", { text: "←" }), h("kbd", { text: "→" }), " 2 s ", h("kbd", { text: "e" }), " to the edit ", h("kbd", { text: "f" }), " theatre");
    host.replaceChildren(this.stage, this.bar, this.keys);
    this.bindTrack();
    this.ro = new ResizeObserver(() => this.scale());
    this.ro.observe(this.stage);
    this.load();
  }

  async load() {
    let rec;
    try {
      rec = await api(`/api/agents/${enc(this.o.agent)}/recording?from_ms=${Math.max(0, Math.floor(this.from))}&to_ms=${Math.ceil(this.to)}`);
    } catch (e) {
      if (this.disposed) return;
      this.fail(e.status === 404 ? `No recording of ${this.o.name} to play: ${e.message}` : `The recording could not be read: ${e.message}`);
      return;
    }
    if (this.disposed) return;
    if (!window.Terminal) { this.fail("The terminal library did not load (cdn.jsdelivr.net), so the recording cannot be played."); return; }
    this.rec = rec;
    this.evs = (rec.events || []).filter((e) => Array.isArray(e) && e.length >= 3 && e[0] >= this.from - 1).sort((a, b) => a[0] - b[0]);
    const last = this.evs.length ? this.evs[this.evs.length - 1][0] : this.from;
    const end = rec.duration_ms != null ? Math.max(rec.duration_ms, last) : null;
    if (end != null && end < this.to) this.to = Math.max(this.from + 1000, end);
    this.edit = Math.max(this.from, Math.min(this.to, this.edit));
    this.term = new window.Terminal({
      cols: rec.width || DEFAULT_SIZE[0], rows: rec.height || DEFAULT_SIZE[1], fontSize: 12, lineHeight: 1.0, fontFamily: MONO,
      disableStdin: true, cursorBlink: false, cursorStyle: "block", scrollback: 0, theme: termTheme(), allowTransparency: false,
    });
    this.term.open(this.inner);
    if (this.term.textarea) { this.term.textarea.tabIndex = -1; this.term.textarea.setAttribute("aria-hidden", "true"); }
    this.reset();
    this.stage.classList.remove("loading");
    this.poster.disabled = false;
    this.btn.disabled = false;
    if (!this.evs.length) this.notice("The recording has no output in this window.");
    else if (rec.truncated) this.notice("The recording stops early here: it reached its size limit.");
    const pct = ((this.edit - this.from) / Math.max(1, this.to - this.from)) * 100;
    this.mark.style.left = `${pct}%`;
    this.mark.title = `The edit, at ${mmss(this.edit - this.from)}`;
    this.track.setAttribute("aria-valuemax", String(Math.round((this.to - this.from) / 1000)));
    this.paint();
    requestAnimationFrame(() => this.scale());
    if (this.wantPlay) this.play();
  }

  fail(text) {
    this.stage.classList.remove("loading");
    this.stage.classList.add("failed");
    this.poster.hidden = true;
    this.notice(text);
    this.bar.hidden = true;
    this.keys.hidden = true;
  }

  notice(text) { this.note.hidden = !text; this.note.textContent = text || ""; }

  reset() {
    this.term.reset();
    this.term.write("\x1b[?25l");
    if (this.rec.before) this.term.write(this.rec.before);
    this.idx = 0;
    this.t = this.from;
  }

  applyTo(t) {
    let buf = "";
    while (this.idx < this.evs.length && this.evs[this.idx][0] <= t) {
      const [, kind, data] = this.evs[this.idx++];
      if (kind === "o") buf += data;
      else if (kind === "r") {
        const m = /^(\d+)x(\d+)$/.exec(String(data));
        if (!m) continue;
        if (buf) { this.term.write(buf); buf = ""; }
        const [c, r] = [Number(m[1]), Number(m[2])];
        this.term.write("", () => { try { this.term.resize(c, r); } catch { /* disposed */ } this.scale(); });
      }
    }
    if (buf) this.term.write(buf);
  }

  seek(t) {
    if (!this.term) return;
    t = Math.max(this.from, Math.min(this.to, t));
    if (t < this.t) this.reset();
    this.applyTo(t);
    const prev = this.t;
    this.t = t;
    this.started = true;
    this.poster.hidden = true;
    this.markState(prev, false);
    this.paint();
  }

  markState(prev, flash) {
    if (!this.started) return;
    const written = this.t >= this.edit;
    if (written !== this.written) {
      this.written = written;
      if (written && flash && prev < this.edit) { this.o.onWrite?.("flash"); this.mark.classList.remove("hit"); void this.mark.offsetWidth; this.mark.classList.add("hit"); }
      else this.o.onWrite?.(written ? "written" : "pending");
    }
  }

  play() {
    if (!this.term) { this.wantPlay = true; return; }
    if (this.t >= this.to) this.seek(this.from);
    this.started = true;
    this.poster.hidden = true;
    this.playing = true;
    if (this.written === null) { this.written = this.t >= this.edit; this.o.onWrite?.(this.written ? "written" : "pending"); }
    this.last = performance.now();
    cancelAnimationFrame(this.raf);
    this.raf = requestAnimationFrame(this.tick);
    this.paint();
  }

  tick = (now) => {
    if (!this.playing || this.disposed) return;
    const dt = Math.min(250, now - this.last);
    this.last = now;
    const next = Math.min(this.to, this.t + dt * this.speed);
    this.applyTo(next);
    const prev = this.t;
    this.t = next;
    this.markState(prev, true);
    this.paint();
    if (next >= this.to) { this.pause(); return; }
    this.raf = requestAnimationFrame(this.tick);
  };

  pause() {
    this.playing = false;
    cancelAnimationFrame(this.raf);
    this.paint();
  }

  toggle() { if (this.playing) this.pause(); else this.play(); }

  restart() { if (!this.term) { this.wantPlay = true; return; } this.seek(this.from); this.play(); }

  setSpeed(x) {
    this.speed = x;
    for (const b of this.speeds.children) b.setAttribute("aria-pressed", String(Number(b.dataset.x) === x));
  }

  paint() {
    const span = Math.max(1, this.to - this.from);
    const rel = Math.max(0, (this.t ?? this.from) - this.from);
    const pct = `${(rel / span) * 100}%`;
    this.fill.style.width = pct;
    this.thumb.style.left = pct;
    this.time.textContent = `${mmss(rel)} / ${mmss(span)}`;
    this.track.setAttribute("aria-valuenow", String(Math.round(rel / 1000)));
    this.track.setAttribute("aria-valuetext", `${mmss(rel)} of ${mmss(span)}; the edit is at ${mmss(this.edit - this.from)}`);
    const ended = this.t >= this.to;
    this.btn.replaceChildren(icon(this.playing ? "pause" : ended ? "rewind" : "play"));
    this.btn.title = this.playing ? "Pause (Space)" : ended ? "Play again (r)" : "Play (Space)";
    this.btn.setAttribute("aria-label", this.playing ? "Pause" : "Play");
    this.stage.classList.toggle("playing", this.playing);
  }

  bindTrack() {
    let wasPlaying = false;
    const at = (e) => {
      const r = this.track.getBoundingClientRect();
      return this.from + Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)) * (this.to - this.from);
    };
    this.track.addEventListener("pointerdown", (e) => {
      if (!this.term) return;
      e.preventDefault();
      this.track.focus({ preventScroll: true });
      this.track.setPointerCapture(e.pointerId);
      wasPlaying = this.playing;
      this.pause();
      this.seek(at(e));
      this.track.classList.add("drag");
    });
    this.track.addEventListener("pointermove", (e) => { if (this.track.hasPointerCapture(e.pointerId)) this.seek(at(e)); });
    const up = (e) => {
      if (!this.track.hasPointerCapture(e.pointerId)) return;
      this.track.releasePointerCapture(e.pointerId);
      this.track.classList.remove("drag");
      if (wasPlaying) this.play();
    };
    this.track.addEventListener("pointerup", up);
    this.track.addEventListener("pointercancel", up);
  }

  key(e) {
    const k = e.key;
    if (k === " " || k === "k") { this.toggle(); return true; }
    if (k === "r") { this.restart(); return true; }
    if (k === "ArrowLeft" || k === "ArrowRight") { this.seek((this.t ?? this.from) + (k === "ArrowLeft" ? -2000 : 2000)); return true; }
    if (k === "Home") { this.seek(this.from); return true; }
    if (k === "End") { this.seek(this.to); return true; }
    if (k === "e") { this.seek(this.edit - 1500); this.play(); return true; }
    if (k === "1" || k === "2" || k === "4") { this.setSpeed(Number(k)); return true; }
    return false;
  }

  scale() {
    const scr = this.inner.querySelector(".xterm-screen");
    if (!scr) return;
    const w = scr.offsetWidth, ht = scr.offsetHeight;
    const room = this.scr.clientWidth;
    if (!w || !ht || room <= 0) return;
    const s = Math.min(room / w, 1.15);
    this.inner.style.transform = `scale(${s})`;
    this.scr.style.height = `${Math.ceil(ht * s)}px`;
  }

  retheme() { if (this.term) this.term.options.theme = termTheme(); }

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    cancelAnimationFrame(this.raf);
    this.ro.disconnect();
    try { this.term?.dispose(); } catch { /* gone */ }
    if (this.started) this.o.onWrite?.("written");
  }
}

// ----------------------------------------------------- the decision card

async function openDecisionCard(d, { push = false, back = false } = {}) {
  const key = `dec\u0001${d.id}`;
  if (T.open && T.key === key && !back) return;
  const req = beginCard("decision", key, (b) => openDecisionCard(d, { back: b }), { push, back });
  for (const el of document.querySelectorAll(".dc")) el.classList.toggle("sel", el.dataset.id === d.id);
  sheetHead("Decision", d.id, "the record's id");
  const a = liveAgent(d.agent);
  const ms = toMs(d.ts);
  const repo = D.repo;
  const all = D.list;
  const byId = new Map(all.map((x) => [x.id, x]));
  const by = d.superseded_by ? byId.get(d.superseded_by) : null;
  const replaces = all.filter((x) => x.superseded_by === d.id);
  const superseded = d.superseded || !!d.superseded_by;
  const body = [];
  body.push(h("blockquote", { class: `dq${superseded ? " struck" : ""}` }, d.text || ""));
  body.push(h("h2", { class: "headline sm" }, a ? [h("b", { class: "hn", text: nameOf(a) }), ` decided this ${ms ? whenPhrase(ms) : ""}`.trimEnd(), "."]
    : [`Decided ${ms ? whenPhrase(ms) : ""} in a Claude Code session outside Workbench`, d.session ? [" (", sessShort(d.session), ")"] : null, "."]));
  if (superseded) {
    body.push(h("div", { class: "callout warn" }, icon("warn", "i"), h("div", null,
      h("b", null, "Superseded", by ? [" ", by.agent ? `by ${nameOf(liveAgent(by.agent))}` : "by another session", by.ts ? ` ${whenPhrase(toMs(by.ts))}` : ""] : d.superseded_by ? [" by ", h("code", { text: d.superseded_by })] : null, by ? ":" : "."),
      by ? h("button", { class: "quote-link", type: "button", onclick: () => openDecisionCard(by, { push: true }) }, `“${by.text}”`) : null)));
  }
  if (replaces.length) {
    body.push(section(replaces.length === 1 ? "Replaces" : `Replaces ${replaces.length}`, h("ol", { class: "replaces" }, replaces.map((x) =>
      h("li", null, h("button", { class: "quote-link struck", type: "button", onclick: () => openDecisionCard(x, { push: true }) }, `“${x.text}”`),
        h("span", { class: "muted", text: ` ${x.agent ? nameOf(liveAgent(x.agent)) : "another session"}${x.ts ? `, ${whenPhrase(toMs(x.ts))}` : ""}` }))))));
  }
  if (a) body.push(section("Who", chain(a)));
  body.push(h("div", { class: "two-col" },
    section("When", facts([["decided", ms ? h("span", null, fmtFull(ms), h("span", { class: "muted", text: ` · ${agoText(ms)}` })) : null], ["session", sessShort(d.session)]])),
    section("Where", facts([["repo", shortPath(repo), repo], ["files", (d.files || []).length ? h("span", { class: "files" }, d.files.map((f) => h("code", { text: f }))) : h("span", { class: "muted", text: "none named" })]]))));
  const linesHost = h("div", { class: "turn-lines" });
  const rwHost = h("div");
  const knewHost = h("div");
  body.push(h("section", { class: "sec" }, h("div", { class: "sec-hrow" }, h("h3", { class: "sec-h", text: "Lines from its turn" }), h("span", { class: "sec-sub", text: a ? `in ${nameOf(a)}'s changes` : "" })), linesHost));
  body.push(rwHost, knewHost);
  $("sh-body").replaceChildren(...body.filter(Boolean));
  if (!a) { linesHost.append(h("p", { class: "empty-s", text: "The session is not one of Workbench's agents, so there is no worktree here to find its lines in." })); return; }
  linesHost.append(h("div", { class: "tracing" }, h("span", { class: "spin", "aria-hidden": "true" }), "Finding the lines its turn wrote…"));
  let found;
  try { found = await turnLines(a, d); } catch (e) {
    if (req !== T.req) return;
    linesHost.replaceChildren(h("p", { class: "empty-s", text: `Its changes could not be read: ${e.message}` }));
    return;
  }
  if (req !== T.req) return;
  if (!found.blocks.length) {
    linesHost.replaceChildren(h("p", { class: "empty-s", text: found.scanned ? `No added line in the files it names came from this turn${found.scanned ? ` (${plural(found.scanned, "block")} checked)` : ""}.` : (d.files || []).length ? `${nameOf(a)}'s changes hold no added line in the files it names.` : "It names no file, so no line can be traced to it." }));
    return;
  }
  const byFile = new Map();
  for (const b of found.blocks) { if (!byFile.has(b.path)) byFile.set(b.path, []); byFile.get(b.path).push(b.lines); }
  linesHost.replaceChildren(...[...byFile].map(([path, runs]) => {
    const rows = [];
    let shown = 0, hidden = 0;
    runs.forEach((lines, ri) => {
      if (ri) rows.push(h("span", { class: "tl-gap", "aria-hidden": "true", text: "⋯" }));
      for (const l of lines) {
        if (shown >= 14) { hidden++; continue; }
        shown++;
        rows.push(h("button", { class: "tl-line", type: "button", title: `${path}:${l.n}: its decision card`, onclick: () => openLineCard(a.id, path, l.n, { text: l.text, push: true }) },
          h("span", { class: "ln", text: String(l.n) }), h("code", { text: l.text || " " })));
      }
    });
    return h("div", { class: "tl-block" },
      h("div", { class: "tl-file" }, icon("file"), h("code", { text: path }), h("span", { class: "muted", text: plural(runs.reduce((n, r) => n + r.length, 0), "line") })),
      h("div", { class: "tl-lines" }, rows),
      hidden ? h("span", { class: "muted more", text: `+${hidden} more lines` }) : null);
  }));
  const card = found.card;
  if (card?.rewind) {
    rwHost.append(h("section", { class: "sec rw-sec" }, h("div", { class: "sec-hrow" }, h("h3", { class: "sec-h", text: "Rewind" }), h("span", { class: "sec-sub", text: `the edit of ${card.line?.path}:${card.line?.line}` })), h("div", { class: "rw" })));
    const first = found.blocks[0];
    const t = { agent: a.id, path: first.path, line: first.lines[0].n };
    T.line = t;
    markLine(t);
    T.player = new Rewind(rwHost.querySelector(".rw"), {
      agent: card.rewind.agent || a.id, from: card.rewind.from_ms, to: card.rewind.to_ms, edit: card.rewind.edit_ms, name: nameOf(a),
      onWrite: (state) => { if (state === "flash") flashLine(t); else markLine(t, state); },
    });
  }
  if (card?.knew?.length) knewHost.append(section(`What it knew · ${card.knew.length}`, h("ol", { class: "recs" }, card.knew.map((k) => recordEl(k, card)))));
}

// The added lines of the agent's diff that the decision's turn wrote: the
// runs of added lines in the files it names, kept when the line card of a
// run's first and last line lists the decision among its turn's notes.
async function turnLines(a, d) {
  const diff = await getDiff(a.id);
  const want = new Set(d.files || []);
  const runs = [];
  for (const f of diff.files || []) {
    if (want.size && !want.has(f.path)) continue;
    for (const hk of f.hunks || []) {
      let n = hk.new_start || 1, cur = null;
      for (const l of hk.lines || []) {
        if (l.kind === "+") {
          const ln = l.new_line ?? n;
          if (!cur) { cur = { path: f.path, lines: [] }; runs.push(cur); }
          cur.lines.push({ n: ln, text: l.text });
          n = ln + 1;
        } else {
          cur = null;
          if (l.kind !== "-") n = (l.new_line ?? n) + 1;
        }
      }
    }
  }
  if (!want.size) return { blocks: [], scanned: 0 };
  const pick = runs.slice(0, 12);
  const matches = (card) => (card?.why || []).some((x) => (x.id && x.id === d.id) || nrm(x.text) === nrm(d.text));
  const jobs = [];
  for (const r of pick) {
    jobs.push(() => getWhy(a.id, r.path, r.lines[0].n).catch(() => null));
    if (r.lines.length > 1) jobs.push(() => getWhy(a.id, r.path, r.lines[r.lines.length - 1].n).catch(() => null));
  }
  const res = new Array(jobs.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(4, jobs.length) }, async () => { while (next < jobs.length) { const i = next++; res[i] = await jobs[i](); } }));
  const blocks = [];
  let k = 0, card = null;
  for (const r of pick) {
    const c1 = res[k++];
    const c2 = r.lines.length > 1 ? res[k++] : c1;
    if (matches(c1) || matches(c2)) {
      const lines = matches(c1) && matches(c2) ? r.lines : matches(c1) ? [r.lines[0]] : [r.lines[r.lines.length - 1]];
      blocks.push({ path: r.path, lines });
      card ||= matches(c1) ? c1 : c2;
    }
  }
  return { blocks, card, scanned: pick.length };
}

// ------------------------------------------------------ the Decisions screen

// The two layers of lines, kept here: the timeline's children are replaced.
const TL_LINKS = $("tl-links");
const TL_HOT = $("tl-hot");
const D = { open: false, repo: null, list: [], missing: [], req: 0, prevZoom: null, lanes: [], cur: -1, cards: [], focus: null, loading: false, err: null };

// Sixth build (ui/levels/repos.js, src/repos.rs): a project opened with the
// picker (the window event `wb-repo`) becomes the project in focus.
window.addEventListener("wb-repo", (e) => {
  if (e.detail) store("local", PROJECT_KEY, e.detail);
  refreshProjects().then(() => { if (D.open && e.detail && e.detail !== D.repo) { D.repo = e.detail; fillRepos(); loadDecisions(); } });
});

function knownRepos() {
  const seen = new Map();
  for (const a of S.agents.values()) if (a.repo) seen.set(a.repo, Math.max(seen.get(a.repo) || 0, toMs(a.state_since) || 0, toMs(a.created) || 0));
  for (const r of P.list) if (r.path && r.exists !== false) seen.set(r.path, Math.max(seen.get(r.path) || 0, r.last_activity_ms || r.last_ms || 0));
  for (const r of recentRepos()) if (!seen.has(r)) seen.set(r, 0);
  if (D.repo && !seen.has(D.repo)) seen.set(D.repo, 0);
  return [...seen.entries()].sort((x, y) => y[1] - x[1]).map(([r]) => r);
}

/** The repository the old screens read: the agent page's, else the project in focus. */
function pickRepo() {
  const z = S.agents.get(S.zoomed);
  if (z?.repo) return z.repo;
  const p = currentProject();
  if (p) return p.path;
  const saved = store("local", DEC_REPO_KEY);
  const list = knownRepos();
  if (saved && list.includes(saved)) return saved;
  return list[0] || saved || null;
}

/** The Decisions timeline (More › Decisions timeline, #/more/decisions). */
function openDecisions({ repo, focus, open = false } = {}) {
  if (focus) { D.focus = focus; D.focusOpen = open; }
  if (!D.open) { if (repo) D.want = repo; go("#/more/decisions"); return; }
  const wanted = repo || D.repo;
  if (wanted && wanted !== D.repo) { D.repo = wanted; fillRepos(); loadDecisions(); } else if (focus) { highlightDecision(focus, { open }); D.focus = null; }
}

/** The router shows it. */
function showDecisions() {
  if (T.open && T.kind === "line") closeSheet({ focus: false });
  $("dec-view").hidden = false;
  if (D.open) return;
  D.open = true;
  D.repo = D.want || pickRepo();
  D.want = null;
  fillRepos();
  loadDecisions();
}

function closeDecisions() {
  if (!D.open) return;
  D.open = false;
  $("dec-view").hidden = true;
  if (T.open && T.kind === "decision") closeSheet({ focus: false });
}

function fillRepos() {
  const sel = $("dec-repo");
  const list = knownRepos();
  const names = projectNames();
  sel.replaceChildren(...list.map((r) => h("option", { value: r, text: names[r] || shortPath(r) })));
  if (!list.length) sel.append(h("option", { value: "", text: "no project yet" }));
  sel.value = D.repo || "";
  sel.title = D.repo || "";
}

$("dec-repo").addEventListener("change", (e) => {
  D.repo = e.target.value;
  store("local", DEC_REPO_KEY, D.repo);
  e.target.title = D.repo;
  $("ask-out").hidden = true;
  loadDecisions();
});

async function loadDecisions(fresh = false) {
  const req = ++D.req;
  const repo = D.repo;
  D.err = null;
  if (!repo) { D.list = []; D.missing = []; renderTimeline(); return; }
  D.loading = true;
  renderTimeline();
  try {
    const r = await getDecisions(repo, fresh);
    if (req !== D.req) return;
    D.list = (r.decisions || []).filter((d) => d && d.id);
    D.missing = r.missing || [];
  } catch (e) {
    if (req !== D.req) return;
    D.list = [];
    D.err = e;
  }
  D.loading = false;
  renderTimeline();
}

const LANES = 5;

function laneKey(d) { return d.agent?.id || "other"; }

function renderTimeline() {
  const tl = $("tl");
  const svg = TL_LINKS;
  svg.replaceChildren();
  TL_HOT.replaceChildren();
  D.hover = null;
  D.cards = [];
  D.cur = -1;
  $("dec-n").textContent = "";
  $("dec-legend").replaceChildren();
  if (!D.repo) {
    tl.replaceChildren(emptyState("No repository yet", "Start an agent on a repository, and its decisions show here as ChloeCtx distils them."));
    return;
  }
  if (D.loading && !D.list.length) { tl.className = "tl"; tl.replaceChildren(h("div", { class: "tl-loading" }, skel(5))); return; }
  if (D.err) {
    tl.replaceChildren(emptyState("The decisions could not be read", D.err.message, true));
    return;
  }
  if (!D.list.length) {
    tl.replaceChildren(emptyState(D.missing.length ? "ChloeCtx could not answer" : "No decisions recorded yet",
      D.missing.length ? D.missing.join(" ") : `ChloeCtx has distilled no decision from the sessions in ${shortPath(D.repo)}. Decisions appear after an agent's turn ends.`, !!D.missing.length));
    return;
  }
  const list = [...D.list].sort((x, y) => (toMs(y.ts) || 0) - (toMs(x.ts) || 0));
  const sup = list.filter((d) => d.superseded || d.superseded_by).length;
  $("dec-n").textContent = `${plural(list.length, "decision")}${sup ? ` · ${sup} superseded` : ""}`;

  // Lanes: agents with decisions, parents before their children, then
  // the sessions Workbench did not start.
  const agentsIn = new Map();
  for (const d of list) if (d.agent?.id) agentsIn.set(d.agent.id, liveAgent(d.agent));
  const ordered = [];
  const visit = (a) => {
    if (ordered.includes(a)) return;
    ordered.push(a);
    [...agentsIn.values()].filter((c) => c.parent === a.id).sort(byCreated).forEach(visit);
  };
  [...agentsIn.values()].filter((a) => !agentsIn.has(a.parent)).sort(byCreated).forEach(visit);
  const hasOther = list.some((d) => !d.agent?.id);
  D.lanes = ordered.map((a, i) => ({ key: a.id, agent: a, color: i < LANES ? `var(--lane-${i + 1})` : "var(--lane-x)" }));
  if (hasOther) D.lanes.push({ key: "other", agent: null, color: "var(--lane-o)" });
  const laneIdx = new Map(D.lanes.map((l, i) => [l.key, i]));

  // Rows: decisions, and the moment each agent lane began.
  const items = list.map((d) => ({ type: "dec", d, ts: toMs(d.ts) || 0, lane: laneIdx.get(laneKey(d)) }));
  for (const l of D.lanes) if (l.agent) items.push({ type: "spawn", a: l.agent, ts: toMs(l.agent.created) || 0, lane: laneIdx.get(l.key), from: laneIdx.has(l.agent.parent) ? laneIdx.get(l.agent.parent) : null });
  items.sort((x, y) => y.ts - x.ts || (x.type === "spawn") - (y.type === "spawn"));

  tl.className = "tl";
  tl.style.setProperty("--lanes", String(D.lanes.length));
  const kids = [h("div", { class: "tl-h tl-gut", style: "grid-row:1;grid-column:1" })];
  D.lanes.forEach((l, i) => {
    const n = list.filter((d) => laneKey(d) === l.key).length;
    const parent = l.agent?.parent ? S.agents.get(l.agent.parent) : null;
    kids.push(h("div", { class: "tl-h lane-h", style: `grid-row:1;grid-column:${i + 2};--lane:${l.color}` },
      l.agent ? agentChip(l.agent) : h("span", { class: "lane-other" }, "Other sessions"),
      h("span", { class: "lane-meta", text: l.agent ? `${parent ? `by ${nameOf(parent)} · ` : ""}${n}` : `${new Set(list.filter((d) => !d.agent?.id).map((d) => d.session)).size} · ${n}` })));
  });
  let row = 2;
  let lastDay = "", lastClock = "";
  for (const it of items) {
    const day = new Date(it.ts).toDateString();
    if (day !== lastDay) {
      lastDay = day;
      lastClock = "";
      kids.push(h("div", { class: "tl-day", style: `grid-row:${row++};grid-column:1/-1` }, h("span", { text: dayLabel(it.ts) })));
    }
    const clock = hm(it.ts);
    kids.push(h("div", { class: `tl-time${clock === lastClock ? " same" : ""}`, style: `grid-row:${row};grid-column:1`, title: fmtFull(it.ts), text: clock }));
    lastClock = clock;
    const l = D.lanes[it.lane];
    if (it.type === "spawn") {
      const by = it.from != null ? D.lanes[it.from].agent : null;
      const p = it.a.parent ? S.agents.get(it.a.parent) : null;
      kids.push(h("div", { class: "sp", style: `grid-row:${row};grid-column:${it.lane + 2};--lane:${l.color}`, "data-lane": String(it.lane), "data-from": it.from ?? "" },
        h("span", { class: "node" }), h("span", { class: "sp-t" }, by ? `spawned by ${nameOf(by)}` : p ? `spawned by ${nameOf(p)}` : "started by you")));
    } else {
      kids.push(decCard(it.d, l, it.lane, row));
    }
    row++;
  }
  tl.replaceChildren(svg, ...kids, TL_HOT);
  // The legend, for narrow screens where lanes fold into one column.
  $("dec-legend").replaceChildren(...D.lanes.map((l) => h("span", { class: "lg", style: `--lane:${l.color}` }, h("i"), l.agent ? nameOf(l.agent) : "other sessions")));
  requestAnimationFrame(() => {
    drawLinks();
    if (D.focus) { const f = D.focus; D.focus = null; highlightDecision(f, { open: D.focusOpen }); }
    else if (T.open && T.kind === "decision") for (const el of tl.querySelectorAll(".dc")) el.classList.toggle("sel", T.key === `dec\u0001${el.dataset.id}`);
  });
}

function dayLabel(ms) {
  const d = new Date(ms);
  if (d.toDateString() === new Date().toDateString()) return "Today";
  if (d.toDateString() === new Date(Date.now() - 864e5).toDateString()) return "Yesterday";
  return d.toLocaleDateString([], { weekday: "long", month: "short", day: "numeric" });
}

function emptyState(title, text, bad = false) {
  return h("div", { class: `tl-empty${bad ? " bad" : ""}` }, icon(bad ? "warn" : "trail", "i art"), h("h3", { text: title }), h("p", { text }));
}

function decCard(d, lane, li, row) {
  const superseded = d.superseded || !!d.superseded_by;
  const by = d.superseded_by ? D.list.find((x) => x.id === d.superseded_by) : null;
  const replaces = D.list.filter((x) => x.superseded_by === d.id);
  const files = d.files || [];
  const open = h("button", { class: "dc-open", type: "button", title: "Open its card (Enter)" },
    h("span", { class: "node", "aria-hidden": "true" }),
    h("span", { class: "dc-text", text: d.text || "" }));
  const meta = h("span", { class: "dc-meta" },
    h("span", { class: "dc-who", text: d.agent ? nameOf(liveAgent(d.agent)) : `session ${String(d.session || "").slice(0, 8)}` }),
    h("span", { class: "dc-when", text: hm(toMs(d.ts) || 0) }),
    files.length ? h("code", { class: "dc-file", title: files.join("\n"), text: files[0].split("/").pop() + (files.length > 1 ? ` +${files.length - 1}` : "") }) : null,
    replaces.length ? h("span", { class: "dc-rep", title: replaces.map((x) => x.text).join("\n") }, `replaces ${replaces.length}`) : null);
  const card = h("article", { class: `dc${superseded ? " superseded" : ""}`, style: `grid-row:${row};grid-column:${li + 2};--lane:${lane.color}`, "data-id": d.id, "data-lane": String(li) }, open, meta);
  if (superseded) {
    card.append(h("button", { class: "dc-sup", type: "button", title: by ? `Superseded by: ${by.text}` : `Superseded${d.superseded_by ? ` by ${d.superseded_by}` : ""}`, onclick: (e) => { e.stopPropagation(); if (by) highlightDecision(by.id, { open: true }); } },
      icon("warn"), by ? `superseded by ${by.agent ? nameOf(liveAgent(by.agent)) : "another session"} ↑` : "superseded"));
  }
  open.addEventListener("click", () => { setDecCursor(D.cards.indexOf(card), { focus: false }); openDecisionCard(d); });
  card.addEventListener("mouseenter", () => relate(d, true));
  card.addEventListener("mouseleave", () => relate(d, false));
  open.addEventListener("focus", () => { const i = D.cards.indexOf(card); if (i >= 0) { D.cards[D.cur]?.classList.remove("cur"); D.cur = i; card.classList.add("cur"); } relate(d, true); });
  open.addEventListener("blur", () => relate(d, false));
  D.cards.push(card);
  return card;
}

function relate(d, on) {
  D.hover = on ? d : null;
  const ids = new Set([d.id, d.superseded_by, ...D.list.filter((x) => x.superseded_by === d.id).map((x) => x.id)].filter(Boolean));
  const linked = on && ids.size > 1;
  for (const el of $("tl").querySelectorAll(".dc")) {
    el.classList.toggle("rel", on && ids.has(el.dataset.id) && el.dataset.id !== d.id);
    el.classList.toggle("src", linked && el.dataset.id === d.id);
  }
  const hot = TL_HOT;
  // Fade the cards an arrow passes over, so it reads as an overlay.
  $("tl").classList.toggle("relating", linked && getComputedStyle(hot).display !== "none");
  if (!on || getComputedStyle(hot).display === "none") { hot.replaceChildren(); return; }
  const defs = document.createElementNS("http://www.w3.org/2000/svg", "defs");
  defs.innerHTML = '<marker id="tl-arrow" viewBox="0 0 10 10" refX="7" refY="5" markerWidth="8" markerHeight="8" orient="auto-start-reverse"><path d="M1 1L9 5L1 9" fill="none" stroke="context-stroke" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></marker>';
  hot.replaceChildren(defs, ...supersedeArrows(d));
}

function highlightDecision(id, { open = false } = {}) {
  const i = D.cards.findIndex((c) => c.dataset.id === id);
  if (i < 0) { toast(`${id} is not among this repository's newest decisions.`); return; }
  setDecCursor(i, { center: true });
  const c = D.cards[i];
  c.classList.remove("ping");
  void c.offsetWidth;
  c.classList.add("ping");
  const d = D.list.find((x) => x.id === id);
  if (open && d) openDecisionCard(d, { push: T.open && T.kind === "decision" });
}

function setDecCursor(i, { focus = true, center = false } = {}) {
  if (!D.cards.length) return;
  i = Math.max(0, Math.min(D.cards.length - 1, i));
  D.cards[D.cur]?.classList.remove("cur");
  D.cur = i;
  const c = D.cards[i];
  c.classList.add("cur");
  if (focus) c.querySelector(".dc-open").focus({ preventScroll: true });
  c.scrollIntoView({ block: center ? "center" : "nearest", inline: "nearest", behavior: center ? "smooth" : "auto" });
}

function decisionsKey(e) {
  const k = e.key;
  if (k === "/") { $("ask-q").focus(); return true; }
  if (!D.cards.length) return false;
  if (k === "j" || k === "ArrowDown") return setDecCursor(D.cur + 1), true;
  if (k === "k" || k === "ArrowUp") return setDecCursor(D.cur < 0 ? 0 : D.cur - 1), true;
  if (k === "Enter" && D.cur >= 0 && !e.target.closest?.("button, a")) { D.cards[D.cur].querySelector(".dc-open").click(); return true; }
  return false;
}

// The lanes' lifelines, where each agent branched off its parent, and an
// arrow from each superseded decision up to what replaced it.
function drawLinks() {
  const tl = $("tl");
  const svg = TL_LINKS;
  if (!D.open || !tl.querySelector(".lane-h") || getComputedStyle(svg).display === "none") { svg.replaceChildren(); return; }
  const base = tl.getBoundingClientRect();
  const W = tl.scrollWidth, H = tl.scrollHeight;
  svg.setAttribute("width", String(W));
  svg.setAttribute("height", String(H));
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  const pos = (el) => { const r = el.getBoundingClientRect(); return [r.left - base.left + r.width / 2, r.top - base.top + r.height / 2]; };
  const NS = "http://www.w3.org/2000/svg";
  const path = (d, cls, extra = {}) => { const p = document.createElementNS(NS, "path"); p.setAttribute("d", d); p.setAttribute("class", cls); for (const [k, v] of Object.entries(extra)) p.setAttribute(k, v); return p; };
  const out = [];
  const headBottom = tl.querySelector(".lane-h").getBoundingClientRect().bottom - base.top;
  D.lanes.forEach((l, i) => {
    const nodes = [...tl.querySelectorAll(`[data-lane="${i}"] .node`)].map(pos);
    if (!nodes.length) return;
    const x = nodes[0][0];
    const ys = nodes.map((p) => p[1]);
    const running = l.agent && S.agents.get(l.agent.id)?.running;
    const top = running ? headBottom : Math.min(...ys);
    const bottom = Math.max(...ys);
    out.push(path(`M${x} ${top}V${bottom}`, `life${l.agent ? "" : " other"}`, { style: `--lane:${l.color}` }));
  });
  // Branches: from the parent's lifeline into the child's first node.
  for (const sp of tl.querySelectorAll(".sp")) {
    if (sp.dataset.from === "") continue;
    const from = Number(sp.dataset.from);
    const anyParent = tl.querySelector(`[data-lane="${from}"] .node`);
    if (!anyParent) continue;
    const [xc, y] = pos(sp.querySelector(".node"));
    const xp = pos(anyParent)[0];
    const lane = D.lanes[Number(sp.dataset.lane)];
    out.push(path(`M${xp} ${y + 22}C${xp} ${y + 4},${xc} ${y + 18},${xc} ${y}`, "branch", { style: `--lane:${lane.color}` }));
  }
  svg.replaceChildren(...out);
  const hot = TL_HOT;
  hot.setAttribute("width", String(W));
  hot.setAttribute("height", String(H));
  hot.setAttribute("viewBox", `0 0 ${W} ${H}`);
  hot.replaceChildren();
  if (D.hover) relate(D.hover, true);
}

// The arrow from a superseded decision up to what replaced it, drawn over
// the cards while either is hovered or has the cursor.
function supersedeArrows(d) {
  const tl = $("tl");
  const base = tl.getBoundingClientRect();
  const NS = "http://www.w3.org/2000/svg";
  const out = [];
  const pairs = D.list.filter((x) => x.superseded_by && (x.id === d.id || x.superseded_by === d.id)).map((x) => [x.id, x.superseded_by]);
  for (const [oldId, newId] of pairs) {
    const a = tl.querySelector(`.dc[data-id="${CSS.escape(oldId)}"]`);
    const b = tl.querySelector(`.dc[data-id="${CSS.escape(newId)}"]`);
    if (!a || !b) continue;
    const A = a.getBoundingClientRect(), B = b.getBoundingClientRect();
    const same = Math.abs(A.left - B.left) < 4;
    const x1 = (same ? A.left + 46 : A.left + A.width / 2) - base.left, y1 = A.top - base.top;
    const x2 = (same ? B.left + 46 : B.left + B.width / 2) - base.left, y2 = B.bottom - base.top + 3;
    const mid = (y1 + y2) / 2;
    const p = document.createElementNS(NS, "path");
    p.setAttribute("d", same ? `M${x1} ${y1}V${y2}` : `M${x1} ${y1}C${x1} ${mid},${x2} ${mid},${x2} ${y2}`);
    p.setAttribute("class", "lk");
    p.setAttribute("marker-end", "url(#tl-arrow)");
    out.push(p);
  }
  return out;
}

new ResizeObserver(() => { if (D.open) { cancelAnimationFrame(D.raf); D.raf = requestAnimationFrame(drawLinks); } }).observe($("tl"));

// ---------------------------------------------------------------- Ask why

$("ask-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const q = $("ask-q").value.trim();
  if (!q || !D.repo) return;
  const out = $("ask-out");
  out.hidden = false;
  out.replaceChildren(h("div", { class: "tracing" }, h("span", { class: "spin", "aria-hidden": "true" }), `Asking ChloeCtx about ${shortPath(D.repo)}…`));
  $("ask-go").disabled = true;
  const req = ++D.askReq;
  try {
    const r = await api("/api/ask", { method: "POST", body: { repo: D.repo, question: q } });
    if (req !== D.askReq) return;
    renderAnswer(q, r);
  } catch (err) {
    if (req !== D.askReq) return;
    out.replaceChildren(h("div", { class: "callout bad" }, icon("warn", "i"), h("div", null, h("b", { text: "ChloeCtx could not be asked." }), h("p", { text: err.message }))));
  } finally {
    $("ask-go").disabled = false;
  }
});
D.askReq = 0;

// `chloectx ask --json` answers {text, ids, ms} (and, deep, {answer,
// confidence}); Workbench adds `agents` (record id -> Agent) and `missing`.
function renderAnswer(q, r) {
  const out = $("ask-out");
  const ids = Array.isArray(r.ids) ? r.ids : [];
  const agentOf = (id) => {
    const m = r.agents;
    if (m && !Array.isArray(m) && m[id]) return liveAgent(m[id]);
    return null;
  };
  const byId = new Map(D.list.map((d) => [d.id, d]));
  const head = h("div", { class: "ans-head" },
    h("span", { class: "ans-q" }, icon("ask"), h("span", { text: q })),
    h("span", { class: "sep" }),
    h("span", { class: "muted", text: [ids.length ? plural(ids.length, "record") : null, r.ms != null ? `${r.ms} ms` : null, r.confidence ? `${r.confidence} confidence` : null].filter(Boolean).join(" · ") }),
    h("button", { class: "btn ghost icon xs", type: "button", title: "Close the answer", "aria-label": "Close the answer", onclick: () => { out.hidden = true; } }, icon("x")));
  const parts = [head];
  if ((r.missing || []).length) parts.push(h("div", { class: "callout info" }, icon("info", "i"), h("div", null, h("b", { text: "ChloeCtx could not answer fully." }), h("p", { text: r.missing.join(" ") }))));
  const answer = typeof r.answer === "string" ? r.answer : r.answer && typeof r.answer === "object" ? (r.answer.text || JSON.stringify(r.answer)) : null;
  if (answer) parts.push(h("p", { class: "ans-text" }, answer));
  const known = ids.map((id) => byId.get(id)).filter(Boolean);
  if (known.length) {
    parts.push(h("ol", { class: "ans-recs" }, known.map((d) => {
      const a = liveAgent(d.agent) || agentOf(d.id);
      const lane = D.lanes.find((l) => l.key === (a?.id || "other"));
      return h("li", null, h("button", { class: `ans-rec${d.superseded || d.superseded_by ? " superseded" : ""}`, type: "button", style: `--lane:${lane?.color || "var(--lane-o)"}`, title: "Show it on the timeline and open its card", onclick: () => highlightDecision(d.id, { open: true }) },
        h("span", { class: "lane-dot" }),
        h("span", { class: "ans-t", text: d.text }),
        h("span", { class: "ans-m" }, h("b", { text: a ? nameOf(a) : "other session" }), ` · ${whenPhrase(toMs(d.ts) || 0)}`, d.superseded || d.superseded_by ? h("span", { class: "flag warn sm" }, "superseded") : null)));
    })));
  }
  // The rest of what it printed: notes that are not decisions, excerpts of
  // sessions, as lines.
  const text = String(r.text || "");
  const rest = text.split("\n").map((l) => l.trimEnd()).filter((l) => l.trim() && !known.some((d) => nrm(l).includes(nrm(d.text))));
  for (let i = rest.length - 1; i >= 0; i--) if (/:$/.test(rest[i]) && (i === rest.length - 1 || /:$/.test(rest[i + 1]))) rest.splice(i, 1);
  if (rest.length && !(rest.length === 1 && /found nothing relevant/i.test(rest[0]) && known.length)) {
    parts.push(h("div", { class: "ans-raw" }, rest.map((l) => {
      const ex = /^\[(.+?) · session (\S+)\] (\w+): (.*)$/.exec(l);
      if (ex) {
        const a = [...S.agents.values()].find((x) => x.session_id === ex[2]) || Object.values(r.agents || {}).find((x) => x?.session_id === ex[2]);
        return h("div", { class: "ans-ex" }, h("span", { class: "muted" }, a ? h("b", { text: nameOf(a) }) : sessShort(ex[2]), ` · ${ex[3]}`), h("q", { text: ex[4] }));
      }
      const note = /^- (\S+) (\w+): (.*)$/.exec(l);
      if (note) return h("div", { class: "ans-note" }, h("span", { class: "tag", text: note[2] }), h("span", null, withIds(note[3])));
      return h("div", { class: /:$/.test(l) ? "ans-sub" : "ans-line" }, withIds(l));
    })));
  }
  if (parts.length === 1) parts.push(h("p", { class: "empty-s", text: "ChloeCtx found nothing relevant in memory." }));
  out.replaceChildren(...parts);
}

// ------------------------------------------------------- live updates

function trailOnEvent(ev) {
  if (!ev || !ev.agent) return;
  if (ev.kind === "PostToolUse" && EDIT_TOOLS.has(ev.tool)) {
    forgetAgent(ev.agent);
    if (C.id === ev.agent) {
      C.stale = true;
      clearTimeout(C.timer);
      C.timer = setTimeout(() => { if (C.id === ev.agent) loadChanges(ev.agent, { fresh: true, quiet: true }); }, 1200);
    }
  }
  if (ev.kind === "Stop" && D.open && S.agents.get(ev.agent)?.repo === D.repo) {
    // A turn ended: ChloeCtx distils its decisions in the background.
    clearTimeout(D.timer);
    D.timer = setTimeout(() => { cache.dec.delete(D.repo); loadDecisions(true); }, 8000);
  }
}

// Keys for the trail, outside a terminal. Answers true when it used the key.
function trailKey(e) {
  if (e.ctrlKey || e.metaKey || e.altKey) return false;
  // A focused question card has its own keys (a digit, Enter, e): they
  // never reach the diff under it.
  if (e.target.closest?.(".qc")) return false;
  const k = e.key;
  const inSheet = e.target.closest?.("#sheet");
  if (T.open && T.player && (inSheet || k === "r" || ((k === " " || k === "ArrowLeft" || k === "ArrowRight") && !e.target.closest?.("#ch-scroll, #dec-view")))) {
    if (!(e.target.closest?.("button") && (k === " " || k === "Enter")) && T.player.key(e)) return true;
  }
  if (T.open && T.player && (k === "1" || k === "2" || k === "4")) { T.player.setSpeed(Number(k)); return true; }
  if (T.open && T.player && k === "e") { T.player.key(e); return true; }
  if (T.open && (k === "f" || k === "F")) { toggleWide(); return true; }
  if (T.open && k === "Backspace" && T.stack.length) { sheetBack(); return true; }
  if (S.zoomed && k === "c" && !e.target.closest?.("#ag-q")) { go(`#/a/${S.zoomed}/changes`); return true; }
  if (S.zoomed && k === "t" && !e.target.closest?.("#ag-q")) { go(`#/a/${S.zoomed}/terminal`); return true; }
  if (changesVisible() && !inSheet && changesKey(e)) return true;
  if (D.open && !inSheet && decisionsKey(e)) return true;
  return false;
}

// ------------------------------------------------------------------ keys

function setPrefix(on) {
  S.prefix = on;
  $("prefix-ind").hidden = !on;
}

function inField(t) {
  if (!t || t.classList?.contains("xterm-helper-textarea")) return false;
  return t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName);
}

const ARROWS = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };

function target() { return S.zoomed || S.selected || S.order[0] || null; }

/** n: Start a change in the project in focus, else a new project. */
function newHere() {
  const p = currentProject();
  go(p ? `#/p/${p.project}/change` : "#/new");
}

/** Out of the terminal, to the agent's Progress. */
function leaveTerminal() {
  if (!S.zoomed) return;
  if (Z.term?.textarea === document.activeElement) Z.term.blur();
  if (Z.typing) setTyping(false);
  go(`#/a/${S.zoomed}`);
}

/** After Ctrl-b (it works everywhere, inside a terminal too). */
function runCommand(e) {
  const k = e.key;
  if (k === "n") return newHere(), true;
  if (k === "?" || (k === "/" && e.shiftKey)) return showHelp(), true;
  if (k === "Escape" || k === "z") { leaveTerminal(); return true; }
  if (k === "x") { const id = target(); if (id) confirmStop(id); return true; }
  if (k === "d") { go("#/more/decisions"); return true; }
  if (k === ".") { Shell.toggle("needs"); return true; }
  if (k === "c" && S.zoomed) { go(`#/a/${S.zoomed}/changes`); return true; }
  if (k === "t" && S.zoomed) { go(`#/a/${S.zoomed}/terminal`); return true; }
  if (ARROWS[k]) return move(ARROWS[k]), true;
  if (/^[1-9]$/.test(k)) { const id = S.order[Number(k) - 1]; if (id) go(`#/a/${id}/terminal`); return true; }
  return false;
}

/** Whether the agent page's terminal shows and its agent runs. */
function zoomTermShown() {
  return !!S.zoomed && !$("zoom-view").hidden && Z.tab === "term" && !!S.agents.get(S.zoomed)?.running;
}

/** Esc typed into a busy agent's terminal: say what it did, and how to leave. */
let escNoted = 0;
function escapeNote(a) {
  if (!a || !["working", "waiting"].includes(a.state) || Date.now() - escNoted < 8000) return;
  escNoted = Date.now();
  toast(`Esc went to ${nameOf(a)}: Claude Code stops what it is doing. To leave the terminal, press Ctrl-b Esc.`);
}

// The goal pages' keys (UX critique, issue 6; power users, 4): j/k between
// cards, ] [ to the next card that needs you, a digit then Enter answers
// (the card's own keys), x selects, Shift and a digit answers the selected
// alike, g and a letter goes to a goal, . the drawer, a the agents.
const CARD = ".qc, .alert-card, .wait-card";
function surface() {
  if (Shell.isOpen()) return $("ov");
  if (!$("zoom-view").hidden) return $("zoom-view");
  if (!$("grid-view").hidden) return $("grid-view");
  return $("goal-view");
}
function visible(list) { return list.filter((c) => c.offsetParent !== null); }
function moveIn(sel, d) {
  const list = visible([...surface().querySelectorAll(sel)]);
  if (!list.length) return false;
  const cur = document.activeElement && document.activeElement.closest ? document.activeElement.closest(sel) : null;
  const i = list.indexOf(cur);
  const j = i < 0 ? (d > 0 ? 0 : list.length - 1) : Math.max(0, Math.min(list.length - 1, i + d));
  list[j].focus({ preventScroll: true });
  list[j].scrollIntoView({ block: "nearest", behavior: "smooth" });
  return true;
}
const GP = { g: false, t: 0 };
function goalKey(e) {
  const k = e.key;
  if (GP.g) {
    GP.g = false;
    const p = currentProject();
    const to = { s: "#/", c: p ? `#/p/${p.project}` : "#/p", r: p ? `#/p/${p.project}/ship` : "#/p", d: "#/debug", t: "#/more/terminals", n: "#/needs" }[k];
    if (to) { go(to); return true; }
    return false;
  }
  if (k === "g") { GP.g = true; clearTimeout(GP.t); GP.t = setTimeout(() => { GP.g = false; }, 1500); return true; }
  if (G.view && G.view.onKey) { try { if (G.view.onKey(e)) return true; } catch (err) { console.error(err); } }
  if (k === ".") { Shell.toggle("needs"); return true; }
  if (k === "a") { Shell.toggle("agents"); return true; }
  if (k === "?") { showHelp(); return true; }
  if (k === "n") { newHere(); return true; }
  if (k === "z" && !S.zoomed) { PREFS.set("compact", !PREFS.get("compact")); toast(PREFS.get("compact") ? "Compact rows: one line per agent" : "Compact rows off"); return true; }
  if (e.shiftKey && /^Digit[1-9]$/.test(e.code)) { Shell.batchAnswer(e.code.slice(5)); return true; }
  if (k === "x") { const c = document.activeElement?.closest?.(CARD); if (c) { Shell.toggleSelect(c); return true; } return false; }
  if (k === "j" || k === "k") return moveIn(CARD, k === "j" ? 1 : -1) || moveIn("[data-nav], [data-href]", k === "j" ? 1 : -1);
  if (k === "]" || k === "[") {
    if (moveIn(CARD, k === "]" ? 1 : -1)) return true;
    if (!Shell.isOpen() && N.count) { Shell.open("needs"); return true; }
    return false;
  }
  if (k === "Enter") {
    const t = e.target.closest?.("[data-href]");
    if (t && !e.target.closest("a, button")) { go(t.dataset.href); return true; }
  }
  return false;
}

/** Esc outside a terminal: close what is open, else up one level. */
function escape() {
  if (Shell.closeSheet()) return;
  if (Shell.close()) return;
  if (T.open) { closeSheet(); return; }
  if (G.view && G.view.onEscape && G.view.onEscape()) return;
  const c = Shell.crumbs();
  if (c.length > 1) go(c[c.length - 2].href);
}

window.addEventListener("keydown", (e) => {
  if (e.isComposing || !$("gate").hidden) return;
  noteActive();
  if (S.prefix) {
    if (["Shift", "Control", "Alt", "Meta", "CapsLock"].includes(e.key)) return;
    e.preventDefault();
    e.stopPropagation();
    setPrefix(false);
    if (e.ctrlKey && (e.key === "b" || e.key === "B")) { Z.sock?.send("\x02"); return; } // C-b C-b: a literal C-b
    runCommand(e);
    return;
  }
  if (e.ctrlKey && !e.altKey && !e.metaKey && (e.key === "b" || e.key === "B")) {
    if (openDialog()) return;
    e.preventDefault();
    e.stopPropagation();
    setPrefix(true);
    return;
  }
  // Ctrl-Esc leaves the terminal from anywhere, the terminal included; Esc
  // there is the agent's (Claude Code interrupts with it), so leaving never
  // sends it.
  if (e.key === "Escape" && e.ctrlKey && !e.altKey && !e.metaKey && S.zoomed && !openDialog()) { e.preventDefault(); e.stopPropagation(); leaveTerminal(); return; }
  // A sheet (the grant check, a batch answer, Tell, the keys) has the keys while it is open.
  if (Shell.sheetOpen()) { if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); Shell.closeSheet(); } return; }
  // The other pages' sheets (the rules' grant check, Notifications, Review's
  // merge and send-back) close themselves on Esc: leave them every key.
  if (document.querySelector('.gs-scrim, .nt-scrim, [role="dialog"][aria-modal="true"]:not(dialog)')) return;
  if (e.key === "Escape" && e.target.id === "ask-q" && !openDialog()) { e.preventDefault(); e.target.blur(); if (!$("ask-out").hidden) $("ask-out").hidden = true; return; }
  // ⌘↵ on a page that has a goal to send (New project's steps).
  if ((e.metaKey || e.ctrlKey) && e.key === "Enter" && G.view && G.view.onKey) { try { if (G.view.onKey(e)) { e.preventDefault(); return; } } catch (err) { console.error(err); } }
  if (e.key === "Escape" && inField(e.target) && !openDialog() && !e.target.closest(".qc")) { e.target.blur(); return; }
  if (openDialog() || inField(e.target) || e.ctrlKey || e.metaKey || e.altKey) return;
  const inTerm = e.target.classList?.contains("xterm-helper-textarea");
  // Every other key belongs to the agent, Esc included, while it runs: no
  // single-key shortcut fires in a terminal.
  if (inTerm && (!S.zoomed || S.agents.get(S.zoomed)?.running)) {
    if (e.key === "Escape" && S.zoomed) escapeNote(S.agents.get(S.zoomed));
    return;
  }
  if (e.key === "Escape") { e.preventDefault(); escape(); return; }
  // On a desktop, typing on the Terminal tab goes to the agent even when the
  // focus has left the terminal (a sentence once began with "n").
  if (zoomTermShown() && !PHONE.matches && !T.open && !Shell.isOpen() && !e.target.closest?.("#ag-q, #ag-head, #ag-tabs") && e.key.length === 1 && e.key !== "[" && e.key !== "]") {
    e.preventDefault(); e.stopPropagation();
    Z.term?.focus();
    Z.sock?.send(e.key);
    return;
  }
  // A sequence wins over a page's single keys (Dev: `g c` on the agent
  // page went to its Changes tab), and ] [ go to the cards that need you
  // before a diff's own keys.
  if (GP.g && goalKey(e)) { e.preventDefault(); e.stopPropagation(); return; }
  if ((e.key === "]" || e.key === "[") && !e.shiftKey && moveIn(CARD, e.key === "]" ? 1 : -1)) { e.preventDefault(); e.stopPropagation(); return; }
  if (!inTerm && trailKey(e)) { e.preventDefault(); e.stopPropagation(); return; }
  if (e.key === "Enter" && !$("grid-view").hidden && e.target.closest?.(".tile")) { e.preventDefault(); go(`#/a/${e.target.closest(".tile").dataset.id}/terminal`); return; }
  if (!$("grid-view").hidden && ARROWS[e.key] && (e.target.closest?.(".tile") || e.target === document.body)) { e.preventDefault(); move(ARROWS[e.key]); return; }
  if (e.key === "/" && !T.open && !(G.r && G.r.page === "more" && G.r.item === "library")) { e.preventDefault(); openQuickSearch(); return; }
  if (goalKey(e)) { e.preventDefault(); e.stopPropagation(); }
}, true);

// A click anywhere drops an armed prefix, as a key would.
window.addEventListener("pointerdown", () => { if (S.prefix) setPrefix(false); }, true);

// ----------------------------------------------------------------- theme

matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  const theme = termTheme();
  for (const t of S.tiles.values()) t.preview?.retheme();
  if (Z.term) Z.term.options.theme = theme;
  T.player?.retheme();
});

// ---------------------------------------------------------------- the pages
//
// JOURNEYS.md, "URL routes": the hash names a page (ui/goals/router.js);
// the old hashes redirect. A goal page is a module with mount(root, ctx)
// in ui/goals/ (Review and Debug are R2's, review.js and debug.js; until
// they are served the old Ship, Trace and Health screens stand in).

// Search (fourth build): the top bar's quick search over code, docs and decisions; `/` opens it.
const quickCtx = () => ({ api, go, openWhy: ({ agent, path, line, text, commit }) => openLineCard(agent, path, line, { text, commit }), repo: () => pickRepo() || "", openDecisions: () => openDecisions() });
function openQuickSearch() { import("/levels/quicksearch.js").then((m) => m.open(quickCtx())).catch((err) => console.error(err)); }
const L = { recent: [] };

/** The context the old screens (ui/levels/) are mounted with. */
function levelCtx(param, extra = {}) {
  return {
    api,
    // The old screens set it on update (flow.js): the live state wins, so a set is dropped.
    get state() { return levelState(); },
    set state(_) { /* levelState() is always current */ },
    params: { agent: param || null, get repo() { return extra.repo || pickRepo() || null; }, ...Object.fromEntries(Object.entries(extra).filter(([k]) => k !== "repo")) },
    go,
    openWhy: ({ agent, path, line, text, commit }) => openLineCard(agent, path, line, { text, commit }),
    openRewind: (o) => openRewindCard(o),
    zoom: (id) => go(`#/a/${id}/terminal`),
  };
}

function levelState() {
  return { agents: [...S.agents.values()], events: L.recent };
}

/** A Rewind of any moment ({agent, from_ms, to_ms, edit_ms?, label?}) in the side sheet. */
function openRewindCard(o) {
  const a = S.agents.get(o.agent);
  beginCard("rewind", `rw\u0001${o.agent}\u0001${o.from_ms}`, (b) => openRewindCard({ ...o, back: b }), { push: true });
  sheetHead("Rewind", a ? nameOf(a) : o.agent);
  const host = h("div", { class: "rw" });
  $("sh-body").replaceChildren(...[
    o.label ? h("p", { class: "rw-label", text: o.label }) : null,
    host].flat().filter((k) => k != null && k !== false));
  T.player = new Rewind(host, { agent: o.agent, from: o.from_ms, to: o.to_ms, edit: o.edit_ms ?? o.to_ms - 3000, name: a ? nameOf(a) : o.agent, onWrite: () => {} });
}

// ------------------------------------------------------- settings, kept here

const PREFS_KEY = "chloedb-workbench-prefs";
const PREFS = {
  v: (() => { const x = parse(store("local", PREFS_KEY) || "{}"); return x && typeof x === "object" ? x : {}; })(),
  get(k) { return !!this.v[k]; },
  set(k, on) { this.v[k] = !!on; store("local", PREFS_KEY, JSON.stringify(this.v)); G.ukey = ""; schedule(); },
};

// ------------------------------------------------------------- projects
// GET /api/repos: each project's short name, its agents' groups, what is
// ready, its unmerged branches, its last activity and what needs you.

const PROJECT_KEY = "chloedb-workbench-open-repo"; // the project in focus (ui/levels/repos.js writes it too)
const P = { list: [], at: 0, busy: null, timer: 0 };
let gotProjects;
const firstProjects = new Promise((r) => { gotProjects = r; });
function refreshProjects() {
  if (!S.token) return Promise.resolve();
  if (P.busy) return P.busy;
  P.busy = api("/api/repos").then((j) => { P.list = j.repos || []; P.at = Date.now(); Z.headKey = ""; }, () => {})
    .finally(() => { P.busy = null; gotProjects(); schedule(); });
  return P.busy;
}
function projectsSoon() { clearTimeout(P.timer); P.timer = setTimeout(refreshProjects, 1500); }
function projectNames() { return Object.fromEntries(P.list.map((p) => [p.path, p.project])); }
function projectByName(n) { return P.list.find((p) => p.project === n) || null; }
function projectOf(path) { return path ? P.list.find((p) => p.path === path) || P.list.find((p) => path.startsWith(p.path + "/")) || null : null; }
function groups() { return Object.assign({}, ...P.list.map((p) => p.groups || {})); }
/** The project in focus: the page's, the agent page's, the last opened, else the most active. */
function currentProject() {
  if (G.r && G.r.project) { const p = projectByName(G.r.project); if (p) return p; }
  const z = S.agents.get(S.zoomed);
  if (z) { const p = projectOf(z.repo); if (p) return p; }
  return projectOf(store("local", PROJECT_KEY)) || P.list[0] || null;
}
/** After a merge (R2's hook): the next ready branch of the project, else the project. */
function nextReady(after) {
  return refreshProjects().then(() => {
    const p = currentProject();
    if (!p) { go("#/"); return; }
    const next = (p.ready || []).find((x) => x.id !== after);
    go(next ? `#/p/${p.project}/ship/${next.id}` : `#/p/${p.project}`);
  });
}

// --------------------------------------------------------- what needs you
// GET /api/needs: the one list and the one count, on the top bar, the tab
// bar, the drawer, Start, Continue, the inbox and the agent page.

const N = { count: 0, items: [], by_repo: {} };
const NQ = { busy: null, again: false, timer: 0, key: "" };
function refreshNeeds() {
  if (!S.token) return Promise.resolve();
  if (NQ.busy) { NQ.again = true; return NQ.busy; }
  NQ.busy = api("/api/needs").then((j) => {
    N.count = j.count || 0;
    N.items = j.items || [];
    N.by_repo = j.by_repo || {};
    const key = N.items.map((i) => i.id).join(",");
    if (key !== NQ.key) { NQ.key = key; Z.headKey = ""; try { NOTIFY && NOTIFY.poke(); } catch (err) { console.error(err); } }
    schedule();
  }, () => {}).finally(() => { NQ.busy = null; if (NQ.again) { NQ.again = false; needsSoon(); } });
  return NQ.busy;
}
function needsSoon() { clearTimeout(NQ.timer); NQ.timer = setTimeout(refreshNeeds, 400); }
setInterval(() => { if (S.token && document.visibilityState === "visible") refreshNeeds(); }, 10000);
/** An answer went through: the card goes at once, the counts follow, and
 *  "Since you left" reads the digest again. */
function answered(id, { quiet = false } = {}) {
  // The focus goes to the next card, not to the page (Dev pressed ] after every answer).
  const cards = visible([...surface().querySelectorAll(CARD)]);
  const at = cards.findIndex((c) => c.dataset.need === id || c.contains(document.activeElement));
  const keyOf = (c) => (c ? (c.dataset.need ? `[data-need="${CSS.escape(c.dataset.need)}"]` : c.dataset.q ? `[data-q="${CSS.escape(c.dataset.q)}"]` : null) : null);
  const nextKey = at >= 0 ? keyOf(cards[at + 1] || cards[at - 1]) : null;
  if (nextKey) setTimeout(() => {
    const a = document.activeElement;
    if (a && a !== document.body && a.isConnected && !a.closest(CARD)) return;
    const el = surface().querySelector(nextKey);
    if (el) el.focus({ preventScroll: false });
  }, 120);
  N.items = N.items.filter((i) => i.id !== id);
  N.count = N.items.length;
  NQ.key = N.items.map((i) => i.id).join(",");
  Z.headKey = "";
  G.ukey = "";
  noteActive();
  schedule();
  if (!quiet) {
    needsSoon();
    projectsSoon();
    try { G.view && G.view.reload && G.view.reload(); } catch (err) { console.error(err); }
  }
}

// ---------------------------------------------------------------- router

const G = { r: null, view: null, req: 0, key: "", ukey: "" };
const GOAL = {
  start: "/goals/start.js", new: "/goals/newproject.js", needs: "/goals/needs.js",
  open: "/goals/continue.js", continue: "/goals/continue.js", understand: "/goals/continue.js", change: "/goals/continue.js",
  ship: "/goals/review.js", debug: "/goals/debug.js", more: "/goals/more.js",
  harness: "/goals/harness/index.js",
};

function go(hash) {
  if (location.hash === hash || (!location.hash && hash === "#/")) route(hash);
  else location.hash = hash;
}
function replaceHash(hash) {
  history.replaceState(history.state, "", hash);
  route(hash);
}
window.addEventListener("hashchange", () => route(location.hash));

function showView(id) {
  for (const v of ["goal-view", "grid-view", "zoom-view", "dec-view"]) $(v).hidden = v !== id;
}

function unmountGoal() {
  try { G.view && G.view.unmount && G.view.unmount(); } catch (err) { console.error(err); }
  G.view = null;
  G.key = "";
  G.req++;
}

/** Whether a redraw now would take something from under the person: a
 *  text box with the focus, or an option picked in a card. */
function busy() {
  const a = document.activeElement;
  if (!a || a === document.body) return false;
  if (!$("goal-view").contains(a)) return false;
  return inField(a) || !!(a.closest(".qc") && a.closest(".qc").querySelector(".qc-opt.sel"));
}
function goalUpdate() {
  if (!G.view || !G.view.update) return;
  const k = JSON.stringify([NQ.key, P.at, [...S.agents.values()].map((a) => `${a.id}${a.state}${a.running ? 1 : 0}`).join(), PREFS.v]);
  if (k === G.ukey || busy()) return;
  G.ukey = k;
  try { G.view.update(levelState()); } catch (err) { console.error(err); }
}

async function route(hash) {
  hash = hash || "#/";
  const r = resolve(hash, { project: currentProject()?.project || null, agentIds: [...S.agents.keys()] });
  if (r.redirect) { replaceHash(r.redirect); return; }
  Shell.close();
  Shell.closeSheet();
  if (T.open) closeSheet({ focus: false });
  document.body.classList.remove("stepping");
  G.r = r;
  Shell.update(); // the tab bar follows the page
  if (r.project) { const p = projectByName(r.project); if (p) store("local", PROJECT_KEY, p.path); }

  if (r.page === "agent") {
    unmountGoal();
    closeDecisions();
    if (!S.agents.has(r.agent)) {
      await Promise.race([firstSnapshot, new Promise((ok) => setTimeout(ok, 3000))]);
      if (G.r !== r) return;
    }
    if (!S.agents.has(r.agent)) {
      unzoom();
      showView("goal-view");
      Shell.setCrumbs([{ label: "Start", href: "#/" }, { label: r.agent, href: hash }]);
      $("goal-view").replaceChildren(h("div", { class: "gp" }, h("div", { class: "gp-main" }, h("h1", { class: "gp-title", text: "No such agent" }), h("p", { class: "gp-sub", text: `Workbench knows no agent ${r.agent}.` }), h("a", { class: "btn", href: "#/" }, "Start"))));
      return;
    }
    showView("zoom-view");
    const a = S.agents.get(r.agent);
    const p = projectOf(a.repo);
    zoom(r.agent, { tab: r.tab === "terminal" ? "term" : r.tab });
    Shell.setCrumbs([{ label: "Start", href: "#/" }, ...(p ? [{ label: p.project, href: `#/p/${p.project}` }] : []), { label: nameOf(a), href: `#/a/${a.id}` }]);
    return;
  }
  unzoom();
  if (r.page === "more" && r.item === "terminals") {
    unmountGoal();
    closeDecisions();
    showView("grid-view");
    Shell.setCrumbs([{ label: "Start", href: "#/" }, { label: "More", href: "#/more" }, { label: "All terminals", href: hash }]);
    render();
    requestAnimationFrame(() => { for (const t of S.tiles.values()) t.preview?.scale(); });
    return;
  }
  if (r.page === "more" && r.item === "decisions") {
    unmountGoal();
    showView("dec-view");
    showDecisions();
    Shell.setCrumbs([{ label: "Start", href: "#/" }, { label: "More", href: "#/more" }, { label: "Decisions timeline", href: hash }]);
    return;
  }
  closeDecisions();
  showView("goal-view");
  // Review and Debug follow their own steps (their hashchange): a step
  // within one of them keeps the page mounted.
  const key = r.page === "ship" || r.page === "harness" ? JSON.stringify([r.page, r.project]) : r.page === "debug" ? JSON.stringify([r.page])
    : JSON.stringify([r.page, r.project, r.agent, r.step, r.item, r.rest, r.tab, r.query.toString()]);
  if (G.view && G.key === key) { G.ukey = ""; goalUpdate(); return; }
  unmountGoal();
  G.key = key;
  const req = G.req;
  const host = $("goal-view");
  host.replaceChildren(h("div", { class: "lv-loading", text: "Loading…" }));
  // The agents and the projects first, so a page opened by its address knows them.
  await Promise.race([Promise.all([firstSnapshot, firstProjects]), new Promise((ok) => setTimeout(ok, 3000))]);
  if (req !== G.req) return;
  const ctx = goalCtx(r);
  let m = null;
  try { m = await import(GOAL[r.page]); } catch (err) {
    if (req === G.req) host.replaceChildren(h("div", { class: "lv-loading bad", text: `This page did not load: ${err.message || err}` }));
    return;
  }
  if (req !== G.req) return;
  host.replaceChildren();
  host.scrollTop = 0;
  try {
    G.view = m.mount(host, ctx);
    G.ukey = "";
  } catch (err) {
    console.error(err);
    host.replaceChildren(h("div", { class: "lv-loading bad", text: `This page did not load: ${err.message || err}` }));
  }
}

/** What a goal page is mounted with: the old screens' context, plus the
 *  project, the crumbs, the needs, the projects and the shell's cards. */
function goalCtx(r) {
  const p = r.project ? projectByName(r.project) : null;
  const ctx = levelCtx(r.agent || null, { repo: p ? p.path : undefined });
  Object.defineProperty(ctx, "params", { value: { ...r, agent: r.agent || null, repo: p ? p.path : pickRepo(), page: r.page, project: r.project || null, query: r.query }, enumerable: true });
  return Object.assign(ctx, {
    project: p || (r.project ? null : currentProject()),
    crumbs: (list) => Shell.setCrumbs(list),
    toast,
    needs: () => N,
    needCard: (it, o) => Shell.needCard(it, o),
    projects: () => P.list,
    projectNames, projectByName, projectOf, refreshProjects,
    prefs: PREFS,
    levelCtx: (extra) => levelCtx(null, extra),
    restart: restartAgent,
    notify: () => { try { NOTIFY && NOTIFY.poke(); } catch (err) { console.error(err); } },
    notifications: () => { try { if (NOTIFY) NOTIFY.open(); else toast("Notifications did not load", true); } catch (err) { console.error(err); } },
    slot: (name) => slot(name, ctx),
    groupOf: Shell.groupOf,
    fleet: Shell.fleet,
    merged: (agentId) => nextReady(agentId),
    stop: (id) => confirmStop(id),
    openDecisions,
  });
}

/** A place on a page another module fills (agent P's rules-suggest.js): empty until it loads. */
function slot(name, ctx) {
  const host = h("div", { class: `slot slot-${name}` });
  import(`/goals/${name}.js`).then((m) => Promise.resolve(m.card ? m.card(ctx) : m.mount ? m.mount(host, ctx) : null))
    .then((el) => { if (el instanceof Node) host.append(el); }).catch(() => {});
  return host;
}

// ----------------------------------------------------------------- start

/** The last time you did something here: a key, a click, an answer, a
 *  return to the page. "Since you left" counts from it (power users, 6). */
let lastActive = Date.now();
function noteActive() { lastActive = Date.now(); }
for (const k of ["pointerdown", "keydown", "wheel", "touchstart"]) document.addEventListener(k, noteActive, { passive: true, capture: true });
window.addEventListener("focus", noteActive);

/** You stop looking: the page is hidden or left. The digest counts from
 *  your last real presence, not from the moment the tab went away. */
function markSeen() {
  if (!S.token) return;
  try {
    fetch("/api/seen", { method: "POST", keepalive: true, headers: { Authorization: `Bearer ${S.token}`, "Content-Type": "application/json" }, body: JSON.stringify({ ts_ms: lastActive }) }).catch(() => {});
  } catch { /* leaving: nothing to do */ }
}
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "hidden") markSeen(); else { noteActive(); refreshNeeds(); } });
window.addEventListener("pagehide", markSeen);
// While the page is shown and used, say so once a minute, so a morning at
// the desk is not "away" (src/digest.rs).
setInterval(() => {
  if (!S.token || document.visibilityState !== "visible" || Date.now() - lastActive > 120000) return;
  fetch("/api/here", { method: "POST", headers: { Authorization: `Bearer ${S.token}`, "Content-Type": "application/json" }, body: "{}" }).catch(() => {});
}, 60000);

Shell.initShell({
  api,
  agents: () => [...S.agents.values()],
  needs: () => N,
  groups,
  project: currentProject,
  projectNames,
  restart: restartAgent,
  openSearch: openQuickSearch,
  toast,
  answered,
  refreshNeeds,
});

// Agent P's interrupt model (ui/goals/notify.js): started once; its bell
// sits in the top bar before More (on a phone, More lists Notifications).
let NOTIFY = null;

let started = false;
function start() {
  if (!started) {
    started = true;
    setInterval(tickTimes, 1000);
    fetch("/api/health").then((r) => r.json()).then((j) => { S.health = j; schedule(); }).catch(() => {});
    refreshProjects();
    refreshNeeds();
    // ChloeCtx is required: say so up front when sessions would miss it.
    import("/levels/setup.js").then((m) => m.checkSetup(api)).catch(() => {});
    import("/goals/notify.js").then((m) => {
      NOTIFY = m.init(goalCtx({ page: "notify", query: new URLSearchParams() }));
      if (NOTIFY && NOTIFY.control) { NOTIFY.control.classList.add("tb", "icon", "nt-top"); $("more-btn").before(NOTIFY.control); }
    }).catch((err) => console.error(err));
  }
  // Settings › Open on the inbox.
  if ((!location.hash || location.hash === "#/") && PREFS.get("openOnInbox")) history.replaceState(history.state, "", "#/needs");
  connectStream();
  route(location.hash || "#/");
  render();
}

S.token = readToken();
if (!S.token) gate("Open the link the server printed");
else start();
