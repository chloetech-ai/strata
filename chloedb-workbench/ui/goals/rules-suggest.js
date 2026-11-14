// Rules: suggested rules from evidence, every grant with Revoke, and the
// grant sheet (JOURNEYS.md, "Rules and notifications"; the UX critique's
// issue 2 and power-user recommendation 3).
//
//   mount(root, ctx)             More › Rules, a full page
//   card(ctx)                    a compact card for Start and Continue (a node; empty when there is nothing)
//   grantSheet(ctx, opts)        the confirm sheet a wide option opens; resolves {choice, pattern?, result?}
//
// ctx is the level ctx ({ api, state, params, go }) plus R1's { project, crumbs }.
// Nothing is written without a confirm: accepting a suggestion and the
// sheet's choices send the exact pattern the sheet showed.
import { h, useStyle, page, pane, label, btn, icon, since, fmtDur, onEscape, repoOf, optionsOf, grantOf, parseJSON } from "/levels/common.js";

const enc = encodeURIComponent;
const base = (p) => String(p || "").split("/").filter(Boolean).pop() || p || "";
const repoPath = (ctx) => (ctx && ctx.project && (ctx.project.path || ctx.project.repo)) || repoOf(ctx);
const ago = (t) => (!t ? "—" : since(t) === "now" ? "just now" : `${since(t)} ago`);
const wait = (ms) => (ms > 0 ? fmtDur(ms) : "no time");
const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

