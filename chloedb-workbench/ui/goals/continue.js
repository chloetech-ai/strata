// B. Continue a project (JOURNEYS.md): Since you left (`#/p/<p>`),
// Understand (`#/p/<p>/understand`) and Start a change (`#/p/<p>/change`),
// three places under one row of steps, with Review and ship beside them.
// `#/p` opens a folder.
import { h, useStyle, page, btn, label, kc, pane, stateDot, since, fmtClock, avatar, churnBar, md, toMs, GROUP_WORDS, icon } from "/levels/common.js";
import { repoPicker } from "/levels/repos.js";
import { groupOf, fleet, fleetWords, ORDER, sheet } from "/goals/shell.js";

const enc = encodeURIComponent;

/** The project's row of steps: places, not an order. */
export function projectSteps(p, now) {
  const ready = (p.counts && p.counts.ready) || 0;
  const tab = (k, href, text) => h("a", { href, "aria-current": k === now ? "page" : null, class: k === now ? "on" : null }, text);
  return h("nav", { class: "g-psteps", "aria-label": p.project },
    tab("continue", `#/p/${p.project}`, "Since you left"),
    tab("understand", `#/p/${p.project}/understand`, "Understand"),
    tab("change", `#/p/${p.project}/change`, "Start a change"),
    h("span", { class: "sp" }),
    h("a", { href: `#/p/${p.project}/ship`, class: `g-ship${now === "ship" ? " on" : ""}` }, `Review and ship${ready ? ` (${ready} ready)` : ""}`, icon("chev", "i sm")));
}

/** Ask about the project, answered in a sheet, with Open in Debug (power users, 7). */
export async function askSheet(ctx, p, q) {
  const body = h("div", { class: "g-ans" }, h("p", {}, "Asking ChloeCtx and the code…"));
  sheet({ kicker: `Ask about ${p.project}`, title: q, body, actions: (done) => [btn("Close", { onclick: done }), btn("Open in Debug", { href: `#/debug/code?q=${enc(q)}`, onclick: done })] });
  try {
    const r = await ctx.api("/api/ask", { method: "POST", body: { repo: p.path, question: q } });
    const text = (r.answer && (r.answer.text || r.answer)) || r.text || "";
    body.replaceChildren(
      text ? h("div", { class: "g-md" }, md(String(text))) : h("p", {}, "ChloeCtx has nothing on this yet. Debug › Why the code is like this reads the code and its history."),
      (r.missing || []).length ? h("p", { class: "g-dim" }, r.missing.join(" · ")) : null,
      r.ms != null ? h("p", { class: "g-dim num" }, `${r.ms} ms · no model`) : null);
  } catch (e) { body.replaceChildren(h("p", { class: "g-bad" }, `Not answered: ${e.message}`)); }
}

