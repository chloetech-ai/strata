// Preview lab (SPEC.md, "Fourth build", use cases 2 and 3), mounted by
// preview.js in one place: mountLab(screenElement, ctx, () => agentId).
//
// - "Test with a persona": a simulated user (a model playing a persona, in a
//   real Chromium through Playwright) does a task on the previewed app;
//   its steps come in live and what got in its way becomes findings, each
//   of which can go to the agent as a steer or to you as a Decide question
//   (/api/swarm/*). Nothing runs until you press Start.
// - The errors strip: what the app threw (the overlay's window.onerror,
//   unhandled rejections and console.error), its 5xx through the preview
//   proxy and its server's OTLP logs (/api/runtime/errors), each joined to
//   the edit that probably caused it, with the Why card of that line.
//
// The screen re-renders its own parts freely; this module keeps three nodes
// of its own (a button in the browser chrome, the strip under the page and
// a drawer over the side column) and puts them back when they are dropped.
import { h, useStyle, avatar, pill, fmtAgo, fmtClock, fmtDur, toMs, onEscape } from "./common.js";

const nameOf = (a) => (a && (a.name || a.id)) || "?";
const base = (p) => String(p || "").split("/").pop();
const SEV = { 3: ["blocker", "crit"], 2: ["major", "warn"], 1: ["minor", "mut"] };
const SOURCE = { overlay: "page", console: "console", proxy: "HTTP", otlp: "OTLP", api: "reported", persona: "persona" };
const TASKS = ["Buy one thing and pay", "Sign up, then change my details", "Do it all with the keyboard only", "Try to break the forms"];

function initials(name) {
  return String(name || "?").split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join("");
}
function personaAv(p, size = 28) {
  let x = 0;
  for (const c of String(p?.id || p?.name || "")) x = (x * 31 + c.charCodeAt(0)) >>> 0;
  return h("span", { class: "pvl-pav", style: { width: size + "px", height: size + "px", "--ph": String([24, 48, 96, 192, 286, 330][x % 6]) }, title: p?.name || "" }, initials(p?.name));
}

/** A step of a persona run, as one line of what it did. */
export function stepText(s) {
  const i = s.input || {};
  const what = i.name ? `“${i.name}”` : i.ref ? `element ${i.ref}` : "";
  switch (s.action) {
    case "look": return "Looked at the page";
    case "click": return `Clicked ${what}`;
    case "type": return `Typed “${String(i.text ?? "").slice(0, 60)}”${i.name ? ` into ${i.name}` : ""}${i.submit ? " and pressed Enter" : ""}`;
    case "choose": return `Chose “${i.option}”${i.name ? ` in ${i.name}` : ""}`;
    case "press": return `Pressed ${i.key}`;
    case "scroll": return `Scrolled ${i.direction || "down"}`;
    case "back": return "Went back";
    case "go": return `Opened ${i.path || "/"}`;
    case "wait": return `Waited ${i.seconds ?? 1} s`;
    case "finish": return i.outcome === "gave_up" ? "Gave up" : "Finished the task";
    default: return s.action || "?";
  }
}

/** The words a steer about a runtime error says. */
export function errorSteer(e, previewAgent) {
  const frames = (e.frames || []).filter((f) => f.in_repo).slice(0, 4).map((f) => `  at ${f.fn || "?"} (${f.file}:${f.line}${f.col ? ":" + f.col : ""})`);
  const lines = [
    `A runtime error in the preview of ${nameOf(previewAgent)}'s app (${SOURCE[e.source] || e.source}${e.url ? `, ${e.url}` : ""}), ${e.count > 1 ? `${e.count} times since ${fmtClock(e.first_ms)}` : `at ${fmtClock(e.first_ms)}`}:`,
    e.message,
    ...(frames.length ? ["Stack (the repository's frames):", ...frames] : []),
  ];
  if (e.culprit) lines.push(`Workbench joined it to the edit to ${e.culprit.file} at ${fmtClock(e.culprit.edit_ms)}, ${fmtDur(e.culprit.before_ms)} before it first appeared${e.culprit.how === "stack" ? " (the stack names that file)" : ""}. Please find the cause and fix it, or say why it is not one.`);
  return lines.join("\n");
}

