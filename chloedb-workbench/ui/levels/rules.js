// The Rules panel (sixth build): the repository's rules, each one on or
// off, edited or deleted in place, and a box to add one without a question
// to answer (POST /api/rules, PATCH and DELETE /api/rules/:id). Decide
// shows it; its "Rules (n)" button opens it at the box.
//   const panel = rulesPanel(ctx, o); panel.view(); panel.open();
//   o = { repo(), rules(), setRules(list), render(), toggle(rule), fresh: Set,
//         questionText(id), busy: Set }
import { h, useStyle, inlineCode, fmtDay } from "./common.js";

const enc = encodeURIComponent;
const base = (p) => String(p || "").split("/").filter(Boolean).pop() || p || "";

export function rulesPanel(ctx, o) {
  const R = { adding: "", editing: null, draft: "", confirm: null, busy: false, err: null, note: null, open: false };

  async function add() {
    const text = R.adding.trim();
    if (!text || R.busy) return;
    if (!o.repo()) { R.err = "Open a repository first: a rule belongs to one."; o.render(); return; }
    R.busy = true; R.err = null; o.render();
    try {
      const r = await ctx.api("/api/rules", { method: "POST", body: { repo: o.repo(), text } });
      o.setRules([r.rule, ...o.rules()]);
      o.fresh && o.fresh.add(r.rule.id);
      R.adding = "";
      R.note = (r.missing || []).length ? `Added. ${r.missing.join(" ")}` : "Added: every agent of this repository gets it at its next prompt, and ChloeCtx keeps it as a decision.";
    } catch (e) { R.err = `The rule was not added: ${e.message}`; }
    R.busy = false; o.render();
  }

  // An edit is a new rule in place of the old one, so that every agent is
  // told the new words (an agent is told each rule once).
  async function save(r) {
    const text = R.draft.trim();
    if (!text || R.busy) return;
    if (text === r.text) { R.editing = null; o.render(); return; }
    R.busy = true; R.err = null; o.render();
    try {
      const n = await ctx.api("/api/rules", { method: "POST", body: { repo: r.repo || o.repo(), text } });
      if (!r.active) await ctx.api(`/api/rules/${enc(n.rule.id)}`, { method: "PATCH", body: { active: false } }).then((x) => Object.assign(n.rule, x.rule)).catch(() => {});
      await ctx.api(`/api/rules/${enc(r.id)}`, { method: "DELETE" });
      o.setRules(o.rules().map((x) => (x.id === r.id ? n.rule : x)));
      o.fresh && o.fresh.add(n.rule.id);
      R.editing = null;
      R.note = r.active ? "Changed: agents are told the new words at their next prompt." : "Changed; it stays off.";
    } catch (e) { R.err = `The rule was not changed: ${e.message}`; }
    R.busy = false; o.render();
  }

  async function remove(r) {
    if (R.busy) return;
    R.busy = true; R.err = null; o.render();
    try {
      const out = await ctx.api(`/api/rules/${enc(r.id)}`, { method: "DELETE" });
      o.setRules(o.rules().filter((x) => x.id !== r.id));
      R.note = `Deleted: no agent is told it any more.${out.ctx_id ? ` ChloeCtx keeps its copy (${out.ctx_id}) as a past decision.` : ""}`;
    } catch (e) { R.err = `The rule was not deleted: ${e.message}`; }
    R.confirm = null; R.busy = false; o.render();
  }

  function row(r) {
    const editing = R.editing === r.id;
    const confirming = R.confirm === r.id;
    const q = o.questionText && r.question ? o.questionText(r.question) : null;
    const meta = [`made ${fmtDay(r.created)}`, q ? `from “${q}”` : r.question ? "from an answer" : "added here", o.fresh && o.fresh.has(r.id) ? "new" : null, r.active ? null : "off"].filter(Boolean).join(" · ");
    return h("li", { class: `${r.active ? "" : "off"}${o.fresh && o.fresh.has(r.id) ? " fresh" : ""}${editing ? " ru-editing" : ""}` },
      h("button", { class: "de-sw", role: "switch", "aria-checked": r.active ? "true" : "false", "aria-label": `${r.active ? "Turn off" : "Turn on"}: ${r.text}`, "data-nav": "", "data-key": `sw:${r.id}`, onclick: () => o.toggle(r) }, h("i")),
      h("div", { class: "de-rule-x" },
        editing
          ? h("form", { class: "ru-edit", onsubmit: (e) => { e.preventDefault(); save(r); } },
            h("textarea", { class: "ru-t", rows: 2, "data-key": `ru-e:${r.id}`, "aria-label": "The rule's words", oninput: (e) => { R.draft = e.target.value; },
              onkeydown: (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); save(r); } if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); R.editing = null; o.render(); } } }, R.draft),
            h("div", { class: "ru-acts" }, h("button", { class: "btn sm primary", type: "submit", "data-key": `ru-s:${r.id}`, disabled: R.busy || !R.draft.trim() }, R.busy ? "Saving…" : "Save"),
              h("button", { class: "btn sm ghost", type: "button", "data-key": `ru-c:${r.id}`, onclick: () => { R.editing = null; o.render(); } }, "Cancel")))
          : h("div", {}, inlineCode(r.text)),
        h("div", { class: "muted" }, meta),
        editing ? null : confirming
          ? h("div", { class: "ru-acts ru-confirm" }, h("span", null, "Delete this rule?"),
            h("button", { class: "btn sm danger", "data-key": `ru-y:${r.id}`, disabled: R.busy, onclick: () => remove(r) }, "Delete"),
            h("button", { class: "btn sm ghost", "data-key": `ru-n:${r.id}`, onclick: () => { R.confirm = null; o.render(); } }, "Keep it"))
          : h("div", { class: "ru-acts ru-quiet" },
            h("button", { class: "lv-link", "data-key": `ru-ed:${r.id}`, onclick: () => { R.editing = r.id; R.draft = r.text; R.confirm = null; o.render(); focusKey(`ru-e:${r.id}`); } }, "Edit"),
            h("button", { class: "lv-link ru-del", "data-key": `ru-d:${r.id}`, onclick: () => { R.confirm = r.id; o.render(); focusKey(`ru-n:${r.id}`); } }, "Delete"))));
  }

  let host = null;
  function focusKey(k) {
    requestAnimationFrame(() => { const x = host && host.querySelector(`[data-key="${CSS.escape(k)}"]`); if (x) { x.focus({ preventScroll: true }); if (x.setSelectionRange && x.value) x.setSelectionRange(x.value.length, x.value.length); } });
  }

  function view() {
    const rules = o.rules();
    const on = rules.filter((r) => r.active).length;
    const repo = o.repo();
    const box = h("textarea", {
      class: "ru-t", rows: 2, "data-key": "ru-add", "aria-label": "A new rule",
      placeholder: repo ? "Totals customers see round half up; the ledger export rounds on its own." : "Open a repository first: a rule belongs to one.",
      disabled: !repo,
      oninput: (e) => { R.adding = e.target.value; const b = host?.querySelector('[data-key="ru-go"]'); if (b) b.disabled = !e.target.value.trim(); },
      onkeydown: (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); add(); } },
    }, R.adding);
    host = h("section", { class: "lv-card de-rules" + (R.open ? " ru-open" : ""), id: "rules-panel" },
      h("div", { class: "de-rules-h" }, h("h2", { class: "lv-h" }, "Rules", h("span", { class: "n" }, `· ${rules.length} · ${on} on`)),
        h("span", { class: "muted" }, repo ? `The active ones reach every agent of ${base(repo)} at its next prompt.` : "The active ones reach every agent of the repository at its next prompt.")),
      h("form", { class: "ru-add", onsubmit: (e) => { e.preventDefault(); add(); } },
        h("label", { class: "ru-lab" }, "Add a rule"), box,
        h("div", { class: "ru-acts" }, h("button", { class: "btn sm primary", type: "submit", "data-key": "ru-go", disabled: R.busy || !R.adding.trim() || !repo }, R.busy && R.adding ? "Adding…" : "Add the rule"),
          h("span", { class: "muted" }, "Say it as you would to a new colleague. ", h("kbd", null, "⌘↵"), " adds it."))),
      R.err ? h("div", { class: "ru-err", role: "alert" }, R.err) : null,
      R.note ? h("div", { class: "ru-note", role: "status" }, R.note) : null,
      rules.length ? h("ul", {}, rules.map(row)) :
        h("div", { class: "muted" }, "No rules yet. Add one above, or tick “Make it a rule” when you answer a question."));
    return host;
  }

  /** Brings the panel into view with its box focused. */
  function open() {
    R.open = true;
    o.render();
    requestAnimationFrame(() => {
      const p = document.getElementById("rules-panel");
      if (!p) return;
      p.scrollIntoView({ behavior: "smooth", block: "start" });
      const t = p.querySelector('[data-key="ru-add"]');
      if (t && !t.disabled) t.focus({ preventScroll: true });
      p.classList.remove("ru-flash"); void p.offsetWidth; p.classList.add("ru-flash");
    });
  }

  return { view, open };
}

