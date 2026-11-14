// The agent page (JOURNEYS.md, B3 "The agent page"): who it is, what it is
// for, its question first when it waits, then four tabs: Progress (the real
// story.js), Changes, Preview (only when it set one) and Terminal.
import { h, useStyle, ctxChip } from "../../levels/common.js";
import { icon, btn, avatar, questionCard, moduleHost, clock } from "./ui.js";
import { agents, A, needs, stateWords } from "./fixtures.js";
import { answered, needsList } from "./shell.js";

const TABS = [["progress", "Progress"], ["changes", "Changes"], ["preview", "Preview"], ["terminal", "Terminal"]];

export function agentPage(root, ctx, id, tab = "progress") {
  const a = agents.find((x) => x.id === id) || A.paybutton;
  const s = stateWords(a);
  ctx.crumbs([{ label: "Home", href: "#/" }, { label: "shop", href: "#/p/shop" }, { label: a.name, href: `#/a/${a.id}` }]);
  const q = needsList().find((x) => x.agent && x.agent.id === a.id);
  const tabs = TABS.filter(([k]) => k !== "preview" || a.preview_url);
  const body = h("div", { class: "ja-body" });
  root.append(h("div", { class: "jp wide ja" },
    h("header", { class: "ja-head" },
      avatar(a, 40),
      h("div", { class: "ja-t" },
        h("div", { class: "jrow" }, h("h1", {}, a.name), h("span", { class: `jstate ${s.tone}` }, s.text)),
        h("div", { class: "ja-meta" }, h("code", { class: "lv-code" }, a.branch), " · ", a.model, " · started ", clock(a.created), a.diff ? [" · ", h("b", { style: { color: "var(--add-ink)" } }, `+${a.diff.added}`), " ", h("b", { style: { color: "var(--del-ink)" } }, `−${a.diff.removed}`)] : null)),
      h("p", { class: "ja-task" }, "“", a.task, "”"),
      h("div", { class: "ja-acts" }, s.group === "ready" ? btn("Review and ship", { primary: true, href: `#/p/shop/ship/${a.id}`, iconName: "ship" }) : null,
        btn("Stop", { sm: true, iconName: "stop" }), btn("", { sm: true, ghost: true, iconName: "down", cls: "icon" }))),
    q ? h("div", { class: "ja-q" }, questionCard(q, { agentLink: false, onAnswer: () => { answered(q.id); location.hash = `#/a/${a.id}`; } })) : null,
    h("div", { class: "ja-tabs", role: "tablist" }, tabs.map(([k, label]) => h("a", { role: "tab", href: `#/a/${a.id}${k === "progress" ? "" : "/" + k}`, "aria-selected": String(k === tab), class: k === tab ? "on" : null },
      k === "terminal" ? icon("term", "ji sm") : null, label, k === "changes" && a.diff ? h("span", { class: "ja-n" }, `+${a.diff.added} −${a.diff.removed}`) : null))),
    body));
  if (tab === "progress") {
    const host = moduleHost(q ? "calc(100vh - 470px)" : "calc(100vh - 230px)", "flat ja-story");
    host.style.minHeight = "420px";
    body.append(host);
    ctx.mountModule("story", host, { agent: a.id });
  } else if (tab === "changes") body.append(changes(a));
  else if (tab === "preview") body.append(preview(a));
  else body.append(terminal(a));
}

