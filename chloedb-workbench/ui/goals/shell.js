// The shell (JOURNEYS.md, "Global elements" and "Phone"): the top bar
// (brand, breadcrumb, Search, Needs you, Agents, More), the phone's tab bar,
// the Needs-you drawer, the agent switcher, the More menu, and the sheets
// every page shares: the grant sheet (a wide "don't ask again" answer,
// agent P's), the batch answer, the keys. One count of what needs you, one count of
// agents and one set of words for their groups, used on every page.
import { h, useStyle, avatar, icon, kc, label, btn, stateDot, since, questionCard, optionsOf, parseJSON, GROUP_WORDS, toMs } from "/levels/common.js";
import * as rules from "/goals/rules-suggest.js";

const $ = (id) => document.getElementById(id);
const enc = encodeURIComponent;
const FINISHED_DAYS = 3;
let A = null; // the app (app.js): api, agents, needs, projects, go, toast, …
const O = { open: null, crumbs: [], sel: new Set(), focus: null };

// ---------------------------------------------------------------- the fleet

/** The group of an agent, in the same words everywhere: needs, working, ready, finished. */
export function groupOf(a) {
  if (!a) return "finished";
  const need = A && A.needs().items.some((i) => i.agent && i.agent.id === a.id);
  if (a.running && (need || a.state === "waiting")) return "needs";
  const g = A && A.groups()[a.id];
  if (g) return g;
  if (a.running && (a.state === "working" || a.state === "starting")) return "working";
  if (a.running) return "ready";
  return "finished";
}

/** The agents Workbench counts (the top bar, the switcher, Continue): the
 *  newest of each name, running or ended in the last 3 days. */
export function fleet(repo) {
  const all = A ? A.agents() : [];
  const cut = Date.now() - FINISHED_DAYS * 86400000;
  const byName = new Map();
  for (const a of all) {
    if (repo && a.repo !== repo) continue;
    const k = `${a.repo}\u0001${a.name}`;
    const o = byName.get(k);
    if (!o || (toMs(a.created) || 0) > (toMs(o.created) || 0)) byName.set(k, a);
  }
  return [...byName.values()].filter((a) => a.running || (toMs(a.ended || a.state_since) || 0) > cut);
}

export const ORDER = ["needs", "working", "ready", "finished"];
export function counts(list) {
  const c = { needs: 0, working: 0, ready: 0, finished: 0 };
  for (const a of list) c[groupOf(a)]++;
  return c;
}
/** "5 agents: 1 needs you, 2 working, 1 ready, 1 finished". */
export function fleetWords(list) {
  const c = counts(list);
  const parts = ORDER.filter((g) => c[g]).map((g) => `${c[g]} ${g === "needs" ? "waiting on you" : GROUP_WORDS[g]}`);
  return `${list.length} agent${list.length === 1 ? "" : "s"}${parts.length ? ": " + parts.join(", ") : ""}`;
}

// ---------------------------------------------------------------- top bar

export function initShell(app) {
  A = app;
  useStyle("shell", CSS);
  $("search-btn").addEventListener("click", () => A.openSearch());
  $("tb-search").addEventListener("click", () => A.openSearch());
  $("needs-btn").addEventListener("click", () => toggle("needs"));
  $("agents-btn").addEventListener("click", () => toggle("agents"));
  $("more-btn").addEventListener("click", () => toggle("more"));
  $("ov").addEventListener("click", (e) => { if (e.target.classList.contains("ov-scrim")) close(); });
  $("ov").addEventListener("keydown", (e) => {
    if (O.open === "agents" && /^[1-9]$/.test(e.key) && !e.target.closest("input,textarea")) {
      const row = $("ov").querySelectorAll(".sw-row[data-n]")[Number(e.key) - 1];
      if (row) { e.preventDefault(); e.stopPropagation(); row.click(); }
    }
  });
  update();
}

