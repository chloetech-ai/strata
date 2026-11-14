// Start (JOURNEYS.md, "Start (Home)" and "First run"): what do you want to
// do? Four goals, the one that fits now filled; your projects, most active
// first; what needs you now, answered in place. With no project known: the
// folder box and Start a new project, and the setup line.
import { h, useStyle, page, btn, label, kc, pane, bank, stateDot, since, fmtClock, icon, avatar } from "/levels/common.js";
import { repoPicker } from "/levels/repos.js";

export function mount(root, ctx) {
  useStyle("g-start", CSS);
  const S = { digest: null, setup: null };
  const el = h("div", { class: "g-start" });
  root.append(el);
  ctx.crumbs([{ label: "Start", href: "#/" }]);
  ctx.api("/api/digest").then((d) => { S.digest = d; draw(); }, () => {});
  ctx.api("/api/chloectx").then((s) => { S.setup = s; draw(); }, () => { S.setup = { ok: false, problem: "Workbench could not check ChloeCtx" }; draw(); });

  function draw() {
    const projects = ctx.projects();
    const first = !projects.length && !ctx.state.agents.length;
    el.replaceChildren(first ? firstRun() : home(projects));
  }

  function setupLine() {
    const s = S.setup;
    if (!s) return h("div", { class: "g-setup" }, stateDot("finished", "Checking ChloeCtx, Claude Code and git…"));
    if (s.ok || s.required === false) return h("div", { class: "g-setup" }, stateDot("working", "Ready: ChloeCtx, Claude Code and git are set up."));
    return h("div", { class: "g-setup bad" }, stateDot("failed", s.problem || "ChloeCtx is not set up"), s.fix ? [" ", h("code", {}, s.fix)] : null);
  }

  function firstRun() {
    let trust = true;
    const picker = repoPicker(ctx, {
      title: "Open a project you have",
      why: "A git checkout. Reading needs no agent: search, the map and history work at once.",
      body: () => ({ trust }),
      onDone: (repo) => { ctx.refreshProjects().then(() => ctx.go(`#/p/${repo.project || ""}${repo.project ? "" : ""}`)); },
    });
    return page({ kicker: "Workbench", title: "What do you want to do?", sub: "Open a project you have, or start a new one. Once a project is open, this page also offers Review and ship, and Find out why something went wrong." },
      bank(2,
        h("div", { class: "g-goal" }, picker,
          h("label", { class: "g-trust" }, h("input", { type: "checkbox", checked: true, onchange: (e) => { trust = e.target.checked; } }),
            h("span", {}, h("b", {}, "Let agents work here"), " Workbench answers Claude Code's “trust this folder?” for this project's folders, so agents do not stop at it."))),
        h("div", { class: "g-goal" }, label("New"), h("b", { class: "g-gt" }, "Start a new project"), h("p", {}, "From an empty folder or an idea: describe it, and a lead agent plans the parts before anything runs."),
          h("div", { class: "g-fact" }, "Workbench runs git init and makes a first commit."), btn("Start a new project", { href: "#/new" }))),
      h("div", { class: "gp-sec" }, setupLine()));
  }

  function home(projects) {
    const N = ctx.needs();
    const d = S.digest;
    const top = projects[0];
    const readyN = projects.reduce((n, p) => n + ((p.counts && p.counts.ready) || 0), 0);
    const readyNames = projects.flatMap((p) => (p.ready || []).map((r) => r.name)).slice(0, 3);
    const needP = projects.find((p) => p.needs > 0);
    const goal = N.count && needP ? "continue" : readyN ? "review" : d && d.happened ? "continue" : "continue";
    const cont = needP || top;
    const happened = d && d.happened ? (d.story || []).length || (d.tiles ? d.tiles.done + d.tiles.decided : 0) : 0;
    const sub = d && d.last_seen_ms
      ? `You last looked ${since(d.last_seen_ms)} ago, at ${fmtClock(d.last_seen_ms)}.${happened ? ` ${happened} thing${happened === 1 ? "" : "s"} happened since.` : " Nothing happened since."}`
      : "Pick what you came to do.";
    const contFact = cont ? [h("b", {}, cont.project), `: ${cont.counts ? [cont.needs ? `${cont.needs} need${cont.needs === 1 ? "s" : ""} you` : null, cont.counts.working ? `${cont.counts.working} working` : null, cont.counts.ready ? `${cont.counts.ready} ready` : null].filter(Boolean).join(", ") || cont.last_activity || "no agents yet" : ""}`] : "no project yet";
    const tile = (k, key, title, text, fact, b, now) => h("div", { class: `g-goal${now ? " now" : ""}` },
      h("div", { class: "g-k" }, label(k), kc(key)), h("b", { class: "g-gt" }, title), h("p", {}, text), h("div", { class: "g-fact" }, fact), b);
    const goals = bank(4,
      tile("New", "g n", "Start a new project", "From an empty folder or an idea.", "Planned before anything runs", btn("Start", { href: "#/new" }), false),
      tile("Continue", "g c", cont ? `Continue ${cont.project}` : "Continue a project", "What happened since you left, and what is next.", contFact, btn(cont ? `Continue ${cont.project}` : "Open a project", { href: cont ? `#/p/${cont.project}` : "#/p", primary: goal === "continue" }), goal === "continue"),
      tile("Review", "g r", "Review and ship", "The change, its checks, and the merge.", readyN ? `${readyN} ready: ${readyNames.join(", ")}` : "Nothing is ready yet", btn("Review", { href: cont ? `#/p/${(projects.find((p) => p.counts && p.counts.ready) || cont).project}/ship` : "#/p", primary: goal === "review" }), goal === "review"),
      tile("Debug", "g d", "Find out why something went wrong", "An agent's turn, a line of code, or a failing test.", N.items.some((i) => i.kind === "alert") ? `${N.items.filter((i) => i.kind === "alert").length} alert asks you` : "Pick a turn, a line or a test", btn("Debug", { href: "#/debug" }), false));
    goals.classList.add("g-goals");
    // Optimize the harness (HARNESS.md, "Where it plugs in"): the goal
    // after the four, as a line.
    const hx = h("a", { class: "g-hx", href: "#/harness" }, icon("spark"), h("span", {}, h("b", {}, "Optimize the harness"), " What cost your agents time this week, and a change to keep."), icon("chev", "i sm"));
    const dense = ctx.prefs.get("compact");
    const rows = projects.map((p) => {
      const c = p.counts || {};
      const st = p.needs ? stateDot("needs", `${p.needs} need${p.needs === 1 ? "s" : ""} you`) : c.working ? stateDot("working", `${c.working} working`) : c.ready ? stateDot("ready", `${c.ready} ready`) : stateDot("finished", p.unmerged && p.unmerged.length ? `${p.unmerged.length} branch${p.unmerged.length === 1 ? "" : "es"} not merged` : "no agents");
      return h("tr", { "data-href": `#/p/${p.project}`, "data-nav": "", tabindex: "0", onclick: (e) => { if (!e.target.closest("a,button")) ctx.go(`#/p/${p.project}`); } },
        h("td", {}, h("b", {}, p.project), dense ? null : h("div", { class: "g-path" }, p.path)),
        h("td", {}, st),
        h("td", { class: "g-hide" }, h("span", { class: "g-avs" }, (ctx.state.agents || []).filter((a) => a.repo === p.path && a.running).slice(0, 5).map((a) => avatar(a, 16)))),
        h("td", { class: "r num g-dim" }, p.last_activity_ms ? `${since(p.last_activity_ms)} ago` : "—"),
        h("td", { class: "r" }, h("span", { class: "g-acts" }, btn("Continue", { sm: true, href: `#/p/${p.project}` }), c.ready ? btn(`Review ${c.ready}`, { sm: true, href: `#/p/${p.project}/ship` }) : null)));
    });
    const needs = N.items.slice(0, 4);
    const insp = [
      h("div", { class: "g-ih" }, label(`Needs you now`, h("span", { class: "num" }, String(N.count)))),
      needs.length ? needs.map((it) => ctx.needCard(it)) : h("p", { class: "g-dim" }, "Nothing needs you."),
      N.count > needs.length ? h("a", { class: "lv-link", href: "#/needs" }, `All ${N.count} in the inbox →`) : null,
      ctx.slot("rules-suggest"),
    ];
    return page({ kicker: "Start", title: "What do you want to do?", sub, wide: true, inspector: insp },
      goals, hx,
      h("div", { class: "gp-sec" }, pane("Your projects", h("a", { href: "#/p" }, "Open another"),
        h("table", { class: "tbl" }, h("thead", {}, h("tr", {}, h("th", {}, "Project"), h("th", {}, "State"), h("th", { class: "g-hide" }, "Agents"), h("th", { class: "r" }, "Active"), h("th", { class: "r" }, ""))), h("tbody", {}, rows)))),
      h("div", { class: "gp-sec" }, setupLine()));
  }

  draw();
  return { update: draw, unmount: () => el.remove() };
}

