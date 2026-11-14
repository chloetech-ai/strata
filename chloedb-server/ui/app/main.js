// The command center's shell: the navigation (four groups and Labs), the
// top bar (which cluster, the time range, live or paused, share, alerts,
// who is signed in), whether the server answers, the router, the command
// palette (Ctrl+K), which finds views, actions and the fleet's shards,
// copies, nodes, tables, statements, alerts and keys, and the shortcuts.

import { h, icon, clear, store, bus, api, at, connectionName, setToken, hasToken, signInNeeded, modal, toast, menu, seg, fmt, copy, connection, can, needs, link, when, whenTitle, span, copyName, copyTitle, isNarrow, logsLink, keyLabel, isIdle, IDLE_SLOW } from "./core.js";
import { state, start, setRange, setWindow, setZoom, setPaused, refreshWho, pendingProposals, homeProblems, homeStatus, RANGES, rangeFromUrl, rangeParams, timeRange, firingAlerts, firingNow, fresh, clusterName, environment, isCoordinator, setPolls, overnight, isSandbox } from "./data.js";
import { VIEWS, NAV, REDIRECTS } from "./views/index.js";
import { loadApps, listApps, appIcon } from "./views/apps.js";
import { openFeedback, setViewOf } from "./feedback.js";
import { openAsk, setWhereOf } from "./ask.js";

const app = document.getElementById("app");
const byId = Object.fromEntries(VIEWS.map((v) => [v.id, v]));
/** A nav entry's view: its own module, or its fallback's. */
const viewOf = (e) => byId[e.id] || (e.fallback && byId[e.fallback]) || null;
const ENTRIES = NAV.flatMap(([g, list]) => list.map((e) => ({ ...e, group: g }))).filter((e) => viewOf(e));
const entryOf = (id) => ENTRIES.find((e) => e.id === id || (!byId[e.id] && e.fallback === id));
/**
 * A view's code (CCW-10): imported the first time it opens, or when its
 * link is pointed at; v.mod is the module's export once it is here.
 */
function loadView(v) {
  if (!v || v.mod) return Promise.resolve(v?.mod);
  v.loading ||= v.load().then((m) => (v.mod = m.default), (err) => { v.loading = null; throw err; });
  return v.loading;
}

// ---------------------------------------------------------------- theme
const applyTheme = (t) => { if (t) document.documentElement.dataset.theme = t; else delete document.documentElement.dataset.theme; bus.emit("theme"); };
const isDark = () => document.documentElement.dataset.theme ? document.documentElement.dataset.theme === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;
function toggleTheme() { const t = isDark() ? "light" : "dark"; applyTheme(t); store.set("theme", t); themeBtn.replaceChildren(icon(isDark() ? "sun" : "moon")); }
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => bus.emit("theme"));

// ---------------------------------------------------------------- the cluster, in the sidebar
const brandName = h("b", {}, location.host);
const brandSub = h("small", {}, "connecting…");
const brand = h("button", { class: "brand", title: "Which cluster this is, and the others you used", "aria-haspopup": "menu" },
  document.createElementNS("http://www.w3.org/2000/svg", "svg"),
  h("div", { class: "clu" }, h("span", { class: "row nw", style: { gap: "4px" } }, brandName, icon("down", "chev")), brandSub));
const markSvg = brand.querySelector("svg");
for (const [k, v] of Object.entries({ class: "mark", viewBox: "0 0 32 32", "aria-hidden": "true" })) markSvg.setAttribute(k, v);
markSvg.innerHTML = '<defs><linearGradient id="lg" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="var(--s1)"/><stop offset="1" stop-color="var(--s7)"/></linearGradient></defs><rect x="2" y="2" width="28" height="28" rx="8" fill="url(#lg)"/><path d="M8 11.5 16 8l8 3.5-8 3.5z" fill="#fff" opacity=".95"/><path d="m8 16 8 3.5 8-3.5" stroke="#fff" stroke-width="2" fill="none" stroke-linejoin="round" opacity=".8"/><path d="m8 20.5 8 3.5 8-3.5" stroke="#fff" stroke-width="2" fill="none" stroke-linejoin="round" opacity=".6"/>';

