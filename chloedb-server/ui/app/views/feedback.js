// Feedback: what people asked of the command center, each with the
// screenshot they marked up (feedback.js takes it), to read, triage and
// put in order. A row of table cc_feedback each: GET /v1/feedback lists
// them (newest first, with thumbnails), GET /v1/feedback/<id> gives one,
// /shot its picture, PATCH sets its status, priority, assignee and
// resolution. Agents read the same rows: with SQL, these routes, or the
// MCP tools list_feedback, get_feedback and update_feedback.

import { h, icon, clear, api, toast, card, empty, failure, seg, can, needs, store, drawer, modal, confirm, copy, fmt, timeEl, sqlBlock, debounce, link } from "../core.js";
import { openFeedback } from "../feedback.js";

// A route of the command center, as the server accepts it: only such a route
// becomes a link (rows written before that check, or around it, stay text).
const isRoute = (r) => typeof r === "string" && /^#\/[^\s\x00-\x1f]*$/.test(r);

const STATUS = [["new", "New"], ["triaged", "Triaged"], ["planned", "Planned"], ["doing", "Doing"], ["done", "Done"], ["wontfix", "Won't do"]];
const OPEN = ["new", "triaged", "planned", "doing"];
const FILTERS = [["open", "Open"], ["new", "New"], ["planned", "Planned"], ["doing", "Doing"], ["done,wontfix", "Closed"], ["all", "All"]];
const KINDS = [["", "All kinds"], ["bug", "Bugs"], ["change", "Changes"], ["idea", "Ideas"], ["question", "Questions"]];
const PRIOS = [[0, "P0", "Now"], [1, "P1", "Next"], [2, "P2", "Soon"], [3, "P3", "Some day"]];
const statusName = (s) => (STATUS.find(([k]) => k === s) || [s, s])[1];
const prioName = (p) => (p == null ? "–" : "P" + p);

let lastNew = null;
let reopen = null;

export default {
  id: "feedback", title: "Feedback", icon: "feedback", group: "Change", range: false,
  keywords: "feedback screenshot annotate annotation bug bugs report idea ideas request requests change wish triage priority prioritize backlog issue issues ux design what is wrong comment",
  render, onParams: (r) => reopen?.(r.params.get("id")),
  badge: () => (lastNew ? { n: lastNew, tone: "info", title: `${fmt.plural(lastNew, "new item")} of feedback` } : null),
};

