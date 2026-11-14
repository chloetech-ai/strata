// B. Continue a project (JOURNEYS.md): since you left, the agents with their
// approvals inline, what changed, the decisions made, the open threads, and
// "Ask about this project". One page, read top to bottom.
import { h, useStyle, ctxChip } from "../../levels/common.js";
import { page, icon, btn, avatar, clock, since } from "./ui.js";
import { agents, happened, lastLooked, stateWords, records } from "./fixtures.js";
import { needsList, answered } from "./shell.js";
import { steps } from "./project.js";

export function continuePage(root, ctx) {
  ctx.crumbs([{ label: "Home", href: "#/" }, { label: "shop", href: "#/p/shop" }, { label: "Since you left", href: "#/p/shop" }]);
  const mins = Math.round((Date.now() - lastLooked) / 60000);
  const order = { needs: 0, ready: 1, working: 2, finished: 3 };
  const list = agents.slice().sort((a, b) => order[stateWords(a).group] - order[stateWords(b).group]);
  root.append(page({ title: "Continue shop", sub: `Since you left at ${clock(lastLooked)}, ${mins} min ago: cents finished its fix, flakehunt made the flaky test stable, paybutton waits on you, freeship is working.`, steps: steps("continue"),
    aside: btn("I'm stepping away", { sm: true, ghost: true, iconName: "pause" }) },
    h("div", { class: "jbig" }, icon("search"), h("input", { placeholder: "Ask about this project: why does the total round half up now?", "aria-label": "Ask about this project" }), btn("Ask", { href: "#/debug/code" })),
    h("div", { class: "jgrid2 jk", style: { marginTop: "16px" } },
      h("section", { class: "jstack" },
        h("h2", { class: "ju-h" }, `Agents (${list.length})`),
        list.map((a) => agentRow(a))),
      h("aside", { class: "jstack" },
        h("section", { class: "jc" }, h("h2", {}, "What changed"),
          h("div", { class: "jlist" },
            changed("wb/cents", "money.py, 2 tests", "+18 −4", "to_cents rounds half up with Decimal; 12 tests pass"),
            changed("wb/flakehunt", "tests/test_coupons.py", "+6 −4", "test_promo_window uses a fixed clock; 15 runs, 0 failures"),
            changed("wb/paybutton", "web/index.html, test_web.py", "+9 −1", "Pay calls submitPayment(total); not tested yet"),
            changed("wb/freeship", "cart.py, coupons.py, a test", "+31 −2", "in progress: the shipping line"))),
        h("section", { class: "jc" }, h("h2", {}, "Decisions made"),
          h("div", { class: "jlist" },
            decision(records[0], "you, answering cents", null),
            decision(records[3], "flakehunt, on its own", true))),
        h("section", { class: "jc" }, h("h2", {}, "Open threads", h("span", { class: "n" }, "3")),
          h("div", { class: "jlist" },
            thread("warn", "The ledger export still needs banker's rounding: nobody has changed it yet.", "from your decision at 7:23", "Start a change"),
            thread("warn", "cents and freeship both change tests/test_cart.py: merge cents first.", "Map", "See the order"),
            thread("crit", "Alert: test_promo_window failed twice in an hour.", "fixed on wb/flakehunt, not merged", "Debug it")))))));
}

function agentRow(a) {
  const s = stateWords(a);
  const q = needsList().find((x) => x.agent && x.agent.id === a.id);
  const no = q && q.options.find((o) => /^No/.test(o.label));
  const lines = happened.filter((x) => x.agent && x.agent.id === a.id && !x.own).map((x) => x.text);
  return h("article", { class: `jc jk-a ${s.group}` },
    h("div", { class: "jrow" }, avatar(a, 26), h("a", { class: "jw-n", href: `#/a/${a.id}` }, a.name), h("span", { class: `jstate ${s.tone}` }, s.text), h("span", { style: { flex: 1 } }),
      a.running ? h("a", { class: "btn ghost icon sm", href: `#/a/${a.id}/terminal`, title: "Its terminal", "aria-label": "Its terminal" }, icon("term", "ji sm")) : null),
    h("div", { class: "jk-line" }, lines[0] ? inline(lines[0]) : a.narration),
    q ? h("div", { class: "jk-q" },
      h("div", { class: "jnote" }, h("b", { style: { color: "var(--text)" } }, `${q.title} `), `${q.tool}, ${q.command.split("\n").length} lines`),
      h("pre", { class: "jq-cmd compact" }, q.command),
      h("div", { class: "jrow" }, btn("Allow once", { primary: true, sm: true, kbd: "1", onclick: () => answered(q.id) }), btn("Deny", { sm: true, kbd: String(no.n), onclick: () => answered(q.id) }),
        btn(q.options[1].label, { sm: true, ghost: true, kbd: "2" }))) : null,
    s.group === "ready" ? h("div", { class: "jrow" }, btn("Review and ship", { sm: true, href: `#/p/shop/ship/${a.id}` }), h("span", { class: "jnote" }, `+${a.diff.added} −${a.diff.removed} in ${a.diff.files} files`)) : null,
    s.group === "finished" ? h("div", { class: "jrow" }, btn("Resume", { sm: true }), btn("Restart with the same task", { sm: true, ghost: true })) : null);
}

function inline(text) { return String(text).split(/(`[^`]+`)/).map((p) => (p.startsWith("`") ? h("code", { class: "lv-code" }, p.slice(1, -1)) : p)); }

function changed(branch, files, n, what) {
  return h("div", {}, h("div", { class: "jrow" }, h("code", { class: "lv-code" }, branch), h("span", { class: "jnote" }, files), h("span", { style: { marginLeft: "auto" }, class: "jnote mono" }, n)), h("div", { class: "jk-sub" }, what));
}
function decision(r, who, ownCall) {
  return h("div", {}, h("div", {}, ctxChip(r.kind), " ", r.text), h("div", { class: "jrow jk-sub" }, h("span", { class: "muted" }, who),
    ownCall ? [h("span", { style: { flex: 1 } }), btn("Keep", { sm: true, ghost: true }), btn("Forget", { sm: true, ghost: true })] : null));
}
function thread(tone, text, sub, action) {
  return h("div", { class: "jrow", style: { alignItems: "flex-start" } }, h("span", { class: `jk-dot ${tone}` }), h("div", { style: { flex: 1 } }, h("div", {}, text), h("div", { class: "jnote" }, sub)), btn(action, { sm: true, ghost: true }));
}

useStyle("j-continue", `
.jk-a{display:flex;flex-direction:column;gap:8px;padding:13px 14px}
.jk-a.needs{border-color:var(--warn-line);box-shadow:0 0 0 3px var(--warn-glow)}
.jk-line{font-size:13.5px}
.jk-q{display:flex;flex-direction:column;gap:7px;background:color-mix(in srgb,var(--warn-soft) 55%,var(--surface));border:1px solid var(--warn-line);border-radius:10px;padding:10px}
.jk-sub{font-size:12.5px;color:var(--text-2);margin-top:3px}
.jk-dot{width:8px;height:8px;border-radius:50%;margin-top:6px;flex:none}
.jk-dot.warn{background:var(--warn)}.jk-dot.crit{background:var(--crit)}
`);
