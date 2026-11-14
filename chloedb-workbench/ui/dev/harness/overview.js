// Optimize the harness: the overview (HARNESS.md, "Screens", 1), the
// suggestions to approve (the phone's reduced harness), and where the goal
// plugs into the rest of Workbench.
import { lbl, btn, gauge, head, lv, kc, ic, chip, st, ticks } from "./dom.js";
import { findings, suggestions } from "./fixtures.js";

/** The week's scorecard, live from ChloeDB (no replay): six gauges. */
export function scorecard() {
  return `<div class="bank g6">
    ${gauge({ label: "Done by checks", v: "9/13", unit: "sessions", on: 9, total: 13, delta: "−1 vs last week", dtone: "bad" })}
    ${gauge({ label: "Interventions", v: "3.1", unit: "a session", on: 15, total: 24, delta: "41 in all, 31 of them prompts", dtone: "" })}
    ${gauge({ label: "Prompts", v: "31", unit: "22 one command", on: 22, total: 31, tone: "warn", delta: "+12 vs last week", dtone: "bad" })}
    ${gauge({ label: "Wall time", v: "14m", unit: "median", on: 14, total: 30, delta: "41m lost to the harness", dtone: "warn" })}
    ${gauge({ label: "Tokens", v: "1.92M", unit: "$14.60", on: 19, total: 30, delta: "210k never read back", dtone: "warn" })}
    ${gauge({ label: "Memory missed", v: "2", unit: "turns", on: 2, total: 24, tone: "bad", delta: "under the threshold", dtone: "" })}
  </div>`;
}

function findingsTable() {
  return `<div class="pane"><table class="tbl find">
    <tr><th>What</th><th>Where</th><th class="r">Sessions</th><th class="r">Cost</th><th class="r dk">Evidence</th></tr>
    ${findings.map((f) => `<tr><td><span class="fk ${f.tone}">${f.kind}</span></td><td><div class="fw">${f.what}</div><div class="fa">${f.agents}</div></td><td class="r num">${f.sessions}</td><td class="r num ${f.tone ? "t-" + f.tone : ""}">${f.cost}</td><td class="r dk">${lv(f.level)}</td></tr>`).join("")}
  </table></div>`;
}

/** A suggestion as an inspector card: its change, evidence, impact and level. */
export function sugCard(s, { approve = false, i = 0 } = {}) {
  const res = s.result === "won" ? `<span class="res won">${ic("check")}won · ${s.res}</span>` : s.result === "inconclusive" ? `<span class="res inc">inconclusive · ${s.res}</span>` : "";
  return `<div class="q sg">
    <div class="hd"><span class="lbl" style="color:var(--text-2)">${s.part}</span>${s.widens ? `<span class="flag">${ic("tri")}widens</span>` : ""}<span class="t">${lv(s.level)}</span></div>
    <div class="ask">${s.title}</div>
    <div class="cmd diffw">${s.diff.map(([g, t]) => `<div class="${g === "+" ? "a" : "d"}"><span class="k">${g}</span> ${t}</div>`).join("")}</div>
    <div class="what">${s.evidence}</div>
    <div class="imp">${s.impact}</div>
    ${approve ? `<div class="what" style="padding-bottom:4px">${s.alt}</div>${res ? `<div style="padding:8px 14px 0">${res}</div>` : ""}` : ""}
    <div class="opts">
      ${approve
        ? (s.widens ? `<a class="opt first" href="#" data-act="approve"><span class="kc">1</span>Approve: it widens what agents may do</a>` : `<a class="opt first" href="#" data-act="approve"><span class="kc">1</span>Keep it in 1.5</a>`) +
          `<a class="opt" href="#/harness/run/7/results"><span class="kc">2</span>See the evidence</a><a class="opt" href="#" data-act="dismiss"><span class="kc">3</span>Not now</a>`
        : `<a class="opt first" href="#/harness/run/7"><span class="kc">${i + 1}</span>Test it on past sessions</a>`}
    </div>
  </div>`;
}

const modes = (on = "suggest") => `<div class="seg" role="radiogroup" aria-label="Auto-learn">
  ${[["suggest", "Suggest"], ["auto", "Auto within limits"], ["nightly", "Nightly"]].map(([k, l]) => `<a class="${k === on ? "on" : ""}" href="#" data-act="mode" data-k="${k}">${l}</a>`).join("")}
</div>`;