function render(page, r) {
  const head = h("div", { class: "page-head" },
    h("div", {}, h("h1", {}, "Feedback"), h("p", {}, "What people asked of this command center, with the screenshots they marked up. Put it in order here; agents read it as table cc_feedback.")),
    h("div", { class: "tools" },
      h("button", { class: "btn sm", title: "How an agent reads and takes items", onclick: () => agentHelp() }, icon("agent"), "For agents"),
      h("button", { class: "btn sm primary", title: "Take this view and mark it up (f)", onclick: () => openFeedback() }, icon("camera"), "Send feedback")));
  let filter = r.params.get("status") || store.get("fb.filter", "open");
  let kind = r.params.get("kind") || "";
  let q = r.params.get("q") || "";
  let order = store.get("fb.order", "priority");
  let data = null, alive = true;
  const counts = h("div", { class: "small dim" });
  const filterSeg = seg(FILTERS, filter, (v) => { filter = v; store.set("fb.filter", v); load(); });
  const kindSel = h("select", { class: "input sm", "aria-label": "Kind", onchange: () => { kind = kindSel.value; load(); } }, KINDS.map(([v, l]) => h("option", { value: v, selected: v === kind }, l)));
  const search = h("input", { class: "input sm", type: "search", placeholder: "Words of the note or marks", value: q, style: { width: "220px" }, oninput: debounce(() => { q = search.value.trim(); load(); }, 300) });
  const orderSeg = seg([["priority", "By priority"], ["new", "Newest"]], order, (v) => { order = v; store.set("fb.order", v); draw(); });
  const bar = h("div", { class: "row between fb-filters", style: { gap: "8px", flexWrap: "wrap", marginBottom: "12px" } },
    h("div", { class: "row nw", style: { gap: "8px", flexWrap: "wrap" } }, filterSeg, kindSel, search), h("div", { class: "row nw", style: { gap: "8px" } }, counts, orderSeg));
  const listBox = h("div", { class: "fb-list" });
  page.append(head, bar, listBox);

  async function load() {
    const p = new URLSearchParams({ status: filter, limit: "300", thumbs: "1" });
    if (kind) p.set("kind", kind);
    if (q) p.set("q", q);
    try { data = await api("/v1/feedback?" + p); }
    catch (e) { clear(listBox, failure(e, "Feedback")); return; }
    if (!alive) return;
    lastNew = data.counts?.status?.new || 0;
    draw();
  }

  function sorted() {
    const rows = (data.feedback || []).slice();
    if (order === "priority") rows.sort((a, b) => (OPEN.includes(b.status) - OPEN.includes(a.status)) || ((a.priority ?? 9) - (b.priority ?? 9)) || String(b.ts).localeCompare(String(a.ts)));
    return rows;
  }

  function draw() {
    const c = data.counts?.status || {};
    const open = OPEN.reduce((n, s) => n + (c[s] || 0), 0);
    counts.textContent = data.exists ? `${fmt.n(open)} open · ${fmt.n(c.new || 0)} new · ${fmt.n((c.done || 0) + (c.wontfix || 0))} closed` : "";
    if (!data.exists) {
      clear(listBox, card({ body: empty("feedback", "No feedback yet", "On any view, press f (or the speech bubble in the top bar): the view is taken as a picture, you draw on it what is wrong or what you want, add a note, and it lands here as a row of table cc_feedback.", h("button", { class: "btn primary", onclick: () => openFeedback() }, icon("camera"), "Send the first")) }));
      return;
    }
    const rows = sorted();
    if (!rows.length) { clear(listBox, card({ body: empty("check", "Nothing here", filter === "open" ? "No open feedback: everything was done or set aside." : "No feedback matches.") })); return; }
    clear(listBox, rows.map(item));
  }

  function item(f) {
    const prio = h("select", { class: "input sm fb-prio", "aria-label": "Priority", title: can("write", "PATCH /v1/feedback/:id") ? "Priority: P0 now, P3 some day" : "Setting the priority " + needs("write", "PATCH /v1/feedback/:id"), disabled: !can("write", "PATCH /v1/feedback/:id"), dataset: { p: f.priority ?? "" }, onclick: (e) => e.stopPropagation(), onchange: (e) => set(f, { priority: e.target.value === "" ? null : +e.target.value }) },
      h("option", { value: "", selected: f.priority == null }, "–"), PRIOS.map(([v, l, t]) => h("option", { value: v, selected: f.priority === v, title: t }, `${l} ${t}`)));
    const st = h("select", { class: "input sm", "aria-label": "Status", disabled: !can("write", "PATCH /v1/feedback/:id"), onclick: (e) => e.stopPropagation(), onchange: (e) => set(f, { status: e.target.value }) }, STATUS.map(([v, l]) => h("option", { value: v, selected: f.status === v }, l)));
    const thumb = f.thumb ? h("img", { class: "fb-thumb", src: f.thumb, alt: "", loading: "lazy" }) : h("div", { class: "fb-thumb none" }, icon("feedback"));
    const nMarks = f.marks_text ? f.marks_text.split("\n").length : 0;
    const el = h("article", { class: "fb-item card" + (OPEN.includes(f.status) ? "" : " closed"), tabIndex: 0, onclick: () => detail(f.id), onkeydown: (e) => { if (e.key === "Enter") detail(f.id); } },
      thumb,
      h("div", { class: "fb-body" },
        h("div", { class: "fb-head" }, h("span", { class: "fb-kind-pill k-" + f.kind }, f.kind), h("b", { class: "fb-t" }, f.title || "(no title)")),
        f.note && f.note !== f.title ? h("div", { class: "fb-note-x small" }, f.note) : null,
        h("div", { class: "small dim fb-meta" },
          h("span", { class: "mono" }, f.id), " · ", f.page || f.route || "a view", " · ", f.author || "someone", " · ", timeEl(Date.parse(f.ts)),
          nMarks ? ` · ${fmt.plural(nMarks, "mark")}` : "", f.assignee ? [" · ", icon("user"), " ", f.assignee] : "")),
      h("div", { class: "fb-tri" }, prio, st));
    return el;
  }

  async function set(f, patch) {
    try { const r = await api(`/v1/feedback/${encodeURIComponent(f.id)}`, { method: "PATCH", body: patch }); Object.assign(f, r); toast(`${f.id}: ${Object.entries(patch).map(([k, v]) => `${k} ${k === "priority" ? prioName(v) : k === "status" ? statusName(v) : v ?? "none"}`).join(", ")}`, "ok"); }
    catch (e) { toast(`Not changed: ${e.message}`, "bad"); }
    load();
  }

  // ---- one item
  let openId = null, drawerRef = null;
  reopen = (id) => { if (id && id !== openId) detail(id); };
  async function detail(id) {
    openId = id;
    try { history.replaceState(null, "", "#/feedback?id=" + encodeURIComponent(id)); } catch { /* old browser */ }
    let f;
    try { f = await api(`/v1/feedback/${encodeURIComponent(id)}`); }
    catch (e) { toast(e.message, "bad"); openId = null; return; }
    const shotBox = h("div", { class: "fb-shot" }, f.shot_url ? h("div", { class: "small dim" }, "Loading the screenshot…") : empty("media", "No screenshot", "This one is a note alone."));
    let shotUrl = null;
    if (f.shot_url) {
      api(f.shot_url, { blob: true }).then((r) => r.blob()).then((b) => {
        shotUrl = URL.createObjectURL(b);
        const img = h("img", { src: shotUrl, alt: "The screenshot, with its marks", title: "Open it full size", onclick: () => full(shotUrl, f) });
        clear(shotBox, img);
      }).catch((e) => clear(shotBox, h("div", { class: "error" }, "The screenshot: " + e.message)));
    }
    const marks = Array.isArray(f.marks) ? f.marks.filter((m) => m.tool !== "redact") : [];
    const writer = can("write", "PATCH /v1/feedback/:id");
    const prio = seg([["", "–"], ...PRIOS.map(([v, l, t]) => [String(v), l, t])], f.priority == null ? "" : String(f.priority), () => {});
    const st = seg(STATUS.map(([v, l]) => [v, l]), f.status, () => {});
    const assignee = h("input", { class: "input", value: f.assignee || "", placeholder: "Who works on it: a person, or an agent's thread", disabled: !writer });
    const resolution = h("textarea", { class: "input", rows: 2, placeholder: "What was done: a pull request's link, or why not", disabled: !writer }, f.resolution || "");
    const save = h("button", { class: "btn primary", disabled: !writer, title: writer ? "" : needs("write", "PATCH /v1/feedback/:id") }, "Save");
    save.onclick = async () => {
      const pv = prio.querySelector("[aria-pressed=true]")?.dataset.v ?? "";
      const body = { status: st.querySelector("[aria-pressed=true]")?.dataset.v || f.status, priority: pv === "" ? null : +pv, assignee: assignee.value.trim() || null, resolution: resolution.value.trim() || null };
      try { await api(`/v1/feedback/${encodeURIComponent(id)}`, { method: "PATCH", body }); toast("Saved", "ok"); drawerRef?.close(); load(); }
      catch (e) { toast("Not saved: " + e.message, "bad"); }
    };
    const del = can("admin") ? h("button", { class: "btn danger", onclick: async () => {
      if (!(await confirm({ title: `Delete ${id}?`, body: "The row and its screenshot are deleted for good. Won't do keeps it, closed.", ok: "Delete", danger: true }))) return;
      try { await api(`/v1/feedback/${encodeURIComponent(id)}`, { method: "DELETE" }); toast("Deleted", "ok"); drawerRef?.close(); load(); } catch (e) { toast(e.message, "bad"); }
    } }, icon("trash"), "Delete") : null;
    const ctx = f.context || {};
    const facts = [
      ["Kind", f.kind], ["Sent", [timeEl(Date.parse(f.ts)), " by ", f.author || "someone"]],
      ["View", isRoute(f.route) ? h("a", { href: f.route, onclick: () => drawerRef?.close() }, `${ctx.title || f.page || "the view"} (${f.route})`) : (f.page || f.route)],
      ["Window", ctx.viewport ? `${ctx.viewport.w} × ${ctx.viewport.h} at ${ctx.viewport.dpr}x, ${ctx.theme || ""}${ctx.capture ? `, taken ${ctx.capture === "page" ? "from the page" : ctx.capture === "screen" ? "from the screen" : "without a picture"}` : ""}` : null],
      ["Server", [f.cluster_name, f.server_version ? ` · ${f.server_version}` : "", ctx.backend ? ` · ${ctx.backend}` : ""]],
      f.updated ? ["Changed", [timeEl(Date.parse(f.updated)), " by ", f.updated_by || "someone"]] : null,
    ].filter((x) => x && x[1] != null && x[1] !== "");
    drawerRef = drawer({
      title: f.title || id, sub: h("span", { class: "mono" }, id), wide: true,
      onClose: () => { openId = null; if (shotUrl) URL.revokeObjectURL(shotUrl); try { history.replaceState(null, "", "#/feedback"); } catch { /* old browser */ } },
      body: h("div", { class: "stack" },
        shotBox,
        f.note ? h("div", { class: "fb-full-note" }, f.note) : null,
        marks.length ? card({ title: "Marks", sub: "numbered on the screenshot, with what was under each", flush: true, body: h("ol", { class: "fb-marklist" }, marks.map((m) => h("li", {},
          h("span", { class: "fb-n", style: { background: m.color || "var(--crit)" } }, String(m.n)),
          h("div", {}, h("b", {}, m.text || m.tool), m.target ? h("div", { class: "small dim" }, [m.target.card ? `in “${m.target.card}” · ` : "", m.target.text ? `“${m.target.text}” · ` : "", h("span", { class: "mono" }, m.target.selector || "")]) : null)))) }) : null,
        ctx.errors?.length ? card({ title: "Errors on the page", flush: true, body: h("pre", { class: "code", style: { margin: 0, border: 0 } }, ctx.errors.map((e) => `${e.at}  ${e.msg}`).join("\n")) }) : null,
        card({ title: "Triage", body: h("div", { class: "stack", style: { gap: "10px" } },
          h("div", {}, h("div", { class: "small dim" }, "Status"), st),
          h("div", {}, h("div", { class: "small dim" }, "Priority: P0 now, P1 next, P2 soon, P3 some day"), prio),
          h("label", {}, h("div", { class: "small dim" }, "Assignee"), assignee),
          h("label", {}, h("div", { class: "small dim" }, "Resolution"), resolution)) }),
        h("dl", { class: "kv" }, facts.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]))),
      foot: [del, h("div", { class: "grow" }), h("button", { class: "btn", title: "A prompt that says what to do and how to mark it done", onclick: () => copy(agentPrompt(f, marks), "Copied: paste it to an agent") }, icon("copy"), "Copy for an agent"), save],
    });
  }

  function full(url, f) {
    modal({ title: f.title || f.id, wide: true, body: h("img", { src: url, alt: "The screenshot", style: { width: "100%", borderRadius: "8px", border: "1px solid var(--line)" } }) });
  }

  load();
  const fresh = () => load();
  addEventListener("chloedb:feedback", fresh);
  if (r.params.get("id")) detail(r.params.get("id"));
  return () => { alive = false; reopen = null; removeEventListener("chloedb:feedback", fresh); };
}

