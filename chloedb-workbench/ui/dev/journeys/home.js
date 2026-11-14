// Start, the home page, and the first run (JOURNEYS.md, "Home"): "What do
// you want to do?" with the four goals, your projects by activity (each with
// Continue and Review), and "Needs you now", answered in place.
import { h, useStyle } from "../../levels/common.js";
import { icon, btn, avatar, clock, since } from "./ui.js";
import { agents, review, lastLooked, happened } from "./fixtures.js";
import { needsList, answered } from "./shell.js";

const GOALS = [
  { icon: "new", title: "Start a new project", text: "From an empty folder or an idea: describe it, get a plan, then agents build it.", href: "#/new", action: "Start" },
  { icon: "rewind", title: "Continue a project", text: "A repository you have, or work started here: what changed, what needs you, ask about it.", href: "#/p/shop", action: "Continue shop" },
  { icon: "ship", title: "Review and ship", text: "The diff, the checks and the decisions behind it; then merge, or send it back.", href: "#/p/shop/ship", action: "Review" },
  { icon: "why", title: "Debug context and project", text: "What an agent knew at a turn, what it missed, and where it went wrong.", href: "#/debug", action: "Debug" },
];

function goal(o, { primary, fact }) {
  return h("a", { class: `jh-obj${primary ? " primary" : ""}`, href: o.href },
    h("span", { class: "jh-ic" }, icon(o.icon, "ji lg")),
    h("b", {}, o.title),
    h("span", { class: "jh-t" }, o.text),
    fact ? h("span", { class: "jh-fact" }, fact) : null,
    h("span", { class: `btn${primary ? " primary" : ""} sm jh-go` }, o.action));
}

export function home(root, ctx) {
  ctx.crumbs([{ label: "Home", href: "#/" }]);
  const needs = needsList();
  const running = agents.filter((a) => a.running);
  const mins = Math.round((Date.now() - lastLooked) / 60000);
  const facts = [null, `shop, since ${clock(lastLooked)}: cents finished, ${needs.length} need you`, `${review.length} ready: cents, flakehunt`, "freeship taxed shipping at turn 2"];
  root.append(h("div", { class: "jp jh" },
    h("header", { class: "jp-head" }, h("div", { class: "jp-ht" },
      h("h1", {}, "What do you want to do?"),
      h("p", { class: "jp-sub" }, `${greeting()} You last looked ${mins} min ago, at ${clock(lastLooked)}. ${happened.length} things happened in shop since.`))),
    h("div", { class: "jh-objs" }, GOALS.map((g, i) => goal(g, { primary: i === 1, fact: facts[i] }))),
    h("div", { class: "jh-cols" },
      h("section", { class: "jc jh-projects" },
        h("h2", {}, "Your projects", h("span", { class: "n" }, "most active first")),
        h("div", { class: "jlist" },
          projectRow({ name: "shop", path: "~/code/shop", agents: running, line: [h("b", { class: "jw-warn" }, `${needs.length} need you`), ` · ${review.length} ready to review · ${running.length} agents`], when: "active now", ready: review.length }),
          projectRow({ name: "invoicer", path: "~/src/invoicer", agents: [], line: "no agents · 31 records in ChloeCtx", when: "3 days ago", ready: 0 }),
          projectRow({ name: "harbor", path: "~/src/harbor", agents: [], line: "2 branches not merged: wb/ledger, wb/web", when: "last week", ready: 2 })),
        h("div", { class: "jrow", style: { marginTop: "10px" } }, btn("Open another folder", { ghost: true, sm: true, iconName: "plus", href: "#/p" }))),
      h("section", { class: "jc jh-now" },
        h("h2", {}, "Needs you now", h("span", { class: "n" }, String(needs.length))),
        needs.length ? needs.map((q) => nowItem(q)) : h("div", { class: "lv-empty" }, "Nothing is blocked on you."),
        h("a", { class: "lv-link", href: "#/p/shop" }, "Everything since you left →")))));
}