// Coordinators used from this browser, newest first. Each address keeps its
// own list (storage is per address), so switching records both ends.
const RECENT = "coordinators";
function remember(url, name, env) {
  const old = (store.get(RECENT, []) || []);
  const was = old.find((x) => x.url === url);
  if (was && was.name === (name || was.name) && Date.now() - was.at < 60e3) return;
  const list = old.filter((x) => x.url !== url);
  list.unshift({ url, name: name || was?.name || null, env: env || was?.env || null, at: Date.now() });
  store.set(RECENT, list.slice(0, 12));
}
const hostOf = (u) => { try { return new URL(u).host; } catch { return u; } };
const sameOrigin = (a, b) => { try { return new URL(a).origin === new URL(b).origin; } catch { return a === b; } };
function switchTo(url) {
  try { const u = new URL(url, location.href); remember(u.origin, null, null); location.href = u.origin + "/#/home"; }
  catch { toast("That is not an address: " + url, "bad"); }
}
// The other coordinators of this cluster: those its shards heard from in
// the last minute (GET /v1/coordinators), else those fleet health lists.
let peers = { at: 0, list: null };
async function otherCoordinators() {
  if (Date.now() - peers.at > 10e3) {
    try { const j = await api("/v1/coordinators", { timeout: 3000 }); peers = { at: Date.now(), list: j.coordinators || [] }; }
    catch { peers = { at: Date.now(), list: null }; }
  }
  return peers.list || state.fleet?.coordinators || [];
}
// The hub's book (GET /v1/admin/hub): the clusters this server's command
// center reaches. Read once a minute at most; a server whose key is not an
// admin's, or a coordinator, has none to show.
let book = { at: 0, list: [] };
async function hubBook() {
  if (Date.now() - book.at > 60e3) {
    try { const j = await api("/v1/admin/hub", { hub: true, timeout: 3000 }); book = { at: Date.now(), list: j.connections || [] }; }
    catch { book = { at: Date.now(), list: [] }; }
  }
  return book.list;
}
const openConnection = (name) => { location.href = name ? `/c/${name}#/home` : "/#/home"; };
// On a connection's page: its color on the top edge and the brand, and
// its name in the tab's title.
if (connectionName) {
  document.documentElement.classList.add("via-hub");
  hubBook().then((list) => {
    const c = list.find((x) => x.name === connectionName);
    if (c?.color) document.documentElement.style.setProperty("--conn", c.color);
  });
}
brand.onclick = async () => {
  const conns = await hubBook();
  if (conns.length || connectionName) {
    const items = [];
    items.push({ label: `This hub · ${location.host}`, icon: connectionName ? "left" : "check", onclick: () => connectionName && openConnection(null) });
    items.push("-");
    for (const c of conns) items.push({ label: `${c.name}${c.environment ? " · " + c.environment : ""}${c.last_error ? " · not answering" : ""}`, icon: c.name === connectionName ? "check" : "cluster", color: c.color, onclick: () => c.name !== connectionName && openConnection(c.name) });
    items.push("-", { label: "Connections and keys…", icon: "key", onclick: () => { location.href = "/#/connections"; } });
    menu(brand, items);
    return;
  }
  const here = location.origin;
  const coords = (await otherCoordinators()).filter((c) => c.url && !c.this && !sameOrigin(c.url, here));
  const recent = (store.get(RECENT, []) || []).filter((x) => x.url !== here && !coords.some((c) => sameOrigin(c.url, x.url)));
  const items = [{ label: `${clusterName() || location.host}${environment() ? " · " + environment() : ""} · here`, icon: "check", onclick: () => {} }];
  if (coords.length) { items.push("-"); for (const c of coords) items.push({ label: `${c.name || hostOf(c.url)} · same cluster${c.name && c.name !== hostOf(c.url) ? " · " + hostOf(c.url) : ""}`, icon: "node", onclick: () => switchTo(c.url) }); }
  if (recent.length) { items.push("-"); for (const r of recent.slice(0, 8)) items.push({ label: `${r.name || hostOf(r.url)}${r.env ? " · " + r.env : ""} · ${fmt.ago(r.at)}`, icon: "clock", onclick: () => switchTo(r.url) }); }
  items.push("-", { label: "Open another coordinator…", icon: "plus", onclick: askCoordinator });
  menu(brand, items);
};
function askCoordinator() {
  const inp = h("input", { class: "input", placeholder: "https://chloedb.example.com or 10.0.3.7:7700", style: { width: "100%" } });
  const ok = h("button", { class: "btn primary" }, "Open");
  const m = modal({ title: "Open another coordinator", body: [h("p", { class: "dim", style: { margin: 0 } }, "Its command center opens in this tab. You sign in there with its own key."), inp], foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), ok] });
  ok.onclick = () => { const v = inp.value.trim(); if (v) { m.close(); switchTo(/^https?:\/\//.test(v) ? v : "http://" + v); } };
  inp.onkeydown = (e) => { if (e.key === "Enter") ok.onclick(); };
}

// ---------------------------------------------------------------- the navigation
const nav = h("nav", { class: "nav", "aria-label": "Views" });
const side = h("aside", { class: "side" }, brand, nav,
  h("div", { class: "side-foot" },
    h("button", { class: "btn ghost sm", title: "Collapse the sidebar ( [ )", onclick: () => toggleSide() }, icon("left"), h("span", {}, "Collapse"))));

const PINS = "labs.pinned";
let labsOpen = store.get("labs.open", false);
function navLink(e) {
  const v = viewOf(e);
  const a = h("a", { href: "#/" + e.id, dataset: { view: e.id }, title: e.title + (e.key ? ` (g then ${e.key})` : "") }, icon(e.icon || v.icon), h("span", { class: "t" }, e.title));
  // Its code starts to load when the link is pointed at, so it is often here by the click.
  const soon = () => { loadView(v).catch(() => { /* said when it opens */ }); };
  a.addEventListener("pointerenter", soon); a.addEventListener("focus", soon); a.addEventListener("touchstart", soon, { passive: true });
  if (e.key) a.append(h("span", { class: "kb", "aria-hidden": "true" }, "g " + e.key));
  if (e.scope && state.whoami && !can(e.scope, e.route)) { a.classList.add("gated"); a.title = `${e.title}: you can look; changing anything here ${needs(e.scope, e.route)}`; a.append(h("span", { class: "lockic" }, icon("lock"))); }
  // The range goes along, so a view opens on the window in use.
  a.addEventListener("click", (ev) => { if (ev.button === 0 && !ev.metaKey && !ev.ctrlKey && !ev.shiftKey) { ev.preventDefault(); location.hash = link(e.id); } });
  return a;
}
// A single server (no shards behind it) leaves the cluster's pages out of
// the sidebar; a coordinator puts watching the fleet before building.
const single = () => !!state.health && !isCoordinator();
let navShape = null;
const shapeOf = () => `${isCoordinator()}|${single()}`;
for (const ev of ["health", "cluster"]) bus.on(ev, () => { if (navShape !== shapeOf()) { drawNav(); drawBadges(); } });
function drawNav() {
  clear(nav);
  navShape = shapeOf();
  const pins = new Set(store.get(PINS, []));
  const groups = isCoordinator() ? ["Watch", "Run the cluster", "Build", "Apps", "Labs"].map((g) => NAV.find((x) => x[0] === g)).filter(Boolean) : NAV;
  for (const [g, all] of groups) {
    const list = all.filter((e) => !(e.cluster && single()));
    if (g === "Apps") {
      const as = listApps();
      if (as.length) { nav.append(h("h6", {}, "Apps")); for (const a of as) nav.append(appLink(a)); }
      continue;
    }
    const es = list.map((e) => ({ ...e, group: g })).filter((e) => viewOf(e));
    if (!es.length) continue;
    if (g !== "Labs") { nav.append(h("h6", {}, g)); for (const e of es) nav.append(navLink(e)); continue; }
    // Labs: pinned ones show; the rest behind a toggle.
    const pinned = es.filter((e) => pins.has(e.id));
    const box = h("div", { class: "labs" + (labsOpen ? " open" : "") });
    const toggle = h("button", { class: "labs-toggle", "aria-expanded": String(labsOpen), title: "Tools for search engineers: ranking, recommendations, demos. Pin one to keep it in the sidebar" },
      icon("labs"), h("span", { class: "t" }, `Labs · ${es.length} tools`), icon(labsOpen ? "down" : "right", "chev"));
    toggle.onclick = () => { labsOpen = !labsOpen; store.set("labs.open", labsOpen); drawNav(); drawBadges(); };
    box.append(toggle);
    for (const e of labsOpen ? es : pinned) box.append(labItem(e, pins.has(e.id)));
    nav.append(box);
  }
  markCurrent();
}
/** An app's entry: #/app/<name>, with the app's own icon. */
function appLink(a) {
  const el = h("a", { href: "#/app/" + encodeURIComponent(a.name), dataset: { view: "app/" + a.name }, title: a.up ? `${a.title}: ${a.description || "an app"}` : `${a.title}: not answering (${a.error})` }, appIcon(a), h("span", { class: "t" }, a.title));
  if (!a.up) el.append(h("span", { class: "count warn", title: "not answering" }, "!"));
  return el;
}
function labItem(e, pinned) {
  const a = navLink(e);
  a.classList.add("lab");
  const pin = h("button", { class: "pin" + (pinned ? " on" : ""), title: pinned ? "Unpin: show it only when Labs is open" : "Pin: keep it in the sidebar", "aria-pressed": String(pinned) }, icon("pin"));
  pin.onclick = (ev) => {
    ev.preventDefault(); ev.stopPropagation();
    const set = new Set(store.get(PINS, []));
    if (set.has(e.id)) set.delete(e.id); else set.add(e.id);
    store.set(PINS, [...set]); drawNav(); drawBadges();
  };
  a.append(pin);
  return a;
}

// Badges: a view's own badge(), else what the shell knows. Alerts, the
// bell and Home show one count: the alerts firing now that nobody
// silenced (firingNow).
function shellBadge(id) {
  if (id === "oncall") { const n = pendingProposals(); return n ? { n, tone: "warn", title: `${fmt.plural(n, "proposal")} (the on-call agent's, and change requests) wait for an admin` } : null; }
  if (id === "home") { const f = state.fleet; if (!f) return null; const n = homeProblems().length; return n ? { n, tone: homeStatus() === "critical" ? "crit" : "warn", title: `${fmt.plural(n, "problem")} on Home` } : null; }
  // Info alerts are counted, in a neutral tone: only a warning or worse turns the count amber or red (F2).
  if (id === "alerts") { const fa = firingNow(); const n = fa.length; return n ? { n, tone: fa.some((a) => a.severity === "critical") ? "crit" : fa.some((a) => a.severity === "warning") ? "warn" : "info", title: `${fmt.plural(n, "alert")} firing${fa.every((a) => a.severity === "info") ? " (info)" : ""}` } : null; }
  if (id === "cluster") {
    const c = state.cluster; if (!c?.shards) return null;
    let down = 0, dark = 0;
    for (const sh of c.shards) { const reps = sh.replicas || []; const up = reps.filter((r) => r.status === "ok" && r.role !== "unknown").length; down += reps.length - up; if (sh.state ? ["no_reads", "down", "no_writes"].includes(sh.state) : (up === 0 || (reps.length > 1 && up * 2 <= reps.length))) dark++; }
    return down ? { n: down, tone: dark ? "crit" : "warn", title: `${fmt.plural(down, "copy", "copies")} not answering` } : null;
  }
  return null;
}
function drawBadges() {
  if (state.paused && drawBadges.done) return;
  drawBadges.done = true;
  for (const a of nav.querySelectorAll("a[data-view]")) {
    // An app's "!" (not answering) is its link's own, drawn with the list of apps.
    if (a.dataset.view.startsWith("app/")) continue;
    const e = ENTRIES.find((x) => x.id === a.dataset.view);
    const v = e && viewOf(e);
    let b = null;
    try { b = v?.mod?.badge && e?.id !== "alerts" && e?.id !== "oncall" ? v.mod.badge() : shellBadge(e?.id); } catch (err) { console.warn(err); }
    let el = a.querySelector(".count");
    if (!b || !b.n) { el?.remove(); continue; }
    if (!el) { el = h("span", { class: "count" }); a.querySelector(".t").after(el); }
    el.className = "count " + (b.tone === "crit" || b.tone === "bad" ? "" : b.tone || "");
    el.textContent = b.n > 99 ? "99+" : String(b.n);
    el.title = b.title || "";
  }
  const fa = firingNow(), n = fa.length;
  bellCount.textContent = n; bellCount.classList.toggle("hidden", !n);
  // The bell is red for a critical alert, amber for a warning, and grey when all that fires is info.
  bellCount.classList.toggle("crit", fa.some((a) => a.severity === "critical"));
  bellCount.classList.toggle("info", !!n && !fa.some((a) => a.severity === "critical" || a.severity === "warning"));
  bell.title = n ? `${fmt.plural(n, "alert")} firing${bellCount.classList.contains("info") ? " (info only)" : ""}` : "Alerts: nothing firing";
}
for (const ev of ["alerts", "fleet", "cluster", "pending"]) bus.on(ev, drawBadges);
bus.on("whoami", () => { drawNav(); drawBadges(); });
// The server's apps, once it says who is signed in (the list needs a key on a closed server).
bus.on("whoami", () => loadApps(true));
bus.on("apps", () => {
  drawNav(); drawBadges();
  if (current?.id === "app" && currentEntry?.id?.startsWith("app/")) { const a = listApps().find((x) => "app/" + x.name === currentEntry.id); if (a) currentEntry.title = a.title; drawCrumbs(); markCurrent(); }
});
setInterval(drawBadges, 5000);

// ---------------------------------------------------------------- the top bar
const crumbs = h("div", { class: "crumbs" });
const envBadge = h("span", { class: "env hidden" });
const searchBtn = h("button", { class: "search-btn", onclick: () => openPalette(), title: "Find a shard, copy, node, table, statement, alert, key, view or action" }, icon("search"), h("span", {}, "Shard, node, table, alert…"), h("kbd", {}, navigator.platform?.includes("Mac") ? "⌘K" : "Ctrl K"));
const rangeSeg = seg(RANGES.map(([k, m]) => [m, k, `The last ${k.replace(/(\d)([a-z])/, "$1 $2")}`]), state.range, (v) => setRange(v), { "aria-label": "Time range" });
const absBtn = h("button", { class: "abs" });
absBtn.onclick = () => pickWindow();
const zoomChip = h("button", { class: "chip hidden", "aria-pressed": "true", title: "Drag across a chart to zoom; this resets it", onclick: () => setZoom(null) }, icon("zout"), "zoomed", h("span", { class: "x" }, "×"));
const liveBtn = h("button", { class: "btn sm live", onclick: () => (signInNeeded() ? bus.emit("unauthorized") : setPaused(!state.paused)) });
const shareBtn = h("button", { class: "btn sm", title: "Copy a link to exactly this: the view, its selection and this window as fixed times (it opens paused) (s)", onclick: () => share() }, icon("share"), h("span", { class: "lbl" }, "Share"));
const bell = h("a", { class: "btn ghost icon", href: "#/alerts", title: "Alerts", style: { position: "relative" } }, icon("alerts"));
const bellCount = h("span", { class: "count hidden bellc" });
bell.append(bellCount);
const themeBtn = h("button", { class: "btn ghost icon", title: "Light or dark (t)", onclick: toggleTheme }, icon(isDark() ? "sun" : "moon"));
// Feedback: this view as a picture, to mark up and send (feedback.js).
const feedbackBtn = h("button", { class: "btn ghost icon fb-open", title: "Feedback: take this view as a picture, mark what is wrong or what you want, and send it (f)", "aria-label": "Send feedback", onclick: () => openFeedback() }, icon("feedback"));
setViewOf(() => ({ id: currentEntry?.id || current?.id || null, title: currentEntry?.title || current?.title || null }));
// Ask: a question about the data, in words, answered by the server's agent (ask.js).
const askBtn = h("button", { class: "btn ghost sm ask-open", title: "Ask a question about your data, in words (Ctrl+J)", "aria-label": "Ask about your data", onclick: () => openAsk() }, icon("sparkle"), h("span", { class: "lbl" }, "Ask"));
setWhereOf(() => ({ title: currentEntry?.title || null, table: current?.id === "table" ? parse().rest[0] || null : null }));
const whoBtn = h("button", { class: "btn ghost sm who", title: "Who you are signed in as" }, icon("user"), h("span", {}, "…"));
whoBtn.onclick = () => menu(whoBtn, [
  { label: "Use another key or token", icon: "access", onclick: askToken },
  { label: "Sign out of this tab", icon: "unlock", onclick: () => { setToken(null); toast("Signed out: the key is forgotten for this tab"); } },
  "-",
  { label: "Manage API keys", icon: "lock", onclick: () => (location.hash = "#/access?tab=keys") },
  { label: "Keyboard shortcuts", icon: "keyboard", onclick: () => shortcuts() },
]);
// Below 900 px: a menu button opens the navigation as a drawer, and one
// button holds the range, the window, Share, the theme and the key.
const navBtn = h("button", { class: "btn ghost icon hamb", "aria-label": "Views", title: "Views", "aria-expanded": "false", onclick: () => openNav(!shell.classList.contains("nav-open")) }, icon("menu"));
const moreBtn = h("button", { class: "btn sm more", "aria-haspopup": "dialog", title: "Time range, Share, theme and key", onclick: () => moreSheet() }, icon("clock"), h("span", { class: "lbl" }, "1h"));
const top = h("header", { class: "top" }, navBtn, crumbs, envBadge, h("div", { class: "sep" }), searchBtn, rangeSeg, absBtn, zoomChip, liveBtn, moreBtn, shareBtn, askBtn, feedbackBtn, bell, themeBtn, whoBtn);
// Says when the server does not answer, and that what shows is old.
const downBar = h("div", { class: "downbar hidden", role: "alert" });
const content = h("main", { class: "content", id: "content" });
const navScrim = h("div", { class: "nav-scrim", onclick: () => openNav(false) });
const shell = h("div", { class: "shell" + (store.get("side", false) && !isNarrow() ? " collapsed" : "") }, side, navScrim, top, content);
content.before(downBar);
app.replaceWith(shell);

function toggleSide() {
  if (isNarrow()) { openNav(!shell.classList.contains("nav-open")); return; }
  shell.classList.toggle("collapsed"); store.set("side", shell.classList.contains("collapsed"));
}
/** The navigation as a drawer, below 900 px. */
function openNav(on) {
  shell.classList.toggle("nav-open", on);
  navBtn.setAttribute("aria-expanded", String(on));
  if (on) setTimeout(() => nav.querySelector("a[aria-current]")?.focus(), 0);
}
// Crossing 900 px: the drawer closes, and the collapsed sidebar comes back on a wide screen.
bus.on("narrow", (narrow) => { openNav(false); shell.classList.toggle("collapsed", !narrow && store.get("side", false)); drawRange(); });
nav.addEventListener("click", (e) => { if (e.target.closest("a[data-view]") && isNarrow()) openNav(false); });

/** Below 900 px: the range, a fixed window, live or paused, Share, the theme and the key, in one sheet. */
function moreSheet() {
  const r = timeRange();
  const ranges = seg(RANGES.map(([k, m]) => [m, k, `The last ${k.replace(/(\d)([a-z])/, "$1 $2")}`]), r.fixed ? "none" : state.range, (v) => { m.close(); setRange(v); }, { "aria-label": "Time range", class: "seg more-ranges" });
  const row = (ic, label, sub, onclick) => h("button", { class: "more-row", onclick: () => { m.close(); onclick(); } }, icon(ic), h("span", { class: "grow" }, label, sub ? h("small", {}, sub) : null), icon("right", "chev"));
  const w = state.whoami;
  // A view with no time axis (range: false) has no range to pick.
  const timed = !top.classList.contains("no-range");
  const m = modal({
    title: timed ? "Time and sharing" : "Sharing and settings",
    body: [
      timed ? h("div", { class: "small dim" }, "Every view shows this window. ", h("b", {}, r.fixed ? `Fixed: ${span(r.from, r.to)}` : `The last ${r.label.replace(/^last /, "")}, following now`), ".") : null,
      timed ? ranges : null,
      timed ? row("clock", "Pick a fixed window", "dates and times, or overnight", pickWindow) : null,
      timed && state.zoom ? row("zout", "Reset the zoom", null, () => setZoom(null)) : null,
      row(state.paused ? "play" : "pause", state.paused ? "Go live" : "Pause", state.paused ? "follow now again" : "nothing redraws while paused", () => setPaused(!state.paused)),
      row("share", "Share a link to this view", timed ? "with this window as fixed times; it opens paused" : null, share),
      row("sparkle", "Ask about your data", "a question in words, answered from the tables", () => setTimeout(() => openAsk(), 250)),
      row("feedback", "Send feedback", "mark up a picture of this view", () => setTimeout(() => openFeedback(), 250)),
      h("hr"),
      row(isDark() ? "sun" : "moon", isDark() ? "Light theme" : "Dark theme", null, toggleTheme),
      row("user", w ? `Signed in as ${w.name}` : hasToken() ? "The key was refused" : "Signed out", w ? `${w.tables ? "limited to " + keyLabel(w) : w.scope + " key"} · use another one` : "use a key or the server's token", askToken),
    ],
    foot: [h("button", { class: "btn", onclick: () => m.close() }, "Close")],
  });
  m.el.classList.add("more-sheet");
}

function drawRange() {
  const r = timeRange();
  rangeSeg.set(r.fixed ? "none" : state.range);
  absBtn.textContent = r.fixed ? span(r.from, r.to) : span(r.from, r.to);
  absBtn.title = `${whenTitle(r.from)} to ${whenTitle(r.to)}${r.fixed ? ", fixed" : ", following now"}. Click to pick another window`;
  absBtn.classList.toggle("fixed", r.fixed);
  zoomChip.classList.toggle("hidden", !state.zoom);
  moreBtn.querySelector(".lbl").textContent = r.fixed ? "Fixed" : (RANGES.find(([, m]) => m === state.range)?.[0] || r.label.replace(/^last /, "")) + (state.zoom ? " · zoomed" : "");
  moreBtn.classList.toggle("fixed", r.fixed);
}
bus.on("range", drawRange);
bus.on("paused", drawRange);
setInterval(() => { if (!state.paused && state.from == null) drawRange(); }, 30000);

/** The status button: green and "Live" only while the data is fresh and the server answers. */
function drawLive() {
  const c = connection();
  const f = fresh("series"), fh = fresh("health");
  let cls, ic, text, title;
  if (!c.ok) { cls = "down"; ic = "bad"; text = "Not answering"; title = `The server has not answered since ${when(c.since_ms)}: ${c.message}. Click to pause`; }
  else if (signInNeeded()) { cls = "stale"; ic = "lock"; text = "Signed out"; title = hasToken() ? "The server refused this key: nothing is asked of it until you enter another (click to sign in)" : "This server needs a key: nothing is asked of it until you enter one (click to sign in)"; }
  else if (state.paused) { cls = "paused"; ic = "play"; text = state.from != null ? "Fixed window" : `Paused at ${when(state.pausedAt)}`; title = "Nothing redraws while paused. Click to go live (p)"; }
  else if ((!f.ok && !f.loading) || (!fh.ok && !fh.loading)) { cls = "stale"; ic = "clock"; text = "Stale"; title = `No fresh data: ${(!f.ok ? f.error : fh.error) || "the last answers are old"}`; }
  else if (isIdle()) { cls = "ok"; ic = null; text = "Live"; title = `Nobody has used this page for half a minute, so it asks the server ${IDLE_SLOW} times less often until you do. Click to pause (p)`; }
  else { cls = "ok"; ic = null; text = "Live"; title = "Live: the data follows now. Click to pause (p)"; }
  const every = state.series.source === "load" ? "1 min" : (state.interval * (isIdle() ? IDLE_SLOW : 1) / 1000) + " s";
  clear(liveBtn, cls === "ok" ? h("span", { class: "dot ok" }) : icon(ic), h("span", { class: "lt" }, text), cls === "ok" ? h("span", { class: "fresh" }, "· " + every) : null);
  liveBtn.setAttribute("aria-label", text);
  liveBtn.className = "btn sm live " + cls;
  liveBtn.title = title;
}
let downShown = false;
function drawDown() {
  const c = connection();
  // A single lost answer does not show the bar: it waits for the next poll to fail too.
  const missed = !c.ok && (Date.now() - c.since_ms > 4000 || !fresh("health").ok);
  if (missed !== downShown) { downShown = missed; downBar.classList.toggle("hidden", !missed); document.body.classList.toggle("not-answering", missed); bus.emit("answering", !missed); }
  if (!missed) return;
  const lastOk = c.last_ok_ms;
  clear(downBar, icon("bad"),
    h("span", { class: "grow" }, h("b", {}, `Not answering since ${when(c.since_ms)}`), ` (${c.message}). `,
      lastOk ? `What shows is from ${when(lastOk)} and greyed where it is old. ` : "Nothing has loaded yet. ",
      "Asking again every 5 s."),
    h("button", { class: "btn sm", onclick: () => { start(); toast("Asking again"); } }, icon("refresh"), "Ask now"));
}
for (const ev of ["connection", "paused", "fresh", "health", "points", "range"]) bus.on(ev, () => { drawLive(); drawDown(); });
setInterval(() => { drawLive(); drawDown(); }, 1000);

function drawWho() {
  const w = state.whoami;
  whoBtn.querySelector("span").textContent = w ? `${w.name} · ${w.tables ? (w.preset || "tables") : w.scope}` : hasToken() ? "key refused" : "signed out";
  // A key limited to tables: its grants, which its widest scope alone would misstate.
  whoBtn.title = w?.tables ? `${w.name}: limited to ${keyLabel(w)}` : w ? `${w.name}: ${w.scope} key` : "";
}
/**
 * The crumb: the view's title (a view without a heading of its own), and
 * the cluster's name, which the sidebar shows too but a phone hides. When
 * the view's heading names it, the crumb shows only the cluster.
 */
function drawCrumbs(v = current) {
  if (!v) return;
  const title = currentEntry?.title || v.title;
  const name = clusterName() || location.host;
  clear(crumbs, icon(currentEntry?.icon || v.icon), h("span", { class: "ctitle" }, title), h("span", { class: "csep", "aria-hidden": "true" }, "·"), h("span", { class: "cname", title: `${name}${environment() ? " (" + environment() + ")" : ""}` }, name), v.sub ? h("span", { class: "sub" }, "· " + v.sub) : null);
  crumbs.title = `${title} on ${name}`;
}
function drawCluster() {
  const name = clusterName(), env = environment();
  const c = state.cluster;
  const coords = state.fleet?.coordinators?.length || null;
  brandName.textContent = connectionName || name || location.host;
  const cn = crumbs.querySelector(".cname");
  if (cn && cn.textContent !== (name || location.host)) drawCrumbs();
  const shards = c?.shards?.length;
  brandSub.textContent = [env || null, coords > 1 ? fmt.plural(coords, "coordinator") : shards != null ? fmt.plural(shards, "shard") : state.health && !isCoordinator() ? "one server" : null].filter(Boolean).join(" · ") || (state.health ? (isSandbox() ? "sandbox" : "environment not set") : "connecting…");
  brand.title = connectionName ? `${connectionName}: ${name || "a cluster"}${env ? " (" + env + ")" : ""}, through this hub (${location.host}), which adds its key to every call. Click for the other connections` : `${name || "This server"}${env ? " (" + env + ")" : ", environment not set"} at ${location.host}. Click for its other coordinators and the ones you used`;
  const prod = env != null && /^prod/i.test(env);
  const sandbox = isSandbox();
  envBadge.className = "env" + (env || sandbox ? "" : " hidden") + (prod ? " prod" : "") + (sandbox ? " sandbox" : "");
  envBadge.textContent = env ? (prod ? "PROD" : env.toUpperCase().slice(0, 12)) : sandbox ? "SANDBOX" : "";
  envBadge.title = env ? `Environment: ${env}${prod ? ". Changes here reach production." : ""}` : sandbox ? "A sandbox: no environment is set and this page is opened on this machine, so writes are on without asking for a reason first (a write in the workbench still shows the rows it changes before it runs). Start the server with --environment (production, staging…) to treat it as one." : "";
  top.classList.toggle("prod", prod);
  document.title = `${currentEntry?.title || "ChloeDB"} · ${name || "ChloeDB"}${prod ? " · PROD" : ""}`;
  if (name || env) remember(location.origin, name, env);
}
bus.on("whoami", () => { drawWho(); drawCluster(); });
for (const ev of ["cluster", "fleet", "health"]) bus.on(ev, drawCluster);

// ---------------------------------------------------------------- a window, picked
function pickWindow() {
  const r = timeRange();
  const loc = (ms) => new Date(ms - new Date(ms).getTimezoneOffset() * 60e3).toISOString().slice(0, 16);
  const a = h("input", { class: "input", type: "datetime-local", value: loc(r.from) });
  const b = h("input", { class: "input", type: "datetime-local", value: loc(r.to) });
  const ok = h("button", { class: "btn primary" }, "Show this window");
  const presets = h("div", { class: "row", style: { gap: "6px" } },
    ...[["Overnight, 18:00 to 08:00", overnight],
      ["Yesterday", () => { const d = new Date(); d.setHours(0, 0, 0, 0); return [d.getTime() - 86400e3, d.getTime()]; }],
      ["Today so far", () => { const d = new Date(); d.setHours(0, 0, 0, 0); return [d.getTime(), Date.now()]; }]]
      .map(([label, f]) => h("button", { class: "btn sm", onclick: () => { const [x, y] = f(); a.value = loc(x); b.value = loc(y); } }, label)));
  let zone = "local"; try { zone = Intl.DateTimeFormat().resolvedOptions().timeZone; } catch { /* old browser */ }
  const m = modal({
    title: "Show a fixed window",
    body: [h("p", { class: "dim", style: { margin: 0 } }, `Every view shows this window, paused, and a shared link opens it. Times are this browser's (${zone}). Over an hour, charts show a point a minute.`),
      presets, h("div", { class: "row nw" }, h("label", { class: "f grow" }, "From", a), h("label", { class: "f grow" }, "To", b))],
    foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), state.from != null ? h("button", { class: "btn", onclick: () => { m.close(); setRange(state.range); } }, "Back to live") : null, ok],
  });
  ok.onclick = () => {
    const x = new Date(a.value).getTime(), y = new Date(b.value).getTime();
    if (!(x < y)) { toast("The window ends before it starts", "bad"); return; }
    m.close(); setWindow(x, Math.min(y, Date.now()));
  };
}