useStyle("rules", `
.ru-add{display:flex;flex-direction:column;gap:6px;margin:4px 0 12px;padding:10px 12px;border:1px dashed var(--line-2);border-radius:10px;background:var(--surface-2)}
.ru-lab{font-size:11.5px;font-weight:650;color:var(--muted);text-transform:uppercase;letter-spacing:.05em}
.ru-t{width:100%;box-sizing:border-box;resize:vertical;min-height:44px;padding:7px 9px;border:1px solid var(--line-2);border-radius:8px;background:var(--surface);color:var(--text);font:inherit;line-height:1.45}
.ru-t:focus{outline:2px solid var(--focus);border-color:var(--accent)}
.ru-acts{display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin-top:4px}
.ru-acts kbd{font:11px var(--mono);border:1px solid var(--line-2);border-radius:4px;padding:0 4px}
.ru-quiet{opacity:.75}.de-rules li:hover .ru-quiet,.ru-quiet:focus-within{opacity:1}
.ru-del{color:var(--crit-ink)}
.ru-confirm span{color:var(--crit-ink);font-weight:600}
.ru-edit{display:flex;flex-direction:column;gap:4px}
.de-rules li.ru-editing{grid-column:1/-1}.de-rules li.ru-editing .de-rule-x{flex:1}
.ru-err{color:var(--crit-ink);background:var(--crit-soft);border-radius:7px;padding:6px 9px;margin-bottom:8px}
.ru-note{color:var(--good-ink);background:var(--good-soft);border-radius:7px;padding:6px 9px;margin-bottom:8px}
.de-rules.ru-flash{animation:ru-flash 1.2s ease-out}
@keyframes ru-flash{0%{box-shadow:0 0 0 3px var(--accent)}100%{box-shadow:var(--shadow-1)}}
`);
