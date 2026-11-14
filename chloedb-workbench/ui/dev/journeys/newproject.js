// A. Start a new project (JOURNEYS.md): Folder › What to build › Who builds
// it › Plan it. Nothing runs until "Start" on the plan, and the plan says
// what every agent starts with (ChloeCtx wired, model, permissions, branch).
import { h, useStyle, ctxChip } from "../../levels/common.js";
import { page, icon, btn } from "./ui.js";

function steps(now) {
  const order = [["where", "Folder", "#/new"], ["what", "What to build", "#/new/what"], ["who", "Who builds it", "#/new/what"], ["plan", "Plan it", "#/new/plan"]];
  const at = order.findIndex(([k]) => k === now);
  return { list: order.map(([k, label, href], i) => ({ label, href, state: i < at ? "done" : i === at ? "now" : "next" })) };
}
const crumbs = (ctx, label) => ctx.crumbs([{ label: "Home", href: "#/" }, { label: "New project", href: "#/new" }, { label, href: "#/new" }]);

export function where(root, ctx) {
  crumbs(ctx, "Folder");
  root.append(page({ title: "Where should it live?", sub: "An empty folder, or a new one. Workbench runs git init and makes a first commit, so agents can work on branches.", steps: steps("where"), mbar: { back: { label: "Home", href: "#/" }, next: { label: "Next", href: "#/new/what" } } },
    h("div", { class: "jbig", style: { maxWidth: "760px" } }, icon("folder"), h("input", { value: "~/code/bakery", "aria-label": "Folder" }), btn("Next", { primary: true, href: "#/new/what", cls: "jm-dup" })),
    h("p", { class: "jnote" }, "~/code/bakery does not exist yet: it will be made. From a terminal: ", h("code", { class: "lv-code" }, "chloedb-workbench new ~/code/bakery"))));
}

export function what(root, ctx) {
  crumbs(ctx, "What to build");
  root.append(page({ title: "What should it be?", sub: "Say it in your own words. A plan comes back for you to edit; nothing is built before you start it.", steps: steps("who"), mbar: { back: { label: "Back", href: "#/new" }, next: { label: "Plan it", href: "#/new/plan" } } },
    h("div", { class: "jgrid2" },
      h("section", { class: "jc jx" },
        h("label", { class: "jx-l" }, "What to build"),
        h("div", { class: "jx-box" },
          h("textarea", { rows: 5 }, "A small order page for a neighbourhood bakery: customers pick bread and pastries for tomorrow morning, see the total, and pay at pickup. The baker sees tomorrow's orders as a list she can print at 5 AM."),
          h("div", { class: "jx-done" }, h("b", {}, "Done when"), " an order placed on the page shows up in the baker's list, and the tests pass.")),
        h("div", { class: "jx-checks" }, h("span", { class: "jchip ok" }, icon("check", "ji sm"), "has a done check"), h("span", { class: "jchip" }, "stack: Python, standard library (you can change it)")),
        h("div", { class: "jn-who" },
          h("span", { class: "jx-k" }, "Who builds it"),
          h("label", { class: "jn-opt on" }, h("input", { type: "radio", name: "who", checked: true }), h("b", {}, "A lead agent"), h("span", {}, "reads your idea, splits it into parts, and starts one agent per part after you approve the plan. Best for more than one afternoon of work.")),
          h("label", { class: "jn-opt" }, h("input", { type: "radio", name: "who" }), h("b", {}, "A single agent"), h("span", {}, "builds it in one go on one branch. Cheaper; best for a small thing."))),
        h("div", { class: "jx-go" }, h("span", { class: "jnote" }, "The lead only plans in this step: about 2 minutes, haiku."), btn("Plan it", { primary: true, href: "#/new/plan", cls: "jm-dup" }))),
      h("aside", { class: "jc" }, h("h2", {}, "Tips"),
        h("p", { class: "ju-k" }, "Name who uses it and what they do: the plan's parts come from that."),
        h("p", { class: "ju-k" }, "A done check lets agents prove they are finished, instead of saying so."),
        h("p", { class: "ju-k muted" }, "Anything you decide later (a price rule, a naming rule) is remembered by ChloeCtx and told to every agent of this project.")))));
}

