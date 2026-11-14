// D. Debug context and project (JOURNEYS.md): pick a session and a turn;
// what the agent knew at that turn (prompt, files, tokens, the ChloeCtx
// memory injected) and what it missed (a record that existed but was not
// injected); the diff where it went wrong; the fixes (pin the record, change
// the threshold, send it back). Other ways in: ask why the code is like this
// (the real trace.js), and what is failing.
import { h, useStyle, ctxChip } from "../../levels/common.js";
import { page, icon, btn, avatar, clock, moduleHost } from "./ui.js";
import { agents, A, debugTurns, knewAt, failing } from "./fixtures.js";
import { meter } from "./dense.js";

const crumbs = (ctx, extra) => ctx.crumbs([{ label: "Home", href: "#/" }, { label: "shop", href: "#/p/shop" }, { label: "Debug", href: "#/debug" }, ...(extra || [])]);
function steps(now) {
  const order = [["pick", "Session and turn", "#/debug"], ["knew", "What it knew", "#/debug/c3e1a004/2"], ["wrong", "Where it went wrong", "#/debug/c3e1a004/2/wrong"], ["fix", "Fix", "#/debug/c3e1a004/2/wrong"]];
  const at = order.findIndex(([k]) => k === now);
  return { list: order.map(([k, label, href], i) => ({ label, href, state: i < at ? "done" : i === at ? "now" : "next" })) };
}
const ways = (now) => h("div", { class: "jd-ways" }, h("span", { class: "jnote" }, "Start from"),
  [["#/debug", "an agent's turn", "pick"], ["#/debug/code", "why the code is like this", "code"], ["#/debug/failing", "what is failing", "failing"]].map(([href, label, k]) =>
    h("a", { href, class: k === now ? "on" : null }, label)));

export function pick(root, ctx) {
  crumbs(ctx);
  const sessions = agents.filter((a) => a.name !== "explain");
  root.append(page({ title: "Debug context and project", sub: "Pick an agent's session and the turn where something went wrong. You will see what it knew then, what it missed, and the change it made.", steps: steps("pick") },
    ways("pick"),
    h("div", { class: "jgrid2" },
      h("section", { class: "jc" },
        h("h2", {}, "Sessions", h("span", { class: "n" }, "shop, today")),
        h("div", { class: "jlist" }, sessions.map((a) => h("div", { class: `jd-s${a.name === "freeship" ? " cur" : ""}` }, avatar(a, 24), h("b", {}, a.name), h("span", { class: "jnote" }, `${a.model} · ${a.name === "freeship" ? 3 : 3} turns · started ${clock(a.created)}`),
          a.name === "freeship" ? h("span", { class: "jchip warn" }, "you flagged turn 2") : null)))),
      h("section", { class: "jc" },
        h("h2", {}, "freeship's turns"),
        h("div", { class: "jlist" }, debugTurns.map((t) => h("a", { class: `jd-t${t.ok === false ? " bad" : ""}`, href: `#/debug/c3e1a004/${t.turn}` },
          h("span", { class: "jd-tn" }, String(t.turn)),
          h("div", {}, h("div", { class: "jrow" }, h("b", {}, t.title), h("span", { class: "muted" }, clock(t.at)), t.flag ? h("span", { class: "jchip crit" }, t.flag) : null),
            h("div", { class: "jnote" }, "“", t.prompt, "”"),
            h("div", { class: "jrow jd-tm" }, h("span", {}, `${t.files} files`), h("span", {}, `${Math.round(t.tokens / 100) / 10}k tokens`), ctxChip(`${t.injected} injected`), t.missed ? h("span", { class: "jchip warn" }, `${t.missed} missed`) : null)),
          t.ok === false ? btn("Open turn 2", { primary: true, sm: true }) : h("span", { class: "muted" }, "›"))))))));
}

