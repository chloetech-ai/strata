// The global frame of the journeys mock (JOURNEYS.md, "Global elements"):
// brand, breadcrumb, Search, Needs you, the agent switcher and More. Nothing
// else is global: no left rail, no level pills, no time bar.
import { h, icon, mark, avatar, questionCard, btn } from "./ui.js";
import { agents, needs, stateWords, PROJECT } from "./fixtures.js";

const S = { crumbs: [], open: null, first: false, needs: needs.slice() };

export function setFirstRun(on) { S.first = on; }
export function setCrumbs(list) { S.crumbs = list; draw(); }
export function openOverlay(which) { S.open = which; draw(); }
export function answered(id) { S.needs = S.needs.filter((q) => q.id !== id); draw(); }
export function needsList() { return S.needs; }

let top, layer, tabs;
export function mountShell(app) {
  top = h("header", { class: "jt" });
  layer = h("div", { class: "jlayer" });
  tabs = h("nav", { class: "jtab", "aria-label": "Sections" });
  app.prepend(top);
  app.append(tabs, layer);
  document.addEventListener("keydown", (e) => {
    if (/^(INPUT|TEXTAREA|SELECT)$/.test(e.target.tagName) || e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === "Escape" && S.open) { S.open = null; draw(); e.preventDefault(); return; }
    if (e.key === ".") { S.open = S.open === "needs" ? null : "needs"; draw(); e.preventDefault(); }
    else if (e.key === "a") { S.open = S.open === "agents" ? null : "agents"; draw(); e.preventDefault(); }
    else if (e.key === "/") { S.open = "search"; draw(); e.preventDefault(); }
    else if (S.open === "agents" && /^[1-9]$/.test(e.key)) { const r = switcherRows()[Number(e.key) - 1]; if (r) { S.open = null; location.hash = `#/a/${r.id}`; } }
  });
  window.addEventListener("hashchange", () => { S.open = null; draw(); });
  draw();
}

function toggle(which) { return () => { S.open = S.open === which ? null : which; draw(); }; }

function draw() {
  if (!top) return;
  const running = agents.filter((a) => a.running);
  const n = S.first ? 0 : S.needs.length;
  top.replaceChildren(...[
    h("a", { class: "brand", href: "#/" }, mark(), "Workbench"),
    h("nav", { class: "crumbs", "aria-label": "Where you are" },
      S.crumbs.map((c, i) => [i ? icon("chev", "ji sm") : null, h("a", { href: c.href || "#/" }, c.label)])),
    h("span", { class: "sep" }),
    h("button", { class: "jt-search", type: "button", onclick: toggle("search") }, icon("search", "ji sm"), S.first ? "Search a project" : `Search ${PROJECT.name}`, h("kbd", {}, "/")),
    h("button", { class: `jt-needs${n ? "" : " none"}`, type: "button", onclick: toggle("needs"), title: "What is blocked on you (.)" },
      n ? [h("b", {}, String(n)), "need you"] : [icon("check", "ji sm"), "Nothing needs you"]),
    S.first ? null : h("button", { class: "jt-agents", type: "button", onclick: toggle("agents"), title: "Your agents (a)" },
      h("span", { class: "stack" }, running.slice(0, 4).map((a) => avatar(a, 22))),
      running.some((a) => a.state === "waiting") ? h("span", { class: "ndot" }) : null,
      h("span", { class: "jt-at" }, `${running.length} agents`), h("span", { class: "jt-an" }, icon("term", "ji sm"), String(running.length)), icon("down", "ji sm")),
    h("button", { class: "btn ghost icon jt-more", type: "button", "aria-label": "More", title: "More", onclick: toggle("more") }, icon("more")),
    phoneTitle(),
    h("button", { class: "btn ghost icon jt-msearch", type: "button", "aria-label": "Search", onclick: toggle("search") }, icon("search")),
  ].filter(Boolean));
  tabs.replaceChildren(...tabBar(n));
  layer.replaceChildren(...overlay());
}

/** On a phone the breadcrumb is a back link to the parent and the page's name (JOURNEYS.md, "Phone"). */
function phoneTitle() {
  const c = S.crumbs, last = c[c.length - 1], up = c.length > 1 ? c[c.length - 2] : null;
  return h("div", { class: "jt-m" },
    up ? h("a", { class: "jt-back", href: up.href || "#/", "aria-label": `Back to ${up.label}` }, icon("back"), h("span", {}, up.label)) : h("a", { class: "brand", href: "#/" }, mark()),
    h("b", {}, last ? last.label : "Workbench"));
}

/** The phone's bottom tab bar: Start, Needs you (with its count), Projects, More. */
function tabBar(n) {
  const hash = location.hash || "#/";
  const at = S.open === "needs" ? "needs" : S.open === "more" || hash.startsWith("#/more") ? "more"
    : hash === "#/" || hash === "#" || hash.startsWith("#/new") ? "start" : "projects";
  const close = () => { if (S.open) { S.open = null; draw(); } };
  const tab = (k, label, ic, o) => h(o.href ? "a" : "button", { class: `jtab-i${at === k ? " on" : ""}`, href: o.href || null, type: o.href ? null : "button", onclick: o.onclick || close, "aria-current": at === k ? "page" : null },
    h("span", { class: "jtab-ic" }, icon(ic), o.badge ? h("b", { class: "jtab-n" }, String(o.badge)) : null), h("span", {}, label));
  return [
    tab("start", "Start", "home", { href: "#/" }),
    tab("needs", "Needs you", "bell", { badge: n, onclick: toggle("needs") }),
    tab("projects", "Projects", "folder", { href: S.first ? "#/p" : "#/p/shop" }),
    tab("more", "More", "more", { onclick: toggle("more") }),
  ];
}