// ---------------------------------------------------------------- share
/** A link to this view with its params and the window as absolute times: it opens paused. */
export function shareLink() {
  const r = parse();
  const p = new URLSearchParams(r.params);
  p.delete("range"); p.delete("from"); p.delete("to");
  if (!top.classList.contains("no-range")) { const { from, to } = rangeParams({ absolute: true }); p.set("from", from); p.set("to", to); }
  return `${location.origin}${location.pathname}#/${[r.id, ...r.rest].map(encodeURIComponent).join("/")}?${p}`;
}
function share() {
  const r = timeRange();
  copy(shareLink(), top.classList.contains("no-range") ? `Link copied: ${currentEntry?.title || "this view"}. It holds no key.` : `Link copied: ${currentEntry?.title || "this view"}, ${span(r.from, r.to)}, opens paused. It holds no key.`);
}

// ---------------------------------------------------------------- router
let current = null, cleanup = null, currentEntry = null;
function parse() {
  const raw = location.hash.replace(/^#\/?/, "");
  const [path, q] = raw.split("?");
  const [id, ...rest] = path.split("/").map((x) => { try { return decodeURIComponent(x); } catch { return x; } });
  return { id: id || "home", rest, params: new URLSearchParams(q || "") };
}
function resolve(r) {
  let id = r.id;
  if (REDIRECTS[id]) {
    id = REDIRECTS[id];
    const q = r.params.toString();
    try { history.replaceState(null, "", location.pathname + location.search + "#/" + [id, ...r.rest].map(encodeURIComponent).join("/") + (q ? "?" + q : "")); } catch { /* old browser */ }
    r.id = id;
  }
  if (byId[id]) return { v: byId[id], e: entryOf(id) };
  // A view's pages: #/alert/<id> belongs to Alerts and #/statement/<fp> to Queries until they have modules of their own.
  if (id === "alert" && byId.alerts) { if (r.rest[0] && !r.params.get("rule")) r.params.set("rule", r.rest[0]); return { v: byId.alerts, e: entryOf("alerts") }; }
  if (id === "statement" && byId.queries) { if (r.rest.length && !r.params.get("fingerprint")) { r.params.set("fingerprint", r.rest.join("/")); r.params.set("tab", "statements"); } return { v: byId.queries, e: entryOf("queries") }; }
  const e = ENTRIES.find((x) => x.id === id);
  if (e) return { v: viewOf(e), e };
  return { v: byId.home || VIEWS[0], e: entryOf("home"), unknown: r.id };
}
function markCurrent() {
  // A page under another (a table's details, a statement) lights its parent.
  const id = currentEntry?.parent || currentEntry?.id;
  for (const a of nav.querySelectorAll("a[data-view]")) { if (a.dataset.view === id) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current"); }
}
// The views that show each copy's numbers (they read the cluster's per-copy
// detail) and the ones that show alerts; the others poll less (SH-11).
const COPY_NUMBERS = new Set(["home", "cluster", "rollout", "system", "statement", "performance", "profile", "alerts", "autoscale"]);
const ALERTS_SHOWN = new Set(["home", "alerts", "timeline", "logs", "statement"]);
let routeSeq = 0;
async function route() {
  const seq = ++routeSeq;
  openNav(false);
  const r = parse();
  const { v, e, unknown } = resolve(r);
  rangeFromUrl(r.params);
  currentEntry = e || { id: v.id, title: v.title, icon: v.icon, parent: v.parent };
  if (v.id === "app" && r.rest[0]) { const a = listApps().find((x) => x.name === r.rest[0]); currentEntry = { id: "app/" + r.rest[0], title: a?.title || r.rest[0], icon: "box" }; }
  markCurrent();
  // A view with no time axis (the SQL workbench, Schema, Labs) hides the range and Live.
  top.classList.toggle("no-range", v.range === false);
  // What it needs polled: no series without a time axis; the cluster in
  // full only where copies' numbers show, the alerts every 5 s only where
  // they show (SH-11).
  setPolls({ cluster: v.cluster || (COPY_NUMBERS.has(v.id) ? "full" : "brief"), alerts: ALERTS_SHOWN.has(v.id) ? "full" : "slow", series: v.range !== false });
  moreBtn.querySelector("svg").replaceWith(icon(v.range === false ? "dots" : "clock"));
  if (unknown && unknown !== "home") toast(`There is no view "${unknown}": this is Home`);
  if (!v.mod) {
    // Its code is not here yet: the view before it goes, and a page says so if it takes a moment.
    try { cleanup?.(); } catch (err) { console.error(err); }
    cleanup = null; current = null;
    drawCrumbs(v);
    const wait = h("div", { class: "page" + (v.full ? " full" : "") }, h("div", { class: "muted small view-loading", role: "status" }, `Loading ${currentEntry?.title || v.title}…`));
    clear(content, wait);
    try { await loadView(v); }
    catch (err) {
      if (seq !== routeSeq) return;
      console.error(err);
      clear(wait, h("div", { class: "error" }, `${currentEntry?.title || v.title} did not load: ${err.message}. `, h("button", { class: "btn sm", onclick: () => location.reload() }, "Reload")));
      return;
    }
    if (seq !== routeSeq) return;
  }
  const m = v.mod;
  if (current === v && m.onParams) { m.onParams(r); return; }
  // An app's page: drawn again only when another app opens.
  if (current === v && v.id === "app" && route.app === r.rest[0]) return;
  route.app = v.id === "app" ? r.rest[0] : null;
  try { cleanup?.(); } catch (err) { console.error(err); }
  current = v;
  drawCrumbs(v);
  drawCluster();
  const page = h("div", { class: "page" + (v.full ? " full" : "") });
  if (v.full) page.style.cssText = "padding:0;max-width:none";
  clear(content, page);
  content.scrollTop = 0;
  try { cleanup = m.render(page, r) || null; } catch (err) { console.error(err); page.append(h("div", { class: "error" }, "This view failed: " + err.message)); }
  // A view that names itself in its own heading leaves the crumb to the cluster (SH-12).
  const titled = () => !!page.querySelector(".page-head h1");
  crumbs.classList.toggle("titled", titled());
  setTimeout(() => { if (current === v) crumbs.classList.toggle("titled", titled()); }, 800);
  if (cleanup && typeof cleanup === "object" && typeof cleanup.stop === "function") { const o = cleanup; cleanup = () => o.stop(); }
  if (typeof cleanup !== "function") cleanup = null;
}
addEventListener("hashchange", route);

// ---------------------------------------------------------------- the key
function askToken() {
  const inp = h("input", { class: "input", type: "password", placeholder: "sk_… or the server's --token", autocomplete: "off", style: { width: "100%" } });
  const save = h("button", { class: "btn primary" }, "Use it");
  const r = parse();
  const shared = +r.params.get("from") > 0 && +r.params.get("to") > 0;
  const what = `${resolve(parse()).e?.title || "a view"}${shared ? `, ${span(+r.params.get("from"), +r.params.get("to"))}` : ""}`;
  const m = modal({
    title: shared ? "Sign in to see what was shared" : "Sign in to this server",
    body: [
      shared ? h("p", { style: { margin: 0 } }, "This link opens ", h("b", {}, what), ` on ${clusterName() || location.host}. A link never holds a key: use your own.`) : null,
      h("p", { class: "dim", style: { margin: 0 } }, "Paste an API key (feed, read, write or admin) or the server's token. It is kept for this tab only, and sent as a bearer token."), inp],
    foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), save],
  });
  const said = h("div", { class: "small", role: "status", "aria-live": "polite" });
  // The key is asked about before it is kept: a key the server refuses is
  // said so here, and the dialog stays open.
  const ok = async () => {
    const t = inp.value.trim();
    if (!t || save.disabled) return;
    save.disabled = true; said.className = "small muted"; said.textContent = "Checking the key…";
    let w = null;
    try {
      const r = await fetch(at("/v1/whoami"), { headers: { authorization: "Bearer " + t, "x-strata-client": "command-center" } });
      if (r.status === 401 || r.status === 403) { save.disabled = false; said.className = "small bad"; said.textContent = "The server refused this key: check that it is whole, and that it was not revoked or expired."; inp.select(); return; }
      if (r.ok) w = await r.json().catch(() => null);
    } catch { /* not answering: kept, and the page says so */ }
    setToken(t); m.close(); refreshWho();
    toast(w?.name ? `Signed in as ${w.name} (${w.tables ? keyLabel(w) : w.scope})` : "Key kept for this tab", "ok");
  };
  save.onclick = ok;
  inp.onkeydown = (e) => { if (e.key === "Enter") ok(); };
  inp.oninput = () => { said.textContent = ""; };
  m.el.querySelector(".bd").append(said);
}
// Asked once for a key the server refuses (or no key), until another key
// is given: Cancel is an answer. A page's own "Enter a key" button always asks.
let asked = false;
bus.on("token", () => { asked = false; });
bus.on("unauthorized", (path) => {
  if (document.querySelector(".modal")) return;
  if (typeof path === "string" && asked) return;
  asked = true;
  askToken();
});