export function overview() {
  return {
    crumb: "shop|harness",
    content: `${head({ kicker: "Optimize the harness · shop · 1.4", title: "The harness cost you 41 minutes this week", sub: "From 14 sessions in ChloeDB: one test command asked 22 times, a decision missed at the turn it mattered, a redo loop ChloeCtx had the fix for. 3 changes are ready to test on those sessions.", tabs: "overview" })}
      <section class="sec">${lbl("This week, live", { right: `<span class="num" style="color:var(--text-2)">Nov 8 – Nov 14 · 14 sessions · 1.3 and 1.4</span>` })}${scorecard()}</section>
      <section class="sec">${lbl("Where the harness cost you", { n: findings.length, link: "Worst first" })}${findingsTable()}</section>
      <section class="sec">${lbl("Agents by version")}
        <div class="pane"><table class="tbl">
          <tr><th>Agent</th><th>State</th><th>Harness</th><th class="dk">Started</th></tr>
          <tr><td><b>freeship</b></td><td>${st("work", "working")}</td><td><code>1.4</code> <span class="mut">a3f9c21</span></td><td class="dk num">07:23</td></tr>
          <tr><td><b>paybutton</b></td><td>${st("need", "needs you")}</td><td><code>1.4</code> <span class="mut">a3f9c21</span></td><td class="dk num">07:14</td></tr>
          <tr><td><b>cents</b></td><td>${st("ready", "ready")}</td><td><code>1.4</code> <span class="mut">a3f9c21</span></td><td class="dk num">07:12</td></tr>
          <tr><td><b>flakehunt</b></td><td>${st("ready", "ready")}</td><td><code>1.4</code> <span class="mut">a3f9c21</span></td><td class="dk num">07:16</td></tr>
          <tr><td><b>checkout-v2</b></td><td>${st("ready", "ready")}</td><td><code>1.3</code> <span class="mut">5d20e4b · started before 1.4</span></td><td class="dk num">Nov 12</td></tr>
        </table></div></section>`,
    insp: `${lbl("Suggestions", { n: 3, link: "All", href: "#/harness/approve" })}
      ${suggestions.slice(0, 2).map((s, i) => sugCard(s, { i })).join("")}
      <a class="more" href="#/harness/approve">+1 more: ${suggestions[2].title.toLowerCase()}</a>
      <div>${lbl("Auto-learn", { strong: true })}<div style="margin-top:10px">${modes()}</div>
        <p class="note">Suggest only: nothing changes until you keep it. A change that widens what agents may do always waits for you, in every mode.</p></div>
      <div class="dock">
        ${btn("Test 3 changes on 10 sessions", { pri: true, k: "⌘↵", href: "#/harness/run/7" })}
        <div class="est"><span>about <b class="num">$6.80</b> of <b class="num">$8.00</b></span><span><b class="num">18 min</b> · 60 runs · sandboxed</span></div>
      </div>`,
    mdock: btn("Test 3 changes", { pri: true, href: "#/harness/run/7" }),
  };
}

/** The phone's harness: the suggestions to approve, one decision each. */
export function approve() {
  return {
    crumb: "shop|harness|suggestions",
    content: `${head({ kicker: "Optimize the harness · run 7 · done 07:52", title: "2 changes won. Keep them?", sub: "Tested on 10 of your sessions, 3 runs each, 4 of them held out. The third change is inside the noise." })}
      <section class="sec">${lbl("Scorecard, 1.4 → 1.5-rc", { link: "Results", href: "#/harness/run/7/results" })}
        <div class="bank g3 sm">
          ${gauge({ label: "Success", v: "8/10", on: 8, total: 10, delta: "was 6/10", dtone: "good" })}
          ${gauge({ label: "Prompts", v: "0.4", on: 1, total: 13, delta: "was 2.6", dtone: "good" })}
          ${gauge({ label: "Held-out", v: "0", on: 0, total: 4, delta: "regressions", dtone: "good" })}
        </div></section>
      <section class="sec">${lbl("Suggestions", { n: 3 })}<div class="stackq">${suggestions.map((s) => sugCard(s, { approve: true })).join("")}</div></section>`,
    insp: `${lbl("Keeping", { strong: true })}
      <div class="kept"><div>${ic("check", "color:var(--good)")}<span><b>Allow PYTHONPATH pytest</b><small>waits for your approval: it widens</small></span></div>
      <div>${ic("check", "color:var(--good)")}<span><b>Pin your decisions to files</b><small>no permission change</small></span></div>
      <div>${ic("x", "color:var(--text-3)")}<span><b>Halve the briefing</b><small>inconclusive; run 5 more ($4.10)</small></span></div></div>
      <div class="dock">${btn("Approve and promote 1.5", { pri: true, k: "⌘↵", href: "#/harness/versions" })}${btn("Save as a candidate", { href: "#/harness/versions" })}</div>`,
    mdock: `${btn("Not now", { href: "#/harness" })}${btn("Approve and promote 1.5", { pri: true, href: "#/harness/versions" })}`,
    tab: "home",
  };
}

