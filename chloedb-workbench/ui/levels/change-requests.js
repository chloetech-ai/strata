// Change requests (SPEC.md, "Fourth build", row 9: guarded actions), a
// section of Decide. An agent may not run a guarded command (a migration, a
// deploy, a push to the default branch, an rm -rf outside its worktree, or a
// command the repo's guarded list names): Workbench's PreToolUse hook denies
// it and points the agent to propose_change. The proposal comes here with its
// why, the agent's diff, the risk and what ChloeCtx remembers about such
// changes; Approve runs it in the agent's worktree (its output kept, the
// agent told the result), Decline tells the agent why. Below the requests:
// the guarded list, which you edit here.
//
//   changeRequests(ctx) -> the section's element, the same one for a ctx, so
//   that Decide can place it in each render.
import { h, useStyle, toMs, fmtAgo, fmtSpan, avatar, ctxChip, pill, repoOf, mergeAgents } from "./common.js";

const enc = encodeURIComponent;
const sections = new WeakMap();
const RISK = { high: ["high risk", "crit"], medium: ["medium risk", "warn"], low: ["low risk", "mut"] };
const isToday = (t) => new Date(toMs(t) || 0).toDateString() === new Date().toDateString();

export function changeRequests(ctx) {
  if (sections.has(ctx)) return sections.get(ctx).el;
  const S = {
    repo: repoOf(ctx), list: [], guarded: null, loaded: false, error: null, edit: false, draft: null, newPrefix: "", newPath: "",
    busy: new Set(), confirm: null, declining: null, reason: "", open: new Set(), note: null, saving: false,
  };
  const el = h("section", { class: "cr-sec", "aria-label": "Change requests" });
  const me = { el };
  sections.set(ctx, me);
  let timer = null, gone = 0;

  async function load() {
    const q = S.repo ? `?repo=${enc(S.repo)}` : "";
    try {
      const [c, g] = await Promise.all([ctx.api(`/api/changes${q}`), S.guarded && !S.edit ? Promise.resolve(S.guarded) : ctx.api(`/api/guarded${q}`).catch(() => null)]);
      S.list = c.requests || [];
      if (g) S.guarded = g;
      S.error = null;
    } catch (e) {
      S.error = e.status === 404 ? null : e.message || String(e);
      S.list = [];
    }
    S.loaded = true;
    render();
  }

  // Polls while the section is on the page; stops once it has been off it a while.
  timer = setInterval(() => {
    if (!el.isConnected) { if (++gone > 3) { clearInterval(timer); sections.delete(ctx); } return; }
    gone = 0;
    if (!S.busy.size && !S.declining && !S.edit) load();
  }, 4000);

  const agentOf = (id) => mergeAgents(ctx.state && ctx.state.agents).get(id) || null;

  async function approve(c) {
    S.busy.add(c.id); S.confirm = null; render();
    try {
      const r = await ctx.api(`/api/changes/${enc(c.id)}/approve`, { method: "POST", body: {} });
      Object.assign(c, r.request || {});
      S.open.add(c.id);
      S.note = c.state === "running" ? { ok: true, text: `\`${c.command}\` is still running in ${c.branch || "its worktree"}: ${c.agent_name || "the agent"} is told when it ends.` } : { ok: c.state === "done", text: c.state === "done" ? `Ran \`${c.command}\`: exit ${c.exit_code}. ${c.agent_name || "The agent"} is told.` : `\`${c.command}\` failed (exit ${c.exit_code ?? "?"}). ${c.agent_name || "The agent"} is told.` };
    } catch (e) {
      S.note = { ok: false, text: e.status === 403 ? "Only you can approve: this needs the token Workbench printed, not an agent's." : `Not run: ${e.message}` };
    }
    S.busy.delete(c.id);
    await load();
  }

  async function decline(c) {
    S.busy.add(c.id); render();
    try {
      const r = await ctx.api(`/api/changes/${enc(c.id)}/decline`, { method: "POST", body: { reason: S.reason.trim() || null } });
      Object.assign(c, r.request || {});
      S.note = { ok: true, text: `Declined. ${c.agent_name || "The agent"} is told${S.reason.trim() ? " why" : ""}.` };
      S.declining = null; S.reason = "";
    } catch (e) { S.note = { ok: false, text: `Not declined: ${e.message}` }; }
    S.busy.delete(c.id);
    await load();
  }

  async function saveGuarded() {
    S.saving = true; render();
    try {
      S.guarded = await ctx.api("/api/guarded", { method: "PUT", body: { repo: S.guarded.repo || S.repo, rules: S.draft } });
      S.edit = false; S.draft = null;
      S.note = { ok: true, text: "The guarded list is saved: agents' next commands are checked against it." };
    } catch (e) { S.note = { ok: false, text: `Not saved: ${e.message}` }; }
    S.saving = false; render();
  }

  // ------------------------------------------------------------- view
  function render() {
    const open = S.list.filter((c) => c.state === "open" || c.state === "running");
    const earlier = S.list.filter((c) => c.state !== "open" && c.state !== "running" && isToday(c.decided || c.created)).slice(0, 6);
    const rules = (S.guarded && S.guarded.rules) || [];
    const on = rules.filter((r) => r.on);
    el.replaceChildren(...[
      h("div", { class: "cr-head" },
        h("h2", { class: "lv-h cr-h" }, "Change requests", open.length ? h("span", { class: "cr-n" }, open.length) : null),
        h("span", { class: "cr-sub" }, "Guarded commands are proposed, never run by an agent: you approve them and Workbench runs them in its worktree."),
        h("span", { class: "cr-sp" }),
        S.guarded ? h("button", { class: "cr-glink", "aria-expanded": String(S.edit), onclick: () => { S.edit = !S.edit; S.draft = S.edit ? rules.map((r) => ({ ...r })) : null; render(); } },
          lockIcon(), `${on.length} guarded`, h("span", { class: "cr-gnames" }, on.slice(0, 3).map((r) => r.label).join(" · ") + (on.length > 3 ? ` · ${on.length - 3} more` : "")), h("b", {}, S.edit ? "Close" : "Edit")) : null),
      S.note ? h("div", { class: "cr-note " + (S.note.ok ? "ok" : "bad"), role: "status" }, mdCode(S.note.text), h("button", { class: "cr-x", "aria-label": "Dismiss", onclick: () => { S.note = null; render(); } }, "×")) : null,
      S.edit ? editor() : null,
      S.error ? h("div", { class: "cr-empty" }, `Change requests could not load: ${S.error}`) : null,
      open.length ? h("div", { class: "cr-grid" + (open.length === 1 ? " one" : "") }, open.map(card)) : S.loaded && !S.error ? h("div", { class: "cr-empty" }, "No change requests waiting. An agent that tries a guarded command is told to propose it with propose_change.") : null,
      earlier.length ? h("div", { class: "cr-earlier" }, h("div", { class: "cr-eh" }, "Earlier today"), earlier.map(row)) : null,
    ].filter(Boolean));
  }

  function card(c) {
    const a = agentOf(c.agent) || { id: c.agent, name: c.agent_name || c.agent };
    const [rl, rt] = RISK[c.risk] || RISK.medium;
    const busy = S.busy.has(c.id) || c.state === "running";
    const d = c.diff || {};
    const files = d.list || [];
    const showDiff = S.open.has("d:" + c.id) || (files.length && files.length <= 2 && !S.open.has("x:" + c.id));
    return h("article", { class: `cr-card r-${c.risk || "medium"}`, "data-key": c.id },
      h("div", { class: "cr-top" },
        avatar(a, 22), h("b", { class: "cr-who" }, a.name), h("span", { class: "cr-mut" }, "wants to run"),
        h("span", { class: "cr-sp" }), pill(rl, rt)),
      h("div", { class: "cr-cmd" }, h("span", { class: "cr-dollar" }, "$"), h("code", {}, c.command)),
      h("div", { class: "cr-where" }, "in ", h("code", {}, c.branch || shortPath(c.cwd)), " · guarded by ", h("b", {}, c.rule_label || c.rule || "the guarded list"), " · ", fmtAgo(c.created)),
      c.why ? h("p", { class: "cr-why" }, c.why) : null,
      (c.risk_why || []).length ? h("ul", { class: "cr-risks" }, c.risk_why.map((r) => h("li", {}, r))) : null,
      files.length ? h("div", { class: "cr-diff" },
        h("button", { class: "cr-dh", onclick: () => { const k = showDiff ? "x:" : "d:"; S.open.delete("d:" + c.id); S.open.delete("x:" + c.id); S.open.add(k + c.id); render(); } },
          h("span", { class: "cr-car" + (showDiff ? " on" : "") }, "▸"), `Its diff · ${d.files} file${d.files === 1 ? "" : "s"} `, h("span", { class: "cr-add" }, `+${d.added}`), " ", h("span", { class: "cr-del" }, `−${d.removed}`)),
        showDiff ? files.slice(0, 3).map((f) => h("div", { class: "cr-file" },
          h("div", { class: "cr-fn" }, h("code", {}, f.path), h("span", { class: "cr-add" }, `+${f.added}`), h("span", { class: "cr-del" }, `−${f.removed}`)),
          (f.lines || []).length ? h("pre", { class: "cr-pre" }, f.lines.slice(0, 9).map((l) => h("span", { class: l[0] === "+" ? "a" : l[0] === "-" ? "d" : l.startsWith("@@") ? "hh" : "" }, l + "\n"))) : null)) : null) : null,
      c.precedent && c.precedent.text ? h("div", { class: "cr-prec" }, ctxChip("ChloeCtx remembers"), h("span", {}, c.precedent.text)) : null,
      c.state === "running" ? h("div", { class: "cr-run" }, h("i", { class: "cr-spin" }), `Running in ${c.branch || shortPath(c.cwd)}…`) : null,
      S.declining === c.id ? h("div", { class: "cr-decl" },
        h("input", { class: "cr-in", type: "text", placeholder: `Why not? ${a.name} is told (optional)`, onfocus: (e) => import("./complete.js").then((m) => m.attachComplete(e.target, { field: "change", ctx, agent: c.agent, repo: c.repo })).catch(() => {}), value: S.reason, "aria-label": "Why you decline", oninput: (e) => { S.reason = e.target.value; },
          onkeydown: (e) => { if (e.key === "Enter") { e.preventDefault(); decline(c); } else if (e.key === "Escape") { e.stopPropagation(); S.declining = null; render(); } } }),
        h("button", { class: "btn sm", disabled: busy, onclick: () => decline(c) }, "Decline"),
        h("button", { class: "btn sm ghost", onclick: () => { S.declining = null; render(); } }, "Cancel"))
      : S.confirm === c.id ? h("div", { class: "cr-acts" },
        h("span", { class: "cr-sure" }, c.risk === "high" ? "High risk: run it now?" : "Run it now?"),
        h("button", { class: "btn sm cr-go danger", disabled: busy, onclick: () => approve(c) }, "Yes, run it"),
        h("button", { class: "btn sm ghost", onclick: () => { S.confirm = null; render(); } }, "Cancel"))
      : h("div", { class: "cr-acts" },
        h("button", { class: "btn sm primary cr-go", disabled: busy, onclick: () => { if (c.risk === "high") { S.confirm = c.id; render(); } else approve(c); } }, busy ? "Running…" : "Approve and run"),
        h("button", { class: "btn sm", disabled: busy, onclick: () => { S.declining = c.id; S.reason = ""; render(); requestAnimationFrame(() => el.querySelector(".cr-in")?.focus()); } }, "Decline"),
        h("span", { class: "cr-mut cr-tiny" }, `runs in its worktree, at most ${c.timeout_s ? Math.round(c.timeout_s / 60) : 10} min, output kept`)));
  }

  function row(c) {
    const ok = c.state === "done";
    const open = S.open.has(c.id);
    const what = c.state === "declined" ? ["declined", "mut"] : ok ? [`ran · exit ${c.exit_code}${c.ms != null ? ` · ${fmtSpan(c.ms)}` : ""}`, "good"] : [`failed · exit ${c.exit_code ?? "?"}`, "crit"];
    return h("div", { class: "cr-row" + (open ? " open" : "") },
      h("button", { class: "cr-rb", "aria-expanded": String(open), onclick: () => { open ? S.open.delete(c.id) : S.open.add(c.id); render(); } },
        h("span", { class: "cr-ic " + (c.state === "declined" ? "no" : ok ? "ok" : "bad") }, c.state === "declined" ? "✕" : ok ? "✓" : "!"),
        h("code", { class: "cr-rc" }, c.command),
        h("span", { class: "cr-mut" }, c.agent_name || ""),
        pill(...what),
        h("span", { class: "cr-sp" }),
        h("span", { class: "cr-mut cr-tiny" }, fmtAgo(c.decided || c.created))),
      open ? h("div", { class: "cr-rbody" },
        c.reason ? h("div", { class: "cr-why" }, h("b", {}, "Why not: "), c.reason) : null,
        c.why ? h("div", { class: "cr-mut" }, c.why) : null,
        c.output != null ? h("pre", { class: "cr-out" }, c.output || "(no output)") : null,
        c.told ? h("div", { class: "cr-mut cr-tiny" }, c.told === "typed" ? `${c.agent_name} was told at once (it was idle).` : `${c.agent_name} is told at its next step.`) : null) : null);
  }

  function editor() {
    const rules = S.draft || [];
    const add = () => {
      const t = S.newPrefix.trim();
      if (!t) return;
      rules.push({ id: null, kind: "prefix", label: t, detail: "commands that start with these words", on: true });
      S.newPrefix = ""; render();
      requestAnimationFrame(() => el.querySelector(".cr-addin")?.focus());
    };
    // A path guard (SPEC.md, "Sixth build: E"): files agents may read, not change.
    const addPath = () => {
      const t = S.newPath.trim().replace(/^\.\//, "");
      if (!t) return;
      rules.push({ id: null, kind: "path", label: t, detail: "files that match this path: agents may read them, not change them", on: true });
      S.newPath = ""; render();
      requestAnimationFrame(() => el.querySelector(".cr-addpath")?.focus());
    };
    const own = (r) => r.kind === "prefix" || r.kind === "path";
    return h("div", { class: "cr-ed" },
      h("div", { class: "cr-edh" }, h("b", {}, "Guarded in ", h("code", {}, (S.guarded && S.guarded.repo ? S.guarded.repo.split("/").pop() : "this repository"))),
        h("span", { class: "cr-mut" }, "An agent's Bash command that matches one is denied and comes here as a request; a guarded path is a file agents may read but not change.")),
      h("div", { class: "cr-rules" }, rules.map((r, i) => h("label", { class: "cr-rule" },
        h("input", { type: "checkbox", checked: r.on, onchange: (e) => { r.on = e.target.checked; } }),
        h("span", {}, own(r) ? h("code", {}, r.label) : h("b", {}, r.label), h("small", {}, r.detail || "")),
        own(r) ? h("button", { class: "cr-x", type: "button", "aria-label": `Remove ${r.label}`, onclick: (e) => { e.preventDefault(); rules.splice(i, 1); render(); } }, "×") : null))),
      h("div", { class: "cr-addrow" },
        h("input", { class: "cr-in cr-addin", type: "text", placeholder: "Add a command: make release, ./scripts/seed-prod …", value: S.newPrefix, oninput: (e) => { S.newPrefix = e.target.value; }, onkeydown: (e) => { if (e.key === "Enter") { e.preventDefault(); add(); } } }),
        h("button", { class: "btn sm", onclick: add }, "Add")),
      h("div", { class: "cr-addrow" },
        h("input", { class: "cr-in cr-addpath", type: "text", placeholder: "Add a path: money.py, migrations/**", value: S.newPath, oninput: (e) => { S.newPath = e.target.value; }, onkeydown: (e) => { if (e.key === "Enter") { e.preventDefault(); addPath(); } } }),
        h("button", { class: "btn sm", onclick: addPath }, "Add path"),
        h("span", { class: "cr-sp" }),
        h("button", { class: "btn sm ghost", onclick: () => { S.edit = false; S.draft = null; render(); } }, "Cancel"),
        h("button", { class: "btn sm primary", disabled: S.saving, onclick: saveGuarded }, S.saving ? "Saving…" : "Save")));
  }

  render();
  load();
  return el;
}

function shortPath(p) { return String(p || "").split("/").slice(-2).join("/"); }
function mdCode(text) {
  return String(text).split(/(`[^`]+`)/).filter(Boolean).map((p) => (p[0] === "`" && p.at(-1) === "`" ? h("code", {}, p.slice(1, -1)) : p));
}
function lockIcon() {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 16 16"); s.setAttribute("class", "cr-lock"); s.setAttribute("aria-hidden", "true");
  s.innerHTML = '<rect x="3" y="7" width="10" height="7" rx="2" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" fill="none" stroke="currentColor" stroke-width="1.5"/>';
  return s;
}

useStyle("change-requests", `
.cr-sec{margin-top:16px;display:flex;flex-direction:column;gap:10px}
.cr-head{display:flex;align-items:center;gap:6px 12px;flex-wrap:wrap}
.cr-h{margin:0;display:inline-flex;align-items:center;gap:7px}
.cr-n{display:inline-grid;place-items:center;min-width:19px;height:19px;padding:0 5px;border-radius:999px;background:var(--warn);color:#1b1404;font-size:11px;letter-spacing:0}
.cr-sub{color:var(--muted);font-size:12.5px}
.cr-sp{flex:1}
.cr-glink{display:inline-flex;align-items:center;gap:7px;border:1px solid var(--line);background:var(--surface);color:var(--text-2);border-radius:999px;height:28px;padding:0 12px 0 10px;font:inherit;font-size:12.5px;cursor:pointer;max-width:100%}
.cr-glink:hover{border-color:var(--line-2);color:var(--text)}
.cr-glink b{color:var(--accent);font-weight:650}
.cr-gnames{color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:420px}
.cr-lock{width:14px;height:14px}
.cr-note{display:flex;align-items:center;gap:10px;border-radius:9px;padding:8px 12px;font-size:12.5px}
.cr-note.ok{background:var(--good-soft);color:var(--good-ink)} .cr-note.bad{background:var(--crit-soft);color:var(--crit-ink)}
.cr-note code,.cr-rc,.cr-cmd code,.cr-fn code,.cr-where code,.cr-edh code,.cr-rule code{font-family:var(--mono);font-size:.92em}
.cr-x{margin-left:auto;border:0;background:none;color:inherit;font-size:16px;line-height:1;cursor:pointer;opacity:.7}
.cr-empty{color:var(--muted);font-size:12.5px;border:1px dashed var(--line-2);border-radius:12px;padding:12px 14px}
.cr-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:14px;align-items:start}
.cr-grid.one{grid-template-columns:minmax(0,860px)}
.cr-card{background:var(--surface);border:1px solid var(--line);border-left:3px solid var(--warn);border-radius:12px;padding:13px 15px 12px;box-shadow:var(--shadow-1);display:flex;flex-direction:column;gap:8px;min-width:0}
.cr-card.r-high{border-left-color:var(--crit)} .cr-card.r-low{border-left-color:var(--line-2)}
.cr-top{display:flex;align-items:center;gap:8px}
.cr-who{font-weight:650}
.cr-mut{color:var(--muted)} .cr-tiny{font-size:11.5px}
.cr-cmd{display:flex;gap:9px;align-items:baseline;background:var(--term-bg);border:1px solid var(--term-line);border-radius:8px;padding:8px 11px;overflow-x:auto}
.cr-cmd code{font-size:13px;color:var(--term-fg);white-space:pre}
.cr-dollar{font-family:var(--mono);color:var(--muted);user-select:none}
.cr-where{font-size:12px;color:var(--muted)} .cr-where b{color:var(--text-2);font-weight:600}
.cr-why{margin:0;color:var(--text-2);font-size:13px;line-height:1.5}
.cr-risks{margin:0;padding:0;list-style:none;display:flex;flex-wrap:wrap;gap:5px}
.cr-risks li{font-size:11.5px;color:var(--text-2);background:var(--surface-3);border-radius:6px;padding:2px 8px}
.cr-card.r-high .cr-risks li:first-child{background:var(--crit-soft);color:var(--crit-ink)}
.cr-diff{border:1px solid var(--line);border-radius:9px;overflow:hidden}
.cr-dh{display:flex;align-items:center;gap:6px;width:100%;border:0;background:var(--surface-2);padding:6px 10px;font:inherit;font-size:12px;color:var(--text-2);cursor:pointer;text-align:left}
.cr-car{display:inline-block;transition:transform .12s;color:var(--muted)} .cr-car.on{transform:rotate(90deg)}
.cr-add{color:var(--add-ink);font-family:var(--mono);font-size:11.5px} .cr-del{color:var(--del-ink);font-family:var(--mono);font-size:11.5px}
.cr-file{border-top:1px solid var(--line)}
.cr-fn{display:flex;gap:8px;align-items:baseline;padding:5px 10px;font-size:12px}
.cr-fn code{color:var(--text);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0}
.cr-pre{margin:0;padding:4px 0 6px;font-family:var(--mono);font-size:11.5px;line-height:1.5;background:var(--term-bg);overflow-x:auto;max-height:180px}
.cr-pre span{display:block;padding:0 10px;white-space:pre;color:var(--term-fg)}
.cr-pre span.a{background:var(--add-bg);color:var(--add-ink)} .cr-pre span.d{background:var(--del-bg);color:var(--del-ink)} .cr-pre span.hh{color:var(--muted)}
.cr-prec{display:flex;gap:9px;align-items:baseline;font-size:12.5px;color:var(--text-2);background:color-mix(in srgb,var(--ctx) 8%,transparent);border-radius:8px;padding:6px 10px}
.cr-prec .lv-ctx{flex:none}
.cr-acts,.cr-decl{display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin-top:2px}
.cr-sure{font-weight:650;color:var(--crit-ink);font-size:12.5px}
.btn.cr-go.danger{background:var(--crit);border-color:var(--crit);color:#fff}
.cr-in{flex:1;min-width:180px;height:30px;border:1px solid var(--line-2);border-radius:8px;background:var(--surface);color:var(--text);padding:0 10px;font:inherit;font-size:12.5px}
.cr-in:focus{outline:none;border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.cr-run{display:flex;align-items:center;gap:8px;font-size:12.5px;color:var(--text-2)}
.cr-spin{width:12px;height:12px;border-radius:50%;border:2px solid var(--line-2);border-top-color:var(--accent);animation:cr-spin .8s linear infinite}
@keyframes cr-spin{to{transform:rotate(360deg)}}
.cr-earlier{background:var(--surface);border:1px solid var(--line);border-radius:12px;overflow:hidden}
.cr-eh{font-size:11px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);padding:8px 14px 4px}
.cr-row+.cr-row{border-top:1px solid var(--line)}
.cr-rb{display:flex;align-items:center;gap:10px;width:100%;border:0;background:none;padding:7px 14px;font:inherit;font-size:12.5px;color:var(--text-2);cursor:pointer;text-align:left}
.cr-rb:hover{background:var(--surface-2)}
.cr-ic{display:inline-grid;place-items:center;width:18px;height:18px;border-radius:50%;font-size:11px;font-weight:800;flex:none}
.cr-ic.ok{background:var(--good-soft);color:var(--good-ink)} .cr-ic.no{background:var(--surface-3);color:var(--muted)} .cr-ic.bad{background:var(--crit-soft);color:var(--crit-ink)}
.cr-rc{color:var(--text);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0;max-width:46%}
.cr-rbody{padding:2px 14px 12px 42px;display:flex;flex-direction:column;gap:6px;font-size:12.5px}
.cr-out{margin:0;max-height:220px;overflow:auto;font-family:var(--mono);font-size:11.5px;line-height:1.5;background:var(--term-bg);color:var(--term-fg);border:1px solid var(--term-line);border-radius:8px;padding:8px 10px;white-space:pre-wrap}
.cr-ed{background:var(--surface);border:1px solid var(--line-2);border-radius:12px;padding:12px 14px;display:flex;flex-direction:column;gap:10px;box-shadow:var(--shadow-1)}
.cr-edh{display:flex;flex-wrap:wrap;gap:4px 12px;align-items:baseline;font-size:13px}
.cr-rules{display:grid;grid-template-columns:repeat(auto-fill,minmax(300px,1fr));gap:6px 14px}
.cr-rule{display:flex;align-items:flex-start;gap:9px;padding:6px 8px;border-radius:8px;cursor:pointer}
.cr-rule:hover{background:var(--surface-2)}
.cr-rule input{margin-top:3px;accent-color:var(--accent)}
.cr-rule>span{display:flex;flex-direction:column;min-width:0;font-size:12.5px}
.cr-rule small{color:var(--muted);font-size:11.5px}
.cr-rule .cr-x{margin-left:auto;color:var(--muted)}
.cr-addrow{display:flex;gap:8px;align-items:center;flex-wrap:wrap}
@media (max-width:1000px){.cr-grid{grid-template-columns:minmax(0,1fr)}.cr-gnames{display:none}}
`);