const CSS = `
.g-goals{grid-template-columns:1.3fr repeat(3,1fr)}
.g-goal{padding:14px var(--s-4) var(--s-4);display:flex;flex-direction:column;gap:6px;min-height:168px;min-width:0}
.g-goal .g-k{display:flex;justify-content:space-between;align-items:center}
.g-gt{font-weight:var(--w-title);font-size:var(--fs-md);letter-spacing:var(--track-md)}
.g-goal p{margin:0;color:var(--text-2)}
.g-goal .g-fact{margin-top:auto;font:var(--w-mid) var(--fs-sm)/var(--lh-sm) var(--mono);color:var(--text);overflow-wrap:anywhere}
.g-goal .btn{align-self:flex-start;margin-top:var(--s-2)}
.g-goal.now{background:linear-gradient(var(--accent-soft),var(--accent-soft)),var(--surface)}
.g-goal.now .g-fact{color:var(--accent-text)}
.g-goal .rp{border:0;box-shadow:none;padding:0;background:none}
.g-trust{display:flex;gap:var(--s-2);align-items:flex-start;font-size:var(--fs-sm);color:var(--text-2);margin-top:var(--s-2)}
.g-trust b{color:var(--text);font-weight:var(--w-mid)}
.g-path{font:var(--w-text) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2);margin-top:2px;overflow-wrap:anywhere}
.g-dim{color:var(--text-2)}
.g-hx{display:flex;align-items:center;gap:var(--s-3);margin-top:var(--s-3);padding:var(--s-3) var(--s-4);min-height:var(--touch);border:var(--hair) solid var(--line);border-radius:var(--r);background:var(--surface);color:var(--text-2);text-decoration:none;font-size:var(--fs-body)}
.g-hx b{color:var(--text);font-weight:var(--w-mid);margin-right:var(--s-1)}.g-hx>span{flex:1;min-width:0}.g-hx:hover{border-color:var(--line-2)}
.g-start .tbl td{white-space:nowrap}.g-start .tbl td:first-child{white-space:normal;width:100%}
.g-acts{display:inline-flex;gap:var(--s-2)}
.g-avs{display:inline-flex}.g-avs .lv-av{border-radius:50%;box-shadow:0 0 0 1.5px var(--surface)}.g-avs .lv-av+.lv-av{margin-left:-5px}
.g-setup{font-size:var(--fs-sm);color:var(--text-2);display:flex;gap:var(--s-2);align-items:center;flex-wrap:wrap}
.g-setup.bad{color:var(--crit-ink)}
.g-ih{display:flex}.g-ih .lbl{flex:1}
.g-start .gp-insp{gap:var(--s-4)}
@media (max-width:1024px){.g-goals{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media (max-width:640px){.g-goals{grid-template-columns:minmax(0,1fr)}.g-goal{min-height:0}.g-hide{display:none}.g-goal .btn{align-self:stretch;height:var(--touch)}.g-goal .kc{display:none}}
`;