useStyle("rules-suggest", `
.rs-page .gp-sec{margin-top:var(--s-6)}
.rs-seg{display:inline-flex;border:var(--hair) solid var(--line-2);border-radius:var(--r-sm);overflow:hidden}
.rs-seg button{border:0;background:none;color:var(--text-2);font:var(--w-mid) var(--fs-sm)/1 var(--sans);height:var(--ctl-h);padding:0 var(--s-3);cursor:pointer}
.rs-seg button+button{border-left:var(--hair) solid var(--line-2)}
.rs-seg button[aria-pressed="true"]{background:var(--accent-soft);color:var(--accent-text)}
.rs-sugg{display:flex;flex-direction:column;gap:var(--s-4)}
.rs-rule{font:var(--w-mid) var(--fs-md)/var(--lh-md) var(--mono);color:var(--text);overflow-wrap:anywhere;letter-spacing:-.01em}
.rs-why{margin:var(--s-2) 0 0;color:var(--text-2);font-size:var(--fs-body);line-height:var(--lh-body)}
.rs-tiles{margin-top:var(--s-4)}
.rs-tile{padding:var(--s-3) var(--s-4)}
.rs-tile b{display:block;font:var(--w-mid) var(--fs-lg)/var(--lh-lg) var(--mono);letter-spacing:-.03em;margin-top:var(--s-1)}
.rs-tile span{color:var(--text-2);font-size:var(--fs-sm)}
.rs-cmds{margin:var(--s-4) 0 0;padding:0;list-style:none;border:var(--hair) solid var(--line);border-radius:var(--r-sm);overflow:hidden}
.rs-cmds li{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:var(--s-1) var(--s-3);padding:var(--s-2) var(--s-3);border-bottom:var(--hair) solid var(--line);align-items:baseline}
.rs-cmds li:last-child{border-bottom:0}
.rs-cmds li.risky{background:var(--warn-soft)}
.rs-cmds code{font:var(--w-text) var(--fs-sm)/var(--lh-sm) var(--mono);white-space:pre-wrap;overflow-wrap:anywhere;color:var(--text)}
.rs-cmds .m{font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2);white-space:nowrap}
.rs-cmds .rk{grid-column:1 / -1;display:flex;gap:var(--s-1);flex-wrap:wrap}
.rs-risk{display:inline-flex;align-items:center;gap:4px;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:.04em;text-transform:uppercase;color:var(--warn-ink)}
.rs-risk::before{content:"";width:6px;height:6px;border-radius:1px;background:var(--warn);transform:rotate(45deg)}
.rs-where{margin-top:var(--s-3);font-size:var(--fs-sm);line-height:var(--lh-sm);color:var(--text-2)}
.rs-where code{font-family:var(--mono);font-size:var(--fs-xs);overflow-wrap:anywhere;color:var(--text)}
.rs-acts{display:flex;gap:var(--s-2);flex-wrap:wrap;margin-top:var(--s-4)}
.rs-empty{padding:var(--s-5) var(--s-4);color:var(--text-2);font-size:var(--fs-body);line-height:var(--lh-body)}
.rs-tbl{width:100%;border-collapse:collapse;table-layout:fixed}
.rs-tbl col.c1{width:24%}.rs-tbl col.c2{width:30%}.rs-tbl col.c3{width:16%}.rs-tbl col.c4{width:10%}.rs-tbl col.c5{width:10%}.rs-tbl col.c6{width:10%}
.rs-tbl th{text-align:left;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2);padding:var(--s-2) var(--s-4);border-bottom:var(--hair) solid var(--line)}
.rs-tbl td{padding:var(--s-3) var(--s-4);border-bottom:var(--hair) solid var(--line);vertical-align:top;font-size:var(--fs-sm);line-height:var(--lh-sm);color:var(--text-2)}
.rs-tbl tr:last-child td{border-bottom:0}
.rs-tbl td.r code{font:var(--w-mid) var(--fs-sm)/var(--lh-sm) var(--mono);color:var(--text);overflow-wrap:break-word}
.rs-tbl td.n{font-family:var(--mono);white-space:nowrap}
.rs-tbl td small{display:block;color:var(--text-3);font-size:var(--fs-xs);margin-top:2px;overflow-wrap:anywhere}
.rs-tbl td.a{text-align:right}
.rs-tbl td small.how{color:var(--warn-ink)}
.rs-note{margin:var(--s-3) 0 0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.rs-msg{margin-top:var(--s-3);font-size:var(--fs-sm);color:var(--good-ink)}
.rs-msg.err{color:var(--crit-ink)}
.rs-card{display:flex;align-items:center;gap:var(--s-3);padding:var(--s-3) var(--s-4);background:var(--surface);border:var(--hair) solid var(--line);border-radius:var(--r);min-width:0}
.rs-card[hidden]{display:none}
.rs-card .g{width:28px;height:28px;border-radius:var(--r-sm);display:grid;place-items:center;background:var(--accent-soft);color:var(--accent-text);flex:none}
.rs-card p{margin:0;flex:1;min-width:0;font-size:var(--fs-body);line-height:var(--lh-body);color:var(--text-2)}
.rs-card p b{color:var(--text);font-weight:var(--w-mid)}
.rs-card code{font-family:var(--mono);font-size:var(--fs-sm);color:var(--text);overflow-wrap:anywhere}
.rs-card .btn{flex:none;min-height:32px}
.gs-scrim{position:fixed;inset:0;z-index:90;background:var(--scrim);display:grid;place-items:center;padding:var(--s-5)}
.gs{width:min(var(--sheet-w),100%);max-height:calc(100vh - 2 * var(--s-5));overflow:auto;background:var(--surface);border-radius:var(--r);box-shadow:var(--shadow-3);border:var(--hair) solid var(--line);display:flex;flex-direction:column}
.gs-hd{padding:var(--s-4) var(--s-5) 0}
.gs-kick{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--warn-ink)}
.gs-title{margin:var(--s-1) 0 0;font:var(--w-title) var(--fs-lg)/var(--lh-lg) var(--sans);letter-spacing:var(--track-md)}
.gs-bd{padding:var(--s-3) var(--s-5) var(--s-4);display:flex;flex-direction:column;gap:var(--s-4)}
.gs-rule{padding:var(--s-3);border-radius:var(--r-sm);background:var(--well);font:var(--w-mid) var(--fs-md)/var(--lh-md) var(--mono);overflow-wrap:anywhere}
.gs-cmd1,.gs-cmd .qc-cmd{margin:0;max-height:240px;overflow:auto}.gs-cmd summary{cursor:pointer;color:var(--text-2);font-size:var(--fs-sm)}.gs-cmd summary code{color:var(--text)}
.gs .kv{margin:0}
.gs .kv dd code{font-family:var(--mono);font-size:var(--fs-sm);overflow-wrap:anywhere}
.gs-wide{padding:var(--s-2) var(--s-3);border-radius:var(--r-sm);background:var(--warn-soft);color:var(--warn-ink);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.gs-wide code{font-family:var(--mono);font-size:var(--fs-xs)}
.gs-alt{border:var(--hair) solid var(--accent-line);border-radius:var(--r-sm);padding:var(--s-3);display:flex;flex-direction:column;gap:var(--s-2)}
.gs-alt code{font:var(--w-mid) var(--fs-body)/var(--lh-body) var(--mono);overflow-wrap:anywhere}
.gs-alt p{margin:0;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm)}
.gs-ft{position:sticky;bottom:0;display:flex;gap:var(--s-2);justify-content:flex-end;flex-wrap:wrap;padding:var(--s-3) var(--s-5) var(--s-4);border-top:var(--hair) solid var(--line);background:var(--surface)}
.gs-err{color:var(--crit-ink);font-size:var(--fs-sm)}
.gs-info{display:flex;flex-direction:column;gap:var(--s-3)}
.gs .rs-cmds{margin:0}
@media (max-width:640px){
  .gs-scrim{place-items:end stretch;padding:0}
  .gs{width:100%;max-height:92vh;border-radius:var(--r) var(--r) 0 0;padding-bottom:env(safe-area-inset-bottom)}
  .gs-hd,.gs-bd{padding-left:var(--s-4);padding-right:var(--s-4)}
  .gs-ft{flex-direction:column-reverse;padding:var(--s-3) var(--s-4) calc(var(--s-3) + env(safe-area-inset-bottom))}
  .gs-ft .btn{width:100%;min-height:var(--touch);justify-content:center}
  .rs-acts .btn,.rs-card .btn{min-height:var(--touch)}
  .rs-card{flex-wrap:wrap}.rs-card .btn{width:100%;justify-content:center}
  .rs-tbl thead{display:none}
  .rs-tbl,.rs-tbl tbody,.rs-tbl tr,.rs-tbl td{display:block;width:100%}
  .rs-tbl tr{padding:var(--s-3) var(--s-4);border-bottom:var(--hair) solid var(--line)}
  .rs-tbl tr:last-child{border-bottom:0}
  .rs-tbl td{padding:2px 0;border:0}
  .rs-tbl td.a{text-align:left;padding-top:var(--s-2)}
  .rs-tbl td.a .btn{min-height:var(--touch);width:100%;justify-content:center}
  .rs-tbl td[data-l]::before{content:attr(data-l) " ";font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);text-transform:uppercase;letter-spacing:var(--track-label);color:var(--text-3)}
  .rs-seg button{height:var(--touch)}
  .rs-pg .gp-ht{flex-basis:100%}
}
@media (pointer:coarse){.rs-seg button,.rs-acts .btn,.gs-ft .btn{min-height:var(--touch)}}
`);