function changes(a) {
  const line = (n, sign, text) => h("div", { class: `ja-dl ${sign === "+" ? "add" : sign === "-" ? "del" : ""}` }, h("span", { class: "ja-ln" }, n || ""), h("span", { class: "ja-sg" }, sign), h("code", {}, text));
  return h("div", { class: "jgrid2" },
    h("section", { class: "jc", style: { padding: 0, overflow: "hidden" } },
      h("div", { class: "ja-file" }, h("b", { class: "mono" }, "web/index.html"), h("span", { class: "jnote" }, "+2 −1")),
      line(10, " ", "  <button id=\"pay\">Pay</button>"),
      line(11, "-", "  <script>document.getElementById('pay').onclick = () => undefinedFn();</script>"),
      line(11, "+", "  <script>document.getElementById('pay').onclick = () => submitPayment(total);</script>"),
      h("div", { class: "ja-dl cur add" }, h("span", { class: "ja-ln" }, "12"), h("span", { class: "ja-sg" }, "+"), h("code", {}, "  <p id=\"paid\" aria-live=\"polite\"></p>"), h("button", { class: "ja-why", type: "button" }, "Why?")),
      h("div", { class: "ja-file" }, h("b", { class: "mono" }, "tests/test_web.py"), h("span", { class: "jnote" }, "+7")),
      line(1, "+", "def test_pay_submits(page):"),
      line(2, "+", "    page.click('#pay')"),
      line(3, "+", "    assert page.text('#paid') == 'Payment of $51.40 submitted'")),
    h("aside", { class: "jc" },
      h("h2", {}, "Blast radius"),
      h("p", {}, h("span", { class: "jchip ok" }, "Low risk"), " 1 handler changed, no Python touched."),
      h("p", { class: "jnote" }, "Why? shows on the line under the cursor (", h("kbd", {}, "y"), "), not on every line."),
      btn("Review and ship", { sm: true, href: "#/p/shop/ship" })));
}

function preview(a) {
  return h("div", { class: "jgrid2" },
    h("section", { class: "jc ja-pv" },
      h("div", { class: "ja-pvbar" }, h("span", { class: "mono jnote" }, a.preview_url), h("span", { style: { flex: 1 } }),
        h("label", { class: "jchip" }, h("input", { type: "checkbox" }), "Ask for a change: click an element to tell the agent"),
        btn("Try it as a customer", { sm: true })),
      h("div", { class: "ja-shop" },
        h("h3", {}, "Checkout"),
        h("div", { class: "ja-li" }, h("span", {}, "Mug × 2"), h("span", {}, "$23.98")), h("div", { class: "ja-li" }, h("span", {}, "Tea, loose leaf × 1"), h("span", {}, "$23.50")),
        h("div", { class: "ja-li" }, h("input", { placeholder: "Coupon code" }), h("button", {}, "Apply")),
        h("div", { class: "ja-li tot" }, h("span", {}, "Total"), h("span", {}, "$51.40")),
        h("button", { class: "ja-pay" }, "Pay"))),
    h("aside", { class: "jc" }, h("h2", {}, "The page is yours to use"), h("p", {}, "Clicks go to the page. Turn on “Ask for a change” to point at an element and tell ", a.name, " what to change."), h("p", { class: "jnote" }, "No runtime errors since the fix. Its server: serve.py on 8123, stopped when the agent stops.")));
}

function terminal(a) {
  // On a phone the terminal is read-only until "Type", which opens an input bar with the keys a phone keyboard lacks.
  const typing = new URLSearchParams(location.search).get("type") === "1";
  const key = (k) => h("button", { class: "btn sm", type: "button" }, k);
  const bar = h("div", { class: "ja-tin", hidden: !typing || null },
    h("div", { class: "ja-tkeys" }, ["Esc", "Tab", "↑", "↓", "1", "2", "3", "Ctrl-C"].map(key)),
    h("div", { class: "ja-trow" }, h("input", { "aria-label": `Type to ${a.name}`, placeholder: `Type to ${a.name}…`, enterkeyhint: "send", autocapitalize: "off", autocorrect: "off", spellcheck: "false" }), btn("Send", { primary: true })));
  const mbar = h("div", { class: "ja-mro" }, icon("eye", "ji sm"), h("span", {}, h("b", {}, "Read only."), " Nothing you tap reaches ", a.name, "."),
    btn(typing ? "Done" : "Type", { primary: !typing, iconName: "kbd", onclick: (e) => { const open = bar.hidden; bar.hidden = !open; e.currentTarget.lastChild.textContent = open ? "Done" : "Type"; if (open) bar.querySelector("input").focus(); } }));
  return h("div", { class: `ja-termwrap${typing ? " typing" : ""}` },
    h("div", { class: "ja-kbar" }, icon("term", "ji sm"), h("b", {}, `Keys go to ${a.name}.`), " Workbench's keys are off while you type here. ", h("kbd", {}, "Ctrl"), h("kbd", {}, "b"), " then ", h("kbd", {}, "Esc"), " to leave."),
    mbar, bar,
    h("pre", { class: "ja-term" },
      "● I'll check that the page's script parses before editing the handler.\n\n",
      "● Bash(python3 - <<'EOF' …)\n\n",
      "  Do you want to proceed?\n",
      "  ❯ 1. Yes\n",
      "    2. Yes, and don't ask again for python3 - in this project\n",
      "    3. No, and tell Claude what to do differently (esc)\n"));
}