export function setCrumbs(list) {
  O.crumbs = list || [];
  const nav = $("crumbs");
  nav.replaceChildren(...O.crumbs.flatMap((c, i) => [
    i ? h("span", { class: "sl" }, "/") : null,
    i === O.crumbs.length - 1 ? h("b", { "aria-current": "page" }, c.label) : h("a", { href: c.href }, c.label),
  ]).filter(Boolean));
  const last = O.crumbs[O.crumbs.length - 1];
  const parent = O.crumbs[O.crumbs.length - 2];
  $("tb-title").textContent = last ? last.label : "Workbench";
  const back = $("tb-back");
  back.hidden = !parent;
  if (parent) { back.href = parent.href; back.setAttribute("aria-label", `Back to ${parent.label}`); }
  document.title = `${A && A.needs().count ? `(${A.needs().count}) ` : ""}${last && O.crumbs.length > 1 ? last.label + " · " : ""}Workbench`;
}
export function crumbs() { return O.crumbs; }

/** The one count, on the button, the tab bar and the title; the agents' avatars. */
export function update() {
  if (!A) return;
  const n = A.needs().count;
  const nb = $("needs-btn");
  nb.classList.toggle("on", n > 0);
  $("needs-n").textContent = String(n);
  $("needs-lb").textContent = n ? "need you" : "Nothing needs you";
  $("needs-n").hidden = !n;
  nb.title = n ? `${n} need${n === 1 ? "s" : ""} you (.)` : "Nothing needs you (.)";
  $("tab-badge").textContent = n ? String(n) : "";
  const list = fleet();
  const running = list.filter((a) => a.running);
  $("agents-avs").replaceChildren(...running.slice(0, 4).map((a) => avatar(a, 16)));
  const c = counts(list);
  // One count of what needs you, on the Needs-you button only (UX critique, issue 4).
  $("agents-lb").textContent = `${list.length} agent${list.length === 1 ? "" : "s"}`;
  $("agents-btn").title = `${fleetWords(list)} (a)`;
  setCrumbs(O.crumbs);
  // An open drawer or switcher is drawn again only when what it lists
  // changed, so a card's pick and the focus stay put.
  const key = drawnKey(O.open);
  if (O.open && key !== O.drawn) {
    O.drawn = key;
    if (O.open === "needs") drawDrawer();
    else if (O.open === "agents") drawSwitcher();
  }
  // the tab bar: which goal the page is under
  const hsh = location.hash || "#/";
  const tab = /^#\/needs/.test(hsh) ? "needs" : /^#\/(p|a|debug|new)\b/.test(hsh) ? "projects" : /^#\/more/.test(hsh) ? "more" : "start";
  for (const a of document.querySelectorAll("#tabbar a")) { if (a.dataset.tab === tab) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current"); }
}

// ---------------------------------------------------------------- overlays

function drawnKey(which) {
  if (which === "needs") return A.needs().items.map((i) => i.id).join(",");
  if (which === "agents") return fleet().map((a) => `${a.id}${a.state}${groupOf(a)}`).join(",");
  return "";
}

export function isOpen() { return O.open; }
export function toggle(which) { if (O.open === which) close(); else open(which); }
export function open(which) {
  O.open = which;
  O.drawn = drawnKey(which);
  $("ov").hidden = false;
  for (const [id, w] of [["needs-btn", "needs"], ["agents-btn", "agents"], ["more-btn", "more"]]) $(id).setAttribute("aria-expanded", String(which === w));
  if (which === "needs") drawDrawer();
  else if (which === "agents") drawSwitcher();
  else if (which === "more") drawMore();
  requestAnimationFrame(() => {
    const first = $("ov").querySelector(which === "needs" ? ".qc, .alert-card, .wait-card" : ".sw-row, .more-row");
    if (first) first.focus({ preventScroll: true });
  });
}
export function close() {
  if (!O.open) return false;
  O.open = null;
  O.sel.clear();
  $("ov").hidden = true;
  $("ov").replaceChildren();
  for (const id of ["needs-btn", "agents-btn", "more-btn"]) $(id).setAttribute("aria-expanded", "false");
  return true;
}

function drawDrawer() {
  const N = A.needs();
  const items = N.items;
  const p = A.project();
  const body = h("div", { class: "dr-body" },
    items.length ? items.map((it) => needCard(it, { big: true })) : h("div", { class: "dr-empty" }, h("b", {}, "Nothing needs you"), "No agent is waiting on you, and no alert asks you. Agents still working are under Agents."));
  $("ov").replaceChildren(
    h("div", { class: "ov-scrim" }),
    h("aside", { class: "drawer", role: "dialog", "aria-label": "Needs you" },
      h("div", { class: "dr-head" }, h("h2", {}, items.length ? `${items.length} need${items.length === 1 ? "s" : ""} you` : "Needs you"), h("span", { class: "sp" }),
        btn("Inbox", { href: "#/needs", ghost: true, sm: true }), btn("", { icon: "x", ghost: true, sm: true, title: "Close (Esc)", onclick: close })),
      body,
      h("div", { class: "dr-foot" },
        h("span", {}, "Oldest first. ", kc("j"), kc("k"), " move, a digit then ", kc("↵"), " answers, ", kc("x"), " selects, ", kc("⇧1"), " answers the selected alike."),
        p ? h("a", { class: "lv-link", href: `#/p/${p.project}`, onclick: close }, "Everything since you left →") : null)));
}

function drawSwitcher() {
  const list = fleet();
  const grouped = ORDER.map((g) => [g, list.filter((a) => groupOf(a) === g).sort((x, y) => (toMs(y.state_since) || 0) - (toMs(x.state_since) || 0))]);
  let n = 0;
  const proj = A.projectNames();
  const row = (a, g) => {
    n++;
    const live = a.state === "waiting" ? a.waiting_for || "waits for you" : a.last_event || a.task || "";
    return h("div", { class: "sw-wrap" },
      h("a", { class: "sw-row", href: `#/a/${a.id}`, "data-n": n <= 9 ? String(n) : null, onclick: close },
        avatar(a, 20),
        h("span", { class: "nm" }, a.name),
        h("span", { class: "wh" }, stateDot(g), " ", proj[a.repo] || "", a.model ? ` · ${a.model}` : "", live ? ` · ${live}` : ""),
        n <= 9 ? kc(String(n)) : h("span")),
      g === "finished" ? h("span", { class: "sw-act" },
        btn("Restart with the same task", { sm: true, onclick: async () => { close(); await A.restart(a.id); } }),
        btn("Terminal", { sm: true, ghost: true, href: `#/a/${a.id}/terminal`, onclick: close })) : null);
  };
  $("ov").replaceChildren(
    h("div", { class: "ov-scrim clear" }),
    h("div", { class: "pop", role: "dialog", "aria-label": "Agents" },
      h("div", { class: "lbl sw-sum" }, h("span", {}, fleetWords(list))),
      list.length ? grouped.filter(([, l]) => l.length).map(([g, l]) => [label(GROUP_WORDS[g], h("span", { class: "num" }, String(l.length))), l.map((a) => row(a, g))])
        : h("div", { class: "none" }, "No agents yet. Start one from a project's Start a change."),
      h("div", { class: "foot" }, h("a", { href: "#/more/terminals", onclick: close }, "All terminals"), h("span", { class: "sp" }), h("span", {}, kc("1"), "…", kc("9"), " open"))));
}

export const MORE = [
  ["terminals", "grid", "All terminals", "Every running session side by side, the old home screen"],
  ["time", "clock", "Where the time went", "Waiting on you, thinking, tools and tests, per agent"],
  ["decisions", "branch", "Decisions timeline", "Every decision of the project, one lane per agent"],
  ["library", "book", "Library", "Docs, notes and code search"],
  ["rules", "check", "Rules and grants", "What agents are told, and what they may do without asking; Revoke"],
  ["settings", "gear", "Settings", "Open on the inbox, compact rows"],
  ["guide", "why", "Guide", "What each page is for"],
  ["keys", "kbd", "Keys", "The keyboard, all of it"],
  ["setup", "spark", "Setup check", "ChloeCtx, Claude Code, git"],
];
function drawMore() {
  $("ov").replaceChildren(
    h("div", { class: "ov-scrim clear" }),
    h("nav", { class: "pop", "aria-label": "More" }, MORE.map(([k, ic, t, sub]) =>
      h("a", { class: "more-row", href: `#/more/${k}`, onclick: close }, icon(ic), h("span", {}, t, h("small", {}, sub)), k === "terminals" ? kc("g t") : k === "keys" ? kc("?") : h("span")))));
}

// ---------------------------------------------------------------- cards

/** A card of what needs you: a question (the one question card), a trust
 *  prompt, an agent waiting with no question read yet, or an alert. */
export function needCard(it, { big = false } = {}) {
  if (it.kind === "alert") return alertCard(it);
  if (it.kind === "waiting") {
    return h("div", { class: "wait-card", tabindex: "0", "data-need": it.id },
      avatar(it.agent, 22),
      h("div", { class: "tx" }, h("b", {}, `${it.agent.name} waits for you`), h("span", {}, it.text || "Claude Code is showing a prompt; Workbench has not read it yet."), " ", h("span", { class: "num" }, since(it.since_ms))),
      btn("Open its terminal", { sm: true, href: `#/a/${it.agent.id}/terminal` }));
  }
  const q = it.question;
  const card = questionCard(q, {
    agent: it.agent, big,
    onAnswer: (i, o) => answer(it, i, o),
    onGrant: (i, o) => grantSheet(it, i, o),
    onText: q.kind === "ask" ? (t) => answerText(it, t) : null,
  });
  card.dataset.need = it.id;
  if (O.sel.has(it.id)) card.classList.add("picked");
  if (it.kind === "trust") card.prepend(h("div", { class: "qc-trust" }, "Claude Code asks whether to trust this folder. Workbench answers it for its own worktrees and for projects you trusted when you opened them."));
  return card;
}

function alertCard(it) {
  const i = it.incident || {};
  return h("div", { class: "alert-card", tabindex: "0", "data-need": it.id },
    h("div", { class: "hd" }, icon("warn", "i sm"), h("b", {}, `Alert · ${i.severity || "asks you"}`), h("span", { class: "t" }, since(it.since_ms))),
    h("div", { class: "ti" }, i.title || "An alert asks you"),
    i.summary || i.detail ? h("p", { class: "qc-what" }, i.summary || i.detail) : null,
    h("div", { class: "act" },
      btn("Open in Debug", { sm: true, href: `#/debug/failing?incident=${enc(i.id || "")}` }),
      btn("Resolve", { sm: true, onclick: async () => { try { await A.api(`/api/incidents/${enc(i.id)}/resolve`, { method: "POST", body: {} }); A.toast("Resolved"); A.refreshNeeds(); } catch (e) { A.toast(`Could not resolve it: ${e.message}`, true); } } })));
}

async function answer(it, i, o) {
  const q = it.question;
  try {
    await A.api(`/api/questions/${enc(q.id)}/answer`, { method: "POST", body: q.kind === "permission" ? { option: i, label: o.label } : { option: i } });
    A.toast(`Sent “${o.n} ${o.label}” to ${it.agent ? it.agent.name : "the agent"}`);
    A.answered(it.id);
  } catch (e) {
    A.toast(e.status === 409 ? `That prompt is gone: nothing was typed (${e.message})` : `Not sent: ${e.message}`, true);
    A.refreshNeeds();
  }
}

async function answerText(it, text) {
  try {
    await A.api(`/api/questions/${enc(it.question.id)}/answer`, { method: "POST", body: { text } });
    A.toast(`Answered ${it.agent ? it.agent.name : "the agent"}`);
    A.answered(it.id);
  } catch (e) { A.toast(`Not sent: ${e.message}`, true); }
}

// ---------------------------------------------------------------- sheets

/** A modal sheet: `{kicker, title, body, actions}`; Esc or the scrim closes it. */
export function sheet({ kicker, title, body, actions }) {
  const host = h("div", { class: "ov gs-ov" });
  const done = () => { host.remove(); };
  host.append(h("div", { class: "ov-scrim", onclick: done }),
    h("div", { class: "gsheet", role: "dialog", "aria-modal": "true", "aria-label": title },
      h("div", { class: "gs-hd" }, kicker ? label(kicker) : null, h("h2", {}, title)),
      h("div", { class: "gs-bd" }, body),
      actions ? h("div", { class: "gs-ft" }, actions(done)) : null));
  host.addEventListener("keydown", (e) => { if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); done(); } });
  document.body.append(host);
  requestAnimationFrame(() => { const b = host.querySelector(".gs-ft .btn:last-child, .gs-ft .btn"); if (b) b.focus(); });
  return { close: done, el: host };
}
export function sheetOpen() { return !!document.querySelector(".gs-ov"); }
export function closeSheet() { const s = document.querySelector(".gs-ov"); if (s) { s.remove(); return true; } return false; }

