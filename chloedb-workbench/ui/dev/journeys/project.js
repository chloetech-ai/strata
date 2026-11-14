// B. Work on a project (JOURNEYS.md): Open › Understand › Change, then the
// way into Review and ship. Understand mounts the real map.js (without its
// time bar); Change shows the composer and the work in progress.
import { h, useStyle, ctxChip } from "../../levels/common.js";
import { page, icon, btn, avatar, questionCard, moduleHost } from "./ui.js";
import { agents, A, PROJECT, known, rules, needs, stateWords, review } from "./fixtures.js";
import { answered } from "./shell.js";

/** The project's own row (JOURNEYS.md, B): Since you left, Understand, Start a change; then Review and ship. */
export function steps(now) {
  const order = [["continue", "Since you left", "#/p/shop", "rewind"], ["understand", "Understand", "#/p/shop/understand", "map"], ["change", "Start a change", "#/p/shop/change", "plus"]];
  return {
    plain: true,
    list: order.map(([k, label, href, ic]) => ({ label, href, icon: ic, state: k === now ? "now" : "next" })),
    after: { label: `Review and ship (${review.length} ready)`, href: "#/p/shop/ship" },
  };
}

// ------------------------------------------------------------ B1 Open
export function open(root, ctx) {
  ctx.crumbs([{ label: "Home", href: "#/" }, { label: "Open a project", href: "#/p" }]);
  root.append(page({ title: "Open a project", sub: "A folder with a git repository. Opening it starts no agent: you can read and search it first." },
    h("div", { class: "jgrid2" },
      h("div", { class: "jstack" },
        h("div", { class: "jbig" }, icon("folder"), h("input", { value: "~/code/shop", "aria-label": "Folder" }), btn("Open shop", { primary: true, href: "#/p/shop/understand" })),
        h("section", { class: "jc" },
          h("h2", {}, "shop"),
          h("dl", { class: "jkv" },
            h("dt", {}, "Repository"), h("dd", {}, "git, on ", h("code", { class: "lv-code" }, "main"), ", clean, last commit 033fb7f “checkout page” 2 days ago"),
            h("dt", {}, "Files"), h("dd", {}, `${PROJECT.files} files, ${PROJECT.lines} lines: Python and one HTML page`),
            h("dt", {}, "Search"), h("dd", {}, `indexed in ${PROJECT.indexed_ms} ms, for you and for its agents`),
            h("dt", {}, "Memory"), h("dd", {}, ctxChip(`ChloeCtx has ${PROJECT.records} records about it`), " ", h("span", { class: "muted" }, "2 of them disagree about rounding")))),
        h("section", { class: "jc jo-trust" },
          h("h3", {}, "Let agents work here?"),
          h("p", {}, "Each agent gets its own copy of shop (a ", h("b", {}, "worktree"), ") on its own branch, ", h("code", { class: "lv-code" }, "wb/<name>"), ". Your checkout is never edited; nothing merges until you ship it."),
          h("p", { class: "muted" }, "Saying yes answers Claude Code's “Do you trust the files in this folder?” for every worktree Workbench makes here, so no agent starts blocked on it."),
          h("label", { class: "jo-check" }, h("input", { type: "checkbox", checked: true }), " Yes, agents may work in copies of shop"))),
      h("aside", { class: "jc" },
        h("h2", {}, "Recent"),
        h("div", { class: "jlist" },
          rec("~/code/shop", "3 agents", true), rec("~/src/invoicer", "3 days ago"), rec("~/src/harbor", "last week")),
        h("p", { class: "jnote" }, "Tab completes folders; git repositories come first.")))));
}
function rec(p, s, cur) { return h("div", { class: "jrow" }, icon("folder", "ji sm"), h("span", { class: "mono", style: { fontWeight: cur ? 650 : 400 } }, p), h("span", { class: "muted", style: { marginLeft: "auto" } }, s)); }