// ---------------------------------------------------------------- palette
const go = (hash) => () => { location.hash = hash; };
const ACTIONS = [
  { name: "Share a link to this view and window", icon: "share", run: () => share(), hint: "s" },
  { name: "Pause or resume the live view", icon: "pause", run: () => setPaused(!state.paused), hint: "p" },
  { name: "Ask a question about your data, in words", icon: "sparkle", run: () => setTimeout(() => openAsk(), 50), hint: "Ctrl J" },
  { name: "Send feedback: mark up a picture of this view", icon: "feedback", run: () => setTimeout(() => openFeedback(), 50), hint: "f" },
  ...RANGES.map(([k, m], i) => ({ name: `Show the last ${k.replace(/(\d)([a-z])/, "$1 $2")}`, icon: "clock", run: () => setRange(m), hint: String(i + 1) })),
  { name: "Show a fixed window (dates and times)", icon: "clock", run: () => pickWindow() },
  { name: "What happened overnight (Timeline, 18:00 to 08:00)", icon: "timeline", run: () => { const [a, b] = overnight(); setWindow(a, b); location.hash = link("timeline"); } },
  { name: "Add a note to the timeline", icon: "note", run: go("#/timeline?note=1"), scope: "admin", route: "POST /v1/events/notes" },
  { name: "Toggle light or dark", icon: "moon", run: () => toggleTheme(), hint: "t" },
  { name: "Collapse or expand the sidebar", icon: "menu", run: () => toggleSide(), hint: "[" },
  { name: "Open another coordinator", icon: "node", run: () => askCoordinator() },
  { name: "Use another API key or token", icon: "access", run: () => askToken() },
  { name: "Create an API key", icon: "plus", run: go("#/access?tab=keys&new=1"), scope: "admin" },
  { name: "Open the raw Prometheus metrics", icon: "ext", run: () => window.open(at("/metrics"), "_blank") },
  { name: "Keyboard shortcuts", icon: "keyboard", run: () => shortcuts(), hint: "?" },
];
export function addActions(items) { ACTIONS.push(...items); }