/** The grant sheet (UX critique, issue 2), agent P's (rules-suggest.js):
 *  before a wide answer is typed, the exact rule, the file it lands in,
 *  what else it would have allowed, and the narrower choice. It answers the
 *  question itself on Confirm; this says what was sent and clears the card. */
export async function grantSheet(it, i, o) {
  const ctx = { api: A.api, get state() { return { agents: A.agents() }; }, project: it.repo ? { path: it.repo } : A.project(), go: (to) => { location.hash = to; } };
  let r;
  try { r = await rules.grantSheet(ctx, { question: it.question, option: i, agent: it.agent, repo: it.repo }); } catch (e) { A.toast(`The check did not open: ${e.message}`, true); return; }
  const who = it.agent ? it.agent.name : "the agent";
  if (r.choice === "confirm") { A.toast(`Sent “${o.n} ${o.label}” to ${who}${r.pattern ? `: ${r.pattern} is written` : ""}`); A.answered(it.id); }
  else if (r.choice === "narrow") { A.toast(`Allowed ${who} this once, and wrote ${r.pattern} for this project's new agents`); A.answered(it.id); }
}

/** Batch answer (power users, 2): the selected cards run the identical
 *  command; option `n` on each, after a confirm. Never across different commands. */
export function batchAnswer(n) {
  const items = A.needs().items.filter((it) => O.sel.has(it.id));
  if (items.length < 2) { A.toast("Select two or more cards with x first"); return; }
  const cmd = (it) => it.question && (parseJSON(it.question.detail, {}) || {}).command;
  const first = cmd(items[0]);
  if (!first || items.some((it) => it.kind !== "question" || it.question.kind !== "permission" || cmd(it) !== first)) {
    A.toast("Only cards that run the identical command can be answered together", true);
    return;
  }
  const picks = items.map((it) => { const opts = optionsOf(it.question); const k = opts.findIndex((o) => String(o.n) === String(n)); return [it, k, opts[k]]; });
  if (picks.some(([, k, o]) => k < 0 || !o)) { A.toast(`Not every selected prompt has an option ${n}`, true); return; }
  if (picks.some(([, , o]) => o.grants)) { A.toast(`Option ${n} grants more than one call: answer it card by card, through its check`, true); return; }
  const names = items.map((it) => it.agent ? it.agent.name : "?");
  sheet({
    kicker: `${items.length} agents ask the same`, title: `Answer “${picks[0][2].label}” on all ${items.length}?`,
    body: [h("pre", { class: "qc-cmd" }, first), h("p", {}, names.join(", "))],
    actions: (done) => [btn("Cancel", { onclick: done }), btn(`${n} ${picks[0][2].label} on ${items.length}`, { primary: true, onclick: async () => {
      done();
      const ok = [];
      for (const [it, k, o] of picks) {
        try { await A.api(`/api/questions/${enc(it.question.id)}/answer`, { method: "POST", body: { option: k, label: o.label } }); ok.push(it.agent ? it.agent.name : "?"); A.answered(it.id, { quiet: true }); } catch { /* gone: nothing typed */ }
      }
      O.sel.clear();
      A.toast(`Answered ${ok.length}: ${ok.join(", ")}${ok.length < picks.length ? ` (${picks.length - ok.length} were gone)` : ""}`);
      A.refreshNeeds();
    } })],
  });
}