// ------------------------------------------------------------ pieces

// The server's sentences mark commands with backticks: show them as code.
function ticks(text) {
  return String(text).split("`").map((t, i) => (i % 2 ? h("code", {}, t) : t));
}

function riskChips(c) {
  return (c.risks || []).length ? h("span", { class: "rk" }, c.risks.map((r) => h("span", { class: "rs-risk" }, r))) : null;
}

/** Commands as a list, risky ones first (the server sorts them so). */
function commandList(cmds, { max = 5, skip } = {}) {
  const list = (cmds || []).filter((c) => !skip || c.command !== skip);
  const ul = h("ul", { class: "rs-cmds" });
  const fill = (n) => ul.replaceChildren(...list.slice(0, n).map((c) => h("li", { class: c.risky ? "risky" : null },
    h("code", {}, c.command),
    h("span", { class: "m" }, `${c.times}×${c.asked ? " · asked" : ""}${c.last_ms ? ` · ${since(c.last_ms)}` : ""}`),
    riskChips(c))));
  fill(max);
  if (list.length <= max) return ul;
  const more = h("button", { class: "lv-link", type: "button", onclick: () => { fill(list.length); more.remove(); } }, `Show all ${list.length}`);
  return h("div", {}, ul, h("div", { style: { marginTop: "var(--s-2)" } }, more));
}

