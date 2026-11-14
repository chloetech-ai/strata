// Evals as rows (HARNESS.md, "Evals"): cases made from real sessions in one
// click, their checks, their results on each version, and "Make this a test".
import { lbl, btn, head, lv, ic, cells } from "./dom.js";

const CASES = [
  ["freeship-shipping-untaxed", "freeship · Nov 14 · turns 1–3", ["fail→pass test_total_shown", "pass→pass 12", "◆ given by turn 2"], "fff", "fff", "fff", "ppp", "$0.21"],
  ["cents-half-up", "cents · Nov 14 · 9 turns", ["fail→pass test_three_333_15pct", "pass→pass 11", "prompts ≤ 2"], "ppf", "ppp", "ppp", "ppp", "$0.33"],
  ["promo-window-stable", "flakehunt · Nov 14 · 5 turns", ["15 runs, 0 failures", "pass→pass 12"], "–", "ppf", "ppf", "ppp", "$0.18"],
  ["totals-per-line", "totals · Nov 9 · 11 turns", ["fail→pass test_line_tax ×3", "pass→pass 9"], "ppp", "ppp", "ppp", "ppp", "$0.47"],
  ["refunds-untaxed", "refunds · Nov 12 · 7 turns · held out", ["fail→pass test_refund_total", "◆ given by turn 3"], "–", "fff", "fpf", "ppp", "$0.29"],
  ["checkout-v2-first-20", "checkout-v2 · Nov 12 · turns 1–20", ["pass→pass 214", "orders table migrates"], "–", "pp", "–", "–", "$1.90"],
  ["no-push-ever", "made by hand · 3 prompts", ["no git push ran", "guarded: denied"], "ppp", "ppp", "ppp", "ppp", "$0.06"],
];

export function evals() {
  return {
    crumb: "shop|harness|evals|shop core",
    content: `${head({ kicker: "Optimize the harness · evals", title: "shop core: 12 cases made from your sessions", sub: "A case is a session played again: its commit, your prompts and answers, and the checks that say it worked. Each version runs them; a change is kept only when they do not get worse.", tabs: "evals" })}
      <div class="suites">${[["shop core", 12, true], ["held out", 4], ["permissions, counted", 31], ["memory, recomputed", 212]].map(([n, c, on]) => `<a class="${on ? "on" : ""}" href="#/harness/evals"><b>${n}</b><span class="num">${c}</span></a>`).join("")}<a href="#" class="new">+ Suite</a></div>
      <section class="sec">${lbl("Cases", { n: "7 of 12", right: '<span class="legend">pass³ on each version ' + cells("p") + cells("f") + " · – not run</span>" })}
        <div class="pane"><table class="tbl ev">
          <tr><th>Case</th><th>Checks</th><th class="c">1.2</th><th class="c">1.3</th><th class="c">1.4</th><th class="c">1.5-rc</th><th class="r dk">A run</th></tr>
          ${CASES.map(([n, from, checks, ...r]) => `<tr class="${n.startsWith("freeship") ? "cur" : ""}"><td><b class="mono" style="font-weight:500">${n}</b><div class="fa">${from}</div></td><td><div class="ck">${checks.map((c) => `<span>${c.replace("◆", '<i class="dm"></i>')}</span>`).join("")}</div></td>${r.slice(0, 4).map((x) => `<td class="c">${x === "–" ? '<span class="mut">–</span>' : cells(x)}</td>`).join("")}<td class="r num dk">${r[4]}</td></tr>`).join("")}
        </table></div></section>
      <section class="sec">${lbl("Runs of the suite", { n: 5 })}
        <div class="pane"><table class="tbl">
          <tr><th>When</th><th>Version</th><th class="r">Cases pass³</th><th class="r">Runs</th><th class="r">Cost</th><th class="dk">Why</th></tr>
          <tr><td class="num">today 07:52</td><td><code>1.5-rc</code></td><td class="r num t-good">11 of 12</td><td class="r num">36</td><td class="r num">$4.60</td><td class="dk mut">optimize run 7</td></tr>
          <tr><td class="num">today 07:52</td><td><code>1.4</code></td><td class="r num">8 of 12</td><td class="r num">36</td><td class="r num">$4.90</td><td class="dk mut">optimize run 7, the baseline</td></tr>
          <tr><td class="num">Nov 13 02:00</td><td><code>1.4</code></td><td class="r num">8 of 11</td><td class="r num">33</td><td class="r num">$4.20</td><td class="dk mut">nightly</td></tr>
          <tr><td class="num">Nov 12 09:14</td><td><code>1.4</code></td><td class="r num">8 of 10</td><td class="r num">30</td><td class="r num">$3.70</td><td class="dk mut">before promoting 1.4</td></tr>
          <tr><td class="num">Nov 8 18:40</td><td><code>1.3</code></td><td class="r num">7 of 9</td><td class="r num">27</td><td class="r num">$3.10</td><td class="dk mut">before promoting 1.3</td></tr>
        </table></div></section>`,
    insp: `${lbl("Make this a test", { strong: true, right: '<span class="mut num">from paybutton</span>' })}
      <div class="q">
        <div class="hd"><b>paybutton</b>today · turns 1–4 · harness 1.4<span class="t">waiting</span></div>
        <div class="ask" style="font-size:13px">The Pay button on the checkout page does nothing</div>
        <dl class="kv sm in"><dt>Starts at</dt><dd><code>033fb7f</code> on main, a clean worktree</dd>
          <dt>Plays</dt><dd>your task and 1 steer · 2 answers, matched by command</dd>
          <dt>Tools</dt><dd>live; the recording is kept for a cheaper replay</dd></dl>
        <div class="lbl" style="padding:10px 14px 0">Checks, proposed</div>
        <div class="cks">
          <label class="on">${ic("check")}<span><b>fail→pass</b> test_web::test_pay_click <small>paybutton wrote it at turn 4 and it fails at 033fb7f. A test an agent wrote counts once you have read it.</small></span></label>
          <label class="on">${ic("check")}<span><b>pass→pass</b> 12 tests that passed at its start</span></label>
          <label class="on">${ic("check")}<span><b>persona</b> “Maya pays for 2 items” on its preview <small>${lv("judged")}</small></span></label>
          <label>${ic("check")}<span><b>at most</b> 2 permission prompts</span></label>
        </div>
        <div class="what" style="padding-bottom:12px">Set: <b>train</b>. One case in three goes to held out, by session, never by turn.</div>
      </div>
      <div class="dock">${btn("Add to shop core", { pri: true, k: "⌘↵" })}${btn("Run it on 1.4 · $0.40", {})}</div>`,
    mdock: btn("Add to shop core", { pri: true }),
  };
}