function agentPrompt(f, marks) {
  const base = location.origin + (location.pathname.startsWith("/c/") ? location.pathname.replace(/\/$/, "") : "");
  return [
    `Feedback ${f.id} on the ChloeDB command center (${f.kind}, ${prioName(f.priority)}, ${f.status}): ${f.title || ""}`,
    `View: ${f.context?.title || f.page || "?"} (${f.route || "?"}), served from chloedb-server/ui/app.`,
    f.note ? `Note: ${f.note}` : null,
    marks.length ? "Marks on the screenshot:\n" + marks.map((m) => `  ${m.n}. ${m.tool}${m.text ? ` "${m.text}"` : ""}${m.target ? ` on ${m.target.card ? `"${m.target.card}" ` : ""}${m.target.selector || ""}${m.target.text ? ` ("${m.target.text}")` : ""}` : ""}`).join("\n") : null,
    `Screenshot: GET ${base}/v1/feedback/${f.id}/shot (the item in full: GET ${base}/v1/feedback/${f.id}, or the MCP tool get_feedback).`,
    `Take it: PATCH ${base}/v1/feedback/${f.id} {"status": "doing", "assignee": "<your thread>", "if_status": "${f.status}"}. When the change is made: {"status": "done", "resolution": "<the pull request>"}.`,
  ].filter(Boolean).join("\n");
}