/** One item of "Needs you now": Allow and Deny are Claude Code's own first and "No" options, by their numbers. */
function nowItem(q) {
  const a = q.agent;
  if (q.kind === "alert") {
    return h("div", { class: "jh-ni" },
      h("div", { class: "jrow" }, h("span", { class: "jq-alert" }, icon("warn", "ji sm")), h("b", {}, "Alert"), h("span", { class: "muted" }, since(q.since))),
      h("div", { class: "jh-nt" }, q.title),
      h("div", { class: "jrow" }, btn("Open in Debug", { sm: true, href: "#/debug/failing" }), btn("Resolve", { sm: true, ghost: true, onclick: () => answered(q.id) })));
  }
  const no = q.options.find((o) => /^No/.test(o.label));
  return h("div", { class: "jh-ni" },
    h("div", { class: "jrow" }, avatar(a, 20), h("b", {}, a.name), h("span", { class: "jq-wait" }, "wants to run"), h("span", { class: "muted" }, since(q.since)), h("span", { style: { flex: 1 } }), h("a", { class: "lv-link", href: `#/a/${a.id}` }, "Open")),
    h("pre", { class: "jq-cmd compact" }, q.command),
    h("div", { class: "jh-nt muted" }, q.explain),
    h("div", { class: "jrow jh-acts" },
      btn("Allow once", { primary: true, sm: true, kbd: "1", onclick: () => answered(q.id) }),
      btn("Deny", { sm: true, kbd: String(no.n), onclick: () => answered(q.id) })),
    // Claude Code's own options open on a tap, not on hover (JOURNEYS.md, "Phone").
    h("details", { class: "jh-more" }, h("summary", { class: "lv-link" }, `All ${q.options.length} options`),
      h("div", { class: "jq-opts" }, q.options.map((o, i) => h("button", { class: `btn sm${i === 0 ? " primary" : ""}`, type: "button", onclick: () => answered(q.id) }, h("kbd", {}, String(o.n)), o.label)))));
}

function projectRow(p) {
  return h("div", { class: "jh-proj" },
    h("span", { class: "jh-pic" }, icon("folder")),
    h("div", { class: "jh-pn" }, h("b", {}, p.name), h("span", { class: "muted mono" }, p.path)),
    h("span", { class: "jh-av" }, p.agents.slice(0, 4).map((a) => avatar(a, 20))),
    h("span", { class: "jh-pl" }, p.line, h("span", { class: "muted" }, ` · ${p.when}`)),
    h("span", { class: "jrow", style: { gap: "6px" } }, btn("Continue", { sm: true, href: "#/p/shop" }), p.ready ? btn(`Review ${p.ready}`, { sm: true, ghost: true, href: "#/p/shop/ship" }) : null));
}

function greeting() {
  const hr = new Date().getHours();
  return hr < 12 ? "Good morning." : hr < 18 ? "Good afternoon." : "Good evening.";
}

export function firstRun(root, ctx) {
  ctx.crumbs([{ label: "Home", href: "#/" }]);
  root.append(h("div", { class: "jp jh first" },
    h("header", { class: "jp-head" }, h("div", { class: "jp-ht" },
      h("h1", {}, "What do you want to do?"),
      h("p", { class: "jp-sub" }, "Workbench runs Claude Code agents for you, each on its own branch so they never edit your checkout, and remembers what they decided and why. That memory is ChloeCtx."))),
    h("div", { class: "jh-two" },
      h("section", { class: "jh-obj primary big" },
        h("span", { class: "jh-ic" }, icon("folder", "ji lg")),
        h("b", {}, "Continue a project you have"),
        h("span", { class: "jh-t" }, "A folder with a git repository. You can read it and ask about it before any agent starts."),
        h("div", { class: "jbig" }, icon("folder"), h("input", { value: "~/code/sh", "aria-label": "Folder" }), btn("Open", { primary: true, href: "#/p/shop" })),
        h("div", { class: "jh-comp" },
          compRow("~/code/shop", "git · main · 14 files", true),
          compRow("~/code/shopfront", "git · main · 212 files"),
          compRow("~/code/shell-scripts", "not a git repository"))),
      h("a", { class: "jh-obj big", href: "#/new" },
        h("span", { class: "jh-ic" }, icon("new", "ji lg")),
        h("b", {}, "Start a new project"),
        h("span", { class: "jh-t" }, "An empty folder, or just an idea. You describe it; a plan comes back for you to change before any agent builds."),
        h("span", { class: "btn sm jh-go" }, "Start"))),
    h("div", { class: "jh-setup" },
      h("b", {}, "Setup"),
      h("span", { class: "jchip ok" }, icon("check", "ji sm"), "ChloeCtx 0.14"),
      h("span", { class: "jchip ok" }, icon("check", "ji sm"), "Claude Code, signed in"),
      h("span", { class: "jchip ok" }, icon("check", "ji sm"), "git 2.47"),
      h("span", { class: "muted" }, "Nothing to fix.")),
    h("p", { class: "jnote jh-later" }, "Once a project is open, this page also has Review and ship, Debug context and project, and what needs you.")));
}