// A substring scores by where it starts (a word start more); letters in order
// ("sqlw" for SQL workbench) only count in a short name and close together,
// so a query does not drag in every long sentence that happens to hold them.
function score(q, text, fuzzy = true) {
  if (!q) return 1;
  q = q.toLowerCase(); const t = String(text).toLowerCase();
  const i = t.indexOf(q);
  if (i >= 0) return 100 - Math.min(i, 60) + (i === 0 || /[\s\-/:_.]/.test(t[i - 1]) ? 20 : 0);
  if (!fuzzy || q.length < 3 || t.length > 40) return 0;
  let j = 0, first = -1;
  for (let k = 0; k < t.length; k++) {
    if (t[k] !== q[j]) continue;
    if (first < 0) { if (k > 0 && !/[\s\-/:_.]/.test(t[k - 1])) continue; first = k; }
    if (++j === q.length) return k - first + 1 <= q.length * 2 + 4 ? 30 + q.length * 2 : 0;
  }
  return 0;
}
function mark(text, q) {
  const i = q ? text.toLowerCase().indexOf(q.toLowerCase()) : -1;
  return i < 0 ? text : [text.slice(0, i), h("mark", {}, text.slice(i, i + q.length)), text.slice(i + q.length)];
}

// What the palette finds besides the cluster: read when it opens, kept a minute.
const found = { at: 0, tables: [], statements: [], keys: [], functions: [] };
async function loadFinds() {
  if (Date.now() - found.at < 60e3) return;
  found.at = Date.now();
  const [t, st, k, fn] = await Promise.allSettled([
    api("/v1/tables", { timeout: 5000 }),
    api("/v1/queries/stats?limit=200", { timeout: 5000 }),
    can("admin") ? api("/v1/admin/keys", { timeout: 5000 }) : Promise.reject(new Error("not admin")),
    api("/v1/functions", { timeout: 5000 })]);
  if (fn.status === "fulfilled") found.functions = (fn.value.functions || []).filter((f) => f.current != null);
  if (t.status === "fulfilled") found.tables = (Array.isArray(t.value) ? t.value : t.value.tables || []).map((x) => typeof x === "string" ? { name: x } : { name: x.name || x.table, docs: x.docs ?? x.rows });
  if (st.status === "fulfilled") found.statements = Array.isArray(st.value) ? st.value : st.value.statements || [];
  found.keys = k.status === "fulfilled" ? (k.value.keys || []).filter((x) => !x.revoked_ms) : [];
  // Keys the statements name (a read key sees them too, without the key list).
  const seen = new Set(found.keys.map((x) => x.name));
  const calls = new Map();
  for (const st of found.statements) for (const x of st.keys || []) if (x.key) calls.set(x.key, (calls.get(x.key) || 0) + (x.calls || 0));
  for (const [name, n] of calls) if (!seen.has(name)) found.keys.push({ name, id: name, calls: n, fromStats: true });
}
const windowText = () => { const r = timeRange(); return span(r.from, r.to); };