function whereText(w) {
  if (!w) return null;
  return h("div", { class: "rs-where" }, "Written to: ", w.text, w.file ? [" (", h("code", {}, w.file), ")"] : null, w.note ? h("div", {}, w.note) : null);
}

// ------------------------------------------------------------ the sheet

function sheetFrame({ kicker, title, body, foot, onClose }) {
  const scrim = h("div", { class: "gs-scrim", role: "presentation" });
  const box = h("div", { class: "gs", role: "dialog", "aria-modal": "true", "aria-label": title },
    h("div", { class: "gs-hd" }, h("div", { class: "gs-kick" }, kicker), h("h2", { class: "gs-title" }, title)),
    h("div", { class: "gs-bd" }, body),
    h("div", { class: "gs-ft" }, foot));
  scrim.append(box);
  scrim.addEventListener("click", (e) => { if (e.target === scrim) onClose(); });
  document.body.append(scrim);
  return scrim;
}

/** The confirm sheet before a rule is written.
 *  opts = { pattern, agent (row or id), file, question (row), option (index), command, repo,
 *           onConfirm(pattern) → Promise (instead of answering the question), title }
 *  Resolves { choice: "confirm" | "narrow" | "cancel", pattern, result }. Nothing is
 *  typed or written before Confirm (or the narrower choice) is pressed. */