// ------------------------------------------------------------ B2 Understand
export function understand(root, ctx) {
  ctx.crumbs([{ label: "Home", href: "#/" }, { label: "shop", href: "#/p/shop" }, { label: "Understand", href: "#/p/shop/understand" }]);
  const host = moduleHost(520);
  root.append(page({ title: "Understand shop", sub: "Ask about the code before you change it. Nothing on this page starts an agent.", steps: steps("understand") },
    h("div", { class: "jbig" }, icon("search"), h("input", { placeholder: "Ask about shop: where is money rounded?", "aria-label": "Ask about shop" }), btn("Ask", { primary: true, href: "#/why?q=where%20is%20money%20rounded" })),
    h("div", { class: "jtry" }, "try:", h("a", { href: "#/why" }, "how does a coupon reach the total?"), h("a", { href: "#/why" }, "when did rounding change?"), h("a", { href: "#/why" }, "what calls to_cents?")),
    h("div", { class: "ju-grid" },
      h("section", {}, h("div", { class: "ju-maph" }, h("h2", { class: "ju-h" }, "Map: files sized by lines"), h("span", { class: "jnote" }, "tinted by the agent changing each; ◆ what ChloeCtx remembers about a file; red hatching where two agents meet")), host),
      h("aside", { class: "jstack" },
        h("section", { class: "jc" },
          h("h2", {}, "What is known about shop", h("span", { class: "n" }, String(known.length))),
          h("div", { class: "jlist" }, known.filter((k) => k.conflict).length ? h("div", { class: "ju-conf" },
            h("div", { class: "jrow" }, icon("warn", "ji sm"), h("b", {}, "Two decisions disagree")),
            known.filter((k) => k.conflict).map((k) => h("div", { class: "ju-k" }, ctxChip(k.kind), " ", k.text, h("span", { class: "muted" }, ` · ${k.when}`))),
            h("div", { class: "jrow", style: { marginTop: "6px" } }, btn("Decide it", { sm: true }), h("span", { class: "jnote" }, "makes a rule every agent is told"))) : null,
          known.filter((k) => !k.conflict).map((k) => h("div", { class: "ju-k" }, ctxChip(k.kind), " ", k.text, h("span", { class: "muted" }, ` · ${k.by}, ${k.when}`)))),
          h("a", { class: "lv-link", href: "#/why/records" }, "All 14 records, and correct or forget one →")),
        h("section", { class: "jc" },
          h("h2", {}, "Agents are told", h("span", { class: "n" }, String(rules.length))),
          rules.map((r) => h("div", { class: "ju-k" }, "• ", r.text)),
          h("div", { class: "jrow", style: { marginTop: "8px" } }, btn("Add a rule", { sm: true, ghost: true, iconName: "plus" }))),
        h("section", { class: "jc" },
          h("h2", {}, "History"),
          h("div", { class: "ju-k" }, h("b", {}, "money.py"), " changed rounding 3 times in 13 days: floor, half up, banker's."),
          h("a", { class: "lv-link", href: "#/why" }, "Trace it →")),
        h("div", { class: "ju-next" }, "Ready to change something? ", btn("Change", { sm: true, href: "#/p/shop/change", iconName: "chev" }))))));
  ctx.mountModule("map", host, {});
}