export function mount(root, ctx) {
  useStyle("g-proj", CSS);
  const el = h("div", { class: "g-proj" });
  root.append(el);
  const step = ctx.params.page;
  const S = { digest: null, decisions: null, rules: null, views: [], draft: null, sending: false };
  let alive = true;

  const p = ctx.project;
  if (step === "open" || !p) {
    ctx.crumbs([{ label: "Start", href: "#/" }, { label: ctx.params.project || "Open a project", href: location.hash }]);
    el.append(page({ kicker: "Projects", title: ctx.params.project ? `No project named ${ctx.params.project}` : "Open a project", sub: "A git checkout: Workbench reads it without an agent, and agents work in their own worktrees of it." },
      openList(ctx), h("div", { class: "gp-sec" }, picker(ctx))));
    return { update() {}, unmount: () => el.remove() };
  }
  const crumbs = { continue: "Since you left", understand: "Understand", change: "Start a change" };
  ctx.crumbs([{ label: "Start", href: "#/" }, { label: p.project, href: `#/p/${p.project}` }, ...(step === "continue" ? [] : [{ label: crumbs[step], href: location.hash }])]);

  // ----------------------------------------------------------- data
  const load = () => {
    if (step === "continue") {
      ctx.api(`/api/digest?repo=${enc(p.path)}`).then((d) => { S.digest = d; draw(); }, () => {});
      ctx.api(`/api/decisions?repo=${enc(p.path)}&limit=30`).then((d) => { S.decisions = d; draw(); }, () => { S.decisions = { decisions: [] }; draw(); });
    }
    if (step === "understand" || step === "change") ctx.api(`/api/rules?repo=${enc(p.path)}`).then((r) => { S.rules = r.rules || []; draw(); }, () => { S.rules = []; draw(); });
  };

  // ----------------------------------------------------------- Since you left
  function sinceSentence(list) {
    const d = S.digest;
    const when = d && d.since_ms ? `Since you left at ${fmtClock(d.since_ms)}, ${since(d.since_ms)} ago` : "Since you last looked";
    const c = list.map((a) => [a, groupOf(a)]);
    const bits = [];
    for (const [a, g] of c) {
      if (g === "needs") bits.push(`${a.name} waits on you`);
      else if (g === "ready") bits.push(`${a.name} is ready to review`);
      else if (g === "working") bits.push(`${a.name} is working`);
    }
    const done = d ? (d.done || []).length : 0;
    if (done) bits.push(`${done} outcome${done === 1 ? "" : "s"} done`);
    if (!bits.length) return `${when}: nothing happened.`;
    return `${when}: ${bits.slice(0, 4).join(", ")}${bits.length > 4 ? `, and ${bits.length - 4} more` : ""}.`;
  }

  function agentRow(a, g, compact) {
    const live = a.state === "waiting" ? a.waiting_for || "waits for you" : a.last_event || a.task || "";
    const r = ((p.ready || []).find((x) => x.id === a.id)) || null;
    const acts = g === "ready" ? btn("Review and ship", { sm: true, href: `#/p/${p.project}/ship/${a.id}` })
      : g === "finished" ? [btn("Restart with the same task", { sm: true, onclick: () => ctx.restart(a.id) })] : null;
    return h("div", { class: `g-arow${compact ? " compact" : ""}`, "data-nav": "", tabindex: "0", "data-href": `#/a/${a.id}` },
      avatar(a, 20),
      h("a", { class: "nm", href: `#/a/${a.id}` }, a.name),
      stateDot(g),
      h("span", { class: "live" }, g === "finished" ? `${GROUP_WORDS.finished} ${since(a.ended || a.state_since)} ago${a.task ? " · " + a.task : ""}` : live),
      r ? churnBar(r.added, r.removed) : h("span"),
      h("span", { class: "acts" }, acts));
  }

  function continuePage() {
    const N = ctx.needs();
    const mine = N.items.filter((i) => i.repo === p.path);
    const list = fleet(p.path);
    const compact = ctx.prefs.get("compact");
    const groups = ORDER.filter((g) => g !== "needs").map((g) => [g, list.filter((a) => groupOf(a) === g)]);
    const asked = h("input", { class: "field", placeholder: `Ask about ${p.project}: why does the total round half up now?`, "aria-label": `Ask about ${p.project}` });
    const ask = h("form", { class: "g-ask", onsubmit: (e) => { e.preventDefault(); const q = asked.value.trim(); if (q) askSheet(ctx, p, q); } }, icon("search", "i"), asked, btn("Ask", { type: "submit", sm: true }));
    const ready = list.filter((a) => groupOf(a) === "ready").length;
    const changed = (p.ready || []).filter((r) => r.files);
    const decs = (S.decisions && S.decisions.decisions) || [];
    const sinceMs = S.digest ? S.digest.since_ms : 0;
    const newDecs = decs.filter((d) => (toMs(d.ts) || 0) >= sinceMs && !d.superseded);
    const insp = [
      h("section", {}, label("What changed", h("span", { class: "num" }, String(changed.length))),
        changed.length ? changed.map((r) => h("a", { class: "g-chg", href: `#/p/${p.project}/ship/${r.id}` }, h("code", {}, r.branch || r.name), h("span", { class: "g-dim" }, `${r.files} file${r.files === 1 ? "" : "s"}`), churnBar(r.added, r.removed)))
          : h("p", { class: "g-dim" }, "No branch has changes waiting.")),
      h("section", {}, label("Decisions made", h("span", { class: "num" }, String(newDecs.length))),
        !S.decisions ? h("p", { class: "g-dim" }, "Reading ChloeCtx…") : newDecs.length ? newDecs.slice(0, 3).map((d) => h("div", { class: "g-dec" }, h("span", { class: "lv-ctx" }, ""), h("div", {}, h("div", {}, d.text), h("small", {}, d.agent ? `${d.agent.name}, on its own` : "you", " · ", since(d.ts), " ago")))) : h("p", { class: "g-dim" }, "None since you left."),
        newDecs.length > 3 ? h("a", { class: "lv-link", href: "#/debug/records" }, `All ${newDecs.length} →`) : null),
      h("section", {}, btn("I'm stepping away: notify me", { onclick: async () => { try { await ctx.api("/api/away/leave", { method: "POST", body: {} }); ctx.toast("Marked. Since you left counts from now."); ctx.notify && ctx.notify(); } catch (e) { ctx.toast(e.message, true); } } })),
      ctx.slot("rules-suggest"),
    ];
    return page({ kicker: "Project", title: `Continue ${p.project}`, sub: sinceSentence(list), aside: ready && !mine.length ? btn(`Review and ship (${ready})`, { primary: true, href: `#/p/${p.project}/ship`, kbd: "g r" }) : null, inspector: insp, wide: true },
      projectSteps(p, "continue"), ask,
      h("div", { class: "gp-sec" }, label(fleetWords(list))),
      mine.length ? h("div", { class: "g-needs" }, label("Needs you", h("span", { class: "num" }, String(mine.length))), mine.map((it) => ctx.needCard(it))) : null,
      groups.filter(([, l]) => l.length).map(([g, l]) => h("div", { class: "gp-sec" }, label(GROUP_WORDS[g], h("span", { class: "num" }, String(l.length))), g === "finished" && !compact
        ? h("details", { class: "g-fold" }, h("summary", {}, `${l.length} finished · show`), h("div", { class: "pane" }, l.map((a) => agentRow(a, g, compact))))
        : h("div", { class: "pane" }, l.map((a) => agentRow(a, g, compact))))),
      !list.length ? h("div", { class: "gp-sec pane g-empty" }, h("b", {}, "No agents in this project yet."), h("p", {}, "Read it first under Understand, or start one."), btn("Start a change", { primary: true, href: `#/p/${p.project}/change`, kbd: "n" })) : null);
  }

  // ----------------------------------------------------------- Understand
  let mapHost = null;
  function understandPage() {
    const q0 = ctx.params.query && ctx.params.query.get("q") || "";
    const asked = h("input", { class: "field", value: q0, placeholder: "Why does the total round half up? Where is tax computed?", "aria-label": "Ask" });
    if (!mapHost) {
      mapHost = h("div", { class: "g-map" });
      import("/levels/map.js").then((m) => { if (alive) S.views.push((m.mountBare || m.mount)(mapHost, ctx.levelCtx({ repo: p.path }))); }).catch((e) => mapHost.replaceChildren(h("p", { class: "g-bad" }, `The map did not load: ${e.message}`)));
    }
    const rules = S.rules || [];
    const insp = [
      h("section", {}, label("Agents are told", h("a", { href: "#/more/rules" }, "Edit")),
        !S.rules ? h("p", { class: "g-dim" }, "Reading the rules…") : rules.length ? rules.slice(0, 6).map((r) => h("div", { class: "g-rule" }, r.text)) : h("p", { class: "g-dim" }, "No rules yet. A rule is a decision agents are told before they start.")),
      h("section", {}, label("History"), h("p", { class: "g-dim" }, "When, why and by whom a line changed."), btn("Why the code is like this", { href: "#/debug/code" })),
    ];
    return page({ kicker: p.project, title: "Understand", sub: "Nothing here changes code or starts an agent.", inspector: insp, wide: true },
      projectSteps(p, "understand"),
      h("form", { class: "g-ask big", onsubmit: (e) => { e.preventDefault(); const q = asked.value.trim(); if (q) askSheet(ctx, p, q); } }, icon("search", "i"), asked, btn("Ask", { type: "submit", primary: true, kbd: "↵" })),
      h("div", { class: "gp-sec" }, label("Who is changing which file"), mapHost));
  }

  // ----------------------------------------------------------- Start a change
  const F = { what: null, done: null, name: null, model: null, edits: null, deny: null, extra: null, split: null, err: null, chips: null };
  function changePage() {
    if (!F.what) {
      F.what = h("textarea", { class: "field", rows: "4", placeholder: "What should change? In your words: Customers get free shipping over $50.", "aria-label": "What should change" });
      F.done = h("input", { class: "field", placeholder: "Done when: `python3 -m pytest -q` passes, and the cart shows free shipping over $50", "aria-label": "Done when" });
      F.name = h("input", { class: "field", placeholder: "picked for you", maxlength: "32", "aria-label": "Name", spellcheck: "false" });
      F.model = h("select", { class: "field", "aria-label": "Model" }, ["", "haiku", "sonnet", "opus"].map((m) => h("option", { value: m }, m || "Claude Code's default")));
      F.edits = h("input", { type: "checkbox", checked: true });
      F.deny = h("input", { type: "checkbox", checked: true });
      F.extra = h("input", { class: "field", placeholder: "Bash(python3 serve.py:*)", "aria-label": "Also allowed without asking", spellcheck: "false" });
      F.split = h("input", { type: "checkbox" });
      F.err = h("div", { class: "g-err", role: "alert", hidden: true });
      F.chips = h("div", { class: "chips" });
      const chips = () => {
        const t = `${F.what.value} ${F.done.value}`;
        const hasDone = /`[^`]+`/.test(F.done.value) || /\bpasses?\b|\bshows?\b/.test(F.done.value);
        const others = ctx.state.agents.filter((a) => a.repo === p.path && a.running && a.state === "working");
        F.chips.replaceChildren(
          h("span", { class: `chip${hasDone ? " add" : ""}` }, hasDone ? "has a done check" : "no done check yet"),
          ...(S.rules || []).filter((r) => r.text && t && r.text.split(/\W+/).some((w) => w.length > 4 && t.toLowerCase().includes(w.toLowerCase()))).slice(0, 2).map((r) => h("span", { class: "chip", title: r.text }, "◆ rule applies")),
          ...others.slice(0, 2).map((a) => h("span", { class: "chip del" }, `${a.name} is working here now`)));
      };
      F.what.addEventListener("input", chips); F.done.addEventListener("input", chips);
      const submit = (e) => { if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); start(); } };
      F.what.addEventListener("keydown", submit); F.done.addEventListener("keydown", submit);
      import("/levels/complete.js").then((m) => { if (alive) m.attachComplete(F.what, { field: "task", ctx, repo: () => p.path }); }).catch(() => {});
      chips();
      if (ctx.params.query && ctx.params.query.get("focus") !== "0") queueMicrotask(() => F.what.focus());
    }
    return page({ kicker: p.project, title: "Start a change", sub: "Say what should change and how you will know it is done.", inspector: changeInsp(), wide: true },
      projectSteps(p, "change"),
      h("form", { class: "g-comp pane", onsubmit: (e) => { e.preventDefault(); start(); } },
        h("div", { class: "bd" },
          h("label", { class: "g-l" }, "What should change"), F.what,
          h("label", { class: "g-l" }, "Done when"), F.done, F.chips,
          h("div", { class: "g-row2" },
            h("div", {}, h("label", { class: "g-l" }, "Model"), F.model),
            h("div", {}, h("label", { class: "g-l" }, "Name"), F.name)),
          h("label", { class: "g-tick" }, F.split, h("span", {}, h("b", {}, "Split it into parts"), " A lead drafts the parts first; nothing starts until you approve the plan.")),
          h("fieldset", { class: "g-perm" }, h("legend", {}, label("May do without asking")),
            h("label", { class: "g-tick" }, F.edits, h("span", {}, h("b", {}, "Edit files on its branch and run the tests"), " It asks for anything else.")),
            h("label", { class: "g-tick" }, F.deny, h("span", {}, h("b", {}, "Never"), " ", h("code", {}, "git push"), ", ", h("code", {}, "rm -rf"), ", ", h("code", {}, "pip install"))),
            h("label", { class: "g-l" }, "Also allowed (Claude Code rules, comma separated)"), F.extra),
          F.err),
        h("div", { class: "g-foot" }, h("span", { class: "g-dim" }, kc("⌘"), kc("↵"), " start"), h("span", { class: "sp" }), btn(S.sending ? "Starting…" : "Start agent", { primary: true, type: "submit", disabled: S.sending }))));
  }

  /** Start a change's inspector: drawn again on its own, so the composer keeps what you typed. */
  function changeInsp() {
    const list = fleet(p.path).filter((a) => a.running);
    return [
      h("section", {}, label("Work in progress", h("span", { class: "num" }, String(list.length))),
        list.length ? h("div", { class: "pane" }, list.map((a) => agentRow(a, groupOf(a), true))) : h("p", { class: "g-dim" }, "No agent is working in this project.")),
      h("section", {}, label("What it starts with"),
        h("dl", { class: "kv" }, h("dt", {}, "Memory"), h("dd", {}, "Remembers this project's decisions (ChloeCtx), in its own session's settings; yours are not touched."),
          h("dt", {}, "Branch"), h("dd", {}, "Its own worktree on wb/<name> from the project's HEAD. Main is never edited."),
          h("dt", {}, "Told"), h("dd", {}, "Your words, the done check, and the project's rules."))),
    ];
  }

  async function start() {
    const what = F.what.value.trim(), done = F.done.value.trim();
    F.err.hidden = true;
    if (!what) { F.err.textContent = "Say what should change first."; F.err.hidden = false; F.what.focus(); return; }
    const task = done ? `${what}\n\nDone when: ${done}` : what;
    S.sending = true;
    const go = el.querySelector(".g-comp .btn.primary");
    if (go) { go.disabled = true; go.textContent = "Starting…"; }
    try {
      if (F.split.checked) {
        ctx.go(`#/new/plan?repo=${enc(p.path)}&text=${enc(task)}`);
        return;
      }
      const allow = F.extra.value.split(",").map((s) => s.trim()).filter(Boolean);
      const deny = F.deny.checked ? ["Bash(git push:*)", "Bash(rm -rf:*)", "Bash(pip install:*)"] : [];
      const body = { repo: p.path, task, worktree: true, permissions: { allow, deny } };
      if (!F.edits.checked) body.ask_edits = true;
      if (F.name.value.trim()) body.name = F.name.value.trim();
      if (F.model.value) body.model = F.model.value;
      const { agent } = await ctx.api("/api/agents", { method: "POST", body });
      ctx.toast(`Started ${agent.name}${agent.branch ? ` on ${agent.branch}` : ""}`);
      F.what = null;
      ctx.go(`#/a/${agent.id}`);
    } catch (e) {
      F.err.textContent = e.status === 429 ? `${e.message}. Stop one first.` : e.message;
      F.err.hidden = false;
    } finally { S.sending = false; if (alive && go) { go.disabled = false; go.textContent = "Start agent"; } }
  }

  function draw() {
    if (!alive) return;
    const fresh = ctx.projectByName(p.project) || p;
    Object.assign(p, fresh);
    // Start a change is drawn once: its fields are kept, the inspector follows.
    if (step === "change" && el.querySelector(".g-comp")) { refreshInsp(); return; }
    el.replaceChildren(step === "understand" ? understandPage() : step === "change" ? changePage() : continuePage());
  }
  function refreshInsp() {
    const old = el.querySelector(".gp-insp");
    if (old) old.replaceChildren(...changeInsp());
  }

  load();
  draw();
  return {
    update: draw,
    reload: () => load(), // after an answer: "Since you left" reads the digest again
    focusComposer: () => { if (F.what) F.what.focus(); },
    unmount: () => { alive = false; for (const v of S.views) try { v.unmount(); } catch {} el.remove(); },
  };
}