export function grantSheet(ctx, opts = {}) {
  const q = opts.question || null;
  const agentId = typeof opts.agent === "string" ? opts.agent : opts.agent && opts.agent.id;
  const agentRow = typeof opts.agent === "object" ? opts.agent : (ctx.state && (ctx.state.agents || []).find((a) => a.id === agentId)) || null;
  const d = q ? parseJSON(q.detail, {}) || {} : {};
  const command = opts.command || d.command || "";
  const opt = q && opts.option != null ? optionsOf(q)[opts.option] : null;
  const g = q && opt ? grantOf(q, opt, agentRow) : null;
  const pattern = opts.pattern || (g && g.rule) || "";
  const file = opts.file || (g && g.where) || "";
  const lasting = g ? g.lasting : true;
  const repo = opts.repo || (agentRow && agentRow.repo) || repoPath(ctx);
  return new Promise((resolve) => {
    let done = false, busy = false;
    const err = h("div", { class: "gs-err", role: "alert" });
    const info = h("div", { class: "gs-info" }, h("p", { class: "rs-note" }, "Reading what else it would allow…"));
    const finish = (r) => { if (done) return; done = true; unesc(); scrim.remove(); resolve(r); };
    const cancel = () => finish({ choice: "cancel", pattern });
    const unesc = onEscape(cancel);
    const answer = async (index, label) => ctx.api(`/api/questions/${enc(q.id)}/answer`, { method: "POST", body: { option: index, label } });
    const confirmBtn = btn(lasting ? "Confirm: write this rule" : "Confirm", { primary: true, onclick: async () => {
      if (busy) return; busy = true; confirmBtn.disabled = true; err.textContent = "";
      try {
        const result = opts.onConfirm ? await opts.onConfirm(pattern) : q && opt ? await answer(opts.option, opt.label) : null;
        finish({ choice: "confirm", pattern, result });
      } catch (e) { err.textContent = `Nothing was written: ${e.message}`; busy = false; confirmBtn.disabled = false; }
    } });
    const cancelBtn = btn("Cancel", { onclick: cancel });
    const body = [
      h("div", { class: "gs-rule", "aria-label": "The rule" }, pattern || "(the rule Claude Code writes)"),
      h("dl", { class: "kv" },
        h("dt", {}, "Written to"), h("dd", {}, file ? h("code", {}, file) : "the agent's settings"),
        h("dt", {}, "Lasts"), h("dd", {}, lasting ? "until you revoke it (More › Rules)" : "this session only"),
        agentRow ? [h("dt", {}, "Agent"), h("dd", {}, agentRow.name)] : null,
        // The whole command (Maya saw line 1 of 2): folded with its count when long.
        command ? [h("dt", {}, "Asked for"), h("dd", {}, command.split("\n").length > 3
          ? h("details", { class: "gs-cmd" }, h("summary", {}, h("code", {}, command.split("\n")[0]), ` … ${command.split("\n").length} lines, show all`), h("pre", { class: "qc-cmd" }, command))
          : h("pre", { class: "qc-cmd gs-cmd1" }, command))] : null),
      info, err,
    ];
    const scrim = sheetFrame({ kicker: "Check before granting", title: opts.title || "This answer writes a rule", body, foot: [cancelBtn, confirmBtn], onClose: cancel });
    cancelBtn.focus();
    if (!pattern || !repo) { info.replaceChildren(h("p", { class: "rs-note" }, "Workbench cannot read this rule; Confirm types the option as Claude Code shows it.")); return; }
    ctx.api(`/api/rules/check?repo=${enc(repo)}&pattern=${enc(pattern)}${agentId ? `&agent=${enc(agentId)}` : ""}${command ? `&command=${enc(command)}` : ""}`)
      .then((c) => {
        const w = c.window || {};
        const others = (w.commands || []).filter((x) => x.command !== command);
        const kids = [];
        if (c.too_wide) kids.push(h("div", { class: "gs-wide" }, "Workbench would not suggest this rule: ", ...ticks(c.too_wide), "."));
        kids.push(label("It would also have allowed", h("span", { class: "num" }, `${w.distinct || 0} in 30 days${w.risky ? `, ${w.risky} risky` : ""}`)));
        kids.push(others.length ? commandList(others, { max: 3 }) : h("p", { class: "rs-note" }, "No other command of the last 30 days matches it."));
        const n = c.narrower;
        if (n) {
          const narrowBtn = btn(q ? "Allow this once, add the narrower rule" : "Use the narrower rule", { onclick: async () => {
            if (busy) return; busy = true; narrowBtn.disabled = true; err.textContent = "";
            try {
              const grant = await ctx.api("/api/rules/grants", { method: "POST", body: { repo, pattern: n.pattern, question: q ? q.id : undefined } });
              let result = grant;
              if (q) {
                const opts2 = optionsOf(q);
                const once = opts2.findIndex((o) => !o.grants && !o.no);
                if (once >= 0) result = await answer(once, opts2[once].label);
              }
              finish({ choice: "narrow", pattern: n.pattern, result });
            } catch (e) { err.textContent = `Nothing more was written: ${e.message}`; busy = false; narrowBtn.disabled = false; }
          } });
          kids.push(h("div", { class: "gs-alt" }, label("Narrower"), h("code", {}, n.pattern),
            h("p", {}, `It allows ${plural(n.distinct || 0, "command")} of the last 30 days${n.risky ? `, ${n.risky} risky` : ", none risky"}${n.prompts ? `, and would have removed ${plural(n.prompts, "prompt")}` : ""}. Workbench writes it for this project's new agents.`),
            h("div", {}, narrowBtn)));
        }
        info.replaceChildren(...kids);
      })
      .catch((e) => info.replaceChildren(h("p", { class: "rs-note" }, `What else it allows could not be read: ${e.message}`)));
  });
}

// ------------------------------------------------------------ the card

/** "12 of 14 waits were pytest. Review a rule →" for Start and Continue.
 *  Returns a node at once; it fills in (or stays hidden) when the data comes. */
export function card(ctx) {
  const el = h("div", { class: "rs-card", hidden: true });
  const repo = repoPath(ctx);
  if (!repo) return el;
  ctx.api(`/api/rules/suggested?repo=${enc(repo)}`).then((r) => {
    const s = (r.suggestions || [])[0];
    if (!s) return;
    const w7 = s.windows["7"], w30 = s.windows["30"];
    const w = w7.prompts > 0 ? w7 : w30;
    const days = w === w7 ? 7 : 30;
    el.replaceChildren(
      h("span", { class: "g" }, icon("check", "i sm")),
      h("p", {}, h("b", {}, `${w.prompts} of ${w.all_prompts} waits`), ` in ${days} days were `, h("code", {}, s.label), `, which you approved every time. A rule saves ~${wait(w.wait_ms)}.`),
      btn("Review a rule", { sm: true, href: "#/more/rules", onclick: (e) => { if (ctx.go) { e.preventDefault(); ctx.go("#/more/rules"); } } }));
    el.hidden = false;
  }).catch(() => {});
  return el;
}