// ------------------------------------------------------------ B3 Change
export function change(root, ctx) {
  ctx.crumbs([{ label: "Home", href: "#/" }, { label: "shop", href: "#/p/shop" }, { label: "Start a change", href: "#/p/shop/change" }]);
  const work = agents.filter((a) => a.running);
  root.append(page({ title: "Start a change in shop", sub: "Describe what should change. Each agent works on its own branch; you review before anything merges.", steps: steps("change") },
    h("section", { class: "jc jx" },
      h("label", { class: "jx-l", for: "jx-task" }, "What should change?"),
      h("div", { class: "jx-box" },
        h("textarea", { id: "jx-task", rows: 3 }, "Checkout shows no line for tax: customers see $47.48 of items and a $51.40 total. Add a “Tax (8.25%)” line between the items and the total."),
        h("div", { class: "jx-done" }, h("b", {}, "Done when"), " test_web.py checks the tax line reads $3.92 for the demo cart, and all tests pass.")),
      h("div", { class: "jx-checks" },
        h("span", { class: "jchip ok" }, icon("check", "ji sm"), "has a done check"),
        h("span", { class: "jchip ctx" }, "◆ rule applies: “Never edit web/ and money.py in the same change”"),
        h("span", { class: "jchip warn" }, icon("warn", "ji sm"), "paybutton is changing web/index.html now")),
      h("div", { class: "jx-grid" },
        h("div", { class: "jx-f" }, h("span", { class: "jx-k" }, "How"),
          h("div", { class: "jx-seg" }, h("label", { class: "on" }, h("input", { type: "radio", name: "how", checked: true }), "One agent"), h("label", {}, h("input", { type: "radio", name: "how" }), "Split it into parts"),
            h("span", { class: "jnote" }, "parts: you list them, or a lead agent plans them"))),
        h("div", { class: "jx-f" }, h("span", { class: "jx-k" }, "May do without asking"),
          h("div", { class: "jx-perms" },
            h("span", { class: "jchip ok" }, icon("check", "ji sm"), "edit files on its branch"),
            h("span", { class: "jchip ok" }, icon("check", "ji sm"), "run the tests (python3 -m pytest)"),
            h("span", { class: "jchip ok" }, icon("check", "ji sm"), "start serve.py"),
            h("span", { class: "jchip" }, "ask: any other command"),
            h("span", { class: "jchip crit" }, "never: git push, rm -rf, pip install"),
            h("span", { class: "jx-sugg" }, ctxChip("you said Yes to python3 -c 9 times today"), " ", h("button", { class: "lv-link", type: "button" }, "allow it")))),
        h("div", { class: "jx-f" }, h("span", { class: "jx-k" }, "Model"),
          h("div", { class: "jrow" }, h("select", { class: "jx-sel" }, h("option", {}, "haiku"), h("option", {}, "sonnet"), h("option", {}, "opus")),
            h("span", { class: "jnote" }, "suggested: haiku finished 4 of 4 small web tasks here, about $0.03 each"))),
        h("div", { class: "jx-f" }, h("span", { class: "jx-k" }, "Name"), h("input", { class: "jx-in mono", value: "taxline" }))),
      h("div", { class: "jx-go" }, h("span", { class: "jnote" }, "It starts on branch wb/taxline from main. ", h("kbd", {}, "Ctrl"), h("kbd", {}, "Enter")), btn("Start agent", { primary: true, href: "#/a/c3e1a002" }))),
    h("div", { class: "jx-wh" }, h("h2", { class: "ju-h" }, `In progress (${work.length})`), h("span", { class: "jnote" }, "one card per agent of shop; a card opens its page")),
    h("div", { class: "jx-heads" }, icon("warn", "ji sm"), h("span", {}, h("b", {}, "cents and freeship both change tests/test_cart.py."), " They will need merging in order: cents first, then freeship rebased on it."), h("a", { class: "lv-link", href: "#/p/shop/understand" }, "See on the map")),
    h("div", { class: "jgrid3 jx-work" }, work.map((a) => workCard(a)))));
}

function workCard(a) {
  const s = stateWords(a);
  const q = needs.find((x) => x.agent && x.agent.id === a.id);
  return h("article", { class: `jc jw ${s.group}` },
    h("div", { class: "jrow" }, avatar(a, 26), h("a", { class: "jw-n", href: `#/a/${a.id}` }, a.name), h("span", { class: `jstate ${s.tone}` }, s.text),
      h("span", { style: { flex: 1 } }), h("a", { class: "btn ghost icon sm", href: `#/a/${a.id}/terminal`, title: "Its terminal", "aria-label": "Its terminal" }, icon("term", "ji sm"))),
    h("div", { class: "jw-task" }, a.task.split(". ")[0] + "."),
    q ? questionCard(q, { compact: true, agentLink: false, onAnswer: () => answered(q.id) })
      : h("div", { class: "jw-line" }, a.narration),
    h("div", { class: "jrow jw-foot" },
      a.diff ? h("span", { class: "jnote" }, h("b", { style: { color: "var(--add-ink)" } }, `+${a.diff.added}`), " ", h("b", { style: { color: "var(--del-ink)" } }, `−${a.diff.removed}`), ` in ${a.diff.files} files`) : null,
      ctxChip(a.name === "cents" ? "recalled 3 records" : a.name === "paybutton" ? "recalled 3 records" : "recalled 1 record"),
      h("span", { style: { flex: 1 } }),
      s.group === "ready" ? btn("Review", { sm: true, href: `#/p/shop/ship/${a.id}` }) : btn("Open", { sm: true, ghost: true, href: `#/a/${a.id}` })));
}

