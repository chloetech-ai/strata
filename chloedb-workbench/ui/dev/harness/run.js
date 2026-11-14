// Optimize run 7 (HARNESS.md, "The optimize loop" and "Reproduce and
// replay"): the sessions replayed under 1.4 (A) and 1.5-rc (B), live
// scores, the sandbox of the replay in progress; then the results, A
// against B, with the divergence of one session's runs.
import { lbl, btn, gauge, head, lv, cells, ic, ticks } from "./dom.js";
import { runRows, score, suggestions } from "./fixtures.js";

const loop = (at) => {
  const s = ["Find", "Propose", "Reproduce", "Score", "Keep"];
  return `<nav class="loop">${s.map((x, i) => `<span class="${i < at ? "done" : i === at ? "now" : ""}"><b class="num">${i + 1}</b>${x}</span>`).join(`<i></i>`)}</nav>`;
};

export function run() {
  const rows = runRows.map((r) => `<tr class="${r.s === "freeship" ? "cur" : ""}">
      <td><b>${r.s}</b><div class="fa">${r.task}</div></td>
      <td><span class="set ${r.set === "held-out" ? "ho" : ""}">${r.set}</span></td>
      <td class="r num dk">${r.turns}</td>
      <td>${cells(r.a.padEnd(3, "q"))}</td>
      <td>${cells(r.b.padEnd(3, "q"))}</td>
      <td class="r num ${/pass|−/.test(r.live) ? "t-good" : ""}">${r.live || '<span class="mut">–</span>'}</td></tr>`).join("");
  return {
    crumb: "shop|harness|optimize|run 7",
    content: `${head({ kicker: "Optimize the harness · run 7 · started 07:31", title: "Reproducing 10 sessions under 1.4 and 1.5-rc", sub: "Each session runs again from its original commit, in a sandbox worktree, with your prompts and answers played from the log. 3 runs a version. 4 sessions are held out: the changes were proposed without them.", tabs: "optimize" })}
      ${loop(2)}
      <div class="bank g4" style="margin-top:20px">
        ${gauge({ label: "Runs", v: "40", unit: "of 60, 2 running", on: 16, total: 24 })}
        ${gauge({ label: "Spent", v: "$3.10", unit: "of $8.00 cap", on: 9, total: 24 })}
        ${gauge({ label: "Elapsed", v: "11m", unit: "about 7m left", on: 15, total: 24 })}
        ${gauge({ label: "Noise floor", v: "±1.6", unit: "turns, A against A", on: 3, total: 24, delta: "from the 3 runs of 1.4 on each session" })}
      </div>
      <section class="sec">${lbl("Sessions", { n: 10, right: `<span class="legend">${cells("p")} pass ${cells("f")} fail ${cells("r")} running ${cells("q")} queued</span>` })}
        <div class="pane"><table class="tbl runs">
          <tr><th>Session</th><th>Set</th><th class="r dk">Turns</th><th>A · 1.4</th><th>B · 1.5-rc</th><th class="r">So far</th></tr>
          ${rows}
          <tr class="skip"><td><b>checkout-v2</b><div class="fa">186 turns: about $9.40 a run, over the cap. Its first 20 turns are a case of their own in Evals.</div></td><td><span class="set">skipped</span></td><td class="r num dk">186</td><td>${cells("sss")}</td><td>${cells("sss")}</td><td class="r mut">–</td></tr>
        </table></div></section>
      <section class="sec">${lbl("Scores so far", { right: '<span class="mut num">20 of 30 B runs to go</span>' })}
        <div class="pane"><table class="tbl cmp">
          <tr><th>Metric</th><th class="r">A · 1.4</th><th class="r">B · 1.5-rc</th><th class="r">Δ</th><th>So far</th></tr>
          <tr><td>Runs passing the checks</td><td class="r num">21 of 30</td><td class="r num">9 of 10</td><td class="r num t-good">+20 pts</td><td><span class="mut">too early: 10 runs</span></td></tr>
          <tr><td>Permission prompts, a session</td><td class="r num">2.6</td><td class="r num">0.3</td><td class="r num t-good">−2.3</td><td>${lv("counted")}</td></tr>
          <tr><td>Wall time, median</td><td class="r num">14m 10s</td><td class="r num">11m 05s</td><td class="r num t-good">−3m 05s</td><td><span class="mut">too early</span></td></tr>
        </table></div></section>`,
    insp: `${lbl("Now replaying", { right: '<span class="pulse"></span>' })}
      <div class="q">
        <div class="hd"><b>freeship</b>B · run 3 · turn 2 of 3<span class="t">01:42</span></div>
        <div class="cmd log">${[
          ["07:42:10", "prompt", `“Go ahead: shipping line first, with tests.” <span class="mut">yours, played</span>`],
          ["07:42:11", "recall", `given 3 · <b class="g">pinned</b> “Shipping is not taxed”`],
          ["07:42:40", "Edit", "cart.py:25"],
          ["07:43:02", "Bash", `PYTHONPATH=. python3 -m pytest -q <span class="g">allowed by 1.5-rc</span> <span class="mut">(1.4 asked)</span>`],
          ["07:43:09", "tests", "14 pass"],
        ].map(([t, k, x]) => `<div><span class="k">${t}</span><b>${k}</b><span>${x}</span></div>`).join("")}</div>
        <div class="what">Model live (sonnet). Tool calls live in the sandbox; none matched the recording after turn 2, so none were played back.</div>
        <div class="opts"><a class="opt" href="#/harness/run/7"><span class="kc">T</span>Watch its terminal</a></div>
      </div>
      <div>${lbl("The sandbox", { strong: true })}
        <ul class="safe">
          <li>${ic("check")}<span>A worktree at <code>033fb7f</code>, cloned without a remote: nothing can be pushed.</span></li>
          <li>${ic("check")}<span>A copy of ChloeCtx's store as of 07:23. Replays never write your memory.</span></li>
          <li>${ic("check")}<span>Guarded commands denied, as for any agent. Its server on port <code>8213</code>.</span></li>
          <li>${ic("check")}<span>Cap <code>$8.00</code> for the run; a run over <code>$0.60</code> stops.</span></li>
          <li>${ic("tri", "color:var(--warn)")}<span>1.5-rc widens one permission. You approved trying it at 07:29.</span></li>
        </ul></div>
      <div>${lbl("Not in the log", { n: 1, strong: true })}
        <p class="note" style="margin-top:8px">totals B run 2 asked something the original never did: <code>python3 -m pip list</code>. Played as you answered such prompts before (no), and counted as an intervention.</p></div>
      <div class="dock">${btn("Stop the run", { k: "⌘." })}${btn("See the results so far", { ghost: true, href: "#/harness/run/7/results" })}</div>`,
    mdock: btn("Stop the run", {}),
  };
}

