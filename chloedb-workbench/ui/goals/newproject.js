// A. Start a new project (JOURNEYS.md): Folder › What to build › Who builds
// it › Plan it. Nothing runs until Start on the plan. Routes `#/new`,
// `#/new/what?dir=`, `#/new/plan?repo=&text=`. The plan is the lead's
// draft (`POST /api/goals` with `plan: "draft"`), editable, then started
// as the goal's outcomes, one agent per part; a part that waits, waits.
import { h, useStyle, page, btn, label, kc, pane, stateDot } from "/levels/common.js";

const enc = encodeURIComponent;
const MAX_AT_ONCE = 3;

// What the person typed, kept while they move between the steps.
const D = { dir: "", idea: "", done: "", who: "lead", plan: null, planFor: "", repo: "", busy: false, err: "" };

export function mount(root, ctx) {
  useStyle("g-new", CSS);
  const el = h("div", { class: "g-new" });
  root.append(el);
  const q = ctx.params.query || new URLSearchParams();
  const step = ctx.params.step === "what" ? "what" : ctx.params.step === "plan" ? "plan" : "folder";
  if (q.get("dir")) D.dir = q.get("dir");
  if (q.get("repo")) D.repo = q.get("repo");
  if (q.get("text") && !D.idea) D.idea = q.get("text");
  let alive = true;
  document.body.classList.add("stepping");
  ctx.crumbs([{ label: "Start", href: "#/" }, { label: "New project", href: "#/new" }, ...(step === "folder" ? [] : [{ label: step === "what" ? "What to build" : "Plan it", href: location.hash }])]);

  const steps = (now) => ({
    list: [
      { label: "Folder", href: "#/new", state: now === "folder" ? "now" : "done" },
      { label: "What to build", href: D.dir || D.repo ? `#/new/what?dir=${enc(D.dir || D.repo)}` : null, state: now === "what" ? "now" : now === "plan" ? "done" : "next" },
      { label: "Who builds it", href: null, state: now === "what" ? "next" : now === "plan" ? "done" : "next" },
      { label: "Plan it", href: null, state: now === "plan" ? "now" : "next" },
    ],
  });

  const bar = (back, next) => h("div", { class: "g-mbar" }, back, next);

  // ------------------------------------------------------------ 1 Folder
  function folder() {
    const input = h("input", { class: "field mono", value: D.dir, placeholder: "/home/you/src/new-thing", "aria-label": "The new project's folder", spellcheck: "false", autocomplete: "off" });
    const err = h("div", { class: "g-err", hidden: !D.err }, D.err);
    const next = async (e) => {
      if (e) e.preventDefault();
      const dir = input.value.trim();
      if (!dir.startsWith("/") && !dir.startsWith("~")) { err.textContent = "The folder is an absolute path, such as /home/you/src/new-thing."; err.hidden = false; return; }
      D.dir = dir; D.err = "";
      try {
        const r = await ctx.api("/api/repos", { method: "POST", body: { path: dir, init: true, trust: true } });
        D.repo = r.repo.path || dir;
        await ctx.refreshProjects();
        ctx.go(`#/new/what?dir=${enc(D.repo)}`);
      } catch (x) { err.textContent = x.message; err.hidden = false; }
    };
    queueMicrotask(() => input.focus());
    return page({ steps: steps("folder"), kicker: "New project", title: "Where should it live?", sub: "An empty folder, or a new one. Workbench runs git init and makes a first commit, so agents can branch from it." },
      h("form", { class: "pane g-form", onsubmit: next },
        h("div", { class: "bd" }, label("Folder"), input, err,
          h("p", { class: "g-dim" }, "A folder that is already a repository works too: nothing in it is changed.")),
        h("div", { class: "g-foot" }, h("span", { class: "sp" }), btn("Next", { primary: true, type: "submit", kbd: "↵" }))),
      bar(btn("Back", { href: "#/", icon: "back" }), btn("Next", { primary: true, onclick: next })));
  }

  // ------------------------------------------------------------ 2, 3 What to build, Who builds it
  function what() {
    const idea = h("textarea", { class: "field", rows: "6", placeholder: "In your own words: a small shop with a cart, coupons and a checkout page. Totals in integer cents.", "aria-label": "What to build" });
    idea.value = D.idea;
    const done = h("input", { class: "field", value: D.done, placeholder: "Done when: `python3 -m pytest -q` passes and the checkout page shows the total", "aria-label": "Done when" });
    const chip = h("span", { class: "chip" });
    const sync = () => { D.idea = idea.value; D.done = done.value; const ok = /`[^`]+`/.test(D.done) || /\bpass(es)?\b/.test(D.done); chip.className = `chip${ok ? " add" : ""}`; chip.textContent = ok ? "has a done check" : "no done check yet: say how you will know it works"; };
    idea.addEventListener("input", sync); done.addEventListener("input", sync); sync();
    import("/levels/complete.js").then((m) => { if (alive) m.attachComplete(idea, { field: "task", ctx, repo: () => D.repo || D.dir }); }).catch(() => {});
    const who = (k, t, sub) => h("label", { class: `g-who${D.who === k ? " on" : ""}` }, h("input", { type: "radio", name: "who", value: k, checked: D.who === k || null, onchange: () => { D.who = k; draw(); } }), h("span", {}, h("b", {}, t), h("small", {}, sub)));
    const go = () => { sync(); if (!D.idea.trim()) { idea.focus(); return; } ctx.go(`#/new/plan?repo=${enc(D.repo || D.dir)}`); };
    queueMicrotask(() => { if (!D.idea) idea.focus(); });
    return page({ steps: steps("what"), kicker: `New project · ${String(D.repo || D.dir || "").split("/").filter(Boolean).pop() || ""}`, title: "What to build", sub: "Say it as you would to a colleague. Nothing runs yet." },
      h("div", { class: "pane g-form" }, h("div", { class: "bd" },
        label("The idea"), idea, label("Done when"), done, h("div", { class: "chips" }, chip))),
      h("div", { class: "gp-sec" }, label("Who builds it"),
        h("div", { class: "g-whos", role: "radiogroup" },
          who("lead", "A lead agent", "It reads the idea, splits it into parts, and starts one agent per part after you approve. It only plans in this step: about a minute, on haiku."),
          who("single", "A single agent", "One branch, one session: cheaper, for a small idea."))),
      h("div", { class: "g-foot plain" }, h("span", { class: "sp" }), btn(D.who === "lead" ? "Plan it" : "Next: check it", { primary: true, onclick: go, kbd: "⌘↵" })),
      bar(btn("Back", { href: "#/new", icon: "back" }), btn(D.who === "lead" ? "Plan it" : "Next", { primary: true, onclick: go })));
  }

  // ------------------------------------------------------------ 4 Plan it
  async function draft(change) {
    const text = (D.done ? `${D.idea}\n\nDone when: ${D.done}` : D.idea) + (change ? `\n\nChange the plan: ${change}` : "");
    D.busy = true; D.err = ""; draw();
    try {
      if (D.who === "single") {
        D.plan = { parts: [{ title: D.idea.split("\n")[0].slice(0, 140), done: D.done || null, after: [], model: null }], drafted_by: "you", missing: [] };
      } else {
        D.plan = await ctx.api("/api/goals", { method: "POST", body: { repo: D.repo || D.dir, text, plan: "draft" } });
      }
      D.planFor = text;
    } catch (e) { D.err = e.message; }
    D.busy = false;
    if (alive) draw();
  }

  async function startPlan() {
    const parts = D.plan.parts;
    D.busy = true; draw();
    try {
      const outcomes = parts.map((p) => ({ title: p.title, detail: p.done || null, after: p.after && p.after.length ? Math.max(...p.after) : null }));
      const r = await ctx.api("/api/goals", { method: "POST", body: { repo: D.repo || D.dir, text: D.done ? `${D.idea}\n\nDone when: ${D.done}` : D.idea, outcomes, worktree: true } });
      const proj = await ctx.refreshProjects().then(() => ctx.projectOf(D.repo || D.dir));
      const n = (r.started || r.agents || []).length;
      ctx.toast(`Started ${n || "the"} agent${n === 1 ? "" : "s"}${(r.missing || []).length ? `; ${r.missing.length} part${r.missing.length === 1 ? "" : "s"} wait` : ""}`);
      Object.assign(D, { idea: "", done: "", plan: null, planFor: "" });
      ctx.go(proj ? `#/p/${proj.project}` : "#/");
    } catch (e) { D.err = e.message; D.busy = false; draw(); }
  }

  function plan() {
    const repo = D.repo || D.dir;
    if (!D.idea.trim()) return page({ steps: steps("plan"), title: "Plan it", sub: "Say what to build first." }, btn("What to build", { href: `#/new/what?dir=${enc(repo)}`, primary: true }));
    if (!D.plan && !D.busy && !D.err) { queueMicrotask(() => draft()); }
    const parts = (D.plan && D.plan.parts) || [];
    const move = (i, d) => { const j = i + d; if (j < 0 || j >= parts.length) return; [parts[i], parts[j]] = [parts[j], parts[i]]; for (const p of parts) p.after = (p.after || []).map((x) => (x === i ? j : x === j ? i : x)); draw(); };
    const rm = (i) => { parts.splice(i, 1); for (const p of parts) p.after = (p.after || []).filter((x) => x !== i).map((x) => (x > i ? x - 1 : x)); draw(); };
    const waitWords = (p, i) => (p.after && p.after.length ? `waits for part ${p.after.map((x) => x + 1).join(", ")}` : i === 0 ? "starts first" : "starts at once");
    const rows = parts.map((p, i) => h("div", { class: "g-part", "data-nav": "", tabindex: "0" },
      h("span", { class: "g-pn num" }, String(i + 1).padStart(2, "0")),
      h("div", { class: "g-pt" },
        h("input", { class: "field", value: p.title, "aria-label": `Part ${i + 1}`, oninput: (e) => { p.title = e.target.value; } }),
        h("input", { class: "field g-pd", value: p.done || "", placeholder: "Done when…", "aria-label": `Part ${i + 1}: done when`, oninput: (e) => { p.done = e.target.value; } }),
        h("div", { class: "g-pm" }, h("span", { class: "chip" }, waitWords(p, i)), h("span", { class: "chip" }, p.model || "Claude Code's default model"))),
      h("div", { class: "g-pa" }, btn("↑", { sm: true, ghost: true, title: "Earlier", onclick: () => move(i, -1), disabled: i === 0 }), btn("↓", { sm: true, ghost: true, title: "Later", onclick: () => move(i, 1), disabled: i === parts.length - 1 }), btn("Remove", { sm: true, ghost: true, onclick: () => rm(i) }))));
    const change = h("input", { class: "field", placeholder: "Tell the lead what to change: merge parts 2 and 3, add a receipts page…", "aria-label": "Tell the lead what to change" });
    const n = parts.length;
    const startLabel = n > MAX_AT_ONCE ? `Start ${n} agents, ${MAX_AT_ONCE} at a time` : `Start ${n} agent${n === 1 ? "" : "s"}`;
    const phone = matchMedia("(max-width: 640px)").matches;
    const insp = [h("section", {}, label("What each agent starts with"),
      h("dl", { class: "kv" },
        h("dt", {}, "Memory"), h("dd", {}, "Remembers this project's decisions (ChloeCtx). Its hooks and memory tools go in each session's own settings; yours are not touched."),
        h("dt", {}, "Told"), h("dd", {}, "Your idea, its part, the done check, and the project's rules."),
        h("dt", {}, "Models"), h("dd", {}, "As set on each part."),
        h("dt", {}, "Branch"), h("dd", {}, "Its own worktree on wb/<part> from main. Main is never edited."),
        h("dt", {}, "Without asking"), h("dd", {}, "Edit files on its branch and run the tests (python3 -m pytest, npm test, cargo test). It asks for anything else, and never runs git push or pip install."),
        h("dt", {}, "At once"), h("dd", {}, `At most ${MAX_AT_ONCE} agents; a part that waits on another waits.`)))];
    return page({ steps: steps("plan"), kicker: `New project · ${String(repo || "").split("/").filter(Boolean).pop() || ""}`, title: "Plan it", sub: D.busy && !D.plan ? "The lead is reading the idea…" : D.plan && D.plan.drafted_by === "words" ? "Your own sentences, one part each: edit them, or tell the lead what to change." : "The lead's draft. Reorder, edit or remove parts before anything starts.", inspector: insp, wide: true },
      D.err ? h("div", { class: "g-err" }, D.err) : null,
      D.plan && (D.plan.missing || []).length ? h("p", { class: "g-dim" }, D.plan.missing.join(" · ")) : null,
      h("div", { class: "pane" }, h("div", { class: "hd" }, label(`The parts (${n})`, D.plan && D.plan.drafted_by === "model" ? h("span", {}, `drafted by ${D.plan.model || "haiku"}`) : null)),
        rows.length ? rows : h("div", { class: "bd" }, stateDot("working", D.busy ? "Drafting…" : "No parts.")),
        h("div", { class: "bd g-add" }, btn("Add a part", { sm: true, icon: "plus", onclick: () => { parts.push({ title: "", done: "", after: [], model: null }); draw(); } }))),
      D.who === "lead" ? h("form", { class: "gp-sec g-ask2", onsubmit: (e) => { e.preventDefault(); if (change.value.trim()) draft(change.value.trim()); } }, change, btn("Redraft", { type: "submit", sm: true, disabled: D.busy })) : null,
      phone ? null : h("div", { class: "g-foot plain" }, h("span", { class: "g-dim" }, "Nothing has run yet."), h("span", { class: "sp" }), btn(D.busy && D.plan ? "Starting…" : startLabel, { primary: true, disabled: D.busy || !n, onclick: startPlan, kbd: "⌘↵" })),
      bar(btn("Back", { href: `#/new/what?dir=${enc(repo)}`, icon: "back" }), btn(startLabel, { primary: true, disabled: D.busy || !n, onclick: startPlan })));
  }

  function draw() {
    if (!alive) return;
    const f = document.activeElement;
    const typing = f && el.contains(f) && f.matches("input,textarea") && step !== "folder";
    if (typing && step === "what") return;
    el.replaceChildren(step === "what" ? what() : step === "plan" ? plan() : folder());
  }
  draw();
  return {
    update() {},
    onKey(e) { if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { const b = el.querySelector(".g-foot .btn.primary"); if (b) { b.click(); return true; } } return false; },
    unmount: () => { alive = false; document.body.classList.remove("stepping"); el.remove(); },
  };
}