export function knew(root, ctx) {
  const a = A.freeship, k = knewAt;
  crumbs(ctx, [{ label: "freeship, turn 2", href: "#/debug/c3e1a004/2" }]);
  const used = k.tokens.memory / k.tokens.budget * 100;
  root.append(page({ title: "What freeship knew at turn 2", sub: "Everything that was in its context when it started the turn, and the records ChloeCtx had but did not put there.", steps: steps("knew"),
    aside: btn("Where it went wrong", { primary: true, href: "#/debug/c3e1a004/2/wrong", iconName: "chev" }) },
    h("div", { class: "jd-grid" },
      h("section", { class: "jc" }, h("h2", {}, "Prompt"), h("div", { class: "jd-prompt" }, avatar("you", 20), "“", k.prompt, "”"),
        h("h2", { style: { marginTop: "14px" } }, "Files"), h("div", { class: "jlist" }, k.files.map(([f, how]) => h("div", { class: "jrow" }, icon("folder", "ji sm"), h("code", { class: "lv-code" }, f), h("span", { class: "jnote" }, how)))),
        h("h2", { style: { marginTop: "14px" } }, "Context"), meter(41, "26.4k tokens of the window"),
        h("div", { class: "jnote", style: { marginTop: "6px" } }, `memory: ${k.tokens.memory} of ${k.tokens.budget} tokens; records under ${k.tokens.threshold.toFixed(2)} are left out`)),
      h("section", { class: "jc" }, h("h2", {}, "ChloeCtx memory injected", h("span", { class: "n" }, String(k.injected.length))), h("div", { class: "jz-kinds" }, h("span", { class: "jchip ctx" }, "2 decisions"), h("span", { class: "jchip ctx" }, "0 rules"), h("span", { class: "jchip ctx" }, "0 facts")),
        h("div", { class: "jlist" }, k.injected.map((r) => rec(r, true)))),
      h("section", { class: "jc jd-missed" }, h("h2", {}, "What it missed", h("span", { class: "n" }, "existed, not injected")),
        h("div", { class: "jlist" }, k.missed.map((r) => rec(r, false)))))));
}

function rec(r, inj) {
  return h("div", { class: `jd-r${!inj && !r.fine ? " miss" : ""}` },
    h("div", { class: "jrow" }, ctxChip(r.kind), h("span", { class: "jd-score" }, r.score.toFixed(2)), h("span", { class: "muted" }, `by ${r.by}${r.when ? ", " + r.when : ""}`)),
    h("div", { class: "jd-rt" }, r.text),
    !inj ? h("div", { class: `jnote${r.fine ? "" : " jd-why"}` }, r.fine ? r.reason : [icon("warn", "ji sm"), " ", r.reason, ": it was about this turn's change."]) : null);
}