export function plan(root, ctx) {
  crumbs(ctx, "Plan it");
  const part = (n, title, done, par, model) => h("div", { class: "jn-part" },
    h("span", { class: "jn-grip" }, "⋮⋮"), h("span", { class: "jn-pn" }, String(n)),
    h("div", { class: "jn-pt" }, h("div", { class: "jn-ptitle", contenteditable: "true" }, title), h("div", { class: "jnote" }, h("b", {}, "Done when "), done)),
    h("span", { class: "jchip" }, par), h("span", { class: "jchip" }, model), btn("", { sm: true, ghost: true, iconName: "x", cls: "icon" }));
  root.append(page({ title: "The plan for bakery", sub: "The lead's draft, from your idea. Edit, reorder or remove parts; nothing runs until you start it.", steps: steps("plan"), mbar: { back: { label: "Back", href: "#/new/what" }, next: { label: "Start 4 agents", href: "#/p/shop/change" } } },
    h("div", { class: "jgrid2" },
      h("section", { class: "jc" },
        h("h2", {}, "Parts", h("span", { class: "n" }, "4, in order"), h("span", { style: { flex: 1 } }), h("span", { class: "jnote", style: { textTransform: "none", letterSpacing: 0, fontWeight: 400 } }, "drafted by the lead in 1 min 48 s, $0.02")),
        part(1, "Products and prices: a products.json, prices in integer cents", "test_products.py loads it and every price is an int", "first", "haiku"),
        part(2, "The order page: pick items for tomorrow, see the total, place the order", "a test posts an order and gets 201 with the total", "after 1", "sonnet"),
        part(3, "The baker's list: tomorrow's orders, printable", "a test sees a placed order in the list", "with 2", "haiku"),
        part(4, "serve.py: one command to run it locally", "python3 serve.py answers on :8000", "with 2", "haiku"),
        h("div", { class: "jrow", style: { marginTop: "10px" } }, btn("Add a part", { sm: true, ghost: true, iconName: "plus" }), h("span", { class: "jnote" }, "or tell the lead what to change: "), h("input", { class: "jx-in", style: { width: "300px" }, placeholder: "e.g. no payment at all, pay at pickup" }))),
      h("aside", { class: "jc jn-start" },
        h("h2", {}, "What each agent starts with"),
        h("dl", { class: "jkv" },
          h("dt", {}, "Memory"), h("dd", {}, ctxChip("ChloeCtx wired"), h("div", { class: "jnote" }, "its hooks and memory tools, in each session's own settings; yours are not touched")),
          h("dt", {}, "Told"), h("dd", {}, "your idea, its part, the done check, and the project's rules (none yet)"),
          h("dt", {}, "Models"), h("dd", {}, "lead: haiku · parts: as set on each (2 haiku, 1 sonnet)"),
          h("dt", {}, "Branch"), h("dd", {}, "its own worktree on ", h("code", { class: "lv-code" }, "wb/<part>"), " from main; main is never edited"),
          h("dt", {}, "Without asking"), h("dd", {}, h("div", { class: "jx-perms" }, h("span", { class: "jchip ok" }, "edit files on its branch"), h("span", { class: "jchip ok" }, "run python3 and the tests"), h("span", { class: "jchip" }, "ask: anything else"), h("span", { class: "jchip crit" }, "never: git push, pip install"))),
          h("dt", {}, "At once"), h("dd", {}, "at most 3 agents; part 2 waits for part 1")),
        h("div", { class: "jx-go" }, btn("Edit", { sm: true, ghost: true }), btn("Start 4 agents", { primary: true, href: "#/p/shop/change", cls: "jm-dup" }))))));
}

useStyle("j-new", `
.jn-who{display:grid;grid-template-columns:150px 1fr;gap:8px 14px;margin-top:16px;align-items:start}
.jn-who .jx-k{grid-row:span 2;padding-top:10px}
.jn-opt{display:grid;grid-template-columns:auto 1fr;gap:2px 8px;border:1px solid var(--line-2);border-radius:10px;padding:10px 12px;cursor:pointer}
.jn-opt input{grid-row:span 2;margin-top:3px}
.jn-opt span{color:var(--text-2);font-size:13px}
.jn-opt.on{border-color:var(--accent);background:var(--accent-soft)}
.jn-part{display:grid;grid-template-columns:16px 26px minmax(0,1fr) auto auto auto;gap:10px;align-items:center;padding:11px 4px;border-top:1px solid var(--line)}
.jn-grip{color:var(--line-2);font-size:12px;cursor:grab}
.jn-pn{display:inline-grid;place-items:center;width:24px;height:24px;border-radius:50%;background:var(--accent-soft);color:var(--accent);font-weight:700;font-size:12px}
.jn-ptitle{font-weight:600;outline:0;border-radius:4px}
.jn-ptitle:focus{box-shadow:0 0 0 2px var(--focus)}
.jn-start .jkv{gap:10px 14px}
`);