/** Entities that match `q`, each with what to do about it. */
function entities(q) {
  const out = [];
  const ql = q.toLowerCase().trim();
  if (!ql) return out;
  const shards = state.cluster?.shards || [];
  // A shard: "117", "shard 117", "s117".
  const num = ql.match(/^(?:shard\s*|s)?(\d{1,4})$/);
  if (num) {
    const n = +num[1];
    const i = shards.findIndex((sh, k) => (sh.shard ?? k) === n);
    if (i >= 0) {
      const sh = shards[i];
      const reps = sh.replicas || [];
      const prim = reps.find((r) => r.role === "primary") || reps[0];
      const L = prim?.detail?.load || {};
      const st = sh.state || sh.status;
      const errs = sh.call_rate != null && sh.calls ? null : null;
      out.push({ grp: "Shard", sc: 300, icon: "shard", name: `Shard ${n}`, sub: [st && st !== "ok" ? st.replace("_", " ") : null, `${reps.filter((r) => r.status === "ok").length} of ${reps.length} copies answer`, L.p99_ms != null ? `p99 ${fmt.ms(L.p99_ms)}` : null, L.errors_s ? `${fmt.si(L.errors_s)} errors/s` : errs, prim?.version?.version ? "on " + prim.version.version : null].filter(Boolean).join(" · "), hint: "Cluster", run: go(link("cluster", { shard: n })), acts: shardActs(n, sh) });
    }
  }
  // Copies: by short name, node name, URL or port.
  shards.forEach((sh, i) => (sh.replicas || []).forEach((r, j) => {
    const nm = copyName(r.url);
    const s = sh.shard ?? i;
    const hay = [nm, r.short, `s${s}-${j}`, r.url, r.node, r.name].filter(Boolean).join(" ");
    // A bare number names a shard or a port, not every address that holds the digit.
    let sc = /^\d+$/.test(ql) ? 0 : score(ql, hay);
    if (num && s === +num[1]) sc = 250 - j;
    if (/^\d{2,5}$/.test(ql) && r.url && new RegExp(":" + ql + "(\\b|$)").test(r.url)) sc = 280;
    if (sc <= 0) return;
    out.push({ grp: "Copies", sc: Math.min(sc, 280), icon: "db", name: nm, sub: [r.role === "unknown" || (r.status && r.status !== "ok") ? "not answering" : r.role, r.zone ? "zone " + r.zone : null, r.node || r.name || null, hostOf(r.url)].filter((x, k, a) => x && a.indexOf(x) === k).join(" · "), title: copyTitle(r.url), hint: "Cluster", run: go(link("cluster", { shard: s, copy: nm })), acts: copyActs(nm, r, s) });
  }));
  // Nodes and zones.
  const nodes = new Map(), zones = new Map();
  shards.forEach((sh) => (sh.replicas || []).forEach((r) => { if (r.node) nodes.set(r.node, (nodes.get(r.node) || 0) + 1); if (r.zone) zones.set(r.zone, (zones.get(r.zone) || 0) + 1); }));
  for (const [n, k] of nodes) { const sc = score(ql, n); if (sc > 0) out.push({ grp: "Nodes", sc, icon: "node", name: n, sub: fmt.plural(k, "copy", "copies"), hint: "Cluster", run: go(link("cluster", { node: n, group: "node" })), acts: [{ name: `Logs of node ${n}`, icon: "logs", hint: "Logs", run: go(logsLink({ server: n })) }, { name: `Its copies in Cluster`, icon: "cluster", hint: "Cluster", run: go(link("cluster", { node: n, group: "node" })) }] }); }
  for (const [z, k] of zones) { const sc = score(ql, "zone " + z); if (sc > 0) out.push({ grp: "Nodes", sc: sc - 5, icon: "cloud", name: "Zone " + z, sub: fmt.plural(k, "copy", "copies"), hint: "Cluster", run: go(link("cluster", { zone: z, group: "zone" })) }); }
  // Alerts.
  const fa = firingAlerts();
  for (const r of state.alerts?.rules || []) {
    const sc = score(ql, `${r.name} ${r.id} ${r.metric || ""} ${r.description || ""}`);
    if (sc <= 0) continue;
    const f = fa.find((a) => a.id === r.id);
    const st = f ? `firing for ${fmt.dur((Date.now() - (f.since_ms || Date.now())) / 1000)}${f.silenced ? ", silenced" : ""}` : (r.status?.state || state.alerts.states?.find((x) => x.rule === r.id)?.state || "ok");
    out.push({ grp: "Alerts", sc: sc + (f ? 30 : 0), icon: "alerts", name: r.name, sub: st, hint: "Alerts", run: go(link("alert/" + encodeURIComponent(r.id))) });
  }
  // Tables.
  for (const t of found.tables) {
    const sc = score(ql, t.name); if (sc <= 0) continue;
    out.push({ grp: "Tables", sc, icon: "table", name: t.name, sub: t.docs != null ? fmt.si(t.docs) + " rows" : "", hint: "Tables", run: go(link(byId.tables ? "tables" : "inspector", { table: t.name })),
      acts: [{ name: `${t.name} in Tables and storage`, icon: "table", hint: "Tables", run: go(link(byId.tables ? "tables" : "inspector", { table: t.name })) }, { name: `Query ${t.name} in the workbench`, icon: "sql", hint: "SQL", run: go(link("sql", { table: t.name })) }, { name: `Its schema`, icon: "schema", hint: "Schema", run: go(link("schema", { table: t.name })) }] });
  }
  // Statements.
  if (ql.length >= 3) for (const st of found.statements) {
    const sql = st.sql || st.example || st.fingerprint || "";
    const sc = score(ql, sql + " " + (st.fingerprint || "")); if (sc <= 0) continue;
    out.push({ grp: "Statements", sc: sc - 10, icon: "queries", name: sql.length > 90 ? sql.slice(0, 88) + "…" : sql, sub: [fmt.plural(st.calls || 0, "call"), st.p99_ms != null ? "p99 " + fmt.ms(st.p99_ms) : null].filter(Boolean).join(" · "), mono: true, hint: "Statement", run: go(link("statement/" + encodeURIComponent(st.fingerprint || sql))) });
  }
  // Keys.
  for (const k of found.keys) {
    const sc = score(ql, `${k.name} ${k.id} ${k.owner || ""}`); if (sc <= 0) continue;
    if (k.fromStats) {
      out.push({ grp: "Keys", sc: sc - 5, icon: "key", name: k.name, sub: `${fmt.plural(k.calls, "call")} in the last 15 min (from the statements)`, hint: "Queries", run: go(link("queries", { key: k.name })),
        acts: [{ name: `Queries from key ${k.name}`, icon: "queries", hint: "Queries", run: go(link("queries", { key: k.name })) }, { name: `Statements of key ${k.name}`, icon: "queries", hint: "Queries", run: go(link("queries", { tab: "statements", key: k.name })) }] });
      continue;
    }
    out.push({ grp: "Keys", sc: sc - 5, icon: "key", name: k.name, sub: [k.scope, k.owner ? "owner " + k.owner : null, k.last_used_ms ? "used " + fmt.ago(k.last_used_ms) : "never used"].filter(Boolean).join(" · "), hint: "Access", run: go(link("access", { tab: "keys", key: k.id })),
      acts: [{ name: `Queries from key ${k.name}`, icon: "queries", hint: "Queries", run: go(link("queries", { key: k.name })) }, { name: `The key in Access and audit`, icon: "access", hint: "Access", run: go(link("access", { tab: "keys", key: k.id })) }] });
  }
  // Saved searches (stored functions): by name and what they are for.
  for (const f of found.functions) {
    const cur = (f.versions || []).find((v) => v.version === f.current) || {};
    const sc = score(ql, `${f.name} ${cur.description || f.description || ""}`); if (sc <= 0) continue;
    out.push({ grp: "Saved searches", sc, icon: "bookmark", name: f.name, sub: cur.description || f.description || (cur.kind === "table" ? "a saved search" : "a stored function"), hint: "Saved", run: go(link("saved/" + encodeURIComponent(f.name))) });
  }
  // A request id.
  if (/^[0-9a-f]{12,32}$/i.test(ql)) out.push({ grp: "Requests", sc: 260, icon: "logs", name: `Request ${ql}`, sub: "its log lines and its query", hint: "Logs", run: go(logsLink({ request: ql })) });
  return out;
}
function shardActs(n, sh) {
  const reps = sh.replicas || [];
  const prim = reps.find((r) => r.role === "primary") || reps[0];
  const pn = prim ? copyName(prim.url) : null;
  const w = windowText();
  return [
    { name: "Its log lines, errors first", icon: "logs", hint: "Logs", run: go(logsLink({ shard: n, level: "warn" })) },
    { name: "Its slowest queries", icon: "queries", hint: "Queries", run: go(link("queries", { shard: n, tab: "statements" })) },
    prim ? { name: `Profile ${pn} for 30 s`, icon: "flame", hint: "Profiler", run: go(link("profile", { tab: "cpu", node: prim.url })) } : null,
    { name: "Its events on the timeline", icon: "timeline", hint: "Timeline", run: go(link("timeline", { shard: n })) },
    { name: `Copy a link to Cluster · shard ${n} · ${w}`, icon: "share", hint: "Share", run: () => copy(location.origin + location.pathname + link("cluster", { shard: n, ...rangeParams({ absolute: true }) }), `Link copied: shard ${n}, ${w}`) },
  ].filter(Boolean);
}
function copyActs(nm, r, shard) {
  return [
    { name: `Logs of ${nm}`, icon: "logs", hint: "Logs", run: go(logsLink({ server: r.url })) },
    { name: `Profile ${nm} for 30 s`, icon: "flame", hint: "Profiler", run: go(link("profile", { tab: "cpu", node: r.url })) },
    { name: `Shard ${shard} in Cluster`, icon: "cluster", hint: "Cluster", run: go(link("cluster", { shard, copy: nm })) },
    { name: `Copy its address (${r.url})`, icon: "copy", hint: "Copy", run: () => copy(r.url, "Address copied") },
  ];
}

