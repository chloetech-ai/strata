// On-call agent: a Claude model looks into a firing alert, or a question
// about the cluster, with the read-only views an operator has (health,
// alerts, events, queries, logs, load, metrics, contain rules), and
// reports what it found. It changes nothing: each action it would take is
// a proposal that an admin approves or rejects here, and an approval is
// made with the approver's key (the audit names them, not the agent).
//
// A run goes on on the server (POST /v1/admin/oncall with wait: false);
// this page polls it while it goes (/v1/admin/oncall/runs/:id), so each
// step shows as the agent takes it. A run can be stopped, and followed up
// with another question in the same conversation. The server keeps the
// latest runs beside its audit log; proposals hold for an hour.

import { h, icon, clear, fmt, card, api, toast, confirm, modal, seg, pill, empty, failure, store, can, needs, every, link, when, timeEl, copy, bus, experimental } from "../core.js";
import { firingAlerts, refreshAlerts, state, setPendingProposals } from "../data.js";

export default {
  id: "oncall", title: "On-call agent", icon: "agent", group: "Operate",
  keywords: "on-call oncall agent claude ai llm investigate incident alert firing why slow root cause diagnose triage proposals approve reject contain cancel silence runbook change request changes inbox dry run plan stale",
  render, onParams,
};
// The navigation's count of proposals waiting for an admin is the shell's
// (data.js pendingProposals), polled whether or not this view was opened.

// ---------------------------------------------------------------- words
const SUGGEST = [
  "Is anything wrong right now?",
  "Why are searches slower than an hour ago?",
  "What changed in the last hour?",
  "Which statements use the most time, and who sends them?",
  "Are any copies down, behind or overloaded?",
];
const EFFORTS = [["", "default"], ["low", "low"], ["medium", "medium"], ["high", "high"]];
const STATE_PILL = { running: ["info", "running"], done: ["ok", "reported"], failed: ["bad", "failed"], stopped: ["disabled", "stopped"] };
const PROPOSAL_PILL = { pending: ["pending", "waits for an admin"], applying: ["info", "applying"], applied: ["ok", "applied"], failed: ["bad", "failed"], rejected: ["disabled", "rejected"], expired: ["disabled", "expired"], withdrawn: ["disabled", "withdrawn"] };
const ACTION_ICON = { contain: "shield", remove_contain: "shield", cancel_query: "stop", silence_alert: "pause", change: "wrench" };
/** What a change request's tier means, for the person approving it. */
const TIER_WORDS = { 1: "bounded, undone by itself", 2: "changes the cluster or its data, can be undone", 3: "loses data or costs money" };
const mins = (m) => (m >= 120 && m % 60 === 0 ? `${m / 60} h` : `${m} min`);
const QUERIES = { running: "Running queries", slow: "Slow queries", recent: "Recent queries", stats: "Statements by fingerprint" };
/** What each tool reads, in words; its icon; and the view that shows the same. */
const TOOLS = {
  health: { icon: "okc", label: (i) => (i.fleet ? "Health of every coordinator" : "Health of the servers and shards"), view: () => link("home") },
  cluster: { icon: "cluster", label: () => "The shards and their copies", view: () => link("cluster") },
  load: { icon: "gauge", label: () => "Each copy's load now", view: () => link("cluster") },
  system: { icon: "cpu", label: () => "The host: CPU, memory, disk", view: () => link("system") },
  alerts: { icon: "alerts", label: (i) => (i.rule ? `Alert ${i.rule}, with its events` : "The alert rules and their states"), view: (i) => (i.rule ? "#/alert/" + encodeURIComponent(i.rule) : link("alerts")) },
  events: { icon: "timeline", label: (i) => `Events of the last ${mins(i.minutes || 60)}${i.kind ? ` (${i.kind})` : ""}`, view: () => link("timeline") },
  queries: { icon: "queries", label: (i) => `${QUERIES[i.which] || "Queries"}${i.limit ? `, ${i.limit} at most` : ""}`, view: () => link("queries") },
  logs: { icon: "logs", label: (i) => `Log lines${i.level ? `, ${i.level} and worse` : ""}${i.words ? `, with "${i.words}"` : ""}`, view: (i) => link("logs", { q: [i.level ? `level>=${i.level}` : null, i.words || null].filter(Boolean).join(" ") || null }) },
  timeseries: { icon: "perf", label: (i) => `Metrics of the last ${mins(i.minutes || 15)}`, view: () => link("performance") },
  contain_rules: { icon: "shield", label: () => "The contain rules in force", view: () => link("queries") },
  propose_contain: { icon: "shield", label: () => "Proposes a contain rule" },
  propose_remove_contain: { icon: "shield", label: () => "Proposes removing a contain rule" },
  propose_cancel_query: { icon: "stop", label: () => "Proposes cancelling a query" },
  propose_silence_alert: { icon: "pause", label: () => "Proposes silencing an alert" },
};
const toolOf = (name) => TOOLS[name] || { icon: "wrench", label: () => name };
const SECTIONS = ["Summary", "Evidence", "Likely cause", "Proposed actions", "Watch"];
const SECTION_RE = new RegExp(`^\\s*(?:#{1,6}\\s*)?(?:\\*\\*)?(${SECTIONS.join("|")})(?:\\*\\*)?\\s*:(?:\\*\\*)?\\s*(.*)$|^\\s*#{1,6}\\s*(${SECTIONS.join("|")})\\s*$`, "i");
const tokens = (u) => (u?.input_tokens || 0) + (u?.output_tokens || 0) + (u?.cache_read_input_tokens || 0);
/** A run's proposals: a run in full lists them, a run in short counts them. */
const propsOf = (r) => (Array.isArray(r?.proposals) ? r.proposals : []);
/** How long until `ms`, in minutes: "in 42 min". */
const left = (ms) => { const m = Math.round((ms - Date.now()) / 60000); return m <= 0 ? "in under a minute" : m >= 90 ? `in ${Math.round(m / 60)} h` : `in ${m} min`; };

