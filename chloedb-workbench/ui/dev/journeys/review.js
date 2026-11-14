// C. Review and ship (JOURNEYS.md): what is ready, then one change (its
// checks, the decisions behind it, the diff and the real ship.js below),
// then Merge, or Send back with a note.
import { h, useStyle, ctxChip } from "../../levels/common.js";
import { page, icon, btn, avatar, moduleHost } from "./ui.js";
import { A, agents, review } from "./fixtures.js";
import { blastCompact, churnBar } from "./dense.js";

const crumbs = (ctx, extra) => ctx.crumbs([{ label: "Home", href: "#/" }, { label: "shop", href: "#/p/shop" }, { label: "Review and ship", href: "#/p/shop/ship" }, ...(extra || [])]);
function steps(now) {
  const order = [["list", "What is ready", "#/p/shop/ship"], ["check", "Check it", "#/p/shop/ship/c3e1a001"], ["ship", "Merge or send back", "#/p/shop/ship/c3e1a001"]];
  const at = order.findIndex(([k]) => k === now);
  return { list: order.map(([k, label, href], i) => ({ label, href, state: i < at ? "done" : i === at ? "now" : "next" })) };
}
const check = (ok, text) => h("span", { class: `jchip ${ok === true ? "ok" : ok === false ? "crit" : "warn"}` }, ok === true ? icon("check", "ji sm") : icon("warn", "ji sm"), text);

export function reviewList(root, ctx) {
  crumbs(ctx);
  root.append(page({ title: "Review and ship", sub: "Changes agents finished, with the checks run on their own branch. Merge them in the order shown.", steps: steps("list") },
    h("section", { class: "jc" },
      h("h2", {}, "Ready", h("span", { class: "n" }, String(review.length))),
      h("div", { class: "jlist" }, review.map((r, i) => h("div", { class: "jr-row" },
        h("span", { class: "jr-ord" }, String(r.order)), avatar(r.agent, 28),
        h("div", { class: "jr-t" }, h("div", { class: "jrow" }, h("b", {}, r.agent.name), h("code", { class: "lv-code" }, r.agent.branch), h("span", { class: "jnote mono" }, r.diff)), h("div", {}, r.title),
          h("div", { class: "jr-checks" }, check(r.tests.ok, r.tests.text), check(r.merge.ok, r.merge.text), check(null, r.confirm))),
        h("span", { class: "jchip ok" }, r.verdict),
        btn(i === 0 ? "Check it" : "Check", { primary: i === 0, sm: true, href: `#/p/shop/ship/${r.agent.id}` }))))),
    h("div", { class: "jr-order" }, icon("branch", "ji sm"), h("span", {}, h("b", {}, "Merge order: cents, then flakehunt."), " They touch different files; freeship (still working) will need rebasing on cents, since both change tests/test_cart.py."), btn("Ship both together", { sm: true, ghost: true })),
    h("section", { class: "jc", style: { marginTop: "14px" } },
      h("h2", {}, "Not ready yet"),
      h("div", { class: "jlist" },
        h("div", { class: "jrow" }, avatar(A.paybutton, 22), h("b", {}, "paybutton"), h("span", { class: "jstate warn" }, "waiting for you"), h("span", { class: "jnote" }, "+9 −1 · its test has not run yet")),
        h("div", { class: "jrow" }, avatar(A.freeship, 22), h("b", {}, "freeship"), h("span", { class: "jstate acc" }, "working"), h("span", { class: "jnote" }, "+31 −2 · 2 of 3 new tests pass"))))));
}