// ------------------------------------------------------------ the page

export function mount(root, ctx) {
  const S = { days: "7", sugg: null, grants: null, err: null, msg: null, busy: new Set() };
  const repo = repoPath(ctx);
  const host = h("div", { class: "rs-page" });
  root.append(host);
  if (ctx.crumbs) ctx.crumbs([{ label: "More", href: "#/more" }, { label: "Rules" }]);

  async function load() {
    if (!repo) { S.err = "Open a project first: rules belong to one."; render(); return; }
    try {
      const [s, g] = await Promise.all([ctx.api(`/api/rules/suggested?repo=${enc(repo)}`), ctx.api(`/api/rules/grants?repo=${enc(repo)}`)]);
      S.sugg = s; S.grants = g; S.err = null;
    } catch (e) { S.err = e.message; }
    render();
  }

  async function review(s) {
    const w = s.windows["30"];
    const r = await grantSheet(ctx, {
      pattern: s.pattern, repo, file: s.where && s.where.file, title: "Add this rule?",
      onConfirm: (pattern) => ctx.api(`/api/rules/suggested/${enc(s.id)}/accept`, { method: "POST", body: { repo, pattern } }),
    });
    if (r.choice === "confirm") S.msg = { text: `Added ${r.pattern}. New agents in ${base(repo)} run it without asking; it would have saved ~${wait(w.wait_ms)} in 30 days.` };
    else if (r.choice === "narrow") S.msg = { text: `Added the narrower ${r.pattern} instead.` };
    if (r.choice !== "cancel") load();
  }

  async function dismiss(s) {
    S.busy.add(s.id); render();
    try { await ctx.api(`/api/rules/suggested/${enc(s.id)}/dismiss`, { method: "POST", body: { repo } }); S.msg = { text: `Dismissed ${s.pattern}. It is not suggested again.` }; }
    catch (e) { S.msg = { text: `Not dismissed: ${e.message}`, err: true }; }
    S.busy.delete(s.id); load();
  }

  async function revoke(g) {
    S.busy.add(g.id); render();
    try {
      const r = await ctx.api(`/api/rules/grants/${enc(g.id)}?repo=${enc(repo)}`, { method: "DELETE" });
      S.msg = { text: `Revoked ${g.pattern}. ${r.note || ""}` };
    } catch (e) { S.msg = { text: `Not revoked: ${e.message}`, err: true }; }
    S.busy.delete(g.id); load();
  }

  function suggestion(s, i) {
    const w = s.windows[S.days];
    return pane("Suggested rule", h("span", { class: "num" }, `${s.windows["30"].prompts} approvals in 30 days`),
      h("div", { class: "bd" },
        h("div", { class: "rs-rule" }, s.pattern),
        h("p", { class: "rs-why" }, s.why, s.agents && s.agents.length ? ` Asked by ${s.agents.join(", ")}.` : ""),
        h("div", { class: "bank rs-tiles keep", style: { "--cols": "3" } },
          h("div", { class: "rs-tile" }, label("Prompts removed"), h("b", {}, String(w.prompts)), h("span", {}, `of ${w.all_prompts} in ${S.days} days`)),
          h("div", { class: "rs-tile" }, label("Wait saved"), h("b", {}, wait(w.wait_ms)), h("span", {}, `of ${wait(w.all_wait_ms)} waiting`)),
          h("div", { class: "rs-tile" }, label("Commands"), h("b", {}, String(w.distinct)), h("span", {}, w.risky ? `${w.risky} risky` : "none risky"))),
        w.commands.length ? [h("div", { style: { marginTop: "var(--s-4)" } }, label(`Every command it would have allowed, last ${S.days} days`)), commandList(w.commands, { max: 5 })]
          : h("p", { class: "rs-note" }, `No command in the last ${S.days} days; the 30 days have ${s.windows["30"].distinct}.`),
        whereText(s.where),
        h("div", { class: "rs-acts" },
          btn("Review and add…", { primary: i === 0, onclick: () => review(s) }),
          btn("Dismiss", { ghost: true, disabled: S.busy.has(s.id), onclick: () => dismiss(s) }))));
  }

  function grantRow(g) {
    const who = g.granted_by === "you" ? `you, via ${g.via}` : g.via;
    return h("tr", {},
      h("td", { class: "r" }, h("code", {}, g.pattern), g.lasting ? null : h("small", {}, g.says || "session only")),
      h("td", { "data-l": "Where" }, g.where && g.where.text, g.where && g.where.file ? h("small", {}, g.where.file) : null, g.how ? h("small", { class: "how" }, g.how) : null),
      h("td", { "data-l": "Granted by" }, who),
      h("td", { class: "n", "data-l": "When" }, ago(g.created_ms)),
      h("td", { class: "n", "data-l": "Used" }, g.used == null ? "—" : `${g.used}×${g.last_used_ms ? `, last ${ago(g.last_used_ms)}` : ""}`),
      h("td", { class: "a" }, g.revocable
        ? btn("Revoke", { sm: true, danger: false, disabled: S.busy.has(g.id), onclick: () => revoke(g) })
        : h("small", {}, g.kind === "session" ? "ends with the session" : "yours to edit")));
  }

  function render() {
    const s = S.sugg, g = S.grants;
    const list = (s && s.suggestions) || [];
    const grants = (g && g.grants) || [];
    const seg = h("div", { class: "rs-seg", role: "group", "aria-label": "Window" },
      ["7", "30"].map((d) => h("button", { type: "button", "aria-pressed": String(S.days === d), onclick: () => { S.days = d; render(); } }, `Last ${d} days`)));
    const p = s && s.prompts ? s.prompts[S.days] : null;
    host.replaceChildren(page({
      kicker: base(repo) || "More",
      title: "Rules",
      sub: "What agents may run without asking you. Suggestions come from prompts you approved again and again, each with everything it would have allowed.",
      aside: seg,
      cls: "rs-pg",
    },
    S.err ? h("div", { class: "pane" }, h("div", { class: "rs-empty" }, S.err)) : null,
    S.msg ? h("div", { class: `rs-msg${S.msg.err ? " err" : ""}`, role: "status" }, S.msg.text) : null,
    h("section", { class: "gp-sec" },
      label("Suggested", h("span", { class: "num" }, p ? `${p.prompts} prompts, ${wait(p.wait_ms)} waiting in ${S.days} days` : "")),
      !s ? h("div", { class: "pane" }, h("div", { class: "rs-empty" }, S.err ? "" : "Reading the prompts you answered…"))
        : list.length ? h("div", { class: "rs-sugg" }, list.map(suggestion))
          : h("div", { class: "pane" }, h("div", { class: "rs-empty" }, `Nothing to suggest: no command was approved ${s.min_approvals} times in 30 days without a No for one like it.`))),
    h("section", { class: "gp-sec" },
      label("Active grants", h("span", { class: "num" }, String(grants.length))),
      h("div", { class: "pane" }, grants.length
        ? h("table", { class: "rs-tbl" }, h("colgroup", {}, [1, 2, 3, 4, 5, 6].map((i) => h("col", { class: `c${i}` }))), h("thead", {}, h("tr", {}, ["Rule", "Where", "Granted by", "When", "Used", ""].map((t) => h("th", {}, t)))), h("tbody", {}, grants.map(grantRow)))
        : h("div", { class: "rs-empty" }, "No grant beyond Workbench's defaults.")),
      g && g.defaults ? h("p", { class: "rs-note" }, g.defaults.text) : null,
      g && (g.revoked || []).length ? h("p", { class: "rs-note" }, `Revoked: ${g.revoked.map((r) => r.pattern).join(", ")}.`) : null)));
  }

  render();
  load();
  return { update() {}, unmount() { host.remove(); } };
}