/** The title of a conversation: its alert, else its first question's first line. */
function titleOf(r) {
  if (!r) return "On-call agent";
  if (r.alert && !r.follows) return `Alert ${r.alert}`;
  const line = String(r.question || "").split("\n")[0].trim();
  return line.length > 90 ? line.slice(0, 89) + "…" : line || r.id;
}

// ---------------------------------------------------------------- text
/**
 * A report's text as nodes: paragraphs, lists, `code` and **bold**, with
 * proposal ids (p12) turned into links to their cards. Text goes in as text.
 */
function richText(text, ids, onId) {
  const out = [];
  let list = null, para = [];
  const flush = () => { if (para.length) { out.push(h("p", {}, inline(para.join(" "), ids, onId))); para = []; } };
  for (const raw of String(text || "").split("\n")) {
    const line = raw.trimEnd();
    const li = /^\s*(?:[-*•]|\d+[.)])\s+(.*)$/.exec(line);
    if (li) { flush(); if (!list) { list = h("ul"); out.push(list); } list.append(h("li", {}, inline(li[1], ids, onId))); continue; }
    if (!line.trim()) { flush(); list = null; continue; }
    if (/^\s*#{1,6}\s/.test(line)) { flush(); list = null; out.push(h("p", {}, h("b", {}, inline(line.replace(/^\s*#{1,6}\s*/, ""), ids, onId)))); continue; }
    if (list && /^\s{2,}\S/.test(raw)) { list.lastChild.append(" ", inline(line.trim(), ids, onId)); continue; }
    list = null;
    para.push(line.trim());
  }
  flush();
  return out;
}
function inline(s, ids, onId) {
  const parts = [];
  const re = /`([^`]+)`|\*\*([^*]+)\*\*|\b(p\d+)\b/g;
  let at = 0, m;
  while ((m = re.exec(s))) {
    if (m.index > at) parts.push(s.slice(at, m.index));
    if (m[1] != null) parts.push(h("code", {}, m[1]));
    else if (m[2] != null) parts.push(h("b", {}, m[2]));
    else if (ids?.has(m[3])) { const id = m[3]; parts.push(h("a", { href: "#", class: "oc-pid", onclick: (e) => { e.preventDefault(); onId(id); } }, id)); }
    else parts.push(m[0]);
    at = re.lastIndex;
  }
  if (at < s.length) parts.push(s.slice(at));
  return parts;
}
/** The report in its sections (Summary, Evidence, ...); the text before them, if any, as `lead`. */
function sections(text) {
  const out = [];
  let cur = { name: null, lines: [] };
  for (const line of String(text || "").split("\n")) {
    const m = SECTION_RE.exec(line);
    if (m) {
      out.push(cur);
      const name = SECTIONS.find((s) => s.toLowerCase() === (m[1] || m[3]).toLowerCase());
      cur = { name, lines: m[2] ? [m[2]] : [] };
    } else cur.lines.push(line);
  }
  out.push(cur);
  return out.filter((s) => s.name || s.lines.join("").trim()).map((s) => ({ name: s.name, text: s.lines.join("\n").trim() }));
}
function pretty(text) {
  try { return JSON.stringify(JSON.parse(text), null, 2); } catch { return text; }
}
function asMarkdown(runs) {
  const out = [`# On-call: ${titleOf(runs[0])}`, ""];
  for (const r of runs) {
    out.push(`**${r.follows ? "Follow-up" : "Asked"}** by ${r.asked_by}, ${new Date(r.started_ms).toISOString()}: ${r.question}`, "");
    out.push(r.answer || r.error || "(no report)", "");
    for (const p of propsOf(r)) out.push(`- ${p.id} (${p.state}): ${p.what}. Why: ${p.why}`);
    if (propsOf(r).length) out.push("");
  }
  return out.join("\n");
}

// ---------------------------------------------------------------- the view
let nextKey = null; // a per-run Claude key for a server that has none: this tab's memory only
let reroute = null;
function onParams(r) { reroute?.(r); }