const CSS = `
.g-form .bd{display:flex;flex-direction:column;gap:var(--s-2)}
.g-foot{display:flex;align-items:center;gap:var(--s-3);padding:var(--s-3) var(--s-4);border-top:var(--hair) solid var(--line)}
.g-foot.plain{border-top:0;padding:var(--s-4) 0 0}
.g-foot .sp{flex:1}
.g-dim{color:var(--text-2);margin:0}
.g-err{padding:var(--s-2) var(--s-3);border-radius:var(--r-sm);background:var(--crit-soft);color:var(--crit-ink)}
.g-whos{display:grid;grid-template-columns:1fr 1fr;gap:var(--s-3)}
.g-who{display:flex;gap:var(--s-3);align-items:flex-start;padding:var(--s-3) var(--s-4);border:var(--hair) solid var(--line);border-radius:var(--r);background:var(--surface);cursor:pointer}
.g-who.on{border-color:var(--accent-line);background:linear-gradient(var(--accent-soft),var(--accent-soft)),var(--surface)}
.g-who b{display:block;font-weight:var(--w-title)}
.g-who small{display:block;color:var(--text-2);font-size:var(--fs-sm);line-height:var(--lh-sm);margin-top:2px}
.g-who input{margin-top:4px;accent-color:var(--accent)}
.g-part{display:grid;grid-template-columns:28px minmax(0,1fr) auto;gap:var(--s-3);padding:var(--s-3) var(--s-4);border-bottom:var(--hair) solid var(--line);align-items:start}
.g-pn{color:var(--text-2);padding-top:8px}
.g-pt{display:flex;flex-direction:column;gap:6px;min-width:0}
.g-pt .field{height:32px}
.g-pd{font-size:var(--fs-sm)}
.g-pm{display:flex;gap:6px;flex-wrap:wrap}
.g-pa{display:flex;gap:2px}
.g-add{display:flex}
.g-ask2{display:flex;gap:var(--s-2);align-items:center}
.g-mbar{display:none}
.chips{display:flex;gap:6px;flex-wrap:wrap}
@media (max-width:640px){
  .g-whos{grid-template-columns:1fr}
  .g-part{grid-template-columns:24px minmax(0,1fr)}
  .g-pa{grid-column:1/-1}
  .g-pt .field{height:44px}
  .g-new .gp-main>.gp-head+*,.g-new .g-foot.plain,.g-new .pane .g-foot{display:none}
  .g-mbar{display:flex;gap:var(--s-2);position:fixed;left:0;right:0;bottom:0;z-index:15;padding:var(--s-2) max(16px,env(safe-area-inset-right)) calc(var(--s-2) + env(safe-area-inset-bottom)) max(16px,env(safe-area-inset-left));background:var(--glass);-webkit-backdrop-filter:var(--glass-blur);backdrop-filter:var(--glass-blur);border-top:var(--hair) solid var(--line)}
  .g-mbar .btn{height:var(--touch)}
  .g-mbar .btn.primary{flex:1}
  .g-new .gp-main{padding-bottom:96px}
  .g-new .gp{display:flex;flex-direction:column}
}
`;