function openPalette(initial = "") {
  if (document.querySelector(".palette")) return;
  const inp = h("input", { placeholder: isCoordinator() ? "Shard 117, s9-2, a node, a table, an alert, a key, a view…" : "A table, a saved search, a view, an alert, a key…", "aria-label": "Search", autocomplete: "off", spellcheck: "false", value: initial });
  const list = h("div", { class: "list", role: "listbox" });
  const closeBtn = h("button", { class: "btn ghost pal-close", type: "button" }, "Cancel");
  const pal = h("div", { class: "palette", role: "dialog", "aria-label": "Command palette" }, h("div", { class: "pal-in" }, icon("search"), inp, closeBtn), list,
    h("div", { class: "foot" }, h("span", {}, h("kbd", {}, "↑"), " ", h("kbd", {}, "↓"), " to move"), h("span", {}, h("kbd", {}, "Enter"), " to open"), h("span", {}, h("kbd", {}, "Tab"), " for its actions"), h("span", {}, h("kbd", {}, "Esc"), " to close"),
      h("span", { class: "sep" }), h("span", {}, isCoordinator() ? "finds shards, copies, nodes, tables, saved searches, statements, alerts, keys, request ids" : "finds tables, saved searches, statements, alerts, keys, request ids")));
  const scrim = h("div", { class: "scrim" });
  document.body.append(scrim, pal);
  loadFinds().then(() => { if (pal.isConnected) draw(); }).catch(() => {});
  let items = [], sel = 0, focus = null; // focus: the entity whose actions show
  const close = () => { pal.remove(); scrim.remove(); };
  scrim.onclick = close; closeBtn.onclick = close;
  function draw() {
    const q = inp.value.trim();
    if (focus) items = focus.acts.map((a) => ({ ...a, grp: focus.name }));
    else {
      const views = ENTRIES.map((e) => { const v = viewOf(e); return { name: e.title, icon: e.icon || v.icon, grp: "Views", hint: e.group + (e.key ? ` · g ${e.key}` : ""), run: go(link(e.id)), sc: Math.max(score(q, e.title), score(q, (v.keywords || "") + " " + e.id, false) * 0.8) * (q ? 0.9 : 1) }; });
      const acts = ACTIONS.filter((a) => !a.scope || can(a.scope, a.route)).map((a) => ({ ...a, grp: "Actions", sc: score(q, a.name, false) * 0.8 }));
      const ents = entities(q).sort((a, b) => b.sc - a.sc);
      // The best match's actions come right after it ("about shard 117").
      const best = ents[0];
      const about = best && best.acts && best.sc >= 250 ? best.acts.map((a) => ({ ...a, grp: `About ${best.name.replace(/^Shard/, "shard")}, ${windowText()}`, sc: best.sc - 1 })) : [];
      const order = ["Shard", "Copies", "Requests", "About", "Nodes", "Alerts", "Tables", "Statements", "Keys", "Views", "Actions"];
      const rank = (g) => { const i = order.findIndex((o) => g.startsWith(o)); return i < 0 ? 99 : i; };
      const all = [...ents, ...about, ...views, ...acts].filter((x) => x.sc > 0);
      if (q) {
        const byG = new Map();
        for (const x of all.sort((a, b) => b.sc - a.sc)) { if (!byG.has(x.grp)) byG.set(x.grp, []); if (byG.get(x.grp).length < (x.grp === "Copies" ? 4 : 6)) byG.get(x.grp).push(x); }
        const top = (v) => Math.max(...v.map((x) => x.sc));
        items = [...byG.entries()].sort((a, b) => {
          const strong = top(a[1]) >= 250 || top(b[1]) >= 250;
          return strong ? rank(a[0]) - rank(b[0]) : (top(b[1]) - top(a[1])) || rank(a[0]) - rank(b[0]);
        }).flatMap(([, v]) => v);
      } else items = all;
    }
    if (sel >= items.length) sel = Math.max(0, items.length - 1);
    clear(list);
    let grp = null;
    if (focus) list.append(h("div", { class: "grp row nw" }, h("button", { class: "btn xs ghost", onclick: () => { focus = null; sel = 0; draw(); inp.focus(); } }, icon("left"), "Back"), "What to do: " + focus.name));
    items.forEach((it, i) => {
      if (it.grp !== grp && !focus) { grp = it.grp; list.append(h("div", { class: "grp" }, grp)); }
      const row = h("div", { class: "it" + (i === sel ? " on" : ""), role: "option", "aria-selected": String(i === sel), title: it.title || null },
        icon(it.icon), h("span", { class: "nm" + (it.mono ? " mono" : "") }, mark(it.name, focus ? "" : q), it.sub ? h("span", { class: "s" }, " · " + it.sub) : null),
        it.acts && !focus ? h("button", { class: "h acts", type: "button", title: "What to do about it (Tab)", onclick: (ev) => { ev.stopPropagation(); focus = it; sel = 0; draw(); } }, `${it.acts.length} actions`, h("span", { class: "tabk" }, " ⇥")) : null,
        it.hint ? h("span", { class: "h" }, it.hint) : null);
      row.onclick = () => { close(); it.run(); };
      row.onmousemove = () => { if (sel === i) return; sel = i; list.querySelectorAll(".it").forEach((el, k) => { el.classList.toggle("on", k === i); el.setAttribute("aria-selected", String(k === i)); }); };
      list.append(row);
    });
    if (!items.length) list.append(h("div", { class: "empty" }, q ? `Nothing matches "${q}". ${isCoordinator() ? "Shards by number (117), copies by name (s9-2) or port, nodes, zones, " : ""}tables, saved searches, statements, alerts and keys are found here.` : "Nothing here"));
    list.querySelector(".on")?.scrollIntoView({ block: "nearest" });
  }
  inp.oninput = () => { sel = 0; focus = null; draw(); };
  inp.onkeydown = (e) => {
    if (e.key === "ArrowDown") { e.preventDefault(); sel = Math.min(items.length - 1, sel + 1); draw(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); sel = Math.max(0, sel - 1); draw(); }
    else if (e.key === "Tab") { e.preventDefault(); if (focus) { if (e.shiftKey) { focus = null; sel = 0; draw(); } } else { const it = items[sel]; if (it?.acts) { focus = it; sel = 0; draw(); } } }
    else if (e.key === "Enter") { e.preventDefault(); const it = items[sel]; if (it) { close(); it.run(); } }
    else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); if (focus) { focus = null; sel = 0; draw(); } else close(); }
  };
  draw();
  inp.focus();
}