function compRow(path, sub, cur) {
  return h("div", { class: `jh-cr${cur ? " cur" : ""}` }, icon("folder", "ji sm"), h("span", { class: "mono" }, path), h("span", { class: "muted" }, sub));
}

useStyle("j-home", `
.jh .jp-head h1{font-size:26px}
.jh-objs{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:12px}
.jh-obj{position:relative;display:flex;flex-direction:column;gap:7px;padding:16px;border-radius:14px;border:1px solid var(--line);background:var(--surface);box-shadow:var(--shadow-1);color:var(--text);text-decoration:none;min-height:190px}
.jh-obj:hover{border-color:var(--line-2);box-shadow:var(--shadow-2)}
.jh-obj b{font-size:15.5px;letter-spacing:-.005em}
.jh-obj .jh-t{color:var(--text-2);font-size:13px}
.jh-obj .jh-fact{font-size:12.5px;color:var(--text);background:var(--surface-2);border:1px solid var(--line);border-radius:8px;padding:6px 8px;margin-top:2px}
.jh-obj .jh-go{margin-top:auto;align-self:flex-start}
.jh-ic{display:inline-grid;place-items:center;width:38px;height:38px;border-radius:11px;background:var(--surface-3);color:var(--text-2)}
.jh-obj.primary{border-color:color-mix(in srgb,var(--accent) 55%,var(--line));box-shadow:0 0 0 3px var(--focus),var(--shadow-1)}
.jh-obj.primary .jh-ic{background:var(--accent);color:var(--accent-ink)}
.jh-obj.primary .jh-fact{background:var(--warn-soft);border-color:var(--warn-line);color:var(--warn-ink);font-weight:600}
.jh-cols{display:grid;grid-template-columns:minmax(0,1fr) 440px;gap:16px;margin-top:20px;align-items:start}
.jh-proj{display:grid;grid-template-columns:26px 140px 100px minmax(0,1fr) auto;align-items:center;gap:10px}
.jh-pic{color:var(--muted);display:inline-flex}
.jh-pn{display:flex;flex-direction:column}.jh-pn .mono{font-size:12px}
.jh-av{display:inline-flex;gap:3px}
.jh-pl{color:var(--text-2);font-size:13px}
.jw-warn{color:var(--warn-ink)}
.jh-now{border-color:var(--warn-line)}
.jh-ni{display:flex;flex-direction:column;gap:7px;padding:10px 0;border-top:1px solid var(--line)}
.jh-ni:first-of-type{border-top:0;padding-top:0}
.jh-ni .jrow{gap:8px}
.jh-nt{font-size:12.5px}
.jh.first{max-width:980px;padding-top:56px}
.jh-two{display:grid;grid-template-columns:1.4fr 1fr;gap:16px;margin-top:22px}
.jh-obj.big{min-height:260px;padding:22px}
.jh-obj.big b{font-size:18px}
.jh-obj.big .jbig{margin-top:8px}
.jh-comp{border:1px solid var(--line);border-radius:10px;overflow:hidden;margin-top:-2px}
.jh-cr{display:flex;gap:10px;align-items:center;padding:7px 12px;font-size:13px}
.jh-cr .muted{margin-left:auto;font-size:12px}
.jh-cr.cur{background:var(--accent-soft)}
.jh-setup{display:flex;align-items:center;gap:8px;margin-top:20px;padding:12px 16px;border:1px solid var(--line);border-radius:12px;background:var(--surface);font-size:13px}
.jh-later{margin-top:16px}
`);