function openList(ctx) {
  const ps = ctx.projects();
  if (!ps.length) return null;
  return pane("Projects Workbench knows", null, h("table", { class: "tbl g-ptbl" }, h("tbody", {}, ps.map((p) => h("tr", { "data-href": `#/p/${p.project}`, tabindex: "0", "data-nav": "", onclick: () => ctx.go(`#/p/${p.project}`) }, h("td", {}, h("b", {}, p.project)), h("td", { class: "g-dim g-ppath" }, h("code", {}, p.path)), h("td", { class: "r" }, btn("Continue", { sm: true, href: `#/p/${p.project}` })))))));
}

function picker(ctx) {
  let trust = true;
  return h("div", {},
    repoPicker(ctx, { title: "Open a folder", body: () => ({ trust }), onDone: async (repo) => { await ctx.refreshProjects(); ctx.go(`#/p/${repo.project}`); } }),
    h("label", { class: "g-tick" }, h("input", { type: "checkbox", checked: true, onchange: (e) => { trust = e.target.checked; } }), h("span", {}, h("b", {}, "Let agents work here"), " Workbench answers Claude Code's “trust this folder?” for this project's folders.")));
}

const CSS = `
.g-ptbl .g-ppath{white-space:normal;width:100%}.g-ptbl .g-ppath code{overflow-wrap:anywhere;word-break:break-all}
@media (max-width:640px){.g-ptbl td.r{display:none}}
.g-psteps{display:flex;gap:var(--s-5);border-bottom:var(--hair) solid var(--line);margin:0 0 var(--s-5);overflow-x:auto;align-items:center}
.g-psteps a{padding:10px 0;font-size:var(--fs-body);color:var(--text-2);margin-bottom:-1px;border-bottom:1.5px solid transparent;text-decoration:none;white-space:nowrap;display:inline-flex;align-items:center;gap:4px}
.g-psteps a.on{color:var(--text);font-weight:var(--w-mid);border-bottom-color:var(--text)}
.g-psteps .sp{flex:1}
.g-psteps .g-ship{color:var(--accent-text)}
.g-ask{display:flex;align-items:center;gap:var(--s-2);padding:0 var(--s-2) 0 var(--s-3);border:var(--hair) solid var(--line-2);border-radius:var(--r-sm);background:var(--surface)}
.g-ask .field{border:0;box-shadow:none;padding:0;background:transparent}
.g-ask:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}
.g-ask svg{color:var(--text-2)}
.g-ask.big{height:44px}
.g-needs{display:flex;flex-direction:column;gap:var(--s-3);margin-top:var(--s-4)}
.g-arow{display:grid;grid-template-columns:20px minmax(90px,auto) auto minmax(0,1fr) auto auto;gap:var(--s-3);align-items:center;padding:10px var(--s-4);border-bottom:var(--hair) solid var(--line);min-height:44px}
.g-arow:last-child{border-bottom:0}
.g-arow .nm{font-weight:var(--w-mid);color:var(--text);text-decoration:none}
.g-arow .live{color:var(--text-2);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.g-arow .acts{display:flex;gap:var(--s-2)}
.g-arow.compact{padding:4px var(--s-3);min-height:32px}
.g-arow:focus-visible{outline:2px solid var(--accent);outline-offset:-2px}
.gp-insp .g-arow{grid-template-columns:20px minmax(0,1fr) auto;padding:8px var(--s-3)}.gp-insp .g-arow .acts,.gp-insp .g-arow .churn,.gp-insp .g-arow>span:empty{display:none}.gp-insp .g-arow .live{grid-column:2/-1;font-size:var(--fs-sm)}
.g-fold summary{cursor:pointer;color:var(--text-2);font-size:var(--fs-sm);padding:var(--s-1) 0 var(--s-2)}
.g-chg{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:2px var(--s-2);padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line);color:var(--text);text-decoration:none}
.g-chg code{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.g-chg .churn{grid-column:1/-1}
.g-dec{display:grid;grid-template-columns:12px minmax(0,1fr);gap:var(--s-2);padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line)}
.g-dec small{display:block;color:var(--text-2);font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);margin-top:2px}
.g-dim{color:var(--text-2);margin:0}
.g-bad{color:var(--crit-ink)}
.g-md p{margin:0 0 var(--s-2)}
.g-empty{padding:var(--s-5);display:flex;flex-direction:column;gap:var(--s-2);align-items:flex-start}
.g-empty p{margin:0;color:var(--text-2)}
.g-map{position:relative;height:min(62vh,560px);border:var(--hair) solid var(--line);border-radius:var(--r);overflow:hidden;background:var(--surface)}
.g-map .lv-screen{position:absolute;inset:0;overflow:auto}
.g-rule{padding:var(--s-2) 0;border-bottom:var(--hair) solid var(--line)}
.g-comp .bd{display:flex;flex-direction:column;gap:var(--s-2)}
.g-l{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2);margin-top:var(--s-2)}
.g-row2{display:grid;grid-template-columns:1fr 1fr;gap:var(--s-3)}
.g-row2>div{display:flex;flex-direction:column;gap:var(--s-2)}
.g-tick{display:flex;gap:var(--s-2);align-items:flex-start;padding:var(--s-2) 0;color:var(--text-2);font-size:var(--fs-sm)}
.g-tick b{color:var(--text);font-weight:var(--w-mid)}
.g-tick input{margin-top:3px;accent-color:var(--accent)}
.g-perm{border:0;border-top:var(--hair) solid var(--line);margin:var(--s-2) 0 0;padding:var(--s-3) 0 0;display:flex;flex-direction:column;gap:var(--s-1)}
.g-perm legend{padding:0 0 var(--s-1)}
.g-err{padding:var(--s-2) var(--s-3);border-radius:var(--r-sm);background:var(--crit-soft);color:var(--crit-ink)}
.g-foot{display:flex;align-items:center;gap:var(--s-3);padding:var(--s-3) var(--s-4);border-top:var(--hair) solid var(--line)}
.g-foot .sp{flex:1}
.chips{display:flex;gap:6px;flex-wrap:wrap}
.g-ans{min-height:60px}
@media (max-width:640px){
  .g-arow{grid-template-columns:20px minmax(0,1fr) auto;row-gap:2px}
  .g-arow .live{grid-column:2/-1}
  .g-arow .churn{grid-column:2/-1}
  .g-arow .acts{grid-column:1/-1}
  .g-arow .acts .btn{flex:1;height:var(--touch)}
  .g-row2{grid-template-columns:1fr}
  .g-map{height:360px}
  .g-foot .btn{flex:1;height:var(--touch)}
  .g-foot .g-dim{display:none}
  .g-proj .gp{display:flex;flex-direction:column}
}
`;