export function reviewOne(root, ctx, id) {
  const a = agents.find((x) => x.id === id) || A.cents;
  crumbs(ctx, [{ label: a.name, href: `#/p/shop/ship/${a.id}` }]);
  const host = moduleHost(640);
  root.append(page({ title: `Review ${a.name}: round customer totals half up`, sub: "What the change reaches, its diff by piece of work, the decision behind it. Then merge it, or send it back with a note.", steps: steps("check") },
    blastCompact({ tone: "good", title: "Blast radius", actions: () => [btn("Merge into main", { primary: true, iconName: "branch" }), btn("Send back with a note")],
      verdict: "Low risk: changes how customer totals round (to_cents); every caller is tested and passes on wb/cents.",
      rings: [
        { n: 1, label: "function changed", names: ["to_cents()"], code: true },
        { n: 6, label: "callers", names: ["Cart.add", "Cart.total", "coupons.discount", "receipt", "admin report", "order email"] },
        { n: 2, label: "public names", names: ["the total on the checkout page", "the receipt"] },
      ],
      exceptions: [
        { tone: "warn", text: "freeship also changes tests/test_cart.py: merge cents first" },
        { tone: "ctx", text: "Replaces banker's rounding for customer totals (b72f105, Nov 9)" },
      ],
      tests: "12 pass on wb/cents · merges cleanly into main" }),
    h("div", { class: "jgrid2", style: { marginTop: "14px" } },
      h("div", { class: "jstack" },
        h("div", { class: "jz-piece high", style: { borderLeftColor: "var(--good)" } },
          h("div", { class: "jrow", style: { padding: "11px 14px" } }, h("span", { class: "jz-risk low" }, "low"), h("b", {}, "to_cents rounds half up with Decimal"), h("span", { class: "jnote" }, "money.py"), h("span", { style: { flex: 1 } }), churnBar(6, 2)),
          h("p", { class: "jz-words" }, "Was round(amount * 100), half to even on a float; now Decimal, half up. 6 callers, all tested."),
          h("div", { class: "jz-hunk" }, h("div", { class: "jz-hh" }, h("span", { class: "jtag" }, "the hunk that matters"), h("code", {}, "money.py · to_cents()")),
            dl(5, "-", "    return round(amount * 100)"), dl(6, "+", "    return int(Decimal(str(amount)).scaleb(2).quantize(Decimal(1), ROUND_HALF_UP))"))),
        h("div", { class: "jz-piece" },
          h("div", { class: "jrow", style: { padding: "11px 14px" } }, h("span", { class: "jz-risk low" }, "low"), h("b", {}, "Tests pin the receipt"), h("span", { class: "jnote" }, "2 test files"), h("span", { style: { flex: 1 } }), churnBar(12, 2)),
          h("p", { class: "jz-words" }, "3 × $3.33 at 15% off is 850, as the receipt prints; 2.675 is 268.")),
        h("div", { class: "jrow" }, h("span", { class: "jnote" }, "Folded: 1 import line."), h("a", { class: "lv-link", style: { marginLeft: "auto" }, href: `#/a/${a.id}/changes` }, "The whole diff"))),
      h("aside", { class: "jstack" },
        h("div", { class: "jr-confirm" },
          h("div", { class: "jrow" }, icon("warn", "ji sm"), h("b", {}, "Confirm the decision behind it")),
          h("div", {}, ctxChip("decision, you, 7:23"), " Customers win: everything a customer sees rounds half up; the ledger export rounds itself."),
          h("div", { class: "jnote" }, "It replaces finance's banker's rounding (b72f105, Nov 9) for customer totals. The ledger export is not changed by this branch."),
          h("div", { class: "jrow" }, btn("Still right", { sm: true }), btn("Change it", { sm: true, ghost: true }))),
        h("div", { class: "jnote" }, "2 routine decisions folded."))),
    h("div", { class: "jr-act jz-act jm-hide" }, btn("Send back with a note", { sm: true }), h("button", { class: "lv-link", type: "button" }, "Open a pull request instead"), btn("Merge into main", { primary: true, iconName: "branch" })),
    h("details", { class: "jz-fold", style: { marginTop: "18px" } }, h("summary", {}, icon("chev", "ji sm"), h("b", {}, "Details"), h("span", { class: "jnote" }, "every function, caller, test and decision, and the pull request draft (today's Ship)")), h("div", { style: { padding: "0 10px 10px" } }, host))));
  ctx.mountModule("ship", host, { agent: a.id });
}
function dl(n, sign, text) { return h("div", { class: `ja-dl ${sign === "+" ? "add" : "del"}` }, h("span", { class: "ja-ln" }, String(n)), h("span", { class: "ja-sg" }, sign), h("code", {}, text)); }

useStyle("j-review", `
.jr-row{display:grid;grid-template-columns:24px 30px minmax(0,1fr) auto auto;gap:12px;align-items:center;padding:14px 2px !important}
.jr-ord{display:inline-grid;place-items:center;width:22px;height:22px;border-radius:50%;background:var(--surface-3);font-weight:700;font-size:12px;color:var(--text-2)}
.jr-t{display:flex;flex-direction:column;gap:4px}
.jr-checks{display:flex;flex-wrap:wrap;gap:6px;margin-top:4px}
.jr-order{display:flex;align-items:center;gap:10px;margin-top:12px;padding:10px 14px;border:1px solid var(--line);border-radius:10px;background:var(--surface-2);font-size:13px}
.jr-order .btn{margin-left:auto}
.jr-bar{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:12px 20px;align-items:start}
.jr-bar .jr-checks{grid-column:1/3;margin:0}
.jr-confirm{display:flex;flex-direction:column;gap:6px;background:var(--warn-soft);border:1px solid var(--warn-line);border-radius:10px;padding:10px 12px}
.jr-confirm .jrow:first-child{color:var(--warn-ink)}
.jr-act{display:flex;flex-direction:column;align-items:flex-end;gap:8px;justify-content:flex-end;height:100%}
.jr-act .btn.primary{height:38px;padding:0 18px;font-size:14px}
.jr-diff{margin-top:14px;padding:12px 0 0;overflow:hidden}
.jr-diff>.jrow{padding:0 14px 10px}
.jr-diff .ja-file{border-top:1px solid var(--line)}
`);