export function mountLab(host, ctx, getAgent) {
  const L = {
    agent: null, open: false, personas: [], available: true, why: null, adapter: null, models: ["haiku", "sonnet"],
    runs: [], run: null, findings: [], runId: null, form: { persona: "first-timer", custom: "", task: "", model: null }, starting: false, formErr: null,
    errors: [], total: 0, otlp: null, errOpen: false, errSel: null, hint: false, busy: new Set(), note: null,
  };
  const agentRow = () => (ctx.state?.agents || []).find((a) => a.id === L.agent) || null;
  const byId = (id) => (ctx.state?.agents || []).find((a) => a.id === id) || null;

  // ------------------------------------------------------------- our nodes
  const btn = h("button", { class: "pvl-btn", type: "button", "aria-expanded": "false", onclick: () => toggle() });
  const strip = h("div", { class: "pvl-strip", role: "region", "aria-label": "Runtime errors" });
  const drawer = h("aside", { class: "pvl-drawer", hidden: true, "aria-label": "Test with a persona" });
  let dropEsc = null;

  function attach() {
    if (!host.isConnected) return;
    const chromeEl = host.querySelector(".pv-chrome");
    if (chromeEl && btn.parentNode !== chromeEl) chromeEl.insertBefore(btn, chromeEl.querySelector(".pv-ov"));
    const browser = host.querySelector(".pv-browser");
    if (browser && strip.parentNode !== browser) browser.append(strip);
    const body = host.querySelector(".pv-body");
    if (body && drawer.parentNode !== body) body.append(drawer);
  }
  const mo = new MutationObserver(attach);
  mo.observe(host, { childList: true, subtree: true });

  function toggle(on = !L.open) {
    L.open = on;
    drawer.hidden = !on;
    btn.setAttribute("aria-expanded", String(on));
    dropEsc?.(); dropEsc = null;
    if (on) dropEsc = onEscape(() => toggle(false));
    renderBtn();
    if (on) renderDrawer();
  }

  // ------------------------------------------------------------------ data
  const q = (o) => Object.entries(o).filter(([, v]) => v != null && v !== "").map(([k, v]) => `${k}=${encodeURIComponent(v)}`).join("&");
  async function loadErrors() {
    const a = agentRow();
    if (!a) return;
    try {
      const r = await ctx.api(`/api/runtime/errors?${q({ repo: a.repo || null, agent: a.repo ? null : a.id, limit: 50 })}`);
      L.errors = r.errors || [];
      L.total = r.total ?? L.errors.length;
      L.otlp = r.otlp || null;
      L.errMissing = r.available === false ? r.why : null;
    } catch (e) { L.errMissing = e.message || String(e); }
    renderStrip();
  }
  async function loadRuns() {
    const a = L.agent;
    if (!a) return;
    try {
      const [p, r] = await Promise.all([
        L.personas.length ? null : ctx.api("/api/swarm/personas").catch((e) => ({ personas: [], available: false, why: e.message })),
        ctx.api(`/api/swarm/runs?${q({ agent: a })}`),
      ]);
      if (p) Object.assign(L, { personas: p.personas || [], available: p.available !== false, why: p.why || null, adapter: p.adapter || null, models: p.models || L.models });
      if (p && !L.form.model) L.form.model = p.default_model || L.models[0];
      L.runs = r.runs || [];
      if (r.available === false) { L.available = false; L.why = r.why || L.why; }
      const live = L.runs.find((x) => x.state === "running" || x.state === "starting");
      if (!L.runId && live) L.runId = live.id;
      if (L.runId) await loadRun();
    } catch (e) { L.why = e.message || String(e); }
    renderBtn();
    renderDrawer();
  }
  async function loadRun() {
    if (!L.runId) { L.run = null; L.findings = []; return; }
    try {
      const r = await ctx.api(`/api/swarm/runs/${encodeURIComponent(L.runId)}`);
      L.run = r.run; L.findings = r.findings || [];
      const i = L.runs.findIndex((x) => x.id === r.run.id);
      if (i >= 0) L.runs[i] = { ...L.runs[i], ...r.run };
    } catch { L.run = null; L.findings = []; }
  }
  const live = () => L.run && (L.run.state === "running" || L.run.state === "starting");

  async function start() {
    const f = L.form;
    if (!f.task.trim()) { L.formErr = "Say what the persona should try to do."; renderDrawer(); return; }
    if (f.persona === "custom" && !f.custom.trim()) { L.formErr = "Describe the person in a few words."; renderDrawer(); return; }
    L.starting = true; L.formErr = null; renderDrawer();
    try {
      const body = { agent: L.agent, task: f.task.trim(), model: f.model || undefined };
      if (f.persona === "custom") body.persona_text = f.custom.trim(); else body.persona = f.persona;
      const r = await ctx.api("/api/swarm/runs", { method: "POST", body });
      if (r.available === false || !r.run) { L.available = false; L.why = r.why || "The persona runner is not available."; }
      else { L.runId = r.run.id; L.run = r.run; L.findings = []; L.runs.unshift(r.run); f.task = ""; }
    } catch (e) { L.formErr = e.message || String(e); }
    L.starting = false;
    renderBtn();
    renderDrawer();
  }
  async function act(key, fn) {
    if (L.busy.has(key)) return;
    L.busy.add(key); renderDrawer(); renderStrip();
    try { await fn(); } catch (e) { L.note = { key, text: e.message || String(e), bad: true }; }
    L.busy.delete(key);
    renderDrawer(); renderStrip();
  }
  const finding = (f, verb) => act(f.id, async () => {
    const r = await ctx.api(`/api/swarm/findings/${encodeURIComponent(f.id)}/${verb}`, { method: "POST", body: {} });
    Object.assign(f, r.finding || {});
    if (verb === "ask") L.note = { key: f.id, text: "Asked in Decide." };
  });

  // ------------------------------------------------------------ the button
  function renderBtn() {
    const run = live() ? L.run : null;
    btn.classList.toggle("on", L.open);
    btn.classList.toggle("live", !!run);
    btn.title = "A simulated user tries a task on this app and reports what got in the way";
    btn.replaceChildren(run ? h("span", { class: "pvl-pulse", "aria-hidden": "true" }) : h("span", { class: "pvl-ico", "aria-hidden": "true" }, "◎"),
      run ? `Testing · step ${run.step_count || 0}` : "Test with a persona");
  }

  // ------------------------------------------------------------- the strip
  function culpritLine(e, short) {
    const c = e.culprit;
    if (!c) return h("span", { class: "lv-sub" }, short ? "no edit before it" : "No agent's edit before it in that worktree: it may come from outside the agents' work.");
    const a = c.agent || byId(c.agent_id) || { id: c.agent_id, name: c.agent_id };
    return h("span", { class: "pvl-cul" }, short ? "probably " : "Probably ", avatar(a, 16), h("b", null, `${nameOf(a)}'s`), " edit to ",
      h("code", { class: "lv-code" }, short ? base(c.file) : c.file), ` at ${fmtClock(c.edit_ms)}`);
  }
  function renderStrip() {
    const errs = L.errors;
    strip.classList.toggle("open", L.errOpen && errs.length > 0);
    strip.classList.toggle("ok", !errs.length);
    if (!errs.length) {
      // replaceChildren writes a null as the text "null" (SPEC.md, "Sixth build: E").
      strip.replaceChildren(...[h("div", { class: "pvl-sh" },
        h("span", { class: "pvl-dot ok", "aria-hidden": "true" }),
        h("b", null, "No runtime errors"),
        h("span", { class: "lv-sub pvl-trunc" }, L.errMissing ? ` · ${L.errMissing}` : " · what the page throws, its 5xx and its server's OTLP logs show up here, with the edit that probably caused them"),
        h("button", { class: "lv-link pvl-hintb", onclick: () => { L.hint = !L.hint; renderStrip(); } }, L.hint ? "Hide" : "Server logs?")),
      L.hint ? otlpHint() : null].filter(Boolean));
      return;
    }
    const top = errs[0];
    const head = h("button", { class: "pvl-sh", type: "button", "aria-expanded": String(L.errOpen), onclick: () => { L.errOpen = !L.errOpen; renderStrip(); } },
      h("span", { class: "pvl-dot", "aria-hidden": "true" }),
      h("b", null, `${errs.length} runtime error${errs.length > 1 ? "s" : ""}`),
      L.errOpen ? h("span", { class: "lv-sub" }, " in this app and its server, newest first") : [h("code", { class: "pvl-msg pvl-trunc" }, top.message), h("span", { class: "pvl-sep" }, "·"), culpritLine(top, true)],
      h("span", { class: "pvl-chev", "aria-hidden": "true" }, L.errOpen ? "▾" : "▴"));
    if (!L.errOpen) { strip.replaceChildren(head); return; }
    strip.replaceChildren(head, h("div", { class: "pvl-list" }, errs.map(errRow)));
  }
  function errRow(e) {
    const sel = L.errSel === e.id;
    const tag = e.source === "proxy" && e.status ? String(e.status) : SOURCE[e.source] || e.source;
    const where = e.file ? `${base(e.file)}:${e.line}` : e.url || "";
    const row = h("div", { class: "pvl-err" + (sel ? " sel" : "") + (e.level === "warn" ? " warn" : "") },
      h("button", { class: "pvl-erh", type: "button", "aria-expanded": String(sel), onclick: () => { L.errSel = sel ? null : e.id; renderStrip(); } },
        h("span", { class: "pvl-tag" + (e.level === "warn" ? " warn" : "") }, tag),
        h("code", { class: "pvl-msg pvl-trunc" }, e.message),
        e.count > 1 ? h("span", { class: "pvl-n" }, `×${e.count}`) : null,
        h("span", { class: "lv-sub pvl-where" }, [where, e.service ? ` · ${e.service}` : "", ` · ${fmtAgo(e.last_ms)}`].join("")),
        sel ? null : h("span", { class: "pvl-culs" }, culpritLine(e, true))));
    if (sel) row.append(errDetail(e));
    return row;
  }
  function errDetail(e) {
    const c = e.culprit;
    const a = c ? (c.agent || byId(c.agent_id)) : null;
    const preview = agentRow();
    const target = a || preview;
    const k = "steer:" + e.id;
    const frames = (e.frames || []).slice(0, 6);
    return h("div", { class: "pvl-det" },
      h("div", { class: "pvl-stack" },
        h("div", { class: "pvl-sth" }, `${e.count > 1 ? `${e.count} times since ${fmtClock(e.first_ms)}` : `at ${fmtClock(e.first_ms)}`} · from ${e.agent_name || nameOf(byId(e.agent))}'s ${e.source === "otlp" ? "server (OTLP)" : e.source === "proxy" ? "preview, through the proxy" : "page"}${e.url ? ` · ${e.url}` : ""}`),
        frames.length ? frames.map((f) => h("div", { class: "pvl-fr" + (f.in_repo ? " in" : "") + (c && f.file === c.file ? " hit" : "") },
          h("span", { class: "pvl-fn" }, f.fn || "?"), h("span", { class: "pvl-ff" }, `${f.file}:${f.line}${f.col ? ":" + f.col : ""}`)))
          : h("pre", { class: "pvl-pre" }, (e.stack || e.message).slice(0, 1200))),
      h("div", { class: "pvl-why" },
        c ? avatar(a, 30) : h("span", { class: "pvl-q", "aria-hidden": "true" }, "?"),
        h("div", { class: "pvl-whyb" },
          h("div", { class: "pvl-whyt" }, culpritLine(e, false)),
          c ? h("div", { class: "lv-sub" }, `${fmtDur(c.before_ms)} before it first appeared · `, c.how === "stack" ? "the stack names this file" : "the last edit in that worktree before it", c.edit_line ? ` · edited line ${c.edit_line}` : "") : null,
          h("div", { class: "pvl-acts" },
            c ? h("button", { class: "btn sm primary", onclick: () => ctx.openWhy({ agent: a.id, path: c.file, line: c.line || c.edit_line || 1 }) }, `Why? ${base(c.file)}:${c.line || c.edit_line}`) : null,
            c ? h("button", { class: "btn sm", onclick: () => ctx.openRewind({ agent: a.id, from_ms: c.edit_ms - 30000, to_ms: c.edit_ms + 4000, edit_ms: c.edit_ms, label: `${nameOf(a)} edits ${c.file}, ${fmtDur(c.before_ms)} before “${e.message.slice(0, 80)}”` }) }, "Rewind") : null,
            target ? h("button", { class: "btn sm", disabled: L.busy.has(k) || target.running === false, onclick: () => act(k, async () => {
              await ctx.api(`/api/agents/${encodeURIComponent(target.id)}/steer`, { method: "POST", body: { text: errorSteer(e, preview) } });
              L.note = { key: k, text: `Sent to ${nameOf(target)}.` };
            }) }, L.busy.has(k) ? "Sending…" : `Send to ${nameOf(target)}`) : null,
            h("button", { class: "btn sm ghost", onclick: () => act("res:" + e.id, async () => {
              await ctx.api(`/api/runtime/errors/${encodeURIComponent(e.id)}/resolve`, { method: "POST", body: {} });
              L.errors = L.errors.filter((x) => x.id !== e.id);
            }) }, "Resolved"),
            L.note?.key === k ? h("span", { class: L.note.bad ? "err" : "lv-sub" }, L.note.text) : null))));
  }
  function otlpHint() {
    const ep = L.otlp?.endpoint || `${location.origin}/v1/logs`;
    return h("div", { class: "pvl-hint" },
      h("div", null, "A dev server started by an agent has ", h("code", { class: "lv-code" }, "CHLOEDB_WORKBENCH_URL"), ", ", h("code", { class: "lv-code" }, "_TOKEN"), " and ", h("code", { class: "lv-code" }, "_AGENT"), " in its environment. Send its OpenTelemetry logs (OTLP/HTTP, JSON) here:"),
      h("pre", { class: "pvl-pre" }, [
        "OTEL_EXPORTER_OTLP_ENDPOINT=$CHLOEDB_WORKBENCH_URL",
        "OTEL_EXPORTER_OTLP_PROTOCOL=http/json",
        "OTEL_EXPORTER_OTLP_HEADERS=\"Authorization=Bearer $CHLOEDB_WORKBENCH_TOKEN,x-workbench-agent=$CHLOEDB_WORKBENCH_AGENT\"",
      ].join("\n")),
      h("div", { class: "lv-sub" }, `Logs of level ERROR and above, and records with an exception, become errors here (${ep}).`));
  }

  // ------------------------------------------------------------- the drawer
  function renderDrawer() {
    if (!L.open) return;
    const a = agentRow();
    const scroll = drawer.querySelector(".pvl-dbody")?.scrollTop || 0;
    const bodyEl = h("div", { class: "pvl-dbody" },
      !L.available ? h("div", { class: "pvl-callout" }, h("b", null, "The persona runner is not available here."), h("div", null, L.why || "")) : null,
      L.run ? runView(L.run) : formView(a),
      L.run ? null : earlier());
    drawer.replaceChildren(
      h("div", { class: "pvl-dh" },
        h("div", { class: "pvl-dht" }, h("b", null, "Test with a persona"),
          h("div", { class: "lv-sub" }, a ? `A simulated user tries ${nameOf(a)}'s app in a real browser.` : "Pick an agent with a preview.")),
        L.run ? h("button", { class: "btn sm", onclick: () => { L.runId = null; L.run = null; L.findings = []; renderDrawer(); } }, "New test") : null,
        h("button", { class: "btn ghost icon sm", "aria-label": "Close (Esc)", title: "Close (Esc)", onclick: () => toggle(false) }, "×")),
      bodyEl);
    bodyEl.scrollTop = scroll;
  }

  function formView(a) {
    const f = L.form;
    const list = [...L.personas, { id: "custom", name: "Someone else", role: "describe them in a few words", traits: [] }];
    const ta = h("textarea", { class: "pvl-ta", rows: "2", placeholder: "Buy two linen totes with the code SAVE10 and pay with the saved card", "aria-label": "The task", oninput: (e) => { f.task = e.target.value; } });
    import("./complete.js").then((m) => m.attachComplete(ta, { field: "persona", ctx, agent: a && a.id, repo: a && a.repo })).catch(() => {}); // Fifth build: autocomplete
    ta.value = f.task;
    return h("div", { class: "pvl-form" },
      h("div", { class: "pvl-lab" }, "Who"),
      h("div", { class: "pvl-pgrid", role: "radiogroup", "aria-label": "Persona" }, list.map((p) => h("button", {
        class: "pvl-pc" + (f.persona === p.id ? " sel" : ""), type: "button", role: "radio", "aria-checked": String(f.persona === p.id),
        onclick: () => { f.persona = p.id; renderDrawer(); },
      }, p.id === "custom" ? h("span", { class: "pvl-pav plus" }, "+") : personaAv(p, 26),
        h("span", { class: "pvl-pct" }, h("b", null, p.name), h("span", null, p.role)),
        p.device === "phone" ? h("span", { class: "pvl-dev", title: "on a phone" }, "phone") : null))),
      f.persona === "custom" ? h("input", { class: "pvl-in", placeholder: "a nurse on night shift, ordering on her phone between rounds", value: f.custom, oninput: (e) => { f.custom = e.target.value; }, "aria-label": "Describe the person" }) : null,
      h("div", { class: "pvl-lab" }, "Task"),
      ta,
      h("div", { class: "pvl-sugg" }, TASKS.map((t) => h("button", { class: "pvl-chip", type: "button", onclick: () => { f.task = t; renderDrawer(); } }, t))),
      h("div", { class: "pvl-go" },
        h("label", { class: "pvl-model" }, "Model ", h("select", { onchange: (e) => { f.model = e.target.value; } }, L.models.map((m) => h("option", { value: m, selected: m === f.model ? true : null }, m)))),
        h("button", { class: "btn primary", disabled: L.starting || !L.available || !a?.preview_url, onclick: start }, L.starting ? "Starting…" : "Start the test")),
      h("div", { class: "lv-sub pvl-fine" }, !a?.preview_url ? `${nameOf(a)} has no preview to test yet.` : L.adapter === "api" ? "Uses your API key; about 25 steps at most, then a review of what it saw." : "Uses claude -p with your Claude login; about 25 steps at most, then a review of what it saw."),
      L.formErr ? h("div", { class: "err" }, L.formErr) : null);
  }

  function earlier() {
    const runs = L.runs.filter((r) => r.id !== L.runId);
    if (!runs.length) return null;
    return h("div", { class: "pvl-sec" }, h("div", { class: "pvl-lab" }, "Earlier tests"),
      runs.slice(0, 8).map((r) => h("button", { class: "pvl-rr", type: "button", onclick: () => { L.runId = r.id; L.allSteps = false; loadRun().then(renderDrawer); } },
        personaAv(r.persona, 24),
        h("span", { class: "pvl-rrb" }, h("b", null, r.persona?.name || "?"), h("span", { class: "pvl-trunc" }, r.task)),
        outcomePill(r),
        h("span", { class: "lv-sub pvl-ago" }, r.findings_count ? `${r.findings_count} finding${r.findings_count > 1 ? "s" : ""} · ` : "", fmtAgo(r.created_ms)))));
  }

  function outcomePill(r) {
    if (r.state === "running" || r.state === "starting") return pill(r.state === "starting" ? "starting" : `step ${r.step_count || 0}`, "acc");
    const o = r.outcome || r.state;
    return pill({ done: "done", gave_up: "gave up", out_of_steps: "out of steps", out_of_time: "out of time", stopped: "stopped", error: "failed", failed: "failed" }[o] || o, o === "done" ? "good" : o === "gave_up" || o === "out_of_steps" || o === "out_of_time" ? "warn" : o === "stopped" ? "mut" : "crit");
  }

  function runView(r) {
    const steps = r.steps || [];
    const running = r.state === "running" || r.state === "starting";
    const by = r.by && r.by !== "you" ? byId(r.by) : null;
    const preview = byId(r.agent) || agentRow();
    return h("div", { class: "pvl-run" },
      h("div", { class: "pvl-rh" },
        personaAv(r.persona, 34),
        h("div", { class: "pvl-rhb" }, h("div", null, h("b", null, r.persona?.name), " · ", h("span", { class: "lv-sub" }, r.persona?.role)),
          h("div", { class: "pvl-task" }, "“", r.task, "”")),
        outcomePill(r)),
      h("div", { class: "pvl-meta lv-sub" },
        `${r.model}${r.adapter === "claude -p" ? " via claude -p" : r.adapter === "api" ? " via your API key" : ""} · started ${fmtClock(r.created_ms)} by `, by ? by.name : "you",
        r.ended_ms ? ` · ${fmtDur(r.ended_ms - r.created_ms)}` : "",
        running ? h("button", { class: "lv-link pvl-stop", onclick: () => act("stop:" + r.id, async () => { const x = await ctx.api(`/api/swarm/runs/${encodeURIComponent(r.id)}/stop`, { method: "POST", body: {} }); L.run = x.run || L.run; }) }, "Stop") : null),
      running ? h("div", { class: "pvl-prog" }, h("b", { style: { width: Math.min(100, ((r.step_count || 0) / (r.max_steps || 25)) * 100) + "%" } })) : null,
      r.error ? h("div", { class: "err" }, r.error) : null,
      findingsView(preview),
      h("div", { class: "pvl-sec" }, h("div", { class: "pvl-lab" }, `Steps`, h("span", { class: "n" }, String(steps.length))),
        steps.length > 6 && !L.allSteps ? h("button", { class: "lv-link pvl-more", onclick: () => { L.allSteps = true; renderDrawer(); } }, `Show ${steps.length - 6} earlier step${steps.length - 6 > 1 ? "s" : ""}`) : null,
        h("ol", { class: "pvl-steps" }, (L.allSteps ? steps : steps.slice(-6)).map((s) => h("li", { class: "pvl-step" + (s.ok === false || (s.events || []).length ? " bad" : "") },
          h("span", { class: "pvl-sn" }, String(s.step)),
          h("div", { class: "pvl-sb" },
            h("div", { class: "pvl-st" }, h("span", null, stepText(s)), s.url ? h("code", { class: "pvl-url" }, s.url) : null),
            s.thought ? h("div", { class: "pvl-th" }, `“${s.thought}”`) : null,
            s.error ? h("div", { class: "pvl-ev" }, s.error) : null,
            (s.events || []).map((ev) => h("div", { class: "pvl-ev" }, `${ev.kind === "http" ? "" : ev.kind + ": "}${ev.text}`))))),
          running ? h("li", { class: "pvl-step next" }, h("span", { class: "pvl-sn" }, h("span", { class: "spin" })), h("div", { class: "pvl-sb lv-sub" }, `${r.persona?.name?.split(" ")[0] || "The persona"} is deciding what to do next…`)) : null)),
      r.answer ? h("div", { class: "pvl-sec" }, h("div", { class: "pvl-lab" }, `${r.persona?.name?.split(" ")[0] || "Their"}'s answer`), h("p", { class: "pvl-ans" }, r.answer)) : null);
  }

  function findingsView(preview) {
    const list = L.findings.filter((f) => f.state !== "dismissed");
    const running = live();
    return h("div", { class: "pvl-sec" }, h("div", { class: "pvl-lab" }, "Findings", h("span", { class: "n" }, String(list.length))),
      list.length ? list.map((f) => {
        const [sev, tone] = SEV[f.severity] || SEV[1];
        const busy = L.busy.has(f.id);
        return h("div", { class: "pvl-f s" + f.severity },
          h("div", { class: "pvl-fh" }, pill(sev, tone), h("b", null, f.title)),
          h("div", { class: "pvl-fd" }, f.detail),
          (f.step != null || f.url) ? h("div", { class: "pvl-fm lv-sub" }, [f.step != null ? `step ${f.step}` : "", f.url || ""].filter(Boolean).join(" · ")) : null,
          h("div", { class: "pvl-acts" },
            f.state === "sent" ? [pill(`sent to ${nameOf(preview)}`, "good"), h("span", { class: "lv-sub" }, fmtAgo(f.updated_ms))]
              : f.state === "asked" ? [pill("waiting in Decide", "acc"), h("button", { class: "lv-link", onclick: () => ctx.go("#/decide") }, "Open Decide")]
                : [
                  h("button", { class: "btn sm primary", disabled: busy || preview?.running === false, onclick: () => finding(f, "send") }, busy ? "…" : `Send to ${nameOf(preview)}`),
                  h("button", { class: "btn sm", disabled: busy, onclick: () => finding(f, "ask") }, "Ask me"),
                  h("button", { class: "btn sm ghost", disabled: busy, onclick: () => finding(f, "dismiss") }, "Dismiss")],
            L.note?.key === f.id && L.note.bad ? h("span", { class: "err" }, L.note.text) : null));
      }) : h("div", { class: "lv-sub pvl-none" }, running ? "Nothing yet: findings show up here as the persona hits them." : "Nothing got in the way."));
  }

  // ---------------------------------------------------------------- ticking
  let n = 0;
  const timer = setInterval(async () => {
    if (!host.isConnected) return stop();
    const id = getAgent();
    if (id !== L.agent) return reload();
    n++;
    const visible = document.visibilityState !== "hidden";
    if (live()) { await loadRun(); renderBtn(); renderDrawer(); if (!live()) loadRuns(); }
    if (visible && n % 4 === 0) loadErrors();
  }, 1500);
  function reload() {
    L.agent = getAgent();
    Object.assign(L, { runs: [], run: null, runId: null, findings: [], errors: [], errSel: null, allSteps: false });
    renderBtn(); renderStrip(); renderDrawer();
    loadErrors();
    loadRuns();
  }
  function stop() {
    clearInterval(timer);
    mo.disconnect();
    dropEsc?.();
    btn.remove(); strip.remove(); drawer.remove();
  }

  attach();
  reload();
  // Sixth build: Health's "Test with a persona" opens this drawer (ui/levels/health.js).
  try { const want = JSON.parse(sessionStorage.getItem("wb.persona") || "null"); if (want && want.agent === L.agent) { sessionStorage.removeItem("wb.persona"); toggle(true); } } catch {}
  return { unmount: stop, open: () => toggle(true), refresh: reload };
}