useStyle("j-agent", `
.ja{padding-top:16px}
.ja-head{display:grid;grid-template-columns:auto auto minmax(0,1fr) auto;align-items:center;gap:6px 16px}
.ja-t h1{margin:0;font-size:22px;font-weight:700}
.ja-t .jrow{gap:12px}
.ja-meta{color:var(--muted);font-size:12.5px;margin-top:2px}
.ja-task{margin:0;color:var(--text-2);font-size:13.5px;max-width:640px;border-left:2px solid var(--line-2);padding-left:12px}
.ja-acts{display:flex;gap:6px;align-items:center}
.ja-q{margin:14px 0 4px}
.ja-tabs{display:flex;gap:2px;border-bottom:1px solid var(--line);margin:14px 0 0}
.ja-tabs a{display:inline-flex;align-items:center;gap:6px;padding:9px 14px;color:var(--text-2);text-decoration:none;font-weight:600;font-size:13.5px;border-bottom:2px solid transparent;margin-bottom:-1px}
.ja-tabs a.on{color:var(--text);border-bottom-color:var(--accent)}
.ja-tabs a:last-child{margin-left:auto;color:var(--muted)}
.ja-n{font:600 11px var(--mono);color:var(--muted)}
.ja-body{padding-top:14px}
.ja-story{border-top:0}
.ja-story .sy-pick{display:none}
.ja-file{display:flex;gap:10px;align-items:center;padding:8px 12px;background:var(--surface-2);border-bottom:1px solid var(--line);border-top:1px solid var(--line)}
.ja-dl{display:grid;grid-template-columns:44px 18px minmax(0,1fr) auto;font:12.5px/1.7 var(--mono);align-items:center}
.ja-dl.add{background:var(--add-bg)}.ja-dl.del{background:var(--del-bg)}
.ja-dl.cur{box-shadow:inset 3px 0 0 var(--accent)}
.ja-ln{color:var(--muted);text-align:right;padding-right:8px}
.ja-sg{color:var(--muted)}
.ja-why{border:1px solid var(--line-2);background:var(--surface);border-radius:6px;font:600 11px var(--sans);padding:2px 8px;margin-right:8px;cursor:pointer;color:var(--accent)}
.ja-pv{padding:0;overflow:hidden}
.ja-pvbar{display:flex;align-items:center;gap:8px;padding:8px 12px;border-bottom:1px solid var(--line);background:var(--surface-2)}
.ja-shop{background:#fff;color:#222;padding:22px 26px;font:15px/1.5 Georgia,serif;min-height:360px}
.ja-shop h3{margin:0 0 12px;font-size:22px}
.ja-li{display:flex;justify-content:space-between;padding:6px 0;border-bottom:1px solid #eee;max-width:380px}
.ja-li.tot{font-weight:700}
.ja-pay{margin-top:14px;background:#1d6b3a;color:#fff;border:0;border-radius:6px;padding:8px 24px;font-size:15px}
.ja-termwrap{border:1px solid var(--term-line);border-radius:12px;overflow:hidden}
.ja-kbar{display:flex;align-items:center;gap:6px;padding:8px 12px;background:var(--accent-soft);color:var(--text);font-size:13px}
.ja-term{margin:0;background:var(--term-bg);color:var(--term-fg);font:13px/1.55 var(--mono);padding:14px 16px;min-height:420px}
`);