/** `x` on a focused card: select it for a batch answer. */
export function toggleSelect(card) {
  const id = card && card.dataset.need;
  if (!id) return;
  if (O.sel.has(id)) O.sel.delete(id); else O.sel.add(id);
  card.classList.toggle("picked", O.sel.has(id));
  A.toast(`${O.sel.size} selected: ⇧ and an option's number answers them alike`);
}

export const KEYS = [
  ["Anywhere outside a text box or a terminal", [
    ["/", "Search (⇧↵ on a line: why is this line like this)"], [".", "Needs you"], ["a", "Agents, then 1…9 to open one"],
    ["g s", "Start"], ["g c", "Continue the project"], ["g r", "Review and ship"], ["g d", "Debug"], ["g t", "All terminals"], ["g n", "The inbox"],
    ["n", "In a project: Start a change"], ["z", "Compact rows on or off"], ["?", "This list"], ["Esc", "Close the drawer or a sheet; else up one level"],
  ]],
  ["Cards that need you", [
    ["j  k", "The next or previous card"], ["]  [", "The next or previous card that needs you, opening the drawer when the page has none"],
    ["1…9", "Pick Claude Code's option with that number (in the focused card only)"], ["↵", "Send the picked option"], ["Esc", "Drop the pick"],
    ["x", "Select the card"], ["⇧1…⇧9", "Answer every selected card with that option, after a check (identical commands only)"],
  ]],
  ["Review and the agent page", [["↵", "Open the focused row"], ["m", "Merge (opens its check)"], ["b", "Send back with a note"], ["y", "In a diff: Why? for the line under the cursor"]]],
  ["In a terminal", [["Ctrl-b Esc", "Leave the terminal (every other key goes to the agent)"], ["Ctrl-b ?", "This list"], ["Ctrl-b Ctrl-b", "A literal Ctrl-b"]]],
];
export function keysSheet() {
  return sheet({ kicker: "Keys", title: "The keyboard", body: KEYS.map(([t, rows]) => [label(t), h("dl", { class: "kv keys2" }, rows.map(([k, d]) => [h("dt", {}, k.split(/\s{2}| /).map((x) => kc(x))), h("dd", {}, d)]))]), actions: (done) => [btn("Close", { onclick: done })] });
}

const CSS = `
.qc.picked,.alert-card.picked{box-shadow:0 0 0 2px var(--accent),var(--shadow-2)}
.qc-trust{padding:var(--s-2) var(--s-4);background:var(--surface-2);border-bottom:var(--hair) solid var(--line);font-size:var(--fs-sm);color:var(--text-2)}
.sw-wrap{display:flex;flex-direction:column}
.sw-act{display:flex;gap:var(--s-2);padding:0 var(--s-4) var(--s-2) 46px;flex-wrap:wrap}
.sw-sum{padding-top:var(--s-3)!important}
.keys2{grid-template-columns:120px minmax(0,1fr)}
.keys2 dt{display:flex;gap:4px;align-items:center;text-transform:none;letter-spacing:0}
.gs-ov{z-index:40}
.dr-body .qc,.dr-body .alert-card,.dr-body .wait-card{flex:none}
@media (max-width:640px),(pointer:coarse){.dr-foot>span:first-child{display:none}}
`;