useStyle("preview-lab", `
.pv-body{position:relative}
.pvl-btn{display:inline-flex;align-items:center;gap:6px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);font:600 11.5px var(--sans);padding:4px 11px;cursor:pointer;white-space:nowrap}
.pvl-btn:hover{color:var(--text);border-color:var(--line-2)}
.pvl-btn.on{background:var(--accent-soft);color:var(--accent);border-color:color-mix(in srgb,var(--accent) 45%,transparent)}
.pvl-btn.live{color:var(--accent)}
.pvl-ico{font-size:13px;line-height:1}
.pvl-pulse{width:8px;height:8px;border-radius:50%;background:var(--accent);box-shadow:0 0 0 0 var(--focus);animation:pvl-p 1.4s infinite}
@keyframes pvl-p{0%{box-shadow:0 0 0 0 var(--focus)}70%{box-shadow:0 0 0 7px transparent}100%{box-shadow:0 0 0 0 transparent}}
.pvl-strip{flex:none;border-top:1px solid var(--line);background:var(--surface);display:flex;flex-direction:column;min-height:0;max-height:52%}
.pvl-strip:not(.ok){border-top-color:color-mix(in srgb,var(--crit) 45%,var(--line))}
.pvl-sh{display:flex;align-items:center;gap:8px;min-width:0;width:100%;height:38px;padding:0 14px;border:0;background:none;color:var(--text);font:inherit;text-align:left;cursor:pointer;flex:none}
.pvl-strip:not(.ok) .pvl-sh{background:color-mix(in srgb,var(--crit) 7%,transparent)}
.pvl-strip.ok .pvl-sh{cursor:default}
.pvl-sh b{white-space:nowrap}
.pvl-dot{width:8px;height:8px;border-radius:50%;background:var(--crit);flex:none;box-shadow:0 0 0 3px color-mix(in srgb,var(--crit) 22%,transparent)}
.pvl-dot.ok{background:var(--good);box-shadow:none}
.pvl-trunc{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0}
.pvl-msg{font:12px var(--mono);color:var(--crit-ink);flex:0 1 auto}
.pvl-sep{color:var(--muted)}
.pvl-cul{white-space:nowrap;color:var(--text-2)}.pvl-cul b{color:var(--text);font-weight:650}
.pvl-cul .lv-av{font-size:9px;border-radius:5px;margin:0 4px 0 1px;vertical-align:-2px}
.pvl-chev{margin-left:auto;color:var(--muted);font-size:11px;padding-left:6px}
.pvl-hintb{margin-left:auto;white-space:nowrap;font-size:12px}
.pvl-hint{padding:4px 14px 12px;display:flex;flex-direction:column;gap:6px;color:var(--text-2);font-size:12.5px}
.pvl-pre{margin:0;padding:8px 10px;border-radius:8px;background:var(--sunken);border:1px solid var(--line);font:11.5px/1.5 var(--mono);color:var(--text-2);white-space:pre-wrap;word-break:break-all}
.pvl-list{overflow:auto;min-height:0;border-top:1px solid var(--line)}
.pvl-err{border-bottom:1px solid var(--line)}
.pvl-err:last-child{border-bottom:0}
.pvl-erh{display:flex;align-items:center;gap:9px;width:100%;min-width:0;padding:8px 14px;border:0;background:none;color:var(--text);font:inherit;text-align:left;cursor:pointer}
.pvl-erh:hover,.pvl-err.sel .pvl-erh{background:var(--surface-2)}
.pvl-tag{flex:none;font:700 10px/18px var(--sans);text-transform:uppercase;letter-spacing:.04em;padding:0 7px;border-radius:5px;background:var(--crit-soft);color:var(--crit-ink)}
.pvl-tag.warn{background:var(--warn-soft);color:var(--warn-ink)}
.pvl-err.warn .pvl-msg{color:var(--warn-ink)}
.pvl-n{flex:none;font:700 11px var(--sans);color:var(--text-2);background:var(--surface-3);border-radius:999px;padding:0 7px;line-height:18px}
.pvl-where{flex:none;font:11.5px var(--mono);white-space:nowrap}
.pvl-culs{margin-left:auto;flex:none;font-size:12px}
.pvl-det{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1.2fr);gap:14px;padding:4px 14px 14px 14px;background:var(--surface-2)}
.pvl-stack{min-width:0;display:flex;flex-direction:column;gap:2px}
.pvl-sth{font-size:12px;color:var(--muted);margin-bottom:5px}
.pvl-fr{display:flex;gap:8px;font:11.5px/1.6 var(--mono);color:var(--muted);min-width:0;padding:0 6px;border-radius:5px}
.pvl-fr.in{color:var(--text-2)}.pvl-fr.in .pvl-fn{color:var(--text)}
.pvl-fr.hit{background:color-mix(in srgb,var(--crit) 12%,transparent)}
.pvl-fn{flex:none}.pvl-ff{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.pvl-why{display:flex;gap:11px;align-items:flex-start;padding:12px;border-radius:10px;background:var(--surface);border:1px solid var(--line-2);box-shadow:var(--shadow-1);min-width:0}
.pvl-why>.lv-av{border-radius:8px;font-size:13px}
.pvl-q{width:30px;height:30px;border-radius:8px;display:grid;place-items:center;background:var(--surface-3);color:var(--muted);font-weight:700;flex:none}
.pvl-whyb{min-width:0;display:flex;flex-direction:column;gap:4px}
.pvl-whyt{font-size:13.5px}.pvl-whyt .pvl-cul{white-space:normal;flex-wrap:wrap;display:inline}
.pvl-whyt .pvl-cul .lv-av{display:none}
.pvl-acts{display:flex;flex-wrap:wrap;align-items:center;gap:6px;margin-top:6px}
.pvl-drawer{position:absolute;top:0;right:0;bottom:0;width:400px;z-index:6;display:flex;flex-direction:column;min-height:0;background:var(--surface);border:1px solid var(--line-2);border-radius:12px;box-shadow:var(--shadow-2);overflow:hidden}
.pvl-drawer[hidden]{display:none}
.pvl-dh{display:flex;align-items:flex-start;gap:8px;padding:14px 14px 12px 16px;border-bottom:1px solid var(--line);flex:none}
.pvl-dht{flex:1;min-width:0}.pvl-dht b{font-size:15px}.pvl-dht .lv-sub{font-size:12.5px;margin-top:2px}
.pvl-dbody{flex:1 1 auto;min-height:0;overflow:auto;padding:14px 16px 18px;display:flex;flex-direction:column;gap:16px}
.pvl-callout{border:1px solid color-mix(in srgb,var(--warn) 45%,transparent);background:var(--warn-soft);color:var(--warn-ink);border-radius:9px;padding:10px 12px;font-size:12.5px;display:flex;flex-direction:column;gap:3px}
.pvl-lab{font-size:11px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);margin-bottom:8px}
.pvl-lab .n{margin-left:6px;font-weight:600;color:var(--text-2)}
.pvl-form{display:flex;flex-direction:column}
.pvl-form .pvl-lab{margin-top:4px}
.pvl-pgrid{display:flex;flex-direction:column;gap:6px;margin-bottom:12px}
.pvl-pc{display:flex;align-items:center;gap:10px;padding:8px 10px;border:1px solid var(--line);border-radius:9px;background:var(--surface);color:var(--text);font:inherit;text-align:left;cursor:pointer;min-width:0}
.pvl-pc:hover{border-color:var(--line-2)}
.pvl-pc.sel{border-color:var(--accent);background:var(--accent-soft);box-shadow:0 0 0 1px var(--accent) inset}
.pvl-pct{display:flex;flex-direction:column;min-width:0;flex:1}.pvl-pct b{font-weight:650}.pvl-pct span{color:var(--muted);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.pvl-dev{font:600 10px var(--sans);text-transform:uppercase;letter-spacing:.05em;color:var(--muted);border:1px solid var(--line);border-radius:5px;padding:1px 5px}
.pvl-pav{display:inline-grid;place-items:center;border-radius:50%;flex:none;font:700 10.5px var(--sans);color:hsl(var(--ph) 45% 22%);background:hsl(var(--ph) 70% 84%)}
.pvl-pav.plus{width:26px;height:26px;background:var(--surface-3);color:var(--text-2);font-size:15px}
.pvl-ta,.pvl-in{width:100%;border:1px solid var(--line-2);border-radius:8px;background:var(--sunken);color:var(--text);padding:8px 10px;font:inherit;outline:none;resize:vertical}
.pvl-in{margin:-4px 0 12px}
.pvl-ta:focus,.pvl-in:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.pvl-sugg{display:flex;flex-wrap:wrap;gap:5px;margin:8px 0 14px}
.pvl-chip{border:1px solid var(--line);border-radius:999px;background:var(--surface-2);color:var(--text-2);font:12px var(--sans);padding:3px 9px;cursor:pointer}
.pvl-chip:hover{color:var(--text);border-color:var(--line-2)}
.pvl-go{display:flex;align-items:center;gap:10px}.pvl-go .btn{margin-left:auto}
.pvl-model{display:flex;align-items:center;gap:6px;color:var(--muted);font-size:12.5px}
.pvl-model select{border:1px solid var(--line-2);border-radius:7px;background:var(--surface);color:var(--text);padding:4px 6px;font:inherit}
.pvl-fine{margin-top:8px;font-size:12px}
.pvl-sec{display:flex;flex-direction:column}
.pvl-rr{display:flex;align-items:center;gap:9px;padding:8px 0;border:0;border-top:1px solid var(--line);background:none;color:var(--text);font:inherit;text-align:left;cursor:pointer;min-width:0}
.pvl-rr:first-of-type{border-top:0}
.pvl-rrb{display:flex;flex-direction:column;min-width:0;flex:1}.pvl-rrb span{color:var(--muted);font-size:12px}
.pvl-ago{white-space:nowrap;font-size:12px}
.pvl-run{display:flex;flex-direction:column;gap:12px}
.pvl-rh{display:flex;gap:10px;align-items:flex-start}
.pvl-rhb{flex:1;min-width:0}.pvl-task{margin-top:3px;color:var(--text);font-size:13.5px}
.pvl-meta{font-size:12px;display:flex;flex-wrap:wrap;align-items:baseline;gap:0 3px;margin-top:-4px}
.pvl-stop{margin-left:auto;color:var(--crit-ink)}
.pvl-prog{height:4px;border-radius:999px;background:var(--surface-3);overflow:hidden;margin-top:-4px}.pvl-prog>b{display:block;height:100%;background:var(--accent);border-radius:999px;transition:width .4s}
.pvl-f{border:1px solid var(--line);border-left:3px solid var(--line-2);border-radius:9px;padding:10px 12px;margin-bottom:8px;display:flex;flex-direction:column;gap:4px;background:var(--surface)}
.pvl-f.s3{border-left-color:var(--crit)}.pvl-f.s2{border-left-color:var(--warn)}
.pvl-fh{display:flex;align-items:baseline;gap:8px}.pvl-fh b{font-weight:650;line-height:1.35}
.pvl-fd{color:var(--text-2);font-size:12.5px}
.pvl-fm{font-size:11.5px;font-family:var(--mono)}
.pvl-f .pvl-acts{margin-top:4px}
.pvl-none{font-size:12.5px;padding:2px 0 4px}
.pvl-more{align-self:flex-start;font-size:12.5px;margin:-2px 0 4px}
.pvl-steps{list-style:none;margin:0;padding:0;display:flex;flex-direction:column}
.pvl-step{display:flex;gap:10px;padding:7px 0;position:relative}
.pvl-step:not(:last-child)::after{content:"";position:absolute;left:10px;top:28px;bottom:-6px;width:1px;background:var(--line)}
.pvl-sn{flex:none;width:21px;height:21px;border-radius:50%;display:grid;place-items:center;background:var(--surface-3);color:var(--text-2);font:700 10.5px var(--sans)}
.pvl-step.bad .pvl-sn{background:var(--crit-soft);color:var(--crit-ink)}
.pvl-step.next .pvl-sn{background:var(--accent-soft)}
.pvl-sn .spin{width:10px;height:10px}
.pvl-sb{min-width:0;flex:1}
.pvl-st{display:flex;align-items:baseline;gap:8px;min-width:0}.pvl-st>span{min-width:0}
.pvl-url{margin-left:auto;font:11px var(--mono);color:var(--muted);white-space:nowrap}
.pvl-th{color:var(--muted);font-style:italic;font-size:12.5px}
.pvl-ev{color:var(--crit-ink);font:11.5px/1.45 var(--mono);margin-top:2px;word-break:break-word}
.pvl-ans{margin:0;padding:10px 12px;border-radius:9px;background:var(--surface-2);border:1px solid var(--line);color:var(--text-2)}
@media (max-width:1180px){.pvl-drawer{width:330px}.pvl-culs{display:none}}
@media (max-width:860px){.pvl-drawer{position:fixed;inset:0 0 0 auto;width:min(100vw,440px);border-radius:0;z-index:40}.pvl-det{grid-template-columns:1fr}.pvl-where{display:none}}
`);