function render(page, route) {
  const head = h("div"), main = h("div", { class: "oc" });
  page.append(head, main);
  const sideBox = h("div", { class: "oc-side stack" }), bodyBox = h("div", { class: "oc-body stack" });
  main.append(bodyBox, sideBox);

  let ov = null, ovErr = null;            // GET /v1/admin/oncall
  let pending = [];                        // proposals waiting, every run's
  const full = new Map();                  // run id -> the run in full
  let selected = null, missing = null;     // the open run (any of its conversation's)
  const draft = { question: "", model: "", effort: "", max_steps: null, note: false, ...store.get("oncall.draft", {}) };
  let followText = "";
  let busy = false, filter = "";
  const open = new Set();                  // steps shown with their answers: "run:index"
  const openReq = new Set();               // proposals' requests and answers shown: "p3", "p3:answer"
  const shownSteps = new Set();            // finished runs whose steps are shown
  let alive = true, stopPoll = null, pace = 0;

  // ---------------------------------------------------------- loading
  const summaries = () => ov?.runs || [];
  const byId = (id) => summaries().find((r) => r.id === id);
  function rootOf(id) {
    let r = byId(id) || full.get(id), seen = 0;
    while (r?.follows && seen++ < 60) { const up = byId(r.follows) || full.get(r.follows); if (!up) break; r = up; }
    return r?.id || id;
  }
  /** The runs of the open conversation, oldest first. */
  function thread() {
    if (!selected) return [];
    const root = rootOf(selected);
    const ids = new Set([root]);
    const all = [...summaries()].sort((a, b) => a.started_ms - b.started_ms);
    for (const r of all) if (r.follows && ids.has(r.follows)) ids.add(r.id);
    if (!ids.has(selected)) ids.add(selected);
    return [...ids].map((id) => full.get(id) || byId(id)).filter(Boolean).sort((a, b) => a.started_ms - b.started_ms);
  }
  const running = () => summaries().some((r) => r.state === "running") || thread().some((r) => r.state === "running");

  async function load() {
    try { ov = await api("/v1/admin/oncall"); ovErr = null; } catch (e) { ovErr = e; }
    try { pending = (await api("/v1/admin/oncall/proposals?state=pending")).proposals || []; setPendingProposals(pending.length); } catch { /* shown with the runs */ }
    if (!alive) return;
    // The open conversation's runs, fetched again when they moved.
    const want = new Set([selected, ...thread().map((r) => r.id)].filter(Boolean));
    await Promise.all([...want].map(async (id) => {
      const s = byId(id), f = full.get(id);
      if (f && s && f.state === s.state && f.state !== "running" && (f.steps?.length ?? 0) === s.steps && propsOf(f).filter((p) => p.state === "pending").length === s.proposals?.pending) return;
      try { full.set(id, await api(`/v1/admin/oncall/runs/${encodeURIComponent(id)}`)); if (id === selected) missing = null; }
      catch (e) { if (id === selected && e.status === 404) missing = id; }
    }));
  }
  async function refresh() { await load(); if (alive) draw(); pollPace(); }
  function pollPace() {
    const ms = running() ? 1500 : 10000;
    if (ms === pace && stopPoll) return;
    stopPoll?.(); pace = ms;
    stopPoll = every(ms, refresh, false, { always: running() });
  }

  // ---------------------------------------------------------- actions
  function keep() { const { question, model, effort, max_steps, note } = draft; store.set("oncall.draft", { question, model, effort, max_steps, note }); }
  async function start(body) {
    if (!can("admin", "POST /v1/admin/oncall")) { toast(needs("admin", "POST /v1/admin/oncall"), "bad"); return false; }
    if (ov && !ov.configured && !nextKey) { toast("This server has no Claude key: paste one under Options, or start the server with ANTHROPIC_API_KEY", "bad"); return false; }
    let ok = false;
    busy = true; draw();
    try {
      const b = { ...body, wait: false };
      if (draft.model.trim()) b.model = draft.model.trim();
      if (draft.effort) b.effort = draft.effort;
      if (draft.max_steps) b.max_steps = draft.max_steps;
      if (draft.note) b.note = true;
      if (nextKey) b.api_key = nextKey;
      const j = await api("/v1/admin/oncall", { body: b });
      selected = j.run;
      location.hash = "#/oncall?run=" + encodeURIComponent(j.run);
      await load();
      toast(body.follows ? "Asked: it goes on in the same conversation" : "The agent is looking into it", "ok");
      ok = true;
    } catch (e) { toast(e.message, "bad"); }
    busy = false; draw(); pollPace();
    return ok;
  }
  async function stopRun(r) {
    try { full.set(r.id, await api(`/v1/admin/oncall/runs/${encodeURIComponent(r.id)}/stop`, { body: {} })); toast("Stopping: it ends before its next model call", "info"); } catch (e) { toast(e.message, "bad"); }
    refresh();
  }
  /** What a change request's dry run said when it was filed. */
  function changeBlock(p) {
    const d = p.plan || {};
    const rows = d.affects && d.affects.rows != null ? `${fmt.n(d.affects.rows)} rows of ${d.affects.table}` : null;
    const line = (k, v) => (v ? h("div", { class: "small" }, h("span", { class: "muted" }, k, ": "), v) : null);
    return h("div", { class: "stack", style: { gap: "4px" } },
      line("Tier", p.tier != null ? `${p.tier}: ${TIER_WORDS[p.tier] || ""}` : null),
      line("Touches", rows),
      line("Undo", d.undo || (d.reversible === false ? "none: it cannot be undone" : null)),
      line("Meanwhile", d.blast),
      line("Filed by", `${p.asked_by}${p.agent ? " (an agent)" : ""} ${fmt.ago(p.created_ms)}`));
  }
  async function approve(p) {
    const change = p.action === "change";
    const danger = p.action === "cancel_query" || (p.action === "contain" && p.body?.action === "refuse") || (change && (p.tier || 0) >= 3);
    const ok = await confirm({
      title: `Approve ${p.id}?`,
      ok: "Approve and apply",
      danger,
      body: h("div", { class: "stack", style: { gap: "10px" } },
        h("p", { style: { margin: 0 } }, h("b", {}, cap(p.what)), "."),
        h("p", { class: "small dim", style: { margin: 0 } }, change ? "Why, it says: " : "Why, the agent says: ", p.why),
        change ? changeBlock(p) : null,
        requestBlock(p, false),
        h("p", { class: "small muted", style: { margin: 0 } }, change
          ? ["It is made now with your key, and the audit log names you and this change request. It is dry run again first: if the cluster moved since ", when(p.created_ms), " (other steps, other rows), it is not applied and fails as stale."]
          : ["It is made now with your key, and the audit log names you, with the proposal's reason. It may no longer fit if the cluster moved since ", when(p.created_ms), "."])),
    });
    if (!ok) return;
    try {
      const j = await api(`/v1/admin/oncall/proposals/${encodeURIComponent(p.id)}/approve`, { body: {} });
      toast(j.state === "applied" ? `${p.id} applied` : `${p.id} failed: ${j.result?.error || "see its card"}`, j.state === "applied" ? "ok" : "bad", 6000);
      if (p.action === "silence_alert") refreshAlerts();
    } catch (e) { toast(e.message, "bad"); }
    full.delete(p.run);
    refresh();
  }
  function reject(p) {
    const why = h("textarea", { class: "input", rows: 2, placeholder: "Why not (optional): kept with the proposal", "aria-label": "Reason", style: { width: "100%", fontFamily: "inherit" } });
    const no = h("button", { class: "btn" }, "Cancel");
    const yes = h("button", { class: "btn primary" }, "Reject");
    const m = modal({ title: `Reject ${p.id}?`, body: [h("p", { class: "dim", style: { marginTop: 0 } }, cap(p.what), ". Nothing is changed."), why], foot: [no, yes] });
    no.onclick = () => m.close();
    yes.onclick = async () => {
      m.close();
      try { await api(`/v1/admin/oncall/proposals/${encodeURIComponent(p.id)}/reject`, { body: { reason: why.value.trim() } }); toast(`${p.id} rejected`, "ok"); } catch (e) { toast(e.message, "bad"); }
      full.delete(p.run);
      refresh();
    };
  }
  async function postNote(r) {
    const text = `On-call agent (${r.id}): ${r.answer}`.slice(0, 2000);
    try { await api("/v1/events/notes", { body: { text } }); toast("Posted on the events timeline", "ok"); } catch (e) { toast(e.message, "bad"); }
  }
  function download(runs) {
    const blob = new Blob([JSON.stringify(runs, null, 2)], { type: "application/json" });
    const a = h("a", { href: URL.createObjectURL(blob), download: `oncall-${runs[0]?.id || "run"}.json` });
    document.body.append(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  }
  function scrollToProposal(id) {
    const el = page.querySelector(`[data-proposal="${id}"]`);
    if (el) { el.scrollIntoView({ block: "center", behavior: "smooth" }); el.classList.add("oc-flash"); setTimeout(() => el.classList.remove("oc-flash"), 1600); }
  }

  // ---------------------------------------------------------- drawing
  function draw() {
    if (!alive) return;
    drawHead(); drawSide();
    if (ovErr && !ov) { clear(bodyBox, failure(ovErr, "The on-call agent")); return; }
    if (!ov) { clear(bodyBox, empty("agent", "Loading", "")); return; }
    if (selected) drawThread(); else drawNew();
  }

  function drawHead() {
    const runs = thread();
    const tools = [];
    if (selected) {
      tools.push(h("a", { class: "btn sm", href: "#/oncall?new=1" }, icon("plus"), "New investigation"));
    }
    clear(head, h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, selected && runs.length ? titleOf(runs[0]) : "On-call agent", " ", experimental("The on-call agent")),
        h("p", {}, selected ? h("a", { href: "#/oncall?new=1", class: "oc-back" }, icon("left"), "On-call agent") : "A Claude model looks into an alert or a question with read-only tools, and reports what it found. Each action it would take waits for an admin's approval here.")),
      h("div", { class: "tools" }, tools)));
  }

  // The side: what waits for an admin, and the conversations kept.
  let sideList = null, sideSearch = null, inbox = null;
  function drawSide() {
    if (!sideList) {
      inbox = h("div");
      sideSearch = h("input", { class: "input sm", type: "search", placeholder: "Find a run", "aria-label": "Find a run", value: filter, style: { width: "100%" }, oninput: (e) => { filter = e.target.value; drawSide(); } });
      sideList = h("div", { class: "oc-runs" });
      clear(sideBox, inbox, card({ title: "Investigations", sub: ov ? `the last ${ov.kept} runs are kept` : "", body: h("div", { class: "stack", style: { gap: "0" } }, h("div", { class: "oc-find" }, sideSearch), sideList), flush: true }));
    }
    clear(inbox, pending.length ? card({
      title: "Waiting for an admin", sub: fmt.plural(pending.length, pending.every((p) => p.action === "change") ? "change request" : "proposal"), cls: "oc-inbox", flush: true,
      body: h("div", { class: "oc-runs" }, pending.map((p) => h("div", { class: "oc-pend" },
        h("button", { class: "oc-pend-what", title: p.action === "change" ? `Filed by ${p.asked_by}: ${p.why}` : "", onclick: () => { if (p.run) location.hash = "#/oncall?run=" + encodeURIComponent(p.run) + "&proposal=" + p.id; else approve(p); } },
          icon(ACTION_ICON[p.action] || "wrench"), h("span", { class: "grow" }, h("b", {}, cap(p.what)), h("span", { class: "small muted" }, ` · ${p.id} · expires ${left(p.expires_ms)}`))),
        h("div", { class: "row nw", style: { gap: "6px" } },
          h("button", { class: "btn sm primary", disabled: !can("admin"), title: can("admin") ? "Approve: see what it does first" : needs("admin"), onclick: () => approve(p) }, "Approve"),
          h("button", { class: "btn sm", disabled: !can("admin"), onclick: () => reject(p) }, "Reject"))))),
    }) : null);
    const roots = summaries().filter((r) => !r.follows || !byId(r.follows));
    const kids = (id) => summaries().filter((r) => r.follows && rootOf(r.id) === id);
    const f = filter.trim().toLowerCase();
    const shown = roots.filter((r) => !f || [r.question, r.alert, r.summary, r.asked_by, r.id, ...kids(r.id).map((k) => k.question + " " + k.summary)].join(" ").toLowerCase().includes(f));
    const cur = selected ? rootOf(selected) : null;
    clear(sideList, shown.length ? shown.map((r) => {
      const ks = kids(r.id);
      const last = ks[ks.length - 1] || r;
      const st = [r, ...ks].some((x) => x.state === "running") ? "running" : last.state;
      const pend = [r, ...ks].reduce((a, x) => a + (x.proposals?.pending || 0), 0);
      return h("a", { class: "oc-run" + (r.id === cur ? " on" : ""), href: "#/oncall?run=" + encodeURIComponent(r.id) },
        h("div", { class: "row between nw", style: { gap: "8px" } }, h("b", { class: "ellipsis" }, titleOf(r)), st === "running" ? h("span", { class: "sim-spin", title: "running" }) : pill(...(STATE_PILL[st] || ["info", st]))),
        last.summary || last.error ? h("div", { class: "small dim oc-clamp" }, last.summary || last.error) : null,
        h("div", { class: "tiny muted" }, [fmt.ago(r.started_ms), r.asked_by, ks.length ? fmt.plural(ks.length, "follow-up") : null].filter(Boolean).join(" · "), pend ? h("span", { class: "badge oc-wait" }, `${pend} waiting`) : null));
    }) : h("div", { class: "small muted", style: { padding: "4px 14px 12px" } }, f ? "No run matches." : "None yet."));
  }

  // ---------------------------------------------------------- a new run
  let newBox = null, firingBox = null, askIn = null, keyIn = null;
  function drawNew() {
    if (!newBox || !bodyBox.contains(newBox)) {
      firingBox = h("div", { class: "oc-firing" });
      askIn = h("textarea", { class: "input oc-ask", rows: 3, placeholder: "What should it look into? For example: why did searches slow down at 14:05?", "aria-label": "Question", value: draft.question,
        oninput: (e) => { draft.question = e.target.value; keep(); },
        onkeydown: (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); ask(); } } });
      keyIn = h("input", { class: "input", type: "password", autocomplete: "off", placeholder: "sk-ant-…", "aria-label": "Claude API key for this tab", value: nextKey || "", oninput: (e) => { nextKey = e.target.value.trim() || null; } });
      const steps = h("input", { class: "input", type: "number", min: 1, max: ov.max_steps, placeholder: String(ov.steps), value: draft.max_steps ?? "", "aria-label": "Model calls at most",
        oninput: (e) => { const v = Number(e.target.value); draft.max_steps = e.target.value === "" ? null : Math.max(1, Math.min(ov.max_steps, v || ov.steps)); keep(); } });
      const options = h("details", { class: "oc-opts" }, h("summary", {}, icon("sliders"), "Options"),
        h("div", { class: "oc-optgrid" },
          h("label", { class: "f" }, "Model", h("input", { class: "input", placeholder: ov.model, value: draft.model, oninput: (e) => { draft.model = e.target.value; keep(); } }), h("span", { class: "small muted" }, "the server's default if empty")),
          h("label", { class: "f" }, "Model calls at most", steps, h("span", { class: "small muted" }, `${ov.steps} by default, ${ov.max_steps} at most`)),
          h("div", { class: "f" }, h("span", {}, "Effort"), seg(EFFORTS, draft.effort, (v) => { draft.effort = v; keep(); }, { "aria-label": "Effort" })),
          h("label", { class: "row nw small", style: { gap: "8px", alignSelf: "end" } }, h("input", { type: "checkbox", checked: draft.note, onchange: (e) => { draft.note = e.target.checked; keep(); } }), "Post the report on the events timeline"),
          !ov.configured ? h("label", { class: "f oc-span2" }, "Claude API key", keyIn, h("span", { class: "small muted" }, "This server has none. The key is sent with each run you start from this tab, and is never kept, on the server or here.")) : null));
      if (!ov.configured) options.open = true;
      const go = h("button", { class: "btn primary", onclick: () => ask() }, icon("play"), "Investigate");
      newBox = h("div", { class: "stack" },
        !ov.configured ? h("div", { class: "sim-warn" }, icon("warn"), h("span", {}, "This server has no Claude key. Start it with ", h("code", {}, "ANTHROPIC_API_KEY"), " set, or paste a key under Options for this tab.")) : null,
        card({ title: "Firing now", sub: "investigate an alert", body: firingBox, flush: true }),
        card({
          title: "Or ask a question",
          body: h("div", { class: "stack", style: { gap: "10px" } }, askIn,
            h("div", { class: "oc-chips" }, SUGGEST.map((s) => h("button", { class: "chip", type: "button", onclick: () => { draft.question = s; askIn.value = s; keep(); askIn.focus(); } }, s))),
            options),
          foot: h("div", { class: "row between", style: { width: "100%", gap: "10px", flexWrap: "wrap" } },
            h("span", { class: "small muted" }, icon("lock"), " Read-only: it looks and proposes. Nothing changes until an admin approves."), go),
        }),
        howItWorks());
      clear(bodyBox, newBox);
    }
    drawFiring();
  }
  function ask() {
    const q = draft.question.trim();
    if (!q) { toast("Ask a question, or pick a firing alert", "bad"); askIn?.focus(); return; }
    start({ question: q });
  }
  function drawFiring() {
    if (!firingBox) return;
    const fa = firingAlerts();
    clear(firingBox, fa.length ? h("div", { class: "oc-alerts" }, fa.map((a) => {
      const last = summaries().find((r) => r.alert === a.id && !r.follows);
      return h("div", { class: "oc-alert" },
        h("span", { class: "dot " + (a.severity === "critical" ? "critical" : "warn") }),
        h("div", { class: "grow", style: { minWidth: 0 } },
          h("a", { href: "#/alert/" + encodeURIComponent(a.id), class: "oc-aname" }, a.name || a.id),
          h("div", { class: "small muted" }, [a.severity, a.since_ms ? `firing for ${fmt.dur((Date.now() - a.since_ms) / 1000)}` : "firing", a.silenced ? "silenced" : null, a.ack ? `acknowledged by ${a.ack.by || "someone"}` : null].filter(Boolean).join(" · "),
            last ? [" · ", h("a", { href: "#/oncall?run=" + encodeURIComponent(last.id) }, `looked into ${fmt.ago(last.started_ms)}`)] : null)),
        h("button", { class: "btn sm primary", disabled: busy || !can("admin", "POST /v1/admin/oncall"), title: can("admin", "POST /v1/admin/oncall") ? "" : needs("admin", "POST /v1/admin/oncall"), onclick: () => start({ alert: a.id }) }, icon("agent"), "Investigate"));
    })) : h("div", { class: "small muted", style: { padding: "12px 14px" } }, state.alerts ? "Nothing is firing." : "Loading the alerts…"));
  }
  function howItWorks() {
    return card({
      title: "How it works",
      body: h("ol", { class: "oc-how" },
        h("li", {}, h("b", {}, "It looks. "), "Health, the shards and their copies, load, the host, alerts, the events timeline, running and slow queries, statements, logs, metrics and contain rules, each read through this server's own routes with your key."),
        h("li", {}, h("b", {}, "It reports. "), "A summary, the evidence it saw, the likely cause and how sure it is, what it proposes, and what to watch. Ask a follow-up to go on in the same conversation."),
        h("li", {}, h("b", {}, "You decide. "), "Each action (limit or refuse a statement or key, cancel a query, silence an alert) is a proposal for an hour. An admin approves it, and it is made with their key, or rejects it.")),
    });
  }

  // ---------------------------------------------------------- a conversation
  let threadBox = null, followBox = null, followIn = null, followBtn = null;
  function drawThread() {
    const runs = thread();
    if (!threadBox || !bodyBox.contains(threadBox)) {
      threadBox = h("div", { class: "stack" });
      followIn = h("textarea", { class: "input oc-ask", rows: 2, placeholder: "Ask a follow-up: it goes on in the same conversation and remembers what it looked at", "aria-label": "Follow-up question", value: followText,
        oninput: (e) => { followText = e.target.value; },
        onkeydown: (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); follow(); } } });
      followBtn = h("button", { class: "btn primary", onclick: () => follow() }, icon("agent"), "Ask");
      followBox = card({ cls: "oc-follow", body: h("div", { class: "oc-follow-row" }, followIn, followBtn) });
      clear(bodyBox, threadBox, followBox);
    }
    if (missing && !runs.length) {
      clear(threadBox, card({ body: empty("agent", `No run ${missing}`, "The server keeps the latest runs; this one is no longer kept.", h("a", { class: "btn sm", href: "#/oncall?new=1" }, "New investigation")) }));
      followBox.hidden = true;
      return;
    }
    if (!runs.length) { clear(threadBox, empty("agent", "Loading", "")); followBox.hidden = true; return; }
    clear(threadBox, runs.map((r) => turn(r, runs)));
    followBox.hidden = false;
    const busyNow = runs.some((r) => r.state === "running") || busy;
    followBtn.disabled = busyNow || !can("admin", "POST /v1/admin/oncall");
    followBtn.title = !can("admin", "POST /v1/admin/oncall") ? needs("admin", "POST /v1/admin/oncall") : busyNow ? "Once it has reported" : "Ask (Ctrl+Enter)";
  }
  function follow() {
    const runs = thread();
    const last = runs[runs.length - 1];
    const q = followText.trim();
    if (!last || !q) { followIn?.focus(); return; }
    followText = ""; followIn.value = "";
    start({ question: q, follows: last.id }).then((ok) => { if (!ok && !followText) { followText = q; if (followIn) followIn.value = q; } });
  }

  /** One run of a conversation: the question, the work, the report, its proposals. */
  function turn(r, runs) {
    const isFull = !!r.steps && Array.isArray(r.steps);
    const ids = new Set(propsOf(r).map((p) => p.id));
    const [tone, word] = STATE_PILL[r.state] || ["info", r.state];
    const took = r.state === "running" ? (Date.now() - r.started_ms) : r.took_ms;
    const meta = [r.model, r.effort ? `effort ${r.effort}` : null, `${fmt.n(r.turns || 0)} of ${fmt.n(r.max_steps)} model calls`, tokens(r.usage) ? `${fmt.si(tokens(r.usage))} tokens` : null, took ? fmt.ms(took) : null].filter(Boolean).join(" · ");
    const tools = [];
    if (r.state === "running") tools.push(h("button", { class: "btn sm", disabled: !can("admin", "POST /v1/admin/oncall/runs/:id/stop") || !!r.stopped_by, onclick: () => stopRun(r) }, icon("stop"), r.stopped_by ? "Stopping…" : "Stop"));
    if (r.state === "done" && r === runs[runs.length - 1]) {
      tools.push(h("button", { class: "btn sm ghost", title: "Copy the conversation as Markdown", onclick: () => copy(asMarkdown(runs), "Copied as Markdown") }, icon("copy"), h("span", { class: "oc-wide" }, "Copy")));
      tools.push(h("button", { class: "btn sm ghost", disabled: !can("admin", "POST /v1/events/notes") || !!r.note, title: r.note ? "It is on the timeline" : can("admin", "POST /v1/events/notes") ? "Post the report as a note on the events timeline" : needs("admin", "POST /v1/events/notes"), onclick: () => postNote(r) }, icon("note"), h("span", { class: "oc-wide" }, r.note ? "On the timeline" : "Post on timeline")));
      tools.push(h("button", { class: "btn sm ghost", title: "Download the conversation as JSON", onclick: () => download(runs) }, icon("download")));
    }
    if (r.state === "failed" || r.state === "stopped") tools.push(h("button", { class: "btn sm", disabled: busy || !can("admin", "POST /v1/admin/oncall"), onclick: () => start(r.follows ? { question: r.question, follows: r.follows } : r.alert && /^Alert \S+ is firing\./.test(r.question) ? { alert: r.alert } : { question: r.question, alert: r.alert || undefined }) }, icon("refresh"), "Run it again"));
    return h("section", { class: "card oc-turn", dataset: { run: r.id } },
      h("div", { class: "hd" },
        h("div", { class: "oc-q grow" },
          h("div", { class: "small muted" }, icon("user"), " ", r.follows ? "Follow-up" : "Asked", " by ", h("b", {}, r.asked_by || "someone"), " · ", timeEl(r.started_ms), " · ", h("span", { class: "mono" }, r.id)),
          h("div", { class: "oc-qtext" }, r.question)),
        h("div", { class: "tools" }, r.state === "running" ? h("span", { class: "pill info" }, h("span", { class: "sim-spin" }), "running") : pill(tone, word), tools)),
      h("div", { class: "bd stack", style: { gap: "12px" } },
        isFull ? work(r) : h("div", { class: "small muted" }, "Loading…"),
        report(r, ids),
        propsOf(r).length ? h("div", { class: "oc-props" }, propsOf(r).map(proposalCard)) : null,
        h("div", { class: "tiny muted" }, meta)));
  }

  /** The steps it took: what it read, what it said between, live while it runs. */
  function work(r) {
    const live = r.state === "running";
    const showAll = live || shownSteps.has(r.id) || !r.answer;
    const steps = r.steps || [];
    const failed = steps.filter((s) => !s.ok).length;
    const summary = h("button", { class: "oc-worksum", "aria-expanded": String(showAll), onclick: () => { if (shownSteps.has(r.id)) shownSteps.delete(r.id); else shownSteps.add(r.id); draw(); } },
      icon(showAll ? "down" : "right"),
      h("span", {}, live ? `Looking: ${fmt.plural(steps.length, "step")} so far` : `Looked at ${fmt.plural(steps.length, "thing")}`, failed ? `, ${failed} failed` : "", r.took_ms ? ` in ${fmt.ms(r.took_ms)}` : ""));
    if (!steps.length && !live) return null;
    if (!showAll) return h("div", { class: "oc-work" }, summary);
    const rows = [];
    const said = r.said || [];
    for (let i = 0; i <= steps.length; i++) {
      for (const s of said.filter((x) => x.before === i)) rows.push(h("li", { class: "oc-said" }, h("span", { class: "oc-ico" }, icon("note")), h("div", { class: "grow" }, s.text)));
      if (i < steps.length) rows.push(stepRow(r, steps[i], i));
    }
    if (live) rows.push(h("li", { class: "oc-step oc-now" }, h("span", { class: "oc-ico" }, h("span", { class: "sim-spin" })),
      h("div", { class: "grow" }, r.stopped_by ? `Stopping (asked by ${r.stopped_by})…` : r.doing && r.doing !== "model" ? `Reading: ${toolOf(r.doing).label({})}` : `Waiting for the model · call ${fmt.n((r.turns || 0) + 1)} of ${fmt.n(r.max_steps)}`),
      h("span", { class: "small muted num" }, fmt.ms(Date.now() - r.started_ms))));
    return h("div", { class: "oc-work" }, steps.length && !live ? summary : null, h("ol", { class: "oc-steps" }, rows));
  }
  function stepRow(r, s, i) {
    const t = toolOf(s.tool), input = s.input || {};
    const key = `${r.id}:${i}`, isOpen = open.has(key);
    const prop = s.proposed ? propsOf(r).find((p) => p.id === s.proposed) : null;
    const label = prop ? h("span", {}, "Proposed ", h("a", { href: "#", class: "oc-pid", onclick: (e) => { e.preventDefault(); e.stopPropagation(); scrollToProposal(prop.id); } }, prop.id), ": ", prop.what) : t.label(input);
    const args = Object.entries(input).filter(([k]) => k !== "why").map(([k, v]) => `${k}: ${typeof v === "string" ? v : JSON.stringify(v)}`).join(", ");
    const row = h("li", { class: "oc-step" + (s.ok ? "" : " bad") + (isOpen ? " open" : "") },
      h("button", { class: "oc-steprow", "aria-expanded": String(isOpen), onclick: () => { if (isOpen) open.delete(key); else open.add(key); draw(); } },
        h("span", { class: "oc-ico" }, icon(s.ok ? t.icon : "bad")),
        h("span", { class: "grow oc-stepl" }, h("span", {}, label), args && !prop ? h("span", { class: "tiny muted mono oc-args" }, ` ${args}`) : null, !s.ok ? h("div", { class: "small oc-err" }, s.error) : null),
        h("span", { class: "small muted num nowrap" }, fmt.ms(s.took_ms))));
    if (isOpen) {
      const text = s.preview || s.error || "";
      const view = t.view && s.ok ? t.view(input) : null;
      row.append(h("div", { class: "oc-preview" },
        h("div", { class: "row between small muted", style: { marginBottom: "4px" } }, h("span", {}, `What it saw: ${s.ok ? "the route's answer" : "the error"}${text.length >= 3990 ? ", cut" : ""}`), view ? h("a", { href: view }, "Open the view", icon("right")) : null),
        h("pre", { class: "code oc-pre" }, pretty(text))));
    }
    return row;
  }

  /** The report in its sections. */
  function report(r, ids) {
    if (r.state === "running") return null;
    if (r.state === "failed") return h("div", { class: "error" }, h("b", {}, "The run failed: "), r.error || "no reason given");
    const parts = sections(r.answer);
    const hasSections = parts.some((p) => p.name);
    if (!hasSections) return h("div", { class: "oc-report" + (r.state === "stopped" ? " dim" : "") }, richText(r.answer, ids, scrollToProposal));
    return h("div", { class: "oc-report" }, parts.map((p) => {
      if (!p.name) return h("div", { class: "oc-lead small dim" }, richText(p.text, ids, scrollToProposal));
      const cls = { Summary: "sum", "Likely cause": "cause", "Proposed actions": "acts", Watch: "watch", Evidence: "ev" }[p.name];
      return h("div", { class: "oc-sec " + cls }, h("div", { class: "oc-sech" }, p.name), h("div", { class: "oc-secb" }, richText(p.text, ids, scrollToProposal)));
    }));
  }

  function shut(key, el) { if (openReq.has(key)) el.open = true; el.addEventListener("toggle", () => { if (el.open) openReq.add(key); else openReq.delete(key); }); return el; }
  function requestBlock(p, keep = true) {
    const el = h("details", { class: "oc-req" }, h("summary", { class: "small" }, "The request it makes: ", h("span", { class: "mono" }, `${p.method} ${p.path}`)),
      p.body ? h("pre", { class: "code oc-pre" }, JSON.stringify(p.body, null, 2)) : h("div", { class: "small muted" }, "No body."));
    return keep ? shut(p.id, el) : el;
  }
  function proposalCard(p) {
    const [tone, word] = PROPOSAL_PILL[p.state] || ["info", p.state];
    const live = p.state === "pending";
    const fp = p.body?.match?.fingerprint;
    return h("div", { class: "oc-prop " + p.state, dataset: { proposal: p.id } },
      h("div", { class: "row between nw", style: { gap: "10px", alignItems: "flex-start" } },
        h("div", { class: "row nw", style: { gap: "10px", alignItems: "flex-start", minWidth: 0 } },
          h("span", { class: "oc-pico" }, icon(ACTION_ICON[p.action] || "wrench")),
          h("div", { style: { minWidth: 0 } },
            h("div", {}, h("span", { class: "mono small muted" }, p.id, " "), h("b", {}, cap(p.what))),
            h("div", { class: "small dim" }, p.why),
            fp ? h("div", { class: "tiny" }, h("a", { href: link("statement/" + encodeURIComponent(fp)) }, "The statement", icon("right"))) : null)),
        pill(tone, word)),
      requestBlock(p),
      h("div", { class: "row between", style: { gap: "10px", flexWrap: "wrap" } },
        h("span", { class: "small muted" }, live ? `Can be approved for another ${left(p.expires_ms).replace(/^in /, "")}` : p.decided_by ? `${cap(word)} by ${p.decided_by} ${fmt.ago(p.decided_ms)}` : p.state === "expired" ? `Expired ${fmt.ago(p.expires_ms)}` : "",
          p.state === "rejected" && p.result?.reason ? `: ${p.result.reason}` : "",
          p.state === "failed" && p.result?.error ? h("span", { class: "oc-err" }, `: ${p.result.error}`) : ""),
        live ? h("div", { class: "row nw", style: { gap: "6px" } },
          h("button", { class: "btn sm", disabled: !can("admin"), title: can("admin") ? "" : needs("admin"), onclick: () => reject(p) }, icon("x"), "Reject"),
          h("button", { class: "btn sm primary", disabled: !can("admin"), title: can("admin") ? "See what it does, then apply it with your key" : needs("admin"), onclick: () => approve(p) }, icon("check"), "Approve")) : null),
      p.state === "applied" && p.result && Object.keys(p.result).length ? shut(p.id + ":answer", h("details", { class: "oc-req" }, h("summary", { class: "small" }, "The route's answer"), h("pre", { class: "code oc-pre" }, JSON.stringify(p.result, null, 2)))) : null);
  }

  // ---------------------------------------------------------- the address
  function fromRoute(r) {
    const p = r?.params || new URLSearchParams();
    const run = p.get("run");
    if (p.get("new")) { selected = null; missing = null; }
    else if (run) { if (run !== selected) { selected = run; missing = null; followText = ""; } }
    else selected = null;
    if (p.get("q") && !selected) { draft.question = p.get("q"); if (askIn) askIn.value = draft.question; }
    const alert = p.get("alert");
    if (alert && p.get("start") && !selected) {
      // From an alert's page: start at once, then show the run.
      history.replaceState(null, "", location.pathname + location.search + "#/oncall");
      load().then(() => { if (alive) start({ alert }); });
    }
    const pid = p.get("proposal");
    if (pid) setTimeout(() => scrollToProposal(pid), 400);
  }
  reroute = (r) => { fromRoute(r); threadBox = null; newBox = null; refresh(); };
  fromRoute(route);

  const offAlerts = bus.on("alerts", () => { if (!selected) drawFiring(); });
  draw();
  refresh();
  if (!state.alerts) refreshAlerts();
  return () => { alive = false; reroute = null; stopPoll?.(); offAlerts?.(); };
}

function cap(s) { s = String(s || ""); return s ? s[0].toUpperCase() + s.slice(1) : s; }