export function results() {
  const verdict = { win: `<span class="vd win">${ic("check")}better</span>`, noise: `<span class="vd noise">inside the noise</span>`, loss: `<span class="vd loss">worse</span>` };
  const track = (blocks) => `<div class="track">${blocks.map(([flex, t, s, cls]) => `<span class="blk ${cls || ""}" style="flex:${flex}"><b>${t}</b><span class="${cls === "bad" ? "bad" : ""}">${s}</span></span>`).join("")}</div>`;
  return {
    crumb: "shop|harness|optimize|run 7|results",
    content: `${head({ kicker: "Optimize the harness · run 7 · 60 runs · $7.10 · 21 min", title: "1.5-rc wins on 7 of 10 sessions, ties 3, loses none", sub: "On the 4 held-out sessions it wins 2 and ties 2. Keep 2 of the 3 changes: the shorter briefing saved tokens inside the noise.", tabs: "optimize" })}
      ${loop(4)}
      <section class="sec">${lbl("Scorecard", { right: '<span class="mut num">10 sessions × 3 runs · 90% intervals, paired by session</span>' })}
        <div class="pane"><table class="tbl cmp">
          <tr><th>Metric</th><th class="r">A · 1.4</th><th class="r">B · 1.5-rc</th><th class="r">Δ</th><th class="r dk">Interval</th><th class="r">Verdict</th></tr>
          ${score.map((m) => `<tr><td>${m.k}<div class="fa">${m.sub}</div></td><td class="r num">${m.a}</td><td class="r num">${m.b}</td><td class="r num ${m.v === "win" ? "t-good" : ""}">${m.d}</td><td class="r num dk mut">${m.ci}</td><td class="r">${verdict[m.v]}</td></tr>`).join("")}
        </table></div></section>
      <section class="sec">${lbl("Divergence · freeship, A run 1 against B run 1", { link: "Pick two runs" })}
        <div class="pane tl div">
          <div class="dvrow"><span class="dvk">A · 1.4</span>${track([[3, "1 · Explored", "21.8k tok · 3 given"], [3.5, "2 · Added the shipping line", "1 missed · taxed shipping", "bad"], [2.2, "3 · Tests: 2 fail", "test_total_shown"], [2.2, "4 · Redo total()", "asked you twice"], [1.6, "5 · Stopped", "checks fail"]])}</div>
          <div class="dvmid"><span class="dvk"></span><span class="dvline"><i style="left:22%"></i><b style="left:22%">turn 2: the first place they differ</b></span></div>
          <div class="dvrow"><span class="dvk">B · 1.5-rc</span>${track([[3, "1 · Explored", "21.9k tok · 3 given"], [3.5, "2 · Added the shipping line", "3 given · 1 pinned", "on"], [2.6, "3 · FREESHIP", "14 pass"], [3.4, "", "", "idle"]])}</div>
        </div>
        <div class="bank two" style="margin-top:12px">
          <div><div class="lbl">What differed in its context at turn 2</div>
            <div class="rec"><div class="score"><span class="v">0.58</span><span class="bar"><i style="width:58%"></i></span>DECISION · PINNED TO cart.py</div><p style="font-weight:500">Shipping is not taxed: tax applies to the items only.</p><div class="note">Given in B (the pin), left out in A (under 0.60). Everything else in the two contexts was the same, to the token.</div></div></div>
          <div><div class="lbl">What it did with it · cart.py</div>
            <div class="hunk" style="margin-top:8px"><div class="mut">A</div><div class="add"><span class="ln">25</span>+ taxable = self.subtotal() + self.shipping()</div><div class="mut" style="margin-top:6px">B</div><div><span class="ln">25</span>  taxable = self.subtotal()</div><div class="add"><span class="ln">27</span>+ return taxable + tax + self.shipping()</div></div></div>
        </div></section>`,
    insp: `${lbl("Keep", { strong: true, right: '<span class="mut num">each change alone, on and off</span>' })}
      <div>
        <div class="fix on"><span class="box">${ic("check", "width:12px;height:12px")}</span><div><b>Allow the test command agents type</b><span>−2.2 prompts a session (counted). It widens: you approved it at 07:29 for testing, and keeping it asks you again.</span></div></div>
        <div class="fix on"><span class="box">${ic("check", "width:12px;height:12px")}</span><div><b>Pin your decisions to their files</b><span>freeship and refunds (held out) pass all 3 runs; no session lost a pass.</span></div></div>
        <div class="fix"><span class="box"></span><div><b>Halve the briefing</b><span>−9k tokens a session, interval −24k … +3k. Run 5 more a session for about $4.10.</span></div></div>
      </div>
      <div>${lbl("Honest about", { strong: true })}
        <ul class="safe">
          <li>${ic("dia")}<span>3 runs a session. pass³ counts a session only when all 3 pass.</span></li>
          <li>${ic("dia")}<span>2 moments were not in the log; both were played by your past answers.</span></li>
          <li>${ic("dia")}<span>test_promo_window is flaky (2 of 15): its check counts as passed only on 3 of 3.</span></li>
          <li>${ic("dia")}<span>checkout-v2 (186 turns) was not replayed.</span></li>
        </ul></div>
      <div class="dock">${btn("Make 1.5 from 2 changes", { pri: true, k: "⌘↵", href: "#/harness/versions" })}${btn("Run 5 more a session · $4.10", {})}</div>`,
    mdock: btn("Make 1.5 from 2 changes", { pri: true, href: "#/harness/versions" }),
  };
}

export { suggestions, ticks };