function overlay() {
  if (!S.open) return [];
  const scrim = h("div", { class: "jscrim", onclick: () => { S.open = null; draw(); } });
  if (S.open === "needs") return [scrim, needsDrawer()];
  if (S.open === "agents") return [scrim, switcher()];
  if (S.open === "more") return [scrim, moreMenu()];
  if (S.open === "search") return [scrim, searchBox()];
  return [];
}

function needsDrawer() {
  return h("aside", { class: "jdrawer", "aria-label": "Needs you" },
    h("div", { class: "jdrawer-h" }, h("h2", {}, "Needs you"), h("span", { class: "muted" }, `${S.needs.length}, oldest first`), h("span", { style: { flex: 1 } }),
      h("a", { class: "lv-link", href: "#/catchup" }, "Catch up on everything"), btn("", { ghost: true, sm: true, iconName: "x", cls: "icon", onclick: () => { S.open = null; draw(); } })),
    h("div", { class: "jdrawer-b" },
      S.needs.length ? S.needs.map((q) => questionCard(q, { compact: false, onAnswer: () => answered(q.id) }))
        : h("div", { class: "lv-empty" }, "Nothing is blocked on you. Agents that finished a turn are under Agents, as Ready to review."),
      h("p", { class: "jnote" }, "Only what an agent is blocked on (a question, a permission, the trust prompt) and alerts that ask you. The same count is on Home and on Catch up.")));
}

export function switcherRows() {
  const order = { needs: 0, working: 1, ready: 2, finished: 3 };
  return agents.slice().sort((a, b) => order[stateWords(a).group] - order[stateWords(b).group]);
}

function switcher() {
  const groups = [["needs", "Needs you"], ["working", "Working"], ["ready", "Ready to review"], ["finished", "Finished"]];
  let i = 0;
  return h("div", { class: "jpop", role: "dialog", "aria-label": "Agents" },
    groups.map(([g, label]) => {
      const list = switcherRows().filter((a) => stateWords(a).group === g);
      if (!list.length) return null;
      return [h("div", { class: "jpop-g" }, label, h("span", {}, String(list.length))), list.map((a) => {
        const s = stateWords(a);
        i++;
        return h("a", { class: `jpop-r${i === 1 ? " cur" : ""}`, href: `#/a/${a.id}` },
          avatar(a, 24),
          h("span", { class: "nm" }, a.name, h("span", { class: "proj" }, `${PROJECT.name} · ${a.model}`), h("span", { class: `jstate ${s.tone}` }, s.text)),
          h("kbd", {}, String(i)),
          g === "finished" ? h("span", { class: "term" }, h("button", { class: "btn sm", type: "button", onclick: (e) => e.preventDefault() }, "Resume"))
            : h("button", { class: "btn ghost icon sm term", type: "button", title: "Its terminal", "aria-label": "Its terminal", onclick: (e) => { e.preventDefault(); location.hash = `#/a/${a.id}/terminal`; } }, icon("term", "ji sm")),
          h("span", { class: "line" }, a.narration));
      })];
    }),
    h("div", { class: "jpop-f" }, h("span", {}, h("kbd", {}, "1"), "…", h("kbd", {}, "9"), " open"), h("span", {}, "the ", icon("term", "ji sm"), " opens its terminal"), h("span", { style: { marginLeft: "auto" } }, h("a", { class: "lv-link", href: "#/more/terminals" }, "All terminals"))));
}

function moreMenu() {
  const item = (label, sub, href) => h("a", { href }, label, h("span", {}, sub));
  return h("nav", { class: "jmore", "aria-label": "More" },
    item("All terminals", "every agent's live terminal, side by side", "#/more/terminals"),
    item("Where the time went", "per agent: thinking, tools, tests, waiting on you", "#/more/time"),
    item("Decisions timeline", "the project's decisions, one lane per agent", "#/more/decisions"),
    item("Library docs", "the docs of the project's libraries", "#/more/library"),
    item("Rules", "what every agent of a project is told", "#/more/rules"),
    h("hr"),
    item("Guide", "these journeys, in words", "#/more/guide"),
    item("Keys", "the keyboard", "#/more/keys"),
    item("Setup check", "ChloeCtx, Claude Code, git", "#/more/setup"));
}

function searchBox() {
  const row = (kind, title, sub) => h("div", { class: "jpop-r" }, h("span", { class: "jtag" }, kind), h("span", { class: "nm" }, title), h("span"), h("span"), h("span", { class: "line" }, sub));
  return h("div", { class: "jsearch", role: "dialog", "aria-label": "Search" },
    h("div", { class: "jbig", style: { border: 0, borderBottom: "1px solid var(--line)", borderRadius: 0, boxShadow: "none" } }, icon("search"), h("input", { value: "to_cents", "aria-label": "Search" }), h("span", { class: "jnote" }, `in ${PROJECT.name}`)),
    h("div", { style: { padding: "6px" } },
      row("code", "money.py:4  def to_cents(amount: float) -> int:", "Dollars to cents, rounding half up (what the receipt prints)"),
      row("code", "cart.py:9  self.lines.append((sku, to_cents(price), qty))", "Cart.add"),
      row("◆", "Customers win: everything a customer sees rounds half up", "decision, by you, 7:23 · cents"),
      row("docs", "decimal: ROUND_HALF_UP", "docs.python.org")),
    h("div", { class: "jpop-f" }, h("span", {}, h("kbd", {}, "Enter"), " open"), h("span", {}, h("kbd", {}, "⇧"), h("kbd", {}, "Enter"), " why is this line like this")));
}