export function wrong(root, ctx) {
  const k = knewAt;
  crumbs(ctx, [{ label: "freeship, turn 2", href: "#/debug/c3e1a004/2" }, { label: "Where it went wrong", href: "#/debug/c3e1a004/2/wrong" }]);
  let file = null;
  root.append(page({ title: "Where it went wrong: shipping got taxed", sub: "freeship added shipping to the taxable amount at turn 2. The decision “Shipping is not taxed” existed, scored 0.58, and was left out.", steps: steps("wrong") },
    // The bad turn's blast radius as a ranked list, worst first (no graph): a row a line on a phone, a row of five on a desktop.
    h("div", { class: "jd-blast jc" }, h("span", { class: "jz-verdict crit" }, h("span", { class: "dot" }), "Turn 2 changed total(): 9 callers reached, 2 tests caught it, 0 reached main."),
      h("ol", { class: "jd-rank" },
        [["crit", 2, "tests failed after it", "test_receipt_total, test_total_shown"],
          ["warn", 9, "callers reached", "receipt, ledger export, coupons, +6"],
          ["", 1, "function changed", "Cart.total()"],
          ["ok", 0, "runtime errors blamed on it", "none in the preview's log"],
          ["ok", 0, "reached main", "wb/freeship is not merged"]].map(([tone, n, what, names]) =>
          h("li", { class: tone }, h("b", { class: "jd-rn" }, String(n)), h("span", { class: "jd-rw" }, what), h("span", { class: "jd-rx" }, names))))),
    h("div", { class: "jgrid2" },
      h("section", { class: "jc", style: { padding: 0, overflow: "hidden" } },
        h("div", { class: "jrow", style: { padding: "12px 14px" } }, h("h2", { style: { margin: 0 } }, "The diff of turn 2"), h("span", { class: "jnote" }, "commit 7b2e0c1 on wb/freeship, 5 min ago"), h("button", { class: "lv-link", style: { marginLeft: "auto" }, type: "button" }, "Rewind its terminal")),
        k.diff.map(([f, n, sign, text]) => {
          const head = f !== file ? h("div", { class: "ja-file" }, h("b", { class: "mono" }, f)) : null;
          file = f;
          return [head, h("div", { class: `ja-dl ${sign === "+" ? "add" : sign === "-" ? "del" : ""}${text.includes("self.shipping()") || text.includes("5680") ? " cur" : ""}` }, h("span", { class: "ja-ln" }, String(n)), h("span", { class: "ja-sg" }, sign), h("code", {}, text))];
        }),
        h("div", { class: "jd-note" }, icon("warn", "ji sm"), " With the decision, the total is 4748 + 392 tax + 499 shipping = ", h("b", {}, "5639"), "; the test pins 5680.")),
      h("aside", { class: "jc jd-fix" },
        h("h2", {}, "Fix it"),
        fix(true, "Pin the record to cart.py", "“Shipping is not taxed” is injected whenever an agent edits cart.py, whatever its score."),
        fix(false, "Lower the threshold for your decisions", "0.60 → 0.50 for decisions by you in shop. 3 more records would have been injected today (+41 tokens)."),
        fix(false, "Make it a rule", "Every agent of shop is told it at every session start."),
        h("div", { class: "jd-send" },
          h("b", {}, "Send freeship back"), h("textarea", { rows: 3 }, "Shipping is not taxed (your decision of Nov 10, now pinned to cart.py). Tax the items only; the demo cart is 5639 with shipping."),
          h("div", { class: "jrow", style: { justifyContent: "flex-end" } }, btn("Apply and send to freeship", { primary: true })))))));
}
function fix(on, title, text) {
  return h("label", { class: `jd-f${on ? " on" : ""}` }, h("input", { type: "checkbox", checked: on }), h("b", {}, title), h("span", {}, text));
}

export function code(root, ctx) {
  crumbs(ctx, [{ label: "Why the code is like this", href: "#/debug/code" }]);
  const host = moduleHost("calc(100vh - 200px)");
  host.style.minHeight = "560px";
  root.append(h("div", { class: "jp" }, ways("code"), host));
  ctx.mountModule("trace", host, { q: "when and why did money rounding change, and how does it work now?" });
}

export function failingPage(root, ctx) {
  crumbs(ctx, [{ label: "What is failing", href: "#/debug/failing" }]);
  const tone = { flaky: "warn", failing: "crit", fixed: "ok" };
  root.append(page({ title: "What is failing in shop", sub: "Tests by name, with flaky ones found per test; the alerts that fired; the files agents keep correcting." },
    ways("failing"),
    h("div", { class: "jgrid2" },
      h("section", { class: "jc" }, h("h2", {}, "Tests"),
        h("div", { class: "jlist" }, failing.tests.map((t) => h("div", {}, h("div", { class: "jrow" }, h("span", { class: `jchip ${tone[t.state]}` }, t.state), h("code", { class: "lv-code" }, t.test), h("span", { class: "jnote", style: { marginLeft: "auto" } }, `${t.failed} of ${t.runs} failed · ${t.commit}`)),
          h("div", { class: "jk-sub" }, t.note, t.fixed ? [" · ", h("b", {}, "fixed on "), t.fixed] : null))))),
      h("aside", { class: "jstack" },
        h("section", { class: "jc" }, h("h2", {}, "Alerts that fired"), failing.incidents.map((i) => h("div", {}, h("div", { class: "jrow" }, h("span", { class: "jchip crit" }, i.severity), h("b", {}, i.title)), h("div", { class: "jnote" }, `rule: ${i.rule}`),
          h("div", { class: "jrow", style: { marginTop: "8px" } }, btn("Open", { sm: true, primary: true }), btn("Resolve", { sm: true, ghost: true })))),
          h("a", { class: "lv-link", href: "#/debug/failing" }, `Alert rules (${failing.alerts}) →`)),
        h("section", { class: "jc" }, h("h2", {}, "Hotspots"), failing.hotspots.map((x) => h("div", { class: "ju-k" }, h("b", { class: "mono" }, x.path), h("span", { class: "muted" }, ` score ${x.score}: `), x.why.join(", "))))))));
}

