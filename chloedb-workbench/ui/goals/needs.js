// The inbox (power users, 1): everything that needs you, across projects,
// in the one question card. Filters by project, agent and kind; sorted by
// the oldest or by the most work an answer unblocks. `#/needs?project=&
// agent=&kind=&sort=` keeps the filters in the address.
import { h, useStyle, page, btn, kc, parseJSON, toMs } from "/levels/common.js";

const KINDS = [["", "All"], ["permission", "Permission"], ["question", "Question"], ["trust", "Trust"], ["alert", "Alert"], ["waiting", "Waiting"]];

export function kindOf(it) {
  if (it.kind === "alert" || it.kind === "trust" || it.kind === "waiting") return it.kind;
  return it.question && it.question.kind === "ask" ? "question" : "permission";
}

const cmdOf = (it) => (it.question && (parseJSON(it.question.detail, {}) || {}).command) || null;

/** How much work an answer unblocks: the agents asking the identical
 *  command (one answer each, batchable), plus the agents it spawned. */
export function blocked(it, all) {
  const c = cmdOf(it);
  const same = c ? all.filter((x) => cmdOf(x) === c).length : 1;
  const kids = (it.agent && Array.isArray(it.agent.children) ? it.agent.children.length : 0);
  return same + kids;
}

export function mount(root, ctx) {
  useStyle("g-needs", CSS);
  const q = ctx.params.query || new URLSearchParams();
  const F = { project: q.get("project") || "", agent: q.get("agent") || "", kind: q.get("kind") || "", sort: q.get("sort") === "blocked" ? "blocked" : "oldest" };
  const el = h("div", { class: "g-needs" });
  root.append(el);
  ctx.crumbs([{ label: "Start", href: "#/" }, { label: "Needs you", href: "#/needs" }]);

  const keep = () => {
    const p = new URLSearchParams();
    for (const k of ["project", "agent", "kind"]) if (F[k]) p.set(k, F[k]);
    if (F.sort !== "oldest") p.set("sort", F.sort);
    const hash = `#/needs${p.toString() ? "?" + p : ""}`;
    if (location.hash !== hash) history.replaceState(history.state, "", hash);
  };

  function draw() {
    const N = ctx.needs();
    const names = ctx.projectNames();
    const all = N.items;
    const projects = [...new Set(all.map((it) => names[it.repo] || it.repo).filter(Boolean))];
    const agents = [...new Map(all.filter((it) => it.agent).map((it) => [it.agent.id, it.agent])).values()];
    let list = all.filter((it) => (!F.project || (names[it.repo] || it.repo) === F.project) && (!F.agent || (it.agent && it.agent.id === F.agent)) && (!F.kind || kindOf(it) === F.kind));
    if (F.sort === "blocked") list = [...list].sort((a, b) => blocked(b, all) - blocked(a, all) || (toMs(a.since_ms) || 0) - (toMs(b.since_ms) || 0));
    const sel = (key, opts, labelText) => h("label", { class: "g-f" }, h("span", {}, labelText),
      h("select", { class: "field", onchange: (e) => { F[key] = e.target.value; keep(); draw(); } },
        opts.map(([v, t]) => h("option", { value: v, selected: F[key] === v ? true : null }, t))));
    const filtered = list.length !== all.length;
    const sub = all.length
      ? `${all.length} across ${projects.length} project${projects.length === 1 ? "" : "s"}. ${F.sort === "blocked" ? "The answers that unblock the most work first." : "Oldest first."}`
      : "Nothing needs you: no agent is waiting on you, and no alert asks you.";
    el.replaceChildren(page({ kicker: "Inbox", title: "Needs you", sub, wide: true },
      all.length ? h("div", { class: "g-fbar" },
        sel("project", [["", "Every project"], ...projects.map((p) => [p, p])], "Project"),
        sel("agent", [["", "Every agent"], ...agents.map((a) => [a.id, a.name])], "Agent"),
        h("div", { class: "g-kinds", role: "group", "aria-label": "Kind" }, KINDS.map(([v, t]) => {
          const n = v ? all.filter((it) => kindOf(it) === v).length : all.length;
          return v && !n ? null : h("button", { type: "button", class: `chip${F.kind === v ? " on" : ""}`, "aria-pressed": String(F.kind === v), onclick: () => { F.kind = v; keep(); draw(); } }, t, h("span", { class: "num" }, ` ${n}`));
        })),
        h("span", { class: "sp" }),
        h("div", { class: "g-sort", role: "group", "aria-label": "Order" },
          btn("Oldest", { sm: true, ghost: F.sort !== "oldest", onclick: () => { F.sort = "oldest"; keep(); draw(); } }),
          btn("Most work blocked", { sm: true, ghost: F.sort !== "blocked", onclick: () => { F.sort = "blocked"; keep(); draw(); } }))) : null,
      h("div", { class: "g-list" },
        list.length ? list.map((it) => {
          const card = ctx.needCard(it, { big: true });
          const proj = names[it.repo];
          return h("div", { class: "g-item" }, proj ? h("div", { class: "g-proj" }, h("a", { href: `#/p/${proj}` }, proj), F.sort === "blocked" && blocked(it, all) > 1 ? h("span", {}, ` · unblocks ${blocked(it, all)}`) : null) : null, card);
        }) : all.length ? h("p", { class: "g-dim" }, "Nothing matches these filters. ", filtered ? h("a", { href: "#/needs", onclick: () => { F.project = F.agent = F.kind = ""; } }, "Show all") : null) : null),
      all.length ? h("p", { class: "g-keys" }, kc("j"), kc("k"), " move · a digit then ", kc("↵"), " answers · ", kc("x"), " selects · ", kc("⇧1"), " answers the selected alike") : null));
  }

  draw();
  return { update: draw, unmount: () => el.remove() };
}

const CSS = `
.g-fbar{display:flex;gap:var(--s-3);align-items:flex-end;flex-wrap:wrap;margin-bottom:var(--s-4)}
.g-fbar .sp{flex:1}
.g-f{display:flex;flex-direction:column;gap:4px;font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);letter-spacing:var(--track-label);text-transform:uppercase;color:var(--text-2)}
.g-f select{min-width:160px;text-transform:none;letter-spacing:0}
.g-kinds{display:flex;gap:var(--s-1);flex-wrap:wrap}
.g-kinds .chip{cursor:pointer;border:0;height:var(--ctl-h);padding:0 var(--s-3);font-size:var(--fs-sm);line-height:1;border-radius:var(--r-sm)}
.g-kinds .chip.on{background:var(--text);color:var(--bg);border-color:var(--text)}
.g-sort{display:flex;gap:var(--s-1)}
.g-list{display:flex;flex-direction:column;gap:var(--s-4);max-width:900px}
.g-item{display:flex;flex-direction:column;gap:var(--s-1)}
.g-proj{font:var(--w-mid) var(--fs-xs)/var(--lh-xs) var(--mono);color:var(--text-2)}
.g-proj a{color:var(--text-2)}
.g-needs .g-dim{color:var(--text-2)}
.g-needs .g-keys{margin-top:var(--s-5);font-size:var(--fs-sm);color:var(--text-2)}
@media (max-width:640px){.g-f{flex:1 1 140px}.g-f select{min-width:0;width:100%}.g-fbar .sp{display:none}.g-needs .g-keys{display:none}}
`;