// ---------------------------------------------------------------- shortcuts
const GKEYS = Object.fromEntries(ENTRIES.filter((e) => e.key).map((e) => [e.key, e]));
function shortcuts() {
  const rows = [
    ["Ctrl K, /", "Find a shard, copy, node, table, statement, alert, key, view or action"],
    ["Ctrl J", "Ask a question about your data, in words"],
    ...ENTRIES.filter((e) => e.key).map((e) => [`g then ${e.key}`, `${e.title} (${e.group})`]),
    ["1 2 3 4 5", "Show the last 15 min, 1 h, 6 h, 24 h, 7 days"],
    ["p", "Pause or go live"], ["s", "Share: copy a link to this view and window"], ["f", "Feedback: mark up a picture of this view and send it"], ["t", "Light or dark"], ["[", "Collapse or expand the sidebar"], ["?", "These shortcuts"], ["Esc", "Close a panel, a dialog or the palette"],
  ];
  modal({ title: "Keyboard shortcuts", wide: true, body: h("dl", { class: "kv shortcuts" }, rows.flatMap(([k, v]) => [h("dt", {}, k.split(", ").map((x, i) => [i ? " or " : "", ...x.split(" then ").map((y, j) => [j ? " then " : "", ...y.split(" ").map((z, m) => [m ? " " : "", h("kbd", {}, z)])])])), h("dd", {}, v)])) });
}

let gPending = 0;
document.addEventListener("keydown", (e) => {
  const typing = e.target.closest?.("input,textarea,select,[contenteditable=true]");
  if ((e.key === "k" || e.key === "K") && (e.ctrlKey || e.metaKey) && !document.querySelector(".fb-ed")) { e.preventDefault(); openPalette(); return; }
  if ((e.key === "j" || e.key === "J") && (e.ctrlKey || e.metaKey) && !document.querySelector(".fb-ed,.palette")) { e.preventDefault(); openAsk(); return; }
  // Feedback takes a drawer or a dialog too, so f works over them.
  // (g then f goes to the Feedback view: the g comes first.)
  const afterG = gPending && Date.now() - gPending < 1200;
  if (e.key === "f" && !afterG && !typing && !e.ctrlKey && !e.metaKey && !e.altKey && !document.querySelector(".palette,.fb-ed,.menu")) { e.preventDefault(); openFeedback(); return; }
  if (typing || e.ctrlKey || e.metaKey || e.altKey || document.querySelector(".modal,.palette,.drawer,.fb-ed")) return;
  if (gPending && Date.now() - gPending < 1200 && GKEYS[e.key]) { gPending = 0; location.hash = link(GKEYS[e.key].id); e.preventDefault(); return; }
  gPending = 0;
  if (e.key === "g") gPending = Date.now();
  else if (e.key === "/") { e.preventDefault(); openPalette(); }
  else if (e.key === "?") shortcuts();
  else if (e.key === "t") toggleTheme();
  else if (e.key === "p") setPaused(!state.paused);
  else if (e.key === "s") share();
  else if (e.key === "[") toggleSide();
  else if (/^[1-5]$/.test(e.key)) setRange(RANGES[+e.key - 1][1]);
});

// ---------------------------------------------------------------- start
drawNav(); drawRange(); drawLive();
// The first view's code loads while the server is asked who is signed in.
loadView(resolve(parse()).v).catch(() => { /* said when it opens */ });
const who = start();
// Who is signed in decides what the views offer: ask that first (briefly).
// start() asks it once (CCW-18: it was asked twice).
Promise.race([who, new Promise((r) => setTimeout(r, 2500))]).then(() => {
  // ONB-11: a sandbox (one server on this machine, no environment named)
  // opened at its bare address lands on Start here until its steps are done.
  if (!location.hash.replace(/^#\/?/, "") && isSandbox() && !isCoordinator() && !store.get("start.done", false)) { try { history.replaceState(null, "", location.pathname + location.search + "#/start"); } catch { /* old browser */ } }
  route(); drawBadges();
});