function agentHelp() {
  const base = location.origin;
  modal({
    title: "Feedback, for agents", wide: true,
    body: [
      h("p", { class: "dim", style: { margin: 0 } }, "Each item is a row of table cc_feedback. A read key reads it, a write key sends and triages it; a key limited to tables needs a grant on cc_feedback. Over MCP (POST /v1/mcp) the tools are list_feedback, get_feedback (which answers the screenshot as an image) and update_feedback."),
      h("div", { class: "small dim" }, "The open items, most urgent first:"),
      sqlBlock("SELECT id, kind, priority, title, note, page, marks_text\nFROM cc_feedback\nWHERE status IN ('new', 'triaged', 'planned')\nORDER BY priority, ts\nLIMIT 20"),
      h("div", { class: "small dim" }, "Over HTTP:"),
      h("pre", { class: "code" }, [
        `curl -H "Authorization: Bearer $KEY" '${base}/v1/feedback?status=open'`,
        `curl -H "Authorization: Bearer $KEY" ${base}/v1/feedback/<id>          # marks with the element under each`,
        `curl -H "Authorization: Bearer $KEY" -o shot.png ${base}/v1/feedback/<id>/shot`,
        `curl -X PATCH -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \\`,
        `  -d '{"status": "doing", "assignee": "my thread", "if_status": "new"}' ${base}/v1/feedback/<id>`,
      ].join("\n")),
      h("div", { class: "small dim" }, "if_status makes taking an item safe: a second agent that asks for it after the first is refused (409). Set status done with a resolution (the pull request's link) once the change is made."),
    ],
  });
}