useStyle("j-project", `
.jo-trust p{margin:6px 0;color:var(--text-2)}
.jo-trust h3{margin-bottom:4px}
.jo-check{display:flex;gap:8px;align-items:center;font-weight:600;margin-top:8px}
.ju-grid{display:flex;flex-direction:column;gap:16px;margin-top:16px}.ju-grid>aside{display:grid;grid-template-columns:1.3fr 1fr 1fr;gap:14px;align-items:start}.ju-grid>aside .ju-next{grid-column:1/-1}
.ju-h{margin:0;font-size:11.5px;font-weight:700;color:var(--muted);text-transform:uppercase;letter-spacing:.06em}
.ju-maph{display:flex;align-items:baseline;gap:12px;margin-bottom:8px}
.ju-k{font-size:13px;color:var(--text);padding:5px 0;line-height:1.45}
.ju-conf{background:var(--warn-soft);border:1px solid var(--warn-line);border-radius:10px;padding:10px 12px;color:var(--warn-ink);margin-bottom:6px}
.ju-conf .ju-k{color:var(--text)}
.ju-next{display:flex;align-items:center;gap:10px;justify-content:flex-end;color:var(--text-2)}
.jx{padding:18px 20px}
.jx-l{display:block;font-weight:650;font-size:15px;margin-bottom:8px}
.jx-box{border:1.5px solid var(--accent);border-radius:12px;box-shadow:0 0 0 4px var(--focus);background:var(--surface);overflow:hidden}
.jx-box textarea{display:block;width:100%;border:0;outline:0;resize:none;padding:12px 14px 6px;font-size:15px;background:transparent;color:var(--text)}
.jx-done{padding:8px 14px 10px;font-size:13.5px;color:var(--text-2);border-top:1px dashed var(--line)}
.jx-checks{display:flex;flex-wrap:wrap;gap:6px;margin:10px 0 4px}
.jx-grid{display:grid;grid-template-columns:150px 1fr;gap:12px 14px;margin-top:14px;align-items:center}
.jx-f{display:contents}
.jx-k{font-size:12.5px;font-weight:650;color:var(--muted)}
.jx-seg{display:flex;align-items:center;gap:6px;flex-wrap:wrap}
.jx-seg label{display:inline-flex;align-items:center;gap:6px;border:1px solid var(--line-2);border-radius:8px;padding:5px 10px;font-size:13px;cursor:pointer}
.jx-seg label.on{border-color:var(--accent);background:var(--accent-soft);color:var(--accent);font-weight:600}
.jx-seg input{margin:0}
.jx-perms{display:flex;flex-wrap:wrap;gap:6px;align-items:center}
.jx-sugg{display:inline-flex;align-items:center;gap:4px;margin-left:6px;font-size:12px}
.jx-sel,.jx-in{height:30px;border:1px solid var(--line-2);border-radius:7px;background:var(--surface);padding:0 8px;font-size:13px}
.jx-in{width:200px}
.jx-go{display:flex;align-items:center;justify-content:flex-end;gap:14px;margin-top:16px;padding-top:14px;border-top:1px solid var(--line)}
.jx-go .btn{height:38px;padding:0 18px;font-size:14px}
.jx-wh{display:flex;align-items:baseline;gap:12px;margin:26px 0 10px}
.jx-heads{display:flex;align-items:center;gap:10px;font-size:13px;color:var(--warn-ink);background:var(--warn-soft);border:1px solid var(--warn-line);border-radius:10px;padding:9px 12px;margin-bottom:12px}
.jx-heads span{color:var(--text)}
.jx-heads .lv-link{margin-left:auto}
.jw{display:flex;flex-direction:column;gap:9px;padding:14px}
.jw.needs{border-color:var(--warn-line)}
.jw-n{font-weight:700;color:var(--text);text-decoration:none;font-size:14.5px}
.jw-task{color:var(--muted);font-size:12.5px}
.jw-line{font-size:13.5px}
.jw .jq{padding:9px 10px;gap:7px}.jw .jq-head{display:none}.jw .jq-foot,.jw .jq-explain{display:none}
.jw-foot{gap:8px;margin-top:auto}
`);