useStyle("j-debug", `
.jd-ways{display:flex;align-items:center;gap:6px;margin:-6px 0 16px}
.jd-ways a{font-size:13px;color:var(--text-2);text-decoration:none;border:1px solid var(--line-2);border-radius:99px;padding:4px 12px}
.jd-ways a.on{border-color:var(--accent);background:var(--accent-soft);color:var(--accent);font-weight:600}
.jd-s{display:flex;align-items:center;gap:10px}
.jd-s.cur{background:var(--accent-soft);border-radius:8px;padding-left:8px !important}
.jd-t{display:grid;grid-template-columns:28px minmax(0,1fr) auto;gap:10px;align-items:center;color:var(--text);text-decoration:none}
.jd-t.bad{background:color-mix(in srgb,var(--crit-soft) 60%,transparent);border-radius:8px;padding-left:8px !important;padding-right:8px !important}
.jd-tn{display:inline-grid;place-items:center;width:26px;height:26px;border-radius:50%;background:var(--surface-3);font-weight:700;font-size:12px}
.jd-tm{gap:12px;font-size:12px;color:var(--muted);margin-top:4px}
.jd-grid{display:grid;grid-template-columns:1fr 1.1fr 1.1fr;gap:14px;align-items:start}
.jd-prompt{display:flex;gap:8px;align-items:flex-start;font-size:14px}
.jd-r{display:flex;flex-direction:column;gap:4px}
.jd-r.miss{background:var(--warn-soft);border:1px solid var(--warn-line) !important;border-radius:10px;padding:10px !important;margin-bottom:6px}
.jd-score{font:600 11.5px var(--mono);color:var(--text-2)}
.jd-rt{font-size:13.5px}
.jd-why{color:var(--warn-ink);display:flex;align-items:center;gap:4px}
.jd-missed{border-color:var(--warn-line)}
.jd-blast{display:flex;flex-direction:column;gap:10px;margin-bottom:14px}
.jd-rank{list-style:none;margin:0;padding:0;display:grid;grid-template-columns:repeat(5,minmax(0,1fr));gap:8px}
.jd-rank li{display:grid;grid-template-columns:auto minmax(0,1fr);gap:0 8px;align-items:baseline;border:1px solid var(--line);border-radius:10px;padding:8px 10px;background:var(--surface-2)}
.jd-rn{font-size:20px;font-weight:700;grid-row:span 2}
.jd-rw{font-weight:600;font-size:13px}.jd-rx{color:var(--text-2);font-size:12px;grid-column:2}
.jd-rank li.crit{border-color:var(--crit-line,var(--crit));background:var(--crit-soft)}.jd-rank li.crit .jd-rn{color:var(--crit-ink)}
.jd-rank li.warn{border-color:var(--warn-line);background:var(--warn-soft)}.jd-rank li.warn .jd-rn{color:var(--warn-ink)}
.jd-rank li.ok .jd-rn{color:var(--good-ink)}
.jd-note{display:flex;align-items:center;gap:6px;padding:10px 14px;font-size:13px;color:var(--warn-ink);background:var(--warn-soft);border-top:1px solid var(--warn-line)}
.jd-fix{display:flex;flex-direction:column;gap:8px}
.jd-f{display:grid;grid-template-columns:auto 1fr;gap:2px 8px;border:1px solid var(--line-2);border-radius:10px;padding:9px 11px;cursor:pointer}
.jd-f input{grid-row:span 2;margin-top:3px}.jd-f span{color:var(--text-2);font-size:12.5px}
.jd-f.on{border-color:var(--accent);background:var(--accent-soft)}
.jd-send{display:flex;flex-direction:column;gap:6px;margin-top:6px;padding-top:10px;border-top:1px solid var(--line)}
.jd-send textarea{border:1px solid var(--line-2);border-radius:8px;padding:8px;background:var(--surface);color:var(--text);font-size:13px;resize:vertical}
`);