/** The phone's scorecard (the overview reflowed). */
export function phoneScore() {
  const o = overview();
  return { ...o, content: o.content };
}

/** Where the goal plugs in: Home's sixth goal, Continue's chip, Debug's Fix, an agent's version. */
export function plugs() {
  const goal = (cls, label, key, title, text, fact) => `<a class="goal${cls}" href="#/harness"><span class="k"><span class="lbl">${label}</span>${kc(key)}</span><b>${title}</b><p>${text}</p><span class="fact">${fact}</span></a>`;
  return {
    crumb: "Home",
    content: `${head({ kicker: "Where it plugs in", title: "Good morning.", sub: "2 things need you, 3 changes are ready to review. The harness has 3 suggestions, tested overnight." })}
      <div class="bank goals six">
        ${goal(" now", "Pick up", "G", "Where you left off", "cents finished, freeship has a question, 1 alert fired.", "6 events since 07:20")}
        ${goal("", "Review", "R", "Review and ship", "Ready to merge? In which order?", "3 ready")}
        ${goal("", "Why", "W", "Find out why", "What an agent knew, what it missed.", "freeship · turn 2")}
        ${goal(" new6", "Optimize", "H", "Optimize the harness", "Find what cost time, test a fix on past sessions, keep what wins.", "3 suggestions")}
        ${goal("", "Work", "O", "Work on a project", "Open, understand, change.", "shop · invoicer")}
        ${goal("", "New", "N", "Start a project", "From an empty folder or an idea.", "git init + plan")}
      </div>
      <section class="sec">${lbl("Continue shop: the header")}
        <div class="pane" style="padding:16px 18px"><div class="lbl">Work on a project · shop</div><div class="h3" style="margin-top:4px">Since you left at 07:20: cents finished its fix, paybutton waits on you.</div>
        <div class="chips"><span class="chip">main</span><span class="chip">5 agents</span><a class="chip hx" href="#/harness">harness 1.4 · 3 suggestions</a></div></div></section>
      <section class="sec">${lbl("An agent records the harness it ran")}
        <div class="pane"><table class="tbl">
          <tr><th>Agent</th><th>State</th><th>Harness</th><th class="r">Turn</th></tr>
          <tr><td><b>freeship</b></td><td>${st("work", "working")}</td><td><code>1.4 a3f9c21</code></td><td class="r num">3</td></tr>
          <tr><td><b>checkout-v2</b></td><td>${st("ready", "ready")}</td><td><code>1.3 5d20e4b</code> <span class="mut">· 1.4 is newer</span></td><td class="r num">186</td></tr>
        </table></div></section>`,
    insp: `${lbl("Find out why · Fix", { strong: true })}
      <div class="fix on"><span class="box">${ic("check", "width:12px;height:12px")}</span><div><b>Pin the record to cart.py</b><span>Given whenever an agent edits cart.py, whatever its score.</span></div></div>
      <div class="fix on"><span class="box">${ic("check", "width:12px;height:12px")}</span><div><b>Make it a rule</b><span>Every agent of shop is told it at session start.</span></div></div>
      <div class="hand">${ic("spark")}<div><b>Both are harness changes.</b> Apply makes harness 1.5-draft with them, opens it in the Editor with their impact on the last 7 days, and offers to test it on the sessions that missed the same record.</div></div>
      <div class="dock">${btn("Apply and send freeship back", { pri: true, k: "⌘↵" })}${btn("Open in the harness editor", { href: "#/harness/edit/memory" })}</div>`,
    tab: "home",
  };
}

export { ticks, chip };
